use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::Value;

const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";
const SESSION_DIR_ENV: &str = "PI_CODING_AGENT_SESSION_DIR";
/// Pi's own limit on a session's first line.
const HEADER_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub cwd: String,
    #[serde(skip)]
    pub path: PathBuf,
    /// Its sessions sit in a flat folder among other folders' sessions, so
    /// only the files whose header names `cwd` are its own.
    #[serde(skip)]
    pub flat: bool,
}

#[derive(Clone, Debug)]
pub struct SessionFile {
    pub project: String,
    pub cwd: String,
    pub path: PathBuf,
    pub session: String,
    pub started_at: String,
    pub bytes: u64,
}

/// The folders pi files sessions in: `--root`, or `<agent dir>/sessions` plus
/// PI_CODING_AGENT_SESSION_DIR, the way pi picks them. Each holds a folder per
/// working folder, session files filed flat, or both.
pub fn sessions_roots(explicit: Option<PathBuf>) -> Result<Vec<PathBuf>> {
    let (primary, extra) = match explicit {
        Some(path) => (expand_tilde(&path.to_string_lossy(), home_dir)?, None),
        None => default_roots(|name| env::var(name).ok())?,
    };
    let roots = [Some(primary.clone()), extra.filter(|dir| *dir != primary)]
        .into_iter()
        .flatten()
        .filter(|root| root.is_dir())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        bail!("sessions directory does not exist: {}", primary.display());
    }
    Ok(roots)
}

fn default_roots(env: impl Fn(&str) -> Option<String>) -> Result<(PathBuf, Option<PathBuf>)> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let home = || var("HOME").map(PathBuf::from).context("HOME is not set");
    let agent = match var(AGENT_DIR_ENV) {
        Some(raw) => expand_tilde(&raw, home)?,
        None => home()?.join(".pi/agent"),
    };
    let flat = var(SESSION_DIR_ENV)
        .map(|raw| expand_tilde(&raw, home))
        .transpose()?;
    Ok((agent.join("sessions"), flat))
}

pub fn discover_projects(roots: &[PathBuf]) -> Result<Vec<Project>> {
    let mut projects = roots
        .iter()
        .map(|root| root_projects(root))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    projects.sort_unstable_by(|left, right| left.cwd.cmp(&right.cwd));
    Ok(projects)
}

