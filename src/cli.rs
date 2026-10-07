use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "lookup",
    version,
    about = "Search Pi sessions without the JSONL noise"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help = "Sessions root [default: ~/.pi/agent/sessions]"
    )]
    pub root: Option<PathBuf>,

    #[arg(
        short,
        long,
        global = true,
        action = clap::ArgAction::Append,
        value_name = "NAME",
        help = "Project path/name substring; repeatable"
    )]
    pub project: Vec<String>,

    #[arg(
        long,
        global = true,
        conflicts_with = "project",
        help = "Search every project"
    )]
    pub all_projects: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(alias = "s", about = "Search normalized session content")]
    Search(SearchArgs),
    #[command(about = "Emit recent conversation context for agent catch-up")]
    Catchup(CatchupArgs),
    #[command(
        alias = "metrics",
        about = "Count session activity and inspect compaction timing"
    )]
    Stats(StatsArgs),
    #[command(about = "Print the final normalized event(s) in a session")]
    Last(LastArgs),
    #[command(alias = "cat", about = "Render one session as a useful timeline")]
    Show(ShowArgs),
    #[command(
        alias = "sum",
        about = "Summarize one session without transcript noise"
    )]
    Summarize(SummarizeArgs),
    #[command(alias = "ls", about = "List recent sessions")]
    Sessions(SessionsArgs),
    #[command(alias = "p", about = "List session projects")]
    Projects(ProjectsArgs),
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    #[arg(value_name = "QUERY")]
    pub query: String,

    #[arg(
        short,
        long,
        value_delimiter = ',',
        value_name = "KIND",
        help = "user, assistant, thinking, tool-call, tool-result, summary"
    )]
    pub kind: Vec<Kind>,

    #[arg(long, conflicts_with = "kind")]
    pub all_kinds: bool,

    #[arg(short = 'e', long, help = "Interpret QUERY as a regular expression")]
    pub regex: bool,

    #[arg(long, help = "Use case-sensitive matching")]
    pub case_sensitive: bool,

    #[arg(long, value_name = "ID", help = "Search only one session id/prefix")]
    pub session: Option<String>,

    #[arg(
        long,
        action = clap::ArgAction::Append,
        value_name = "ID",
        help = "Exclude a session id/prefix; repeatable"
    )]
    pub exclude_session: Vec<String>,

    #[arg(long, help = "Exclude the current working directory's active session")]
    pub exclude_active: bool,

    #[arg(long, value_name = "NAME", help = "Only matching tool calls/results")]
    pub tool: Option<String>,

    #[arg(long, value_name = "DATE", help = "ISO date/timestamp lower bound")]
    pub since: Option<String>,

    #[arg(long, value_name = "DATE", help = "ISO date/timestamp upper bound")]
    pub until: Option<String>,

    #[arg(long, value_enum, default_value = "newest", help = "Result ordering")]
    pub sort: SortOrder,

    #[arg(
        long,
        default_value_t = 0,
        value_name = "N",
        help = "Include N neighboring records of the selected kinds"
    )]
    pub around: usize,

    #[arg(
        short = 'n',
        long,
        default_value_t = 50,
        help = "Maximum matches before --around expansion; 0 is unlimited"
    )]
    pub limit: usize,

    #[arg(short = 'C', long, default_value_t = 120, value_name = "CHARS")]
    pub context: usize,

    #[arg(
        long,
        help = "Print complete records; disables the default JSON text limit"
    )]
    pub full: bool,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,

    #[arg(
        long,
        requires = "json",
        conflicts_with = "full",
        value_name = "BYTES",
        help = "Maximum text bytes per JSON record [default: 4096]"
    )]
    pub max_text: Option<usize>,

    #[arg(
        long,
        requires = "json",
        conflicts_with_all = ["full", "max_text"],
        help = "Emit JSON metadata without record text"
    )]
    pub metadata_only: bool,

    #[arg(long, help = "Print scan statistics to stderr")]
    pub stats: bool,
}

