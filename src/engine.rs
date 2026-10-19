use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder, bytes};
use serde::Serialize;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    cli::{Kind, SortOrder},
    custom::{
        MSG, Msg, RELAY, Relay, compaction_relay, content_text, exit_kind, parse_msg, parse_relay,
        reset_label, tool_messages,
    },
    naming::{SHORT_ID, display_timestamp, iso_timestamp, short_id, unique_prefix_lengths},
    source::SessionFile,
};

const AMBIGUOUS_SHOWN: usize = 6;
const RECENT_FAILURES: usize = 3;

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub project: String,
    pub cwd: String,
    pub session: String,
    pub started_at: String,
    pub timestamp: String,
    pub line: usize,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A tool result pi marked as failed (`isError`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    /// A custom record's customType: `msg`, `relay`, or another extension's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<Msg>,
    /// Relay's details on its custom messages and on its reset compactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<Relay>,
    pub text: String,
    pub path: String,
}

impl Record {
    pub fn is_msg(&self) -> bool {
        self.kind == Kind::Custom && self.custom_type.as_deref() == Some(MSG)
    }
}

/// Catchup's default kinds. With [`conversation`], that's what the user
/// typed, the assistant's answers, and msg traffic.
pub const CONVERSATION: [Kind; 3] = [Kind::User, Kind::Assistant, Kind::Custom];

/// Whether a record is conversation: custom records only when they're msg
/// messages, so relay's nudges and other extensions' messages stay out.
pub fn conversation(record: &Record) -> bool {
    record.kind != Kind::Custom || record.is_msg()
}

#[derive(Debug)]
pub struct SearchOptions<'a> {
    pub kinds: &'a [Kind],
    pub query: &'a str,
    pub regex: bool,
    pub case_sensitive: bool,
    pub tool: Option<&'a str>,
    pub errors_only: bool,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub sort: SortOrder,
    pub around: usize,
    pub limit: usize,
}

#[derive(Debug, Default)]
pub struct SearchStats {
    pub files: usize,
    pub bytes: u64,
    pub candidate_lines: usize,
    pub parsed_lines: usize,
    pub elapsed: Duration,
}