fn root_projects(root: &Path) -> Result<Vec<Project>> {
    let (dirs, files) = root_entries(root)?;
    let flat = files
        .iter()
        .filter_map(|file| session_cwd(file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|cwd| project_with_cwd(root.to_owned(), cwd, true));
    Ok(dirs
        .into_par_iter()
        .map(project_from_dir)
        .collect::<Vec<_>>()
        .into_iter()
        .chain(flat)
        .collect())
}

/// The session files a command looks at: every project's, the current
/// folder's, or the selected ones. A selector that looks like a path picks
/// that folder and every folder inside it, judged by each session's header,
/// so it works for folders that no longer exist. Any other selector is a
/// path/name substring.
pub fn scope_files(
    roots: &[PathBuf],
    selectors: &[String],
    all_projects: bool,
) -> Result<Vec<SessionFile>> {
    if all_projects {
        return session_files(&discover_projects(roots)?);
    }
    if selectors.is_empty() {
        return cwd_scope(roots)?.with_context(|| {
            format!(
                "no sessions found for {}; pass --project NAME or --all-projects",
                env::current_dir().unwrap_or_default().display()
            )
        });
    }

    let (paths, words): (Vec<&String>, Vec<&String>) = selectors
        .iter()
        .partition(|selector| is_path_like(selector));
    let folders = paths
        .iter()
        .map(|raw| folder_spellings(raw, cwd_spellings))
        .collect::<Result<Vec<_>>>()?
        .concat();
    let projects = match words.is_empty() {
        true => Vec::new(),
        false => matching_projects(discover_projects(roots)?, &words),
    };
    let in_folders = match folders.is_empty() {
        true => Vec::new(),
        false => folder_files(roots, &folders)?,
    };
    if projects.is_empty() && in_folders.is_empty() {
        bail!("no project matched: {}", selectors.join(", "));
    }

    let mut files = session_files(&projects)?;
    let listed = files
        .iter()
        .map(|file| file.path.clone())
        .collect::<HashSet<_>>();
    files.extend(
        in_folders
            .into_iter()
            .filter(|file| !listed.contains(&file.path)),
    );
    sort_newest_first(&mut files);
    Ok(files)
}

/// Projects listed by `lookup projects`: path selectors keep the projects
/// in or under that folder, other selectors match the name or folder.
pub fn listed_projects(roots: &[PathBuf], selectors: &[String]) -> Result<Vec<Project>> {
    let projects = discover_projects(roots)?;
    if selectors.is_empty() {
        return Ok(projects);
    }
    let (paths, words): (Vec<&String>, Vec<&String>) = selectors
        .iter()
        .partition(|selector| is_path_like(selector));
    let folders = paths
        .iter()
        .map(|raw| folder_spellings(raw, cwd_spellings))
        .collect::<Result<Vec<_>>>()?
        .concat();
    let words = words
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>();
    Ok(projects
        .into_iter()
        .filter(|project| {
            inside(&project.cwd, &folders)
                || words.iter().any(|word| {
                    format!("{} {}", project.name, project.cwd)
                        .to_lowercase()
                        .contains(word)
                })
        })
        .collect())
}

pub fn session_files(projects: &[Project]) -> Result<Vec<SessionFile>> {
    let (flat, nested): (Vec<&Project>, Vec<&Project>) =
        projects.iter().partition(|project| project.flat);
    let mut files = nested
        .par_iter()
        .map(|project| files_for_project(project))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .chain(flat_project_files(&flat)?)
        .collect::<Vec<_>>();
    sort_newest_first(&mut files);
    Ok(files)
}

pub fn global_session_files(roots: &[PathBuf], selector: &str) -> Result<Vec<SessionFile>> {
    let selector = selector.to_lowercase();
    let wanted = |path: &PathBuf| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.to_lowercase().contains(&selector))
    };
    let mut files = Vec::new();
    for root in roots {
        let (dirs, flat) = root_entries(root)?;
        for dir in dirs {
            files.extend(
                session_paths(&dir)?
                    .into_iter()
                    .filter(wanted)
                    .map(|path| session_file(&project_from_session(dir.clone(), &path), path)),
            );
        }
        files.extend(
            flat.into_iter()
                .filter(wanted)
                .map(|path| session_file(&project_from_session(root.clone(), &path), path)),
        );
    }
    sort_newest_first(&mut files);
    Ok(files)
}

/// Every session id and working folder under the roots, one read of each
/// folder, for naming them uniquely in human output. Unreadable folders are
/// skipped: this only shapes display.
pub fn catalog(roots: &[PathBuf]) -> (Vec<String>, Vec<String>) {
    let (ids, cwds): (Vec<Vec<String>>, Vec<Vec<String>>) = roots
        .iter()
        .filter_map(|root| root_entries(root).ok())
        .flat_map(|(dirs, flat)| {
            let mut folders = dirs
                .par_iter()
                .filter_map(|dir| session_paths(dir).ok())
                .map(|paths| folder_catalog(&paths, 1))
                .collect::<Vec<_>>();
            folders.push(folder_catalog(&flat, flat.len()));
            folders
        })
        .unzip();
    (ids.concat(), cwds.concat())
}

/// A folder's session ids, and the working folders its first `headers`
/// files name: one for pi's per-folder layout, all of them for a flat one.
fn folder_catalog(paths: &[PathBuf], headers: usize) -> (Vec<String>, Vec<String>) {
    (
        paths
            .iter()
            .filter_map(|path| session_id_from_path(path))
            .collect(),
        paths
            .iter()
            .take(headers)
            .filter_map(|path| session_cwd(path))
            .collect(),
    )
}

pub fn current_session_file() -> Option<SessionFile> {
    let path = env::var_os("PI_SESSION_FILE").map(PathBuf::from)?;
    if !path.is_file() {
        return None;
    }
    let directory = path.parent()?.to_owned();
    let project = project_from_session(directory, &path);
    Some(session_file(&project, path))
}

pub fn current_session_id() -> Option<String> {
    env::var("PI_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env::var_os("PI_SESSION_FILE")
                .map(PathBuf::from)
                .and_then(|path| session_id_from_path(&path))
        })
}