#[derive(Debug, Args)]
pub struct CatchupArgs {
    #[arg(
        value_name = "SESSION",
        help = "Optional session id/prefix; defaults to the current Pi session"
    )]
    pub session: Option<String>,

    #[arg(
        short = 'n',
        long,
        default_value_t = 20,
        help = "Recent user and assistant records to emit; 0 is unlimited"
    )]
    pub limit: usize,

    #[arg(short = 'C', long, default_value_t = 1000, value_name = "CHARS")]
    pub context: usize,

    #[arg(
        long,
        help = "Print complete records; disables the default JSON text limit"
    )]
    pub full: bool,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,

    #[arg(
        long,
        requires = "json",
        conflicts_with = "full",
        value_name = "BYTES",
        help = "Maximum text bytes per JSON record [default: 4096]"
    )]
    pub max_text: Option<usize>,

    #[arg(
        long,
        requires = "json",
        conflicts_with_all = ["full", "max_text"],
        help = "Emit JSON metadata without record text"
    )]
    pub metadata_only: bool,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(
        value_name = "SESSION",
        help = "Optional session id/prefix; defaults to the current Pi session"
    )]
    pub session: Option<String>,

    #[arg(
        short = 'n',
        long,
        default_value_t = 20,
        help = "Recent compactions to list; 0 lists all"
    )]
    pub limit: usize,

    #[arg(long, help = "Emit one machine-readable JSON object")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LastArgs {
    #[arg(
        value_name = "SESSION",
        help = "Session id/prefix, filename, or 'latest'"
    )]
    pub session: String,

    #[arg(
        short,
        long,
        value_delimiter = ',',
        value_name = "KIND",
        help = "Filter kinds; defaults to all normalized kinds"
    )]
    pub kind: Vec<Kind>,

    #[arg(
        short = 'n',
        long,
        default_value_t = 1,
        help = "Final records to print"
    )]
    pub limit: usize,

    #[arg(short = 'C', long, default_value_t = 240, value_name = "CHARS")]
    pub context: usize,

    #[arg(
        long,
        help = "Print complete records; disables the default JSON text limit"
    )]
    pub full: bool,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,

    #[arg(
        long,
        requires = "json",
        conflicts_with = "full",
        value_name = "BYTES",
        help = "Maximum text bytes per JSON record [default: 4096]"
    )]
    pub max_text: Option<usize>,

    #[arg(
        long,
        requires = "json",
        conflicts_with_all = ["full", "max_text"],
        help = "Emit JSON metadata without record text"
    )]
    pub metadata_only: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    #[arg(
        value_name = "SESSION",
        help = "Session id/prefix, filename, or 'latest'"
    )]
    pub session: String,

    #[arg(short, long, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<Kind>,

    #[arg(long, conflicts_with = "kind")]
    pub all_kinds: bool,

    #[arg(
        short = 'n',
        long,
        default_value_t = 0,
        help = "Keep only the last N records"
    )]
    pub limit: usize,

    #[arg(short = 'C', long, default_value_t = 240, value_name = "CHARS")]
    pub context: usize,

    #[arg(
        long,
        help = "Print complete records; disables the default JSON text limit"
    )]
    pub full: bool,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,

    #[arg(
        long,
        requires = "json",
        conflicts_with = "full",
        value_name = "BYTES",
        help = "Maximum text bytes per JSON record [default: 4096]"
    )]
    pub max_text: Option<usize>,

    #[arg(
        long,
        requires = "json",
        conflicts_with_all = ["full", "max_text"],
        help = "Emit JSON metadata without record text"
    )]
    pub metadata_only: bool,
}

#[derive(Debug, Args)]
pub struct SummarizeArgs {
    #[arg(
        value_name = "SESSION",
        help = "Session id/prefix, filename, or 'latest'"
    )]
    pub session: String,

    #[arg(
        long,
        default_value_t = 8,
        value_name = "N",
        help = "Maximum compaction milestones; 0 disables them"
    )]
    pub milestones: usize,

    #[arg(
        long,
        default_value_t = 3,
        value_name = "N",
        help = "Opening user messages to include"
    )]
    pub opening: usize,

    #[arg(long, help = "Emit one machine-readable JSON object")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
    #[arg(
        short = 'n',
        long,
        default_value_t = 20,
        help = "Maximum sessions; 0 is unlimited"
    )]
    pub limit: usize,

    #[arg(
        value_name = "FILTER",
        help = "Optional id, date, filename, or title substring"
    )]
    pub filter: Option<String>,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ProjectsArgs {
    #[arg(value_name = "FILTER", help = "Optional path/name substring")]
    pub filter: Option<String>,

    #[arg(long, help = "Emit machine-readable JSONL")]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SortOrder {
    Newest,
    Oldest,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    User,
    Assistant,
    Thinking,
    ToolCall,
    ToolResult,
    Summary,
}

impl Kind {
    pub const ALL: [Self; 6] = [
        Self::User,
        Self::Assistant,
        Self::Thinking,
        Self::ToolCall,
        Self::ToolResult,
        Self::Summary,
    ];

    pub const DEFAULT: [Self; 3] = [Self::User, Self::Assistant, Self::ToolCall];

    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::ToolCall => "tool-call",
            Self::ToolResult => "tool-result",
            Self::Summary => "summary",
        }
    }
}

pub fn selected_kinds(kinds: &[Kind], all: bool) -> Vec<Kind> {
    match (all, kinds.is_empty()) {
        (true, _) => Kind::ALL.to_vec(),
        (false, true) => Kind::DEFAULT.to_vec(),
        (false, false) => kinds.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, Kind, SortOrder};
    use clap::Parser;

    #[test]
    fn parses_search_order_and_neighbors() {
        let cli = Cli::try_parse_from([
            "lookup", "search", "needle", "--sort", "oldest", "--around", "2",
        ])
        .unwrap();
        let Command::Search(args) = cli.command else {
            panic!("expected search command");
        };
        assert_eq!(args.sort, SortOrder::Oldest);
        assert_eq!(args.around, 2);
    }

    #[test]
    fn parses_catchup_without_a_session() {
        let cli = Cli::try_parse_from(["lookup", "catchup"]).unwrap();
        let Command::Catchup(args) = cli.command else {
            panic!("expected catchup command");
        };
        assert_eq!(args.session, None);
        assert_eq!(args.limit, 20);
        assert_eq!(args.context, 1000);
    }

    #[test]
    fn parses_stats_for_current_session() {
        let cli = Cli::try_parse_from(["lookup", "stats"]).unwrap();
        let Command::Stats(args) = cli.command else {
            panic!("expected stats command");
        };
        assert_eq!(args.session, None);
        assert_eq!(args.limit, 20);
    }

    #[test]
    fn parses_last_kind_filter() {
        let cli = Cli::try_parse_from([
            "lookup",
            "last",
            "abcdef12",
            "-n",
            "3",
            "-k",
            "user,assistant",
        ])
        .unwrap();
        let Command::Last(args) = cli.command else {
            panic!("expected last command");
        };
        assert_eq!(args.limit, 3);
        assert_eq!(args.kind, [Kind::User, Kind::Assistant]);
    }
}
