mod cli;
mod engine;
mod naming;
mod output;
mod source;

use std::{collections::HashMap, io, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, bail};
use clap::Parser;

use cli::{
    CatchupArgs, Cli, Command, Kind, LastArgs, ProjectsArgs, SearchArgs, SessionsArgs, ShowArgs,
    StatsArgs, SummarizeArgs, selected_kinds,
};
use engine::{
    Record, SearchOptions, search, select_session, session_digest, session_matches,
    session_metrics, session_records, session_title, validate_bound, within_bounds,
};
use naming::{Names, iso_timestamp};
use output::{
    DisplayOptions, ProjectRow, SessionRow, human_bytes, print_digest, print_metrics,
    print_projects, print_records, print_sessions,
};
use source::{
    SessionFile, active_session_path, catalog, current_session_file, current_session_id,
    global_session_files, listed_projects, scope_files, session_files, sessions_roots,
};

const DEFAULT_JSON_TEXT_BYTES: usize = 4096;

/// Where a command looks: the session roots and the `-p`/`--all-projects`
/// selection.
struct Scope<'a> {
    roots: &'a [PathBuf],
    selectors: &'a [String],
    all_projects: bool,
}

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
    let roots = sessions_roots(cli.root)?;
    let scope = Scope {
        roots: &roots,
        selectors: &cli.project,
        all_projects: cli.all_projects,
    };
    match cli.command {
        Command::Search(args) => search_command(&scope, &args),
        Command::Catchup(args) => catchup_command(&scope, &args),
        Command::Stats(args) => stats_command(&scope, &args),
        Command::Last(args) => last_command(&scope, &args),
        Command::Show(args) => show_command(&scope, &args),
        Command::Summarize(args) => summarize_command(&scope, &args),
        Command::Sessions(args) => sessions_command(&scope, &args),
        Command::Projects(args) => projects_command(&scope, &args),
    }
}