/// The current folder's most recently written session: a guess for when pi
/// doesn't say which session is running.
pub fn active_session_path(roots: &[PathBuf]) -> Result<Option<PathBuf>> {
    Ok(cwd_scope(roots)?
        .unwrap_or_default()
        .into_iter()
        .map(|file| file.path)
        .max_by_key(|path| modified_at_ns(path)))
}

/// The current folder's sessions, under its physical path and, when the
/// shell came through a symlink, its logical one (`/tmp` and `/private/tmp`).
/// None when no session was ever filed there.
fn cwd_scope(roots: &[PathBuf]) -> Result<Option<Vec<SessionFile>>> {
    let spellings = cwd_spellings()?;
    let names = spellings
        .iter()
        .map(|spelling| encode_cwd(spelling))
        .collect::<BTreeSet<_>>();
    let projects = roots
        .iter()
        .flat_map(|root| names.iter().map(move |name| root.join(name)))
        .filter(|dir| dir.is_dir())
        .map(project_from_dir)
        .collect::<Vec<_>>();
    let flat = flat_files(roots, |cwd| {
        spellings.iter().any(|spelling| spelling == Path::new(cwd))
    })?;
    if projects.is_empty() && flat.is_empty() {
        return Ok(None);
    }
    let mut files = session_files(&projects)?;
    files.extend(flat);
    sort_newest_first(&mut files);
    Ok(Some(files))
}

fn cwd_spellings() -> Result<Vec<PathBuf>> {
    let physical = env::current_dir().context("failed to determine current directory")?;
    let logical = env::var_os("PWD")
        .map(PathBuf::from)
        .filter(|pwd| pwd.is_absolute() && *pwd != physical && same_folder(pwd, &physical));
    Ok([Some(physical), logical].into_iter().flatten().collect())
}

fn same_folder(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}

fn is_path_like(selector: &str) -> bool {
    matches!(selector, "." | ".." | "~")
        || ["/", "./", "../", "~/"]
            .iter()
            .any(|prefix| selector.starts_with(prefix))
}

/// A folder selector as written (made absolute against the current folder)
/// and, when it exists, resolved through symlinks.
fn folder_spellings(raw: &str, bases: impl Fn() -> Result<Vec<PathBuf>>) -> Result<Vec<PathBuf>> {
    let expanded = expand_tilde(raw, home_dir)?;
    let given = match expanded.is_absolute() {
        true => vec![normalize(&expanded)],
        false => bases()?
            .iter()
            .map(|base| normalize(&base.join(&expanded)))
            .collect(),
    };
    Ok(given
        .into_iter()
        .flat_map(|path| {
            let resolved = fs::canonicalize(&path).ok();
            [Some(path), resolved]
        })
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn normalize(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut normal, component| {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normal.pop();
                }
                other => normal.push(other.as_os_str()),
            }
            normal
        })
}

/// `cwd` is one of `folders` or inside one, component-wise: `…/5` holds
/// `…/5/src`, not `…/50`.
fn inside(cwd: &str, folders: &[PathBuf]) -> bool {
    folders
        .iter()
        .any(|folder| Path::new(cwd).starts_with(folder))
}

