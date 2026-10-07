use std::{
    env,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub cwd: String,
    #[serde(skip)]
    pub path: PathBuf,
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

pub fn sessions_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let root = match explicit {
        Some(path) => expand_home(path)?,
        None => home_dir()?.join(".pi/agent/sessions"),
    };
    if !root.is_dir() {
        bail!("sessions directory does not exist: {}", root.display());
    }
    Ok(root)
}

pub fn discover_projects(root: &Path) -> Result<Vec<Project>> {
    let mut projects = fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| project_from_dir(entry.path()))
        })
        .collect::<Vec<_>>();
    projects.sort_unstable_by(|left, right| left.cwd.cmp(&right.cwd));
    Ok(projects)
}

pub fn select_projects(
    root: &Path,
    selectors: &[String],
    all_projects: bool,
) -> Result<Vec<Project>> {
    let projects = discover_projects(root)?;
    if all_projects {
        return Ok(projects);
    }

    if selectors.is_empty() {
        let cwd = env::current_dir().context("failed to determine current directory")?;
        let expected = encode_cwd(&cwd);
        let selected = projects
            .into_iter()
            .filter(|project| {
                project
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == expected)
            })
            .collect::<Vec<_>>();
        if selected.is_empty() {
            bail!(
                "no sessions found for {}; pass --project NAME or --all-projects",
                cwd.display()
            );
        }
        return Ok(selected);
    }

    let selectors = selectors
        .iter()
        .map(|selector| selector.to_lowercase())
        .collect::<Vec<_>>();
    let selected = projects
        .into_iter()
        .filter(|project| {
            let haystack = format!("{} {}", project.cwd, project.path.display()).to_lowercase();
            selectors.iter().any(|selector| haystack.contains(selector))
        })
        .collect::<Vec<_>>();

    if selected.is_empty() {
        bail!("no project matched: {}", selectors.join(", "));
    }
    Ok(selected)
}

pub fn session_files(projects: &[Project]) -> Result<Vec<SessionFile>> {
    let mut files = projects
        .iter()
        .map(files_for_project)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    files.sort_unstable_by(|left, right| right.started_at.cmp(&left.started_at));
    Ok(files)
}

pub fn global_session_files(root: &Path, selector: &str) -> Result<Vec<SessionFile>> {
    let selector = selector.to_lowercase();
    let mut files = Vec::new();
    for directory in fs::read_dir(root)?.filter_map(Result::ok) {
        if !directory.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for entry in fs::read_dir(directory.path())?.filter_map(Result::ok) {
            let path = entry.path();
            let matches = path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_lowercase().contains(&selector));
            if matches {
                let project = project_from_session(directory.path(), &path);
                files.push(session_file(&project, path));
            }
        }
    }
    files.sort_unstable_by(|left, right| right.started_at.cmp(&left.started_at));
    Ok(files)
}

pub fn project_stats(project: &Project) -> Result<(usize, u64)> {
    let files = files_for_project(project)?;
    Ok((files.len(), files.iter().map(|file| file.bytes).sum()))
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

pub fn active_session_path(root: &Path) -> Result<Option<PathBuf>> {
    let cwd = env::current_dir().context("failed to determine current directory")?;
    let directory = root.join(encode_cwd(&cwd));
    if !directory.is_dir() {
        return Ok(None);
    }
    Ok(fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .max_by_key(|path| modified_at_ns(path)))
}

fn files_for_project(project: &Project) -> Result<Vec<SessionFile>> {
    let entries = fs::read_dir(&project.path)
        .with_context(|| format!("failed to read {}", project.path.display()))?;
    Ok(entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path
                .extension()
                .is_some_and(|extension| extension == "jsonl"))
            .then(|| session_file(project, path))
        })
        .collect())
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
    project_with_cwd(path, cwd)
}

fn project_from_session(directory: PathBuf, session: &Path) -> Project {
    let cwd = session_cwd(session).unwrap_or_else(|| decode_dir_name(&directory));
    project_with_cwd(directory, cwd)
}

fn project_with_cwd(path: PathBuf, cwd: String) -> Project {
    let name = Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&cwd)
        .to_owned();
    Project { name, cwd, path }
}

fn first_session_cwd(dir: &Path) -> Option<String> {
    let path = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })?;
    session_cwd(&path)
}

fn session_cwd(path: &Path) -> Option<String> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str::<Value>(&line)
        .ok()?
        .get("cwd")?
        .as_str()
        .map(str::to_owned)
}

fn encode_cwd(cwd: &Path) -> String {
    format!(
        "--{}--",
        cwd.to_string_lossy()
            .trim_start_matches('/')
            .replace('/', "-")
    )
}

fn decode_dir_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim_matches('-')
        .to_owned()
}

fn expand_home(path: PathBuf) -> Result<PathBuf> {
    let value = path.to_string_lossy();
    match value.strip_prefix("~/") {
        Some(rest) => Ok(home_dir()?.join(rest)),
        None => Ok(path),
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[cfg(test)]
mod tests {
    use super::{encode_cwd, session_id_from_path};
    use std::path::Path;

    #[test]
    fn encodes_pi_session_directory_name() {
        assert_eq!(
            encode_cwd(Path::new("/Users/me/code/demo")),
            "--Users-me-code-demo--"
        );
    }

    #[test]
    fn extracts_session_id_from_pi_filename() {
        assert_eq!(
            session_id_from_path(Path::new("/tmp/2026-09-01T00-00-00Z_01a04453-d395.jsonl")),
            Some("01a04453-d395".to_owned())
        );
    }
}
