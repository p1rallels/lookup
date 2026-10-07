mod cli;
mod engine;
mod output;
mod source;

use std::{io, path::Path, process::ExitCode};

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;

use cli::{Cli, Command, selected_kinds};
use engine::{
    SearchOptions, search, select_session, session_digest, session_matches, session_metrics,
    session_records, session_title,
};
use output::{
    DisplayOptions, ProjectRow, SessionRow, human_bytes, print_digest, print_metrics,
    print_projects, print_records, print_sessions,
};
use source::{
    SessionFile, active_session_path, current_session_file, current_session_id, discover_projects,
    global_session_files, project_stats, select_projects, session_files, sessions_root,
};

const DEFAULT_JSON_TEXT_BYTES: usize = 4096;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lookup: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = sessions_root(cli.root)?;

    match cli.command {
        Command::Search(args) => {
            let mut files = scoped_files(
                &root,
                &cli.project,
                cli.all_projects,
                args.session.as_deref(),
            )?;
            if let Some(selector) = args.session.as_deref() {
                files = vec![select_session(&files, selector)?.clone()];
            }
            files.retain(|file| {
                !args
                    .exclude_session
                    .iter()
                    .any(|selector| session_matches(file, selector))
            });
            if args.exclude_active
                && let Some(active) = active_session_path(&root)?
            {
                files.retain(|file| file.path != active);
            }
            let kinds = selected_kinds(&args.kind, args.all_kinds);
            let output = search(
                &files,
                &SearchOptions {
                    kinds: &kinds,
                    query: &args.query,
                    regex: args.regex,
                    case_sensitive: args.case_sensitive,
                    tool: args.tool.as_deref(),
                    since: args.since.as_deref(),
                    until: args.until.as_deref(),
                    sort: args.sort,
                    around: args.around,
                    limit: args.limit,
                },
            )?;
            print_records(
                &output.records,
                &DisplayOptions {
                    query: Some(&args.query),
                    regex: args.regex,
                    case_sensitive: args.case_sensitive,
                    context: args.context,
                    full: args.full,
                    json: args.json,
                    max_text: json_text_limit(args.json, args.full, args.max_text),
                    metadata_only: args.metadata_only,
                },
            )?;
            if args.stats {
                eprintln!(
                    "scanned {} files ({}) in {:.3}s; {} candidate lines, {} parsed, {} matches, {} output records",
                    output.stats.files,
                    human_bytes(output.stats.bytes),
                    output.stats.elapsed.as_secs_f64(),
                    output.stats.candidate_lines,
                    output.stats.parsed_lines,
                    output.match_count,
                    output.records.len(),
                );
            }
        }
        Command::Catchup(args) => {
            let file = target_session_file(
                &root,
                &cli.project,
                cli.all_projects,
                args.session.as_deref(),
                "catchup",
            )?;
            let mut records = session_records(&file, &[cli::Kind::User, cli::Kind::Assistant])?;
            if args.limit > 0 && records.len() > args.limit {
                records.drain(..records.len() - args.limit);
            }
            print_records(
                &records,
                &DisplayOptions {
                    query: None,
                    regex: false,
                    case_sensitive: false,
                    context: args.context,
                    full: args.full,
                    json: args.json,
                    max_text: json_text_limit(args.json, args.full, args.max_text),
                    metadata_only: args.metadata_only,
                },
            )?;
        }
        Command::Stats(args) => {
            let file = target_session_file(
                &root,
                &cli.project,
                cli.all_projects,
                args.session.as_deref(),
                "stats",
            )?;
            let metrics = session_metrics(&file, args.limit)?;
            print_metrics(&metrics, args.json)?;
        }
        Command::Last(args) => {
            let files = scoped_files(&root, &cli.project, cli.all_projects, Some(&args.session))?;
            let file = select_session(&files, &args.session)?;
            let kinds = if args.kind.is_empty() {
                cli::Kind::ALL.to_vec()
            } else {
                args.kind.clone()
            };
            let mut records = session_records(file, &kinds)?;
            if args.limit > 0 && records.len() > args.limit {
                records.drain(..records.len() - args.limit);
            }
            print_records(
                &records,
                &DisplayOptions {
                    query: None,
                    regex: false,
                    case_sensitive: false,
                    context: args.context,
                    full: args.full,
                    json: args.json,
                    max_text: json_text_limit(args.json, args.full, args.max_text),
                    metadata_only: args.metadata_only,
                },
            )?;
        }
        Command::Show(args) => {
            let files = scoped_files(&root, &cli.project, cli.all_projects, Some(&args.session))?;
            let file = select_session(&files, &args.session)?;
            let kinds = selected_kinds(&args.kind, args.all_kinds);
            let mut records = session_records(file, &kinds)?;
            if args.limit > 0 && records.len() > args.limit {
                records.drain(..records.len() - args.limit);
            }
            print_records(
                &records,
                &DisplayOptions {
                    query: None,
                    regex: false,
                    case_sensitive: false,
                    context: args.context,
                    full: args.full,
                    json: args.json,
                    max_text: json_text_limit(args.json, args.full, args.max_text),
                    metadata_only: args.metadata_only,
                },
            )?;
        }
        Command::Summarize(args) => {
            let files = scoped_files(&root, &cli.project, cli.all_projects, Some(&args.session))?;
            let file = select_session(&files, &args.session)?;
            let digest = session_digest(file, args.opening, args.milestones)?;
            print_digest(&digest, args.json)?;
        }
        Command::Sessions(args) => {
            let projects = select_projects(&root, &cli.project, cli.all_projects)?;
            let files = session_files(&projects)?;
            let filter = args.filter.as_deref().map(str::to_lowercase);
            let mut listed = Vec::new();
            for file in files {
                let title = session_title(&file)?;
                let matches = filter.as_ref().is_none_or(|filter| {
                    format!(
                        "{} {} {} {} {}",
                        file.project, file.cwd, file.session, file.started_at, title
                    )
                    .to_lowercase()
                    .contains(filter)
                });
                if matches {
                    listed.push((file, title));
                }
                if args.limit > 0 && listed.len() >= args.limit {
                    break;
                }
            }
            let rows = listed
                .iter()
                .map(|(file, title)| SessionRow {
                    project: &file.project,
                    cwd: &file.cwd,
                    session: &file.session,
                    started_at: &file.started_at,
                    bytes: file.bytes,
                    title,
                    path: file.path.to_string_lossy().into_owned(),
                })
                .collect::<Vec<_>>();
            print_sessions(&rows, args.json)?;
        }
        Command::Projects(args) => {
            let selectors = cli
                .project
                .iter()
                .chain(args.filter.iter())
                .map(|value| value.to_lowercase())
                .collect::<Vec<_>>();
            let projects = discover_projects(&root)?
                .into_iter()
                .filter(|project| {
                    selectors.is_empty()
                        || selectors.iter().any(|selector| {
                            format!("{} {}", project.name, project.cwd)
                                .to_lowercase()
                                .contains(selector)
                        })
                })
                .collect::<Vec<_>>();
            let stats = projects
                .par_iter()
                .map(project_stats)
                .collect::<Result<Vec<_>>>()?;
            let rows = projects
                .iter()
                .zip(stats)
                .map(|(project, (sessions, bytes))| ProjectRow {
                    name: &project.name,
                    cwd: &project.cwd,
                    sessions,
                    bytes,
                })
                .collect::<Vec<_>>();
            print_projects(&rows, args.json)?;
        }
    }
    Ok(())
}