/// Sessions whose header puts them in one of `folders` or inside one. Pi's
/// folder names are lossy (`5/x` and `5-x` share one), so the name only
/// shortlists folders and each file's header decides.
fn folder_files(roots: &[PathBuf], folders: &[PathBuf]) -> Result<Vec<SessionFile>> {
    let keep = |cwd: &str| inside(cwd, folders);
    let shortlisted = |dir: &PathBuf| {
        dir.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| folders.iter().any(|folder| may_hold(name, folder)))
    };
    let nested = roots
        .iter()
        .map(|root| root_entries(root).map(|(dirs, _)| dirs))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .filter(shortlisted)
        .map(|dir| {
            Ok(session_paths(&dir)?
                .into_iter()
                .filter_map(|path| header_file(&dir, path, false, &keep))
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;
    let flat = flat_files(roots, keep)?;
    Ok(nested.into_iter().flatten().chain(flat).collect())
}

/// Pi's folder for `folder` or for a folder inside it.
fn may_hold(name: &str, folder: &Path) -> bool {
    if !folder
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return true;
    }
    let own = encode_cwd(folder);
    let stem = own.strip_suffix('-').unwrap_or(&own);
    name.starts_with(stem) && name.ends_with("--")
}

fn flat_files(roots: &[PathBuf], keep: impl Fn(&str) -> bool) -> Result<Vec<SessionFile>> {
    Ok(roots
        .iter()
        .map(|root| root_entries(root).map(|(_, files)| (root, files)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flat_map(|(root, files)| {
            files
                .into_iter()
                .filter_map(|path| header_file(root, path, true, &keep))
                .collect::<Vec<_>>()
        })
        .collect())
}

fn header_file(
    folder: &Path,
    path: PathBuf,
    flat: bool,
    keep: &impl Fn(&str) -> bool,
) -> Option<SessionFile> {
    let cwd = session_cwd(&path).filter(|cwd| keep(cwd))?;
    Some(session_file(
        &project_with_cwd(folder.to_owned(), cwd, flat),
        path,
    ))
}

fn matching_projects(projects: Vec<Project>, words: &[&String]) -> Vec<Project> {
    let words = words
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>();
    projects
        .into_iter()
        .filter(|project| {
            let haystack = match project.flat {
                true => project.cwd.to_lowercase(),
                false => format!("{} {}", project.cwd, project.path.display()).to_lowercase(),
            };
            words.iter().any(|word| haystack.contains(word))
        })
        .collect()
}

/// A root's folders (one per working folder) and the session files filed
/// flat in it.
fn root_entries(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (dirs, others): (Vec<_>, Vec<_>) = fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_type().ok().map(|kind| (kind, entry.path())))
        .partition(|(kind, _)| kind.is_dir());
    let files = others
        .into_iter()
        .filter(|(kind, path)| kind.is_file() && is_session_path(path))
        .map(|(_, path)| path)
        .collect();
    Ok((dirs.into_iter().map(|(_, path)| path).collect(), files))
}

fn session_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_session_path(path))
        .collect())
}

fn is_session_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

fn files_for_project(project: &Project) -> Result<Vec<SessionFile>> {
    Ok(session_paths(&project.path)?
        .into_iter()
        .map(|path| session_file(project, path))
        .collect())
}

/// A flat folder's files, read once per folder: each file goes to the
/// selected project its header names.
fn flat_project_files(projects: &[&Project]) -> Result<Vec<SessionFile>> {
    let by_folder = projects.iter().fold(
        BTreeMap::<&Path, Vec<&Project>>::new(),
        |mut folders, project| {
            folders.entry(&project.path).or_default().push(project);
            folders
        },
    );
    by_folder
        .into_iter()
        .map(|(folder, owners)| {
            let (_, files) = root_entries(folder)?;
            Ok(files
                .into_iter()
                .filter_map(|path| {
                    let cwd = session_cwd(&path)?;
                    owners
                        .iter()
                        .find(|owner| owner.cwd == cwd)
                        .map(|owner| session_file(owner, path))
                })
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()
        .map(|files| files.into_iter().flatten().collect())
}

fn sort_newest_first(files: &mut [SessionFile]) {
    files.sort_unstable_by(|left, right| right.started_at.cmp(&left.started_at));
}

fn session_file(project: &Project, path: PathBuf) -> SessionFile {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let (started_at, session) = stem.split_once('_').unwrap_or((&stem, &stem));
    let started_at = started_at.to_owned();
    let session = session.to_owned();
    let bytes = path.metadata().map_or(0, |metadata| metadata.len());
    SessionFile {
        project: project.name.clone(),
        cwd: project.cwd.clone(),
        path,
        session,
        started_at,
        bytes,
    }
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let session = stem.split_once('_').map_or(stem, |(_, session)| session);
    (!session.is_empty()).then(|| session.to_owned())
}

fn modified_at_ns(path: &Path) -> u128 {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos())
}

fn project_from_dir(path: PathBuf) -> Project {
    let cwd = first_session_cwd(&path).unwrap_or_else(|| decode_dir_name(&path));
    project_with_cwd(path, cwd, false)
}

fn project_from_session(directory: PathBuf, session: &Path) -> Project {
    let cwd = session_cwd(session).unwrap_or_else(|| decode_dir_name(&directory));
    project_with_cwd(directory, cwd, false)
}

fn project_with_cwd(path: PathBuf, cwd: String, flat: bool) -> Project {
    let name = Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&cwd)
        .to_owned();
    Project {
        name,
        cwd,
        path,
        flat,
    }
}

fn first_session_cwd(dir: &Path) -> Option<String> {
    let path = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| is_session_path(path))?;
    session_cwd(&path)
}