pub struct SearchOutput {
    pub records: Vec<Record>,
    pub match_count: usize,
    pub stats: SearchStats,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ActivityCounts {
    pub events: usize,
    pub message_events: usize,
    pub turns: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_result_messages: usize,
    pub assistant_text_parts: usize,
    pub thinking_blocks: usize,
    pub tool_calls: usize,
    pub custom_messages: usize,
    /// Custom messages by customType.
    pub custom_types: BTreeMap<String, usize>,
    pub compactions: usize,
    /// Compactions relay wrote for its resets.
    pub relay_resets: usize,
    pub model_changes: usize,
    pub thinking_level_changes: usize,
    pub session_events: usize,
    pub other_events: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompactionTiming {
    pub number: usize,
    pub line: usize,
    pub finished_at: String,
    pub tokens_before: Option<u64>,
    pub from_hook: Option<bool>,
    pub previous_event_at: Option<String>,
    pub previous_event: Option<String>,
    pub previous_stop_reason: Option<String>,
    pub preceding_event_gap_ms: Option<u64>,
    /// Set when relay wrote it: which reset, at what context percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<Relay>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionMetrics {
    pub project: String,
    pub cwd: String,
    pub session: String,
    pub started_at: String,
    pub ended_at: String,
    pub elapsed_ms: Option<u64>,
    pub bytes: u64,
    pub counts: ActivityCounts,
    pub usage: Usage,
    pub models: Vec<String>,
    pub compactions_listed: usize,
    pub compaction_timing_note: &'static str,
    pub compactions: Vec<CompactionTiming>,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DigestEntry {
    pub timestamp: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LatestState {
    pub goal: String,
    pub in_progress: String,
    pub blocked: String,
    pub next_steps: String,
}

/// Token and cost totals over a session's assistant messages, as pi recorded
/// them (cost in USD at the model's list price).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    /// The last assistant message's stop reason: stop, length, toolUse,
    /// aborted or error.
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    /// Set when the last message left the turn open: `thinking` (a prompt or
    /// a tool result the model hadn't answered) or `running-tools`. Either
    /// it's still running, or it stopped there.
    pub open_turn: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub edits: usize,
    pub writes: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Failures {
    pub count: usize,
    pub recent: Vec<FailedTool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedTool {
    pub timestamp: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionDigest {
    pub project: String,
    pub cwd: String,
    pub session: String,
    pub started_at: String,
    pub started_at_iso: Option<String>,
    pub ended_at: String,
    pub elapsed_ms: Option<u64>,
    pub bytes: u64,
    pub counts: BTreeMap<String, usize>,
    pub outcome: Outcome,
    pub opening: Vec<DigestEntry>,
    pub milestones: Vec<DigestEntry>,
    pub latest_state: LatestState,
    pub files_changed: Vec<FileChange>,
    pub failed_tools: Failures,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<MsgTraffic>,
    pub final_message: Option<DigestEntry>,
    pub path: String,
}

/// The msg messages a session sent and got, by peer.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MsgTraffic {
    pub sent: usize,
    pub got: usize,
    /// Of those got, the ones a human sent (`/msg` or the msg cli).
    pub from_user: usize,
    pub peers: Vec<Peer>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Peer {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub sent: usize,
    pub got: usize,
    /// `ended` or `died`, when its last such message said so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<String>,
}

#[derive(Clone, Copy)]
enum FileTool {
    Edit,
    Write,
}

/// Edit and write calls waiting on their results, and the files they changed.
#[derive(Default)]
struct Changes {
    pending: HashMap<String, (FileTool, String)>,
    done: BTreeMap<String, FileChange>,
}

struct Matcher {
    text: Regex,
    raw: Option<bytes::Regex>,
}

struct PreviousEvent {
    timestamp: Option<String>,
    label: String,
    stop_reason: Option<String>,
}

struct FileMatches {
    records: Vec<Record>,
    candidate_lines: usize,
    parsed_lines: usize,
}

pub fn search(files: &[SessionFile], options: &SearchOptions<'_>) -> Result<SearchOutput> {
    validate_bound(options.since)?;
    validate_bound(options.until)?;
    let matcher = Matcher::new(options.query, options.regex, options.case_sensitive)?;
    let kinds = options.kinds.iter().copied().collect::<HashSet<_>>();
    let started = Instant::now();

    let matches = files
        .par_iter()
        .map(|file| search_file(file, options, &matcher, &kinds))
        .collect::<Result<Vec<_>>>()?;

    let mut matched_records = matches
        .iter()
        .flat_map(|result| result.records.iter().cloned())
        .collect::<Vec<_>>();
    sort_records(&mut matched_records, options.sort);
    if options.limit > 0 {
        matched_records.truncate(options.limit);
    }
    let match_count = matched_records.len();
    let mut records = expand_around(files, &matched_records, options.kinds, options.around)?;
    sort_records(&mut records, options.sort);

    Ok(SearchOutput {
        records,
        match_count,
        stats: SearchStats {
            files: files.len(),
            bytes: files.iter().map(|file| file.bytes).sum(),
            candidate_lines: matches.iter().map(|result| result.candidate_lines).sum(),
            parsed_lines: matches.iter().map(|result| result.parsed_lines).sum(),
            elapsed: started.elapsed(),
        },
    })
}

fn sort_records(records: &mut [Record], order: SortOrder) {
    records.sort_by(|left, right| match order {
        SortOrder::Newest => right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| right.line.cmp(&left.line)),
        SortOrder::Oldest => left
            .timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.started_at.cmp(&right.started_at))
            .then_with(|| left.line.cmp(&right.line)),
    });
}

fn expand_around(
    files: &[SessionFile],
    matches: &[Record],
    kinds: &[Kind],
    around: usize,
) -> Result<Vec<Record>> {
    if around == 0 {
        return Ok(matches.to_vec());
    }

    let mut expanded = Vec::new();
    for file in files {
        let path = file.path.to_string_lossy();
        let file_matches = matches
            .iter()
            .filter(|record| record.path == path)
            .collect::<Vec<_>>();
        if file_matches.is_empty() {
            continue;
        }
        let timeline = session_records(file, kinds)?;
        let mut indexes = BTreeSet::new();
        for matched in file_matches {
            for (index, _) in timeline
                .iter()
                .enumerate()
                .filter(|(_, record)| same_record(record, matched))
            {
                let start = index.saturating_sub(around);
                let end = index
                    .saturating_add(around.saturating_add(1))
                    .min(timeline.len());
                indexes.extend(start..end);
            }
        }
        expanded.extend(indexes.into_iter().map(|index| timeline[index].clone()));
    }
    Ok(expanded)
}

fn same_record(left: &Record, right: &Record) -> bool {
    left.line == right.line
        && left.kind == right.kind
        && left.tool == right.tool
        && left.text == right.text
}

pub fn session_records(file: &SessionFile, kinds: &[Kind]) -> Result<Vec<Record>> {
    let selected = kinds.iter().copied().collect::<HashSet<_>>();
    let Some(map) = map_file(&file.path)? else {
        return Ok(Vec::new());
    };
    Ok(map
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .filter_map(|(index, line)| records_from_line(line, file, index + 1).ok())
        .flatten()
        .filter(|record| selected.contains(&record.kind))
        .collect())
}

pub fn session_metrics(file: &SessionFile, compaction_limit: usize) -> Result<SessionMetrics> {
    let mut counts = ActivityCounts::default();
    let mut usage = Usage::default();
    let mut models = Vec::new();
    let mut compactions = Vec::new();
    let mut first_timestamp = None;
    let mut last_timestamp = None;
    let mut previous = None;
    let Some(map) = map_file(&file.path)? else {
        return Ok(empty_session_metrics(file));
    };

    for (index, line) in map.split(|byte| *byte == b'\n').enumerate() {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        counts.events += 1;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let timestamp = event
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if first_timestamp.is_none() {
            first_timestamp = timestamp.clone();
        }
        if timestamp.is_some() {
            last_timestamp = timestamp.clone();
        }

        match event_type {
            "message" => {
                count_message(&event, &mut counts);
                note_assistant(&event, &mut usage, &mut models);
            }
            "custom_message" => {
                counts.custom_messages += 1;
                let custom_type = event.get("customType").and_then(Value::as_str);
                *counts
                    .custom_types
                    .entry(custom_type.unwrap_or("custom").to_owned())
                    .or_insert(0) += 1;
            }
            "compaction" => {
                counts.compactions += 1;
                let relay = compaction_relay(&event);
                counts.relay_resets += usize::from(relay.is_some());
                let preceding_event_gap_ms = previous
                    .as_ref()
                    .and_then(|event: &PreviousEvent| event.timestamp.as_deref())
                    .zip(timestamp.as_deref())
                    .and_then(|(start, end)| duration_ms(start, end));
                compactions.push(CompactionTiming {
                    number: counts.compactions,
                    line: index + 1,
                    finished_at: timestamp.clone().unwrap_or_default(),
                    tokens_before: event.get("tokensBefore").and_then(Value::as_u64),
                    from_hook: event.get("fromHook").and_then(Value::as_bool),
                    previous_event_at: previous.as_ref().and_then(|event| event.timestamp.clone()),
                    previous_event: previous.as_ref().map(|event| event.label.clone()),
                    previous_stop_reason: previous
                        .as_ref()
                        .and_then(|event| event.stop_reason.clone()),
                    preceding_event_gap_ms,
                    relay,
                });
            }
            "model_change" => counts.model_changes += 1,
            "thinking_level_change" => counts.thinking_level_changes += 1,
            "session" => counts.session_events += 1,
            _ => counts.other_events += 1,
        }

        let role = event.pointer("/message/role").and_then(Value::as_str);
        previous = Some(PreviousEvent {
            timestamp,
            label: role.map_or_else(|| event_type.to_owned(), |role| format!("message/{role}")),
            stop_reason: event
                .pointer("/message/stopReason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }

    if compaction_limit > 0 && compactions.len() > compaction_limit {
        compactions.drain(..compactions.len() - compaction_limit);
    }
    let started_at = first_timestamp.unwrap_or_else(|| file.started_at.clone());
    let ended_at = last_timestamp.unwrap_or_else(|| started_at.clone());
    Ok(SessionMetrics {
        project: file.project.clone(),
        cwd: file.cwd.clone(),
        session: file.session.clone(),
        started_at: started_at.clone(),
        ended_at: ended_at.clone(),
        elapsed_ms: duration_ms(&started_at, &ended_at),
        bytes: file.bytes,
        counts,
        usage,
        models,
        compactions_listed: compactions.len(),
        compaction_timing_note: "preceding_event_gap_ms is an upper bound, not a recorded compaction duration; Pi records completion but no explicit start",
        compactions,
        path: file.path.to_string_lossy().into_owned(),
    })
}

fn empty_session_metrics(file: &SessionFile) -> SessionMetrics {
    SessionMetrics {
        project: file.project.clone(),
        cwd: file.cwd.clone(),
        session: file.session.clone(),
        started_at: file.started_at.clone(),
        ended_at: file.started_at.clone(),
        elapsed_ms: None,
        bytes: file.bytes,
        counts: ActivityCounts::default(),
        usage: Usage::default(),
        models: Vec::new(),
        compactions_listed: 0,
        compaction_timing_note: "preceding_event_gap_ms is an upper bound, not a recorded compaction duration; Pi records completion but no explicit start",
        compactions: Vec::new(),
        path: file.path.to_string_lossy().into_owned(),
    }
}

fn count_message(event: &Value, counts: &mut ActivityCounts) {
    counts.message_events += 1;
    match event.pointer("/message/role").and_then(Value::as_str) {
        Some("user") => {
            counts.turns += 1;
            counts.user_messages += 1;
        }
        Some("assistant") => counts.assistant_messages += 1,
        Some("toolResult") => counts.tool_result_messages += 1,
        _ => {}
    }
    for part in event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match part.get("type").and_then(Value::as_str) {
            Some("text")
                if event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
            {
                counts.assistant_text_parts += 1;
            }
            Some("thinking") => counts.thinking_blocks += 1,
            Some("toolCall") => counts.tool_calls += 1,
            _ => {}
        }
    }
}

fn note_assistant(event: &Value, usage: &mut Usage, models: &mut Vec<String>) {
    let Some(message) = event
        .get("message")
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
    else {
        return;
    };
    if let Some(recorded) = message.get("usage") {
        let tokens = |key: &str| recorded.get(key).and_then(Value::as_u64).unwrap_or(0);
        usage.input = usage.input.saturating_add(tokens("input"));
        usage.output = usage.output.saturating_add(tokens("output"));
        usage.cache_read = usage.cache_read.saturating_add(tokens("cacheRead"));
        usage.cache_write = usage.cache_write.saturating_add(tokens("cacheWrite"));
        usage.total_tokens = usage.total_tokens.saturating_add(tokens("totalTokens"));
        usage.cost_usd += recorded
            .pointer("/cost/total")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
    }
    let model = match (
        message.get("provider").and_then(Value::as_str),
        message.get("model").and_then(Value::as_str),
    ) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) => Some(model.to_owned()),
        _ => None,
    };
    if let Some(model) = model.filter(|model| !models.contains(model)) {
        models.push(model);
    }
}

fn duration_ms(start: &str, end: &str) -> Option<u64> {
    let start = OffsetDateTime::parse(start, &Rfc3339).ok()?;
    let end = OffsetDateTime::parse(end, &Rfc3339).ok()?;
    u64::try_from((end - start).whole_milliseconds()).ok()
}

/// What a session set out to do and how it ended: the opening prompts,
/// compaction milestones, how the last turn stopped, the files it changed,
/// the tools that failed, and its final assistant message.
pub fn session_digest(
    file: &SessionFile,
    opening_limit: usize,
    milestone_limit: usize,
) -> Result<SessionDigest> {
    let mut digest = empty_digest(file);
    let Some(map) = map_file(&file.path)? else {
        return Ok(digest);
    };
    let mut snapshots = Vec::new();
    let mut span: Option<(String, String)> = None;
    let mut changes = Changes::default();
    let mut traffic = MsgTraffic::default();

    for (index, line) in map.split(|byte| *byte == b'\n').enumerate() {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(timestamp) = event.get("timestamp").and_then(Value::as_str) {
            let first = span.map_or_else(|| timestamp.to_owned(), |(first, _)| first);
            span = Some((first, timestamp.to_owned()));
        }
        for record in records_from_event(&event, file, index + 1) {
            digest.ended_at = record.timestamp.clone();
            *digest
                .counts
                .entry(record.kind.label().to_owned())
                .or_insert(0) += 1;
            match record.kind {
                Kind::User if digest.opening.len() < opening_limit => {
                    digest.opening.push(DigestEntry {
                        timestamp: record.timestamp,
                        text: clipped_compact(&record.text, 600),
                    });
                }
                Kind::Summary => {
                    let state = summary_state(&record.text);
                    let milestone = match &record.relay {
                        Some(relay) => Some(reset_milestone(relay, &state.goal)),
                        None => Some(state.goal.clone()).filter(|goal| !goal.is_empty()),
                    };
                    snapshots.extend(milestone.map(|text| DigestEntry {
                        timestamp: record.timestamp,
                        text,
                    }));
                    let goal = record.relay.and_then(|relay| relay.goal);
                    digest.latest_state = match (state.goal.is_empty(), goal) {
                        (true, Some(goal)) => LatestState {
                            goal: clipped_compact(&goal, 1_200),
                            ..state
                        },
                        _ => state,
                    };
                }
                Kind::Custom => {
                    if let Some(msg) = &record.msg {
                        note_msg(&mut traffic, msg, &file.session);
                    }
                }
                Kind::Assistant => {
                    digest.final_message = Some(DigestEntry {
                        timestamp: record.timestamp,
                        text: record.text,
                    });
                }
                _ => {}
            }
        }
        note_turn(&event, &mut digest.outcome);
        note_failure(&event, index + 1, &mut digest.failed_tools);
        note_change(&event, &mut changes);
    }

    digest.milestones = sampled_entries(snapshots, milestone_limit);
    digest.msg = (traffic.sent + traffic.got > 0).then_some(traffic);
    digest.files_changed = changes.done.into_values().collect();
    digest.elapsed_ms = span.and_then(|(first, last)| duration_ms(&first, &last));
    Ok(digest)
}

fn empty_digest(file: &SessionFile) -> SessionDigest {
    SessionDigest {
        project: file.project.clone(),
        cwd: file.cwd.clone(),
        session: file.session.clone(),
        started_at: file.started_at.clone(),
        started_at_iso: iso_timestamp(&file.started_at),
        ended_at: file.started_at.clone(),
        elapsed_ms: None,
        bytes: file.bytes,
        counts: BTreeMap::new(),
        outcome: Outcome::default(),
        opening: Vec::new(),
        milestones: Vec::new(),
        latest_state: LatestState::default(),
        files_changed: Vec::new(),
        failed_tools: Failures::default(),
        msg: None,
        final_message: None,
        path: file.path.to_string_lossy().into_owned(),
    }
}

/// `relay reset #2 at 48%: <its note>`, else its goal.
fn reset_milestone(relay: &Relay, summary_goal: &str) -> String {
    let about = [
        relay.note.as_deref(),
        relay.goal.as_deref(),
        Some(summary_goal),
    ]
    .into_iter()
    .flatten()
    .find(|text| !text.trim().is_empty());
    match about {
        Some(text) => format!("{}: {}", reset_label(relay), clipped_compact(text, 600)),
        None => reset_label(relay),
    }
}

/// Counts a msg message as sent when this session is its sender, else as
/// got, from its peer or from a human.
fn note_msg(traffic: &mut MsgTraffic, msg: &Msg, session: &str) {
    let outgoing = msg.from.id.eq_ignore_ascii_case(session);
    match (outgoing, msg.by_user, msg.to.as_deref()) {
        (true, _, to) => {
            traffic.sent = traffic.sent.saturating_add(1);
            if let Some(to) = to {
                let peer = peer(&mut traffic.peers, to);
                peer.sent = peer.sent.saturating_add(1);
            }
        }
        (false, true, _) => {
            traffic.got = traffic.got.saturating_add(1);
            traffic.from_user = traffic.from_user.saturating_add(1);
        }
        (false, false, _) => {
            traffic.got = traffic.got.saturating_add(1);
            let peer = peer(&mut traffic.peers, &msg.from.id);
            peer.got = peer.got.saturating_add(1);
            peer.name = msg.from.name.clone().or(peer.name.take());
            peer.exit = exit_kind(msg).map(str::to_owned).or(peer.exit.take());
        }
    }
}

fn peer<'a>(peers: &'a mut Vec<Peer>, id: &str) -> &'a mut Peer {
    let index = match peers
        .iter()
        .position(|peer| peer.id.eq_ignore_ascii_case(id))
    {
        Some(index) => index,
        None => {
            peers.push(Peer {
                id: id.to_owned(),
                name: None,
                sent: 0,
                got: 0,
                exit: None,
            });
            peers.len() - 1
        }
    };
    &mut peers[index]
}

/// Where the turn stood at the last message. Pi appends each message whole
/// when it ends, so a prompt, a tool result, or a msg or relay message (they
/// wake the model or keep it going) means the model owed an answer, and an
/// assistant message that called tools means they were running. Other roles
/// and entries say nothing.
fn note_turn(event: &Value, outcome: &mut Outcome) {
    let message = match event.get("type").and_then(Value::as_str) {
        Some("message") => event.get("message"),
        Some("custom_message") => {
            if let Some(MSG | RELAY) = event.get("customType").and_then(Value::as_str) {
                outcome.open_turn = Some("thinking");
            }
            return;
        }
        _ => None,
    };
    let Some(message) = message else {
        return;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user" | "toolResult") => outcome.open_turn = Some("thinking"),
        Some("assistant") => {
            let stop_reason = message.get("stopReason").and_then(Value::as_str);
            outcome.open_turn = (stop_reason == Some("toolUse")).then_some("running-tools");
            outcome.stop_reason = stop_reason.map(str::to_owned);
            outcome.error_message = message
                .get("errorMessage")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        _ => {}
    }
}

fn note_failure(event: &Value, line: usize, failures: &mut Failures) {
    let Some(message) = event.get("message").filter(|message| {
        message.get("role").and_then(Value::as_str) == Some("toolResult")
            && message.get("isError").and_then(Value::as_bool) == Some(true)
    }) else {
        return;
    };
    let text = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    failures.count = failures.count.saturating_add(1);
    failures.recent.push(FailedTool {
        timestamp: event
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        line,
        tool: message
            .get("toolName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        text: clipped_tail(&text, 300),
    });
    if failures.recent.len() > RECENT_FAILURES {
        failures.recent.remove(0);
    }
}

/// Files changed through pi's edit and write tools, counted once a call's
/// result comes back without an error.
fn note_change(event: &Value, changes: &mut Changes) {
    let Some(message) = event.get("message") else {
        return;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(file_call)
            .for_each(|(id, tool, path)| {
                changes.pending.insert(id, (tool, path));
            }),
        Some("toolResult") => {
            let failed = message.get("isError").and_then(Value::as_bool) == Some(true);
            let call = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .and_then(|id| changes.pending.remove(id))
                .filter(|_| !failed);
            if let Some((tool, path)) = call {
                let change = changes
                    .done
                    .entry(path.clone())
                    .or_insert_with(|| FileChange {
                        path,
                        edits: 0,
                        writes: 0,
                    });
                match tool {
                    FileTool::Edit => change.edits = change.edits.saturating_add(1),
                    FileTool::Write => change.writes = change.writes.saturating_add(1),
                }
            }
        }
        _ => {}
    }
}

fn file_call(part: &Value) -> Option<(String, FileTool, String)> {
    if part.get("type").and_then(Value::as_str) != Some("toolCall") {
        return None;
    }
    let tool = match part.get("name").and_then(Value::as_str)? {
        "edit" => FileTool::Edit,
        "write" => FileTool::Write,
        _ => return None,
    };
    let path = part.pointer("/arguments/path")?.as_str()?.to_owned();
    let id = part.get("id")?.as_str()?.to_owned();
    Some((id, tool, path))
}

pub fn session_title(file: &SessionFile) -> Result<String> {
    let Some(map) = map_file(&file.path)? else {
        return Ok(String::new());
    };
    Ok(map
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(index, line)| records_from_line(line, file, index + 1).ok())
        .flatten()
        .find(|record| record.kind == Kind::User)
        .map_or_else(String::new, |record| compact(&record.text)))
}

pub fn session_matches(file: &SessionFile, selector: &str) -> bool {
    let selector = selector.to_lowercase();
    file.session.to_lowercase().starts_with(&selector)
        || file
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_lowercase().contains(&selector))
}

fn id_matches(file: &SessionFile, selector: &str) -> bool {
    file.session
        .to_lowercase()
        .starts_with(&selector.to_lowercase())
}

/// `latest`, or the one session whose id starts with the selector. Only when
/// no id does, the one whose file name contains it (a date, say).
pub fn select_session<'a>(files: &'a [SessionFile], selector: &str) -> Result<&'a SessionFile> {
    if selector.eq_ignore_ascii_case("latest") {
        return files.first().context("no sessions found");
    }
    let by_id = files
        .iter()
        .filter(|file| id_matches(file, selector))
        .collect::<Vec<_>>();
    let matches = match by_id.is_empty() {
        true => files
            .iter()
            .filter(|file| session_matches(file, selector))
            .collect(),
        false => by_id,
    };
    match matches.as_slice() {
        [] => bail!("no session matched '{selector}'"),
        [file] => Ok(*file),
        _ => bail!("{}", ambiguity(selector, &matches, files)),
    }
}

fn ambiguity(selector: &str, matches: &[&SessionFile], files: &[SessionFile]) -> String {
    let ids = files
        .iter()
        .map(|file| file.session.clone())
        .collect::<Vec<_>>();
    let lengths = unique_prefix_lengths(&ids, SHORT_ID);
    let choices = matches
        .iter()
        .take(AMBIGUOUS_SHOWN)
        .map(|file| {
            let length = lengths.get(&file.session).copied().unwrap_or(SHORT_ID);
            let title = session_title(file)
                .map(|title| clipped_compact(&title, 60))
                .unwrap_or_default();
            format!(
                "  {}  {}  {}  {title}",
                short_id(&file.session, length),
                display_timestamp(&file.started_at),
                file.cwd,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let more = match matches.len().saturating_sub(AMBIGUOUS_SHOWN) {
        0 => String::new(),
        more => format!("\n  … and {more} more"),
    };
    format!(
        "session selector '{selector}' matches {} sessions; use a longer prefix:\n{choices}{more}",
        matches.len()
    )
}

pub fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn summary_state(text: &str) -> LatestState {
    LatestState {
        goal: clipped_compact(&markdown_section(text, "## Goal"), 1_200),
        in_progress: clipped_compact(&markdown_section(text, "### In Progress"), 1_200),
        blocked: clipped_compact(&markdown_section(text, "### Blocked"), 1_200),
        next_steps: clipped_compact(&markdown_section(text, "## Next Steps"), 1_200),
    }
}

fn markdown_section(text: &str, heading: &str) -> String {
    text.lines()
        .skip_while(|line| line.trim() != heading)
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ")
}

fn clipped_tail(text: &str, limit: usize) -> String {
    let text = compact(text);
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

fn clipped_compact(text: &str, limit: usize) -> String {
    let text = compact(text);
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn sampled_entries(entries: Vec<DigestEntry>, limit: usize) -> Vec<DigestEntry> {
    if limit == 0 {
        return Vec::new();
    }
    if entries.len() <= limit {
        return entries;
    }
    if limit == 1 {
        return entries.last().cloned().into_iter().collect();
    }
    let last = entries.len() - 1;
    (0..limit)
        .map(|index| entries[index * last / (limit - 1)].clone())
        .collect()
}

fn search_file(
    file: &SessionFile,
    options: &SearchOptions<'_>,
    matcher: &Matcher,
    kinds: &HashSet<Kind>,
) -> Result<FileMatches> {
    let Some(map) = map_file(&file.path)? else {
        return Ok(FileMatches {
            records: Vec::new(),
            candidate_lines: 0,
            parsed_lines: 0,
        });
    };
    let mut records = Vec::new();
    let mut candidate_lines = 0;
    let mut parsed_lines = 0;

    for (index, line) in map.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() || !matcher.matches_raw(line) {
            continue;
        }
        candidate_lines += 1;
        let Ok(candidates) = records_from_line(line, file, index + 1) else {
            continue;
        };
        parsed_lines += 1;
        records.extend(candidates.into_iter().filter(|record| {
            kinds.contains(&record.kind)
                && (!options.errors_only || record.error)
                && within_bounds(&record.timestamp, options.since, options.until)
                && tool_matches(record, options.tool)
                && matcher.matches_record(record)
        }));
    }

    Ok(FileMatches {
        records,
        candidate_lines,
        parsed_lines,
    })
}

fn records_from_line(
    line: &[u8],
    file: &SessionFile,
    line_number: usize,
) -> serde_json::Result<Vec<Record>> {
    serde_json::from_slice::<Value>(line).map(|event| records_from_event(&event, file, line_number))
}

fn records_from_event(event: &Value, file: &SessionFile, line_number: usize) -> Vec<Record> {
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or(&file.started_at);
    let record =
        |kind, text: &str| make_record(file, timestamp, line_number, (kind, None, false), text);

    match event.get("type").and_then(Value::as_str) {
        Some("compaction") => event
            .get("summary")
            .and_then(Value::as_str)
            .map(|summary| Record {
                relay: compaction_relay(event),
                ..record(Kind::Summary, summary)
            })
            .into_iter()
            .collect(),
        Some("custom_message") => vec![custom_record(event, record(Kind::Custom, ""))],
        _ => event
            .get("message")
            .map(|message| {
                let carried = match message.get("isError").and_then(Value::as_bool) {
                    Some(true) => Vec::new(),
                    _ => tool_messages(message),
                };
                message_records(message, file, timestamp, line_number)
                    .into_iter()
                    .chain(
                        carried
                            .into_iter()
                            .map(|carried| msg_record(carried, "", record(Kind::Custom, ""))),
                    )
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// A custom message: msg's and relay's with their details, any other
/// extension's with its type and text.
fn custom_record(event: &Value, base: Record) -> Record {
    let custom_type = event.get("customType").and_then(Value::as_str);
    let content = content_text(event.get("content"));
    match (custom_type, event.get("details")) {
        (Some(MSG), Some(details)) => msg_record(details, &content, base),
        (Some(RELAY), details) => Record {
            custom_type: Some(RELAY.to_owned()),
            relay: Some(details.map(parse_relay).unwrap_or_default()),
            text: content,
            ..base
        },
        _ => Record {
            custom_type: custom_type.map(str::to_owned),
            text: content,
            ..base
        },
    }
}

/// A msg message, by its Message object: the text the sender wrote, not the
/// framing the model read around it.
fn msg_record(message: &Value, framed: &str, base: Record) -> Record {
    Record {
        custom_type: Some(MSG.to_owned()),
        msg: parse_msg(message),
        text: message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or(framed)
            .to_owned(),
        ..base
    }
}

fn message_records(
    message: &Value,
    file: &SessionFile,
    timestamp: &str,
    line_number: usize,
) -> Vec<Record> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = message
        .get("toolName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let failed =
        role == "toolResult" && message.get("isError").and_then(Value::as_bool) == Some(true);
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };

    content
        .iter()
        .filter_map(|part| normalize_part(part, role, tool_name.as_deref()))
        .map(|(kind, tool, text)| {
            let error = failed && kind == Kind::ToolResult;
            make_record(file, timestamp, line_number, (kind, tool, error), &text)
        })
        .collect()
}

fn normalize_part(
    part: &Value,
    role: &str,
    result_tool: Option<&str>,
) -> Option<(Kind, Option<String>, String)> {
    let part_type = part.get("type").and_then(Value::as_str)?;
    match (role, part_type) {
        ("user", "text") => Some((Kind::User, None, part.get("text")?.as_str()?.to_owned())),
        ("assistant", "text") => Some((
            Kind::Assistant,
            None,
            part.get("text")?.as_str()?.to_owned(),
        )),
        ("assistant", "thinking") => Some((
            Kind::Thinking,
            None,
            part.get("thinking")?.as_str()?.to_owned(),
        )),
        ("assistant", "toolCall") => {
            let name = part.get("name")?.as_str()?.to_owned();
            let arguments = part
                .get("arguments")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_owned());
            Some((Kind::ToolCall, Some(name), arguments))
        }
        ("toolResult", "text") => Some((
            Kind::ToolResult,
            result_tool.map(str::to_owned),
            part.get("text")?.as_str()?.to_owned(),
        )),
        _ => None,
    }
}

fn make_record(
    file: &SessionFile,
    timestamp: &str,
    line: usize,
    (kind, tool, error): (Kind, Option<String>, bool),
    text: &str,
) -> Record {
    Record {
        project: file.project.clone(),
        cwd: file.cwd.clone(),
        session: file.session.clone(),
        started_at: file.started_at.clone(),
        timestamp: timestamp.to_owned(),
        line,
        kind,
        tool,
        error,
        custom_type: None,
        msg: None,
        relay: None,
        text: text.to_owned(),
        path: file.path.to_string_lossy().into_owned(),
    }
}

fn tool_matches(record: &Record, expected: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(expected) => record
            .tool
            .as_deref()
            .is_some_and(|tool| tool.eq_ignore_ascii_case(expected)),
    }
}

pub fn within_bounds(timestamp: &str, since: Option<&str>, until: Option<&str>) -> bool {
    let lower_ok = since.is_none_or(|bound| compare_timestamp(timestamp, bound).is_ge());
    let upper_ok = until.is_none_or(|bound| compare_timestamp(timestamp, bound).is_le());
    lower_ok && upper_ok
}

fn compare_timestamp(timestamp: &str, bound: &str) -> std::cmp::Ordering {
    if bound.len() == 10 {
        timestamp.get(..10).unwrap_or(timestamp).cmp(bound)
    } else {
        timestamp.cmp(bound)
    }
}

pub fn validate_bound(bound: Option<&str>) -> Result<()> {
    if let Some(value) = bound
        && (value.len() < 10
            || value.as_bytes().get(4).is_none_or(|byte| *byte != b'-')
            || value.as_bytes().get(7).is_none_or(|byte| *byte != b'-'))
    {
        bail!("invalid ISO date/timestamp: {value}");
    }
    Ok(())
}

fn map_file(path: &Path) -> Result<Option<Mmap>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    let map = unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to map {}", path.display()))?;
    Ok(Some(map))
}

impl Matcher {
    fn new(query: &str, is_regex: bool, case_sensitive: bool) -> Result<Self> {
        let pattern = if is_regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let text = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .with_context(|| format!("invalid regex: {query}"))?;
        let raw = if is_regex {
            None
        } else {
            let encoded = serde_json::to_string(query)?;
            let fragment = encoded
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(query);
            Some(
                bytes::RegexBuilder::new(&regex::escape(fragment))
                    .case_insensitive(!case_sensitive)
                    .build()?,
            )
        };
        Ok(Self { text, raw })
    }

    fn matches_raw(&self, line: &[u8]) -> bool {
        self.raw
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(line))
    }

    fn matches_record(&self, record: &Record) -> bool {
        self.text.is_match(&record.text)
            || record
                .tool
                .as_deref()
                .is_some_and(|tool| self.text.is_match(tool))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONVERSATION, DigestEntry, Matcher, SearchOptions, compare_timestamp, conversation,
        normalize_part, sampled_entries, search, select_session, session_digest, session_matches,
        session_metrics, session_records, summary_state,
    };
    use crate::{
        cli::{Kind, SortOrder},
        source::SessionFile,
    };
    use serde_json::json;
    use std::{cmp::Ordering, fs, path::PathBuf};

    #[test]
    fn normalizes_tool_calls_without_unrelated_fields() {
        let part = json!({
            "type": "toolCall",
            "id": "call-1",
            "name": "bash",
            "arguments": {"command": "rg needle"}
        });
        let (kind, tool, text) = normalize_part(&part, "assistant", None).unwrap();
        assert_eq!(kind, Kind::ToolCall);
        assert_eq!(tool.as_deref(), Some("bash"));
        assert_eq!(text, r#"{"command":"rg needle"}"#);
    }

    #[test]
    fn fixed_matcher_handles_json_escaped_quotes() {
        let matcher = Matcher::new(r#"say "hi""#, false, false).unwrap();
        assert!(matcher.matches_raw(br#"{"text":"say \"hi\""}"#));
    }

    #[test]
    fn date_bounds_compare_calendar_dates() {
        assert_eq!(
            compare_timestamp("2026-04-20T10:00:00Z", "2026-04-20"),
            Ordering::Equal
        );
    }

    #[test]
    fn extracts_useful_compaction_sections() {
        let state = summary_state(
            "## Goal\nship recovery\n## Progress\n### In Progress\n- final review\n### Blocked\n- rollout\n## Next Steps\n1. verify\n## Context\nnoise",
        );
        assert_eq!(state.goal, "ship recovery");
        assert_eq!(state.in_progress, "- final review");
        assert_eq!(state.blocked, "- rollout");
        assert_eq!(state.next_steps, "1. verify");
    }

    #[test]
    fn samples_milestones_across_the_full_timeline() {
        let entries = (0..5)
            .map(|index| DigestEntry {
                timestamp: index.to_string(),
                text: index.to_string(),
            })
            .collect();
        let sampled = sampled_entries(entries, 3);
        assert_eq!(
            sampled
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["0", "2", "4"]
        );
    }

    #[test]
    fn expands_neighbors_after_limiting_matches() {
        let path =
            std::env::temp_dir().join(format!("lookup-around-test-{}.jsonl", std::process::id()));
        let events = [
            ("2026-01-01T00:00:01Z", "user", "before"),
            ("2026-01-01T00:00:02Z", "assistant", "needle"),
            ("2026-01-01T00:00:03Z", "user", "after"),
        ]
        .map(|(timestamp, role, text)| {
            json!({
                "type": "message",
                "timestamp": timestamp,
                "message": {
                    "role": role,
                    "content": [{"type": "text", "text": text}]
                }
            })
            .to_string()
        })
        .join("\n");
        fs::write(&path, format!("{events}\n")).unwrap();
        let file = SessionFile {
            project: "demo".to_owned(),
            cwd: "/tmp/demo".to_owned(),
            path: path.clone(),
            session: "around-test".to_owned(),
            started_at: "2026-01-01T00-00-00Z".to_owned(),
            bytes: path.metadata().unwrap().len(),
        };
        let output = search(
            &[file],
            &SearchOptions {
                kinds: &[Kind::User, Kind::Assistant],
                query: "needle",
                regex: false,
                case_sensitive: false,
                tool: None,
                errors_only: false,
                since: None,
                until: None,
                sort: SortOrder::Oldest,
                around: 1,
                limit: 1,
            },
        )
        .unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(output.match_count, 1);
        assert_eq!(
            output
                .records
                .iter()
                .map(|record| record.text.as_str())
                .collect::<Vec<_>>(),
            ["before", "needle", "after"]
        );
    }

    #[test]
    fn counts_activity_and_reports_compaction_event_gap() {
        let path =
            std::env::temp_dir().join(format!("lookup-stats-test-{}.jsonl", std::process::id()));
        let events = [
            json!({"type":"session","timestamp":"2026-01-01T00:00:00Z"}),
            json!({
                "type":"message",
                "timestamp":"2026-01-01T00:00:01Z",
                "message":{"role":"user","content":[{"type":"text","text":"go"}]}
            }),
            json!({
                "type":"message",
                "timestamp":"2026-01-01T00:00:02Z",
                "message":{
                    "role":"assistant",
                    "stopReason":"toolUse",
                    "content":[
                        {"type":"thinking","thinking":"hmm"},
                        {"type":"text","text":"running"},
                        {"type":"toolCall","name":"bash","arguments":{}}
                    ]
                }
            }),
            json!({
                "type":"message",
                "timestamp":"2026-01-01T00:00:03Z",
                "message":{"role":"toolResult","content":[{"type":"text","text":"ok"}]}
            }),
            json!({
                "type":"compaction",
                "timestamp":"2026-01-01T00:00:05Z",
                "tokensBefore":123,
                "fromHook":false,
                "summary":"state"
            }),
        ]
        .map(|event| event.to_string())
        .join("\n");
        fs::write(&path, format!("{events}\n")).unwrap();
        let file = SessionFile {
            project: "demo".to_owned(),
            cwd: "/tmp/demo".to_owned(),
            path: path.clone(),
            session: "stats-test".to_owned(),
            started_at: "2026-01-01T00-00-00Z".to_owned(),
            bytes: path.metadata().unwrap().len(),
        };
        let metrics = session_metrics(&file, 20).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(metrics.counts.events, 5);
        assert_eq!(metrics.counts.turns, 1);
        assert_eq!(metrics.counts.user_messages, 1);
        assert_eq!(metrics.counts.assistant_messages, 1);
        assert_eq!(metrics.counts.tool_result_messages, 1);
        assert_eq!(metrics.counts.thinking_blocks, 1);
        assert_eq!(metrics.counts.tool_calls, 1);
        assert_eq!(metrics.counts.compactions, 1);
        assert_eq!(metrics.elapsed_ms, Some(5_000));
        assert_eq!(metrics.compactions[0].preceding_event_gap_ms, Some(2_000));
        assert_eq!(metrics.compactions[0].tokens_before, Some(123));
    }

    #[test]
    fn matches_session_ids_and_filenames() {
        let file = SessionFile {
            project: "demo".to_owned(),
            cwd: "/tmp/demo".to_owned(),
            path: PathBuf::from("2026-01-01T00-00-00Z_abcdef12-3456.jsonl"),
            session: "abcdef12-3456".to_owned(),
            started_at: "2026-01-01T00-00-00Z".to_owned(),
            bytes: 1,
        };
        assert!(session_matches(&file, "ABCDEF12"));
        assert!(session_matches(&file, "2026-01-01"));
        assert!(!session_matches(&file, "deadbeef"));
    }

    fn demo_file(path: PathBuf, session: &str, started_at: &str) -> SessionFile {
        let bytes = path.metadata().map_or(1, |metadata| metadata.len());
        SessionFile {
            project: "demo".to_owned(),
            cwd: "/tmp/demo".to_owned(),
            path,
            session: session.to_owned(),
            started_at: started_at.to_owned(),
            bytes,
        }
    }

    fn write_events(name: &str, events: &[serde_json::Value]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lookup-{name}-test-{}.jsonl", std::process::id()));
        let lines = events
            .iter()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, format!("{lines}\n")).unwrap();
        path
    }

    #[test]
    fn prefers_id_prefixes_and_names_unique_choices() {
        let files = [
            demo_file(
                PathBuf::from("/nowhere/2026-01-02T00-00-01Z_01a151a6-4175-71a6.jsonl"),
                "01a151a6-4175-71a6",
                "2026-01-02T00-00-01Z",
            ),
            demo_file(
                PathBuf::from("/nowhere/2026-01-02T00-00-00Z_01a151a6-4047-76d7.jsonl"),
                "01a151a6-4047-76d7",
                "2026-01-02T00-00-00Z",
            ),
            demo_file(
                PathBuf::from("/nowhere/2026-01-01T00-00-00Z_9f00-01a151a6.jsonl"),
                "9f00-01a151a6",
                "2026-01-01T00-00-00Z",
            ),
        ];
        let error = select_session(&files, "01a151a6").unwrap_err().to_string();
        assert!(error.contains("matches 2 sessions"), "{error}");
        assert!(
            error.contains("01a151a6-41  2026-01-02 00:00:01"),
            "{error}"
        );
        assert!(
            error.contains("01a151a6-40  2026-01-02 00:00:00"),
            "{error}"
        );
        assert_eq!(
            select_session(&files, "01A151A6-41").unwrap().session,
            "01a151a6-4175-71a6"
        );
        assert_eq!(
            select_session(&files, "9f00").unwrap().session,
            "9f00-01a151a6"
        );
        assert_eq!(
            select_session(&files, "2026-01-01").unwrap().session,
            "9f00-01a151a6"
        );
        assert!(select_session(&files, "2026-01-02").is_err());
    }

    #[test]
    fn flags_failed_tool_results_and_filters_on_them() {
        let path = write_events(
            "errors",
            &[
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:01Z",
                    "message": {"role": "toolResult", "toolName": "bash", "isError": true,
                        "content": [{"type": "text", "text": "boom: exit 1"}]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:02Z",
                    "message": {"role": "toolResult", "toolName": "bash", "isError": false,
                        "content": [{"type": "text", "text": "boom: fine"}]}
                }),
            ],
        );
        let file = demo_file(path.clone(), "errors-test", "2026-01-01T00-00-00Z");
        let options = |errors_only| SearchOptions {
            kinds: &[Kind::ToolResult],
            query: "boom",
            regex: false,
            case_sensitive: false,
            tool: None,
            errors_only,
            since: None,
            until: None,
            sort: SortOrder::Oldest,
            around: 0,
            limit: 0,
        };
        let all = search(std::slice::from_ref(&file), &options(false)).unwrap();
        let failed = search(std::slice::from_ref(&file), &options(true)).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            all.records
                .iter()
                .map(|record| record.error)
                .collect::<Vec<_>>(),
            [true, false]
        );
        assert_eq!(
            failed
                .records
                .iter()
                .map(|record| record.text.as_str())
                .collect::<Vec<_>>(),
            ["boom: exit 1"]
        );
    }

    #[test]
    fn digests_how_a_session_ended() {
        let path = write_events(
            "digest",
            &[
                json!({"type": "session", "timestamp": "2026-01-01T00:00:00Z", "cwd": "/tmp/demo"}),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:01Z",
                    "message": {"role": "user", "content": [{"type": "text", "text": "do it"}]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:02Z",
                    "message": {"role": "assistant", "stopReason": "toolUse", "content": [
                        {"type": "toolCall", "id": "c1", "name": "edit",
                            "arguments": {"path": "src/a.rs", "edits": []}},
                        {"type": "toolCall", "id": "c2", "name": "write",
                            "arguments": {"path": "src/b.rs", "content": "x"}},
                        {"type": "toolCall", "id": "c3", "name": "bash",
                            "arguments": {"command": "cargo test"}}
                    ]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:03Z",
                    "message": {"role": "toolResult", "toolCallId": "c1", "toolName": "edit",
                        "isError": false, "content": [{"type": "text", "text": "ok"}]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:04Z",
                    "message": {"role": "toolResult", "toolCallId": "c2", "toolName": "write",
                        "isError": true, "content": [{"type": "text", "text": "permission denied"}]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:05Z",
                    "message": {"role": "toolResult", "toolCallId": "c3", "toolName": "bash",
                        "isError": true, "content": [{"type": "text",
                            "text": "test failed\nCommand exited with code 101"}]}
                }),
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:01:00Z",
                    "message": {"role": "assistant", "stopReason": "stop", "content": [
                        {"type": "text", "text": "done: a.rs edited\n- check b.rs perms"}
                    ]}
                }),
            ],
        );
        let file = demo_file(path.clone(), "digest-test", "2026-01-01T00-00-00Z");
        let done = session_digest(&file, 3, 8).unwrap();
        let prompt = json!({
            "type": "message",
            "timestamp": "2026-01-01T00:02:00Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "and more"}]}
        });
        let text = format!("{}{prompt}\n", fs::read_to_string(&path).unwrap());
        fs::write(&path, text).unwrap();
        let open = session_digest(&file, 3, 8).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(done.outcome.stop_reason.as_deref(), Some("stop"));
        assert_eq!(done.outcome.open_turn, None);
        assert_eq!(
            done.final_message.as_ref().map(|entry| entry.text.as_str()),
            Some("done: a.rs edited\n- check b.rs perms")
        );
        assert_eq!(done.elapsed_ms, Some(60_000));
        assert_eq!(done.started_at_iso.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            done.files_changed
                .iter()
                .map(|change| (change.path.as_str(), change.edits, change.writes))
                .collect::<Vec<_>>(),
            [("src/a.rs", 1, 0)]
        );
        assert_eq!(done.failed_tools.count, 2);
        assert_eq!(
            done.failed_tools
                .recent
                .iter()
                .map(|failure| failure.tool.as_deref())
                .collect::<Vec<_>>(),
            [Some("write"), Some("bash")]
        );
        assert_eq!(
            done.failed_tools.recent[1].text,
            "test failed Command exited with code 101"
        );
        assert_eq!(open.outcome.open_turn, Some("thinking"));
        assert_eq!(open.opening.len(), 2);
    }

    #[test]
    fn stats_total_usage_and_models() {
        let assistant = |model: serde_json::Value, usage: serde_json::Value| {
            json!({
                "type": "message",
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "assistant", "stopReason": "stop", "content": [],
                    "provider": model["provider"], "model": model["model"], "usage": usage}
            })
        };
        let path = write_events(
            "usage",
            &[
                assistant(
                    json!({"provider": "anthropic", "model": "opus"}),
                    json!({"input": 10, "output": 20, "cacheRead": 300, "cacheWrite": 4,
                        "totalTokens": 334, "cost": {"total": 0.5}}),
                ),
                assistant(
                    json!({"provider": "anthropic", "model": "opus"}),
                    json!({"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4,
                        "totalTokens": 10, "cost": {"total": 0.25}}),
                ),
                assistant(json!({"model": "gpt"}), json!(null)),
            ],
        );
        let file = demo_file(path.clone(), "usage-test", "2026-01-01T00-00-00Z");
        let metrics = session_metrics(&file, 20).unwrap();
        fs::remove_file(path).unwrap();
        let usage = &metrics.usage;
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.total_tokens
            ),
            (11, 22, 303, 8, 344)
        );
        assert!((usage.cost_usd - 0.75).abs() < 1e-9);
        assert_eq!(metrics.models, ["anthropic/opus", "gpt"]);
    }

    const MAIN: &str = "01a15300-0000-7000-8000-00000000000a";
    const CSV: &str = "01a15301-0000-7000-8000-00000000000c";

    /// A session from fixtures/tandem: an orchestrator (`main`) and its csv
    /// child, written from the msg and relay contracts.
    fn tandem(session: &str) -> SessionFile {
        let folder = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/tandem"));
        let path = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.to_string_lossy()
                    .ends_with(&format!("_{session}.jsonl"))
            })
            .unwrap();
        let started_at = path.file_name().unwrap().to_string_lossy()[..24].to_owned();
        demo_file(path, session, &started_at)
    }