fn json_text_limit(json: bool, full: bool, requested: Option<usize>) -> Option<usize> {
    (json && !full).then(|| requested.unwrap_or(DEFAULT_JSON_TEXT_BYTES))
}

fn target_session_file(
    root: &Path,
    selectors: &[String],
    all_projects: bool,
    selector: Option<&str>,
    command: &str,
) -> Result<SessionFile> {
    if let Some(selector) = selector {
        let files = scoped_files(root, selectors, all_projects, Some(selector))?;
        return Ok(select_session(&files, selector)?.clone());
    }
    if let Some(file) = current_session_file() {
        return Ok(file);
    }
    let selector = current_session_id().with_context(|| {
        format!("not running inside a Pi session; pass an explicit SESSION to {command}")
    })?;
    let files = scoped_files(root, selectors, all_projects, Some(&selector))?;
    Ok(select_session(&files, &selector)?.clone())
}

fn scoped_files(
    root: &Path,
    selectors: &[String],
    all_projects: bool,
    session: Option<&str>,
) -> Result<Vec<SessionFile>> {
    let exact_session = session.filter(|selector| !selector.eq_ignore_ascii_case("latest"));
    if selectors.is_empty()
        && !all_projects
        && let Some(selector) = exact_session
    {
        return global_session_files(root, selector);
    }
    session_files(&select_projects(root, selectors, all_projects)?)
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}