fn session_cwd(path: &Path) -> Option<String> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?.take(HEADER_LIMIT))
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str::<Value>(&line)
        .ok()?
        .get("cwd")?
        .as_str()
        .map(str::to_owned)
}

/// Pi's getDefaultSessionDirPath.
fn encode_cwd(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let rest = raw.strip_prefix(['/', '\\']).unwrap_or(&raw);
    format!("--{}--", rest.replace(['/', '\\', ':'], "-"))
}

fn decode_dir_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim_matches('-')
        .to_owned()
}

/// `~` and `~/…`, the way pi expands them.
fn expand_tilde(raw: &str, home: impl Fn() -> Result<PathBuf>) -> Result<PathBuf> {
    match raw.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => Ok(home()?.join(rest.trim_start_matches('/'))),
        _ => Ok(PathBuf::from(raw)),
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[cfg(test)]
mod tests {
    use super::{
        default_roots, discover_projects, encode_cwd, expand_tilde, folder_files, inside,
        is_path_like, may_hold, normalize, session_files, session_id_from_path,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn env(pairs: &'static [(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lookup-source-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(dir: &Path, stamp: &str, id: &str, cwd: &str) {
        fs::create_dir_all(dir).unwrap();
        let header = format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-10-15T05:53:43.794Z","cwd":"{cwd}"}}"#
        );
        fs::write(
            dir.join(format!("{stamp}_{id}.jsonl")),
            format!("{header}\n"),
        )
        .unwrap();
    }

    fn ids(files: &[super::SessionFile]) -> Vec<&str> {
        let mut ids = files
            .iter()
            .map(|file| file.session.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn encodes_pi_session_directory_name() {
        assert_eq!(
            encode_cwd(Path::new("/Users/me/code/demo")),
            "--Users-me-code-demo--"
        );
        assert_eq!(encode_cwd(Path::new(r"C:\w\a:b")), "--C--w-a-b--");
    }

    #[test]
    fn extracts_session_id_from_pi_filename() {
        assert_eq!(
            session_id_from_path(Path::new("/tmp/2026-09-01T00-00-00Z_01a04453-d395.jsonl")),
            Some("01a04453-d395".to_owned())
        );
    }

    #[test]
    fn roots_follow_pis_environment() {
        assert_eq!(
            default_roots(env(&[("HOME", "/h")])).unwrap(),
            (PathBuf::from("/h/.pi/agent/sessions"), None)
        );
        assert_eq!(
            default_roots(env(&[
                ("HOME", "/h"),
                ("PI_CODING_AGENT_DIR", "~/agent"),
                ("PI_CODING_AGENT_SESSION_DIR", "/t/s"),
            ]))
            .unwrap(),
            (
                PathBuf::from("/h/agent/sessions"),
                Some(PathBuf::from("/t/s"))
            )
        );
        assert_eq!(
            default_roots(env(&[
                ("PI_CODING_AGENT_DIR", "/a"),
                ("PI_CODING_AGENT_SESSION_DIR", ""),
            ]))
            .unwrap(),
            (PathBuf::from("/a/sessions"), None)
        );
        assert!(default_roots(env(&[])).is_err());
    }

    #[test]
    fn expands_tilde_like_pi() {
        let home = || Ok(PathBuf::from("/h"));
        assert_eq!(expand_tilde("~", home).unwrap(), PathBuf::from("/h"));
        assert_eq!(
            expand_tilde("~/x/y", home).unwrap(),
            PathBuf::from("/h/x/y")
        );
        assert_eq!(expand_tilde("~x", home).unwrap(), PathBuf::from("~x"));
        assert_eq!(expand_tilde("/abs", home).unwrap(), PathBuf::from("/abs"));
    }

    #[test]
    fn only_path_shaped_selectors_are_folders() {
        ["/x", "~", "~/x", ".", "..", "./x", "../x"]
            .iter()
            .for_each(|selector| assert!(is_path_like(selector), "{selector}"));
        ["x", ".forks", "~x", "pfs-cli/.forks/1", "code/pi"]
            .iter()
            .for_each(|selector| assert!(!is_path_like(selector), "{selector}"));
    }

    #[test]
    fn normalizes_paths_lexically() {
        assert_eq!(normalize(Path::new("/a/./b/../c/")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
    }

    #[test]
    fn folders_hold_themselves_and_their_subfolders_only() {
        let fork = [PathBuf::from("/w/repo/5")];
        assert!(inside("/w/repo/5", &fork));
        assert!(inside("/w/repo/5/src", &fork));
        assert!(!inside("/w/repo/50", &fork));
        assert!(!inside("/w/repo/5-x", &fork));
        assert!(!inside("/w/repo", &fork));
    }

    #[test]
    fn shortlists_pi_folders_that_may_hold_a_folder() {
        let fork = Path::new("/w/repo/5");
        assert!(may_hold("--w-repo-5--", fork));
        assert!(may_hold("--w-repo-5-src-ui--", fork));
        assert!(may_hold("--w-repo-5-x--", fork));
        assert!(!may_hold("--w-repo-50--", fork));
        assert!(!may_hold("--w-repo--", fork));
        assert!(!may_hold("--w-repo-5-", fork));
        assert!(may_hold("--anything--", Path::new("/")));
    }

    #[test]
    fn selects_a_folder_by_each_sessions_header() {
        let root = scratch("folders");
        let flat = scratch("folders-flat");
        write_session(
            &root.join("--w-repo-5--"),
            "2026-01-01T00-00-01Z",
            "a5",
            "/w/repo/5",
        );
        write_session(
            &root.join("--w-repo-5-src--"),
            "2026-01-01T00-00-02Z",
            "b5src",
            "/w/repo/5/src",
        );
        write_session(
            &root.join("--w-repo-5-x--"),
            "2026-01-01T00-00-03Z",
            "c5x",
            "/w/repo/5-x",
        );
        write_session(
            &root.join("--w-repo-50--"),
            "2026-01-01T00-00-04Z",
            "d50",
            "/w/repo/50",
        );
        write_session(
            &root.join("--w-repo--"),
            "2026-01-01T00-00-05Z",
            "erepo",
            "/w/repo",
        );
        write_session(&flat, "2026-01-01T00-00-06Z", "fflat5", "/w/repo/5");
        write_session(&flat, "2026-01-01T00-00-07Z", "gflatrepo", "/w/repo");
        let found = folder_files(&[root.clone(), flat.clone()], &[PathBuf::from("/w/repo/5")]);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&flat);
        let found = found.unwrap();
        assert_eq!(ids(&found), ["a5", "b5src", "fflat5"]);
        assert!(
            found
                .iter()
                .any(|file| file.session == "b5src" && file.cwd == "/w/repo/5/src")
        );
    }

    #[test]
    fn groups_flat_sessions_by_their_folder() {
        let root = scratch("flat");
        write_session(&root, "2026-01-01T00-00-01Z", "one", "/w/a");
        write_session(&root, "2026-01-01T00-00-02Z", "two", "/w/b");
        write_session(&root, "2026-01-01T00-00-03Z", "three", "/w/a");
        write_session(
            &root.join("--w-c--"),
            "2026-01-01T00-00-04Z",
            "four",
            "/w/c",
        );
        let projects = discover_projects(std::slice::from_ref(&root)).unwrap();
        let only_a = projects
            .iter()
            .filter(|project| project.cwd == "/w/a")
            .cloned()
            .collect::<Vec<_>>();
        let all = session_files(&projects);
        let a = session_files(&only_a);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            projects
                .iter()
                .map(|project| (project.cwd.as_str(), project.flat))
                .collect::<Vec<_>>(),
            [("/w/a", true), ("/w/b", true), ("/w/c", false)]
        );
        assert_eq!(ids(&all.unwrap()), ["four", "one", "three", "two"]);
        assert_eq!(ids(&a.unwrap()), ["one", "three"]);
    }
}