fn search_command(scope: &Scope<'_>, args: &SearchArgs) -> Result<()> {
    let mut files = scoped_files(scope, args.session.as_deref())?;
    if let Some(selector) = args.session.as_deref() {
        files = vec![select_session(&files, selector)?.clone()];
    }
    files.retain(|file| {
        !args
            .exclude_session
            .iter()
            .any(|selector| session_matches(file, selector))
    });
    if args.exclude_active {
        exclude_active(scope.roots, &mut files)?;
    }
    let kinds = search_kinds(args)?;
    let output = search(
        &files,
        &SearchOptions {
            kinds: &kinds,
            query: &args.query,
            regex: args.regex,
            case_sensitive: args.case_sensitive,
            tool: args.tool.as_deref(),
            errors_only: args.errors,
            since: args.since.as_deref(),
            until: args.until.as_deref(),
            sort: args.sort,
            around: args.around,
            limit: args.limit,
        },
    )?;
    let names = display_names(
        scope.roots,
        output.records.iter().map(|record| record.cwd.clone()),
        args.json,
    );
    print_records(
        &output.records,
        &DisplayOptions {
            names: &names,
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
    Ok(())
}

fn catchup_command(scope: &Scope<'_>, args: &CatchupArgs) -> Result<()> {
    let file = target_session_file(scope, args.session.as_deref(), "catchup")?;
    let records = last_records(
        session_records(&file, &[Kind::User, Kind::Assistant])?,
        args.limit,
    );
    let names = display_names(scope.roots, [file.cwd.clone()], args.json);
    print_records(
        &records,
        &timeline_display(
            &names,
            args.context,
            args.full,
            args.json,
            args.max_text,
            args.metadata_only,
        ),
    )
}

fn stats_command(scope: &Scope<'_>, args: &StatsArgs) -> Result<()> {
    let file = target_session_file(scope, args.session.as_deref(), "stats")?;
    print_metrics(&session_metrics(&file, args.limit)?, args.json)
}

fn last_command(scope: &Scope<'_>, args: &LastArgs) -> Result<()> {
    let files = scoped_files(scope, Some(&args.session))?;
    let file = select_session(&files, &args.session)?;
    let kinds = match args.kind.is_empty() {
        true => Kind::ALL.to_vec(),
        false => args.kind.clone(),
    };
    let records = last_records(session_records(file, &kinds)?, args.limit);
    let names = display_names(scope.roots, [file.cwd.clone()], args.json);
    print_records(
        &records,
        &timeline_display(
            &names,
            args.context,
            args.full,
            args.json,
            args.max_text,
            args.metadata_only,
        ),
    )
}

fn show_command(scope: &Scope<'_>, args: &ShowArgs) -> Result<()> {
    let files = scoped_files(scope, Some(&args.session))?;
    let file = select_session(&files, &args.session)?;
    let kinds = selected_kinds(&args.kind, args.all_kinds);
    let records = last_records(session_records(file, &kinds)?, args.limit);
    let names = display_names(scope.roots, [file.cwd.clone()], args.json);
    print_records(
        &records,
        &timeline_display(
            &names,
            args.context,
            args.full,
            args.json,
            args.max_text,
            args.metadata_only,
        ),
    )
}

fn summarize_command(scope: &Scope<'_>, args: &SummarizeArgs) -> Result<()> {
    let files = scoped_files(scope, Some(&args.session))?;
    let file = select_session(&files, &args.session)?;
    print_digest(
        &session_digest(file, args.opening, args.milestones)?,
        args.json,
    )
}

fn sessions_command(scope: &Scope<'_>, args: &SessionsArgs) -> Result<()> {
    let (since, until) = (args.since.as_deref(), args.until.as_deref());
    validate_bound(since)?;
    validate_bound(until)?;
    let filter = args.filter.as_deref().map(str::to_lowercase);
    let in_range = |started: Option<&str>| {
        (since.is_none() && until.is_none())
            || started.is_some_and(|started| within_bounds(started, since, until))
    };
    let matches = |file: &SessionFile, title: &str| {
        filter.as_ref().is_none_or(|filter| {
            format!(
                "{} {} {} {} {}",
                file.project, file.cwd, file.session, file.started_at, title
            )
            .to_lowercase()
            .contains(filter)
        })
    };
    let listed = scope_files(scope.roots, scope.selectors, scope.all_projects)?
        .into_iter()
        .map(|file| (iso_timestamp(&file.started_at), file))
        .filter(|(started, _)| in_range(started.as_deref()))
        .map(|(started, file)| session_title(&file).map(|title| (file, title, started)))
        .filter(|listed| {
            listed
                .as_ref()
                .map_or(true, |(file, title, _)| matches(file, title))
        })
        .take(match args.limit {
            0 => usize::MAX,
            limit => limit,
        })
        .collect::<Result<Vec<_>>>()?;
    let names = display_names(
        scope.roots,
        listed.iter().map(|(file, _, _)| file.cwd.clone()),
        args.json,
    );
    let rows = listed
        .iter()
        .map(|(file, title, started)| SessionRow {
            project: &file.project,
            cwd: &file.cwd,
            session: &file.session,
            started_at: &file.started_at,
            started_at_iso: started.clone(),
            bytes: file.bytes,
            title,
            path: file.path.to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>();
    print_sessions(&rows, args.json, &names)
}

fn projects_command(scope: &Scope<'_>, args: &ProjectsArgs) -> Result<()> {
    let selectors = scope
        .selectors
        .iter()
        .chain(args.filter.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut projects = listed_projects(scope.roots, &selectors)?;
    let totals = session_files(&projects)?.into_iter().fold(
        HashMap::<String, (usize, u64)>::new(),
        |mut totals, file| {
            let (sessions, bytes) = totals.entry(file.cwd).or_default();
            *sessions = sessions.saturating_add(1);
            *bytes = bytes.saturating_add(file.bytes);
            totals
        },
    );
    projects.dedup_by(|later, earlier| later.cwd == earlier.cwd);
    let names = display_names(scope.roots, [], args.json);
    let rows = projects
        .iter()
        .map(|project| {
            let (sessions, bytes) = totals.get(&project.cwd).copied().unwrap_or_default();
            ProjectRow {
                name: &project.name,
                cwd: &project.cwd,
                sessions,
                bytes,
            }
        })
        .collect::<Vec<_>>();
    print_projects(&rows, args.json, &names)
}

fn search_kinds(args: &SearchArgs) -> Result<Vec<Kind>> {
    let kinds = match (args.errors, args.kind.is_empty() && !args.all_kinds) {
        (true, true) => vec![Kind::ToolResult],
        _ => selected_kinds(&args.kind, args.all_kinds),
    };
    if args.errors && !kinds.contains(&Kind::ToolResult) {
        bail!("--errors only matches tool results; add tool-result to --kind");
    }
    Ok(kinds)
}

/// The running Pi session when pi says which one it is, else the current
/// folder's most recently written session.
fn exclude_active(roots: &[PathBuf], files: &mut Vec<SessionFile>) -> Result<()> {
    match current_session_id() {
        Some(id) => files.retain(|file| !file.session.eq_ignore_ascii_case(&id)),
        None => {
            if let Some(active) = active_session_path(roots)? {
                files.retain(|file| file.path != active);
            }
        }
    }
    Ok(())
}

/// Human output names sessions by their shortest unique id prefix and
/// folders by their shortest unique path suffix, across every root.
fn display_names(roots: &[PathBuf], cwds: impl IntoIterator<Item = String>, json: bool) -> Names {
    match json {
        true => Names::default(),
        false => {
            let (ids, known) = catalog(roots);
            let cwds = known.into_iter().chain(cwds).collect::<Vec<_>>();
            Names::new(&ids, &cwds)
        }
    }
}

fn timeline_display(
    names: &Names,
    context: usize,
    full: bool,
    json: bool,
    max_text: Option<usize>,
    metadata_only: bool,
) -> DisplayOptions<'_> {
    DisplayOptions {
        names,
        query: None,
        regex: false,
        case_sensitive: false,
        context,
        full,
        json,
        max_text: json_text_limit(json, full, max_text),
        metadata_only,
    }
}

fn last_records(mut records: Vec<Record>, limit: usize) -> Vec<Record> {
    if limit > 0 && records.len() > limit {
        records.drain(..records.len() - limit);
    }
    records
}

fn json_text_limit(json: bool, full: bool, requested: Option<usize>) -> Option<usize> {
    (json && !full).then(|| requested.unwrap_or(DEFAULT_JSON_TEXT_BYTES))
}

fn target_session_file(
    scope: &Scope<'_>,
    selector: Option<&str>,
    command: &str,
) -> Result<SessionFile> {
    if let Some(selector) = selector {
        let files = scoped_files(scope, Some(selector))?;
        return Ok(select_session(&files, selector)?.clone());
    }
    if let Some(file) = current_session_file() {
        return Ok(file);
    }
    let selector = current_session_id().with_context(|| {
        format!("not running inside a Pi session; pass an explicit SESSION to {command}")
    })?;
    let files = scoped_files(scope, Some(&selector))?;
    Ok(select_session(&files, &selector)?.clone())
}

/// An explicit session id with no `-p` is looked up everywhere; anything
/// else goes through the project scope.
fn scoped_files(scope: &Scope<'_>, session: Option<&str>) -> Result<Vec<SessionFile>> {
    let exact_session = session.filter(|selector| !selector.eq_ignore_ascii_case("latest"));
    if scope.selectors.is_empty()
        && !scope.all_projects
        && let Some(selector) = exact_session
    {
        return global_session_files(scope.roots, selector);
    }
    scope_files(scope.roots, scope.selectors, scope.all_projects)
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}