    #[test]
    fn reads_custom_messages_and_msgs_in_msg_tool_results() {
        let records = session_records(&tandem(MAIN), &[Kind::Custom]).unwrap();
        let summary = records
            .iter()
            .map(|record| {
                (
                    record.line,
                    record.custom_type.as_deref().unwrap_or(""),
                    record.msg.as_ref().map(|msg| msg.from.id.as_str()),
                    record.text.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (5, "msg", Some(CSV), "keep the header row in the output?"),
                (7, "msg", Some(MAIN), "yes, keep the header row"),
                (
                    9,
                    "msg",
                    Some(CSV),
                    "done: csv parser in src/csv.rs, 3 files, tests pass. kept the header row."
                ),
                (
                    10,
                    "msg",
                    Some("01a15302-0000-7000-8000-00000000000d"),
                    "db's process is gone without ending (pid 4242)"
                ),
                (
                    11,
                    "msg",
                    Some("01a152ff-0000-7000-8000-0000000000ff"),
                    "merge csv now, rerun db after"
                ),
                (
                    12,
                    "relay",
                    None,
                    "45% of context used: finish the step, commit, then call relay with a note for whoever picks this up."
                ),
                (15, "relay", None, "Context reset #1 at 48%."),
                (18, "handoff", None, "another extension's note"),
            ]
        );
        assert_eq!(
            records[2].msg.as_ref().unwrap().kind.as_deref(),
            Some("ended")
        );
        assert!(records[4].msg.as_ref().unwrap().by_user);
        assert_eq!(records[6].relay.as_ref().unwrap().n, Some(1));
        let users = session_records(&tandem(MAIN), &[Kind::User]).unwrap();
        assert_eq!(users.len(), 1);

        let child = session_records(&tandem(CSV), &[Kind::Custom]).unwrap();
        assert_eq!(
            child
                .iter()
                .map(|record| (record.line, record.msg.as_ref().unwrap().from.id.as_str()))
                .collect::<Vec<_>>(),
            [(4, CSV), (4, MAIN)]
        );
    }

    #[test]
    fn failed_msg_tools_carry_no_messages() {
        let sent = json!({"v": 1, "id": "m1", "ts": 1, "from": {"id": "me"}, "to": "you",
            "text": "hi", "delivery": "steer"});
        let path = write_events(
            "msg-error",
            &[json!({
                "type": "message",
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "toolResult", "toolName": "msg_send", "isError": true,
                    "content": [{"type": "text", "text": "no such session"}],
                    "details": {"message": sent}}
            })],
        );
        let file = demo_file(path.clone(), "msg-error-test", "2026-01-01T00-00-00Z");
        let records = session_records(&file, &[Kind::Custom, Kind::ToolResult]).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            records.iter().map(|record| record.kind).collect::<Vec<_>>(),
            [Kind::ToolResult]
        );
    }

    #[test]
    fn conversation_keeps_msg_traffic_and_leaves_relay_out() {
        let lines = session_records(&tandem(MAIN), &CONVERSATION)
            .unwrap()
            .into_iter()
            .filter(conversation)
            .map(|record| (record.line, record.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                (2, Kind::User),
                (3, Kind::Assistant),
                (5, Kind::Custom),
                (7, Kind::Custom),
                (9, Kind::Custom),
                (10, Kind::Custom),
                (11, Kind::Custom),
                (13, Kind::Assistant),
                (17, Kind::Assistant),
            ]
        );
    }

    #[test]
    fn digests_msg_traffic_and_relay_resets() {
        let digest = session_digest(&tandem(MAIN), 3, 8).unwrap();
        let traffic = digest.msg.unwrap();
        assert_eq!((traffic.sent, traffic.got, traffic.from_user), (1, 4, 1));
        assert_eq!(
            traffic
                .peers
                .iter()
                .map(|peer| (
                    peer.name.as_deref(),
                    peer.sent,
                    peer.got,
                    peer.exit.as_deref()
                ))
                .collect::<Vec<_>>(),
            [
                (Some("csv"), 1, 2, Some("ended")),
                (Some("db"), 0, 1, Some("died"))
            ]
        );
        assert_eq!(
            digest
                .milestones
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["relay reset #1 at 48%: csv merged; db died, rerun it in fork 2"]
        );
        assert_eq!(
            digest.latest_state.goal,
            "fan out csv and db, merge what lands"
        );
        assert_eq!(digest.counts.get("custom"), Some(&8));
        assert_eq!(digest.counts.get("user"), Some(&1));
        assert_eq!(digest.outcome.open_turn, None);

        let child = session_digest(&tandem(CSV), 3, 8).unwrap().msg.unwrap();
        assert_eq!((child.sent, child.got), (1, 1));
        assert_eq!(child.peers[0].name.as_deref(), Some("main"));
    }

    #[test]
    fn a_trailing_msg_leaves_the_turn_open() {
        let path = write_events(
            "msg-open",
            &[
                json!({
                    "type": "message",
                    "timestamp": "2026-01-01T00:00:01Z",
                    "message": {"role": "assistant", "stopReason": "stop",
                        "content": [{"type": "text", "text": "waiting on csv"}]}
                }),
                json!({
                    "type": "custom_message",
                    "timestamp": "2026-01-01T00:00:02Z",
                    "customType": "msg",
                    "content": "framed",
                    "details": {"from": {"id": "csv-id", "name": "csv"}, "to": "open-test",
                        "text": "done", "kind": "ended"}
                }),
                json!({
                    "type": "custom_message",
                    "timestamp": "2026-01-01T00:00:03Z",
                    "customType": "other",
                    "content": "ignored"
                }),
            ],
        );
        let file = demo_file(path.clone(), "open-test", "2026-01-01T00-00-00Z");
        let digest = session_digest(&file, 3, 8).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(digest.outcome.open_turn, Some("thinking"));
    }

    #[test]
    fn stats_count_custom_messages_and_relay_resets() {
        let metrics = session_metrics(&tandem(MAIN), 20).unwrap();
        let counts = &metrics.counts;
        assert_eq!(counts.custom_messages, 6);
        assert_eq!(
            counts
                .custom_types
                .iter()
                .map(|(custom_type, count)| (custom_type.as_str(), *count))
                .collect::<Vec<_>>(),
            [("handoff", 1), ("msg", 3), ("relay", 2)]
        );
        assert_eq!((counts.compactions, counts.relay_resets), (1, 1));
        assert_eq!(counts.other_events, 0);
        let relay = metrics.compactions[0].relay.as_ref().unwrap();
        assert_eq!(
            (relay.n, relay.percent.as_ref().and_then(|p| p.as_u64())),
            (Some(1), Some(48))
        );
    }
}
