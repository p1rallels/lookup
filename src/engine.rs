use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
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
    source::SessionFile,
};

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
    pub text: String,
    pub path: String,
}

#[derive(Debug)]
pub struct SearchOptions<'a> {
    pub kinds: &'a [Kind],
    pub query: &'a str,
    pub regex: bool,
    pub case_sensitive: bool,
    pub tool: Option<&'a str>,
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
    pub compactions: usize,
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

#[derive(Clone, Debug, Serialize)]
pub struct SessionDigest {
    pub project: String,
    pub cwd: String,
    pub session: String,
    pub started_at: String,
    pub ended_at: String,
    pub bytes: u64,
    pub counts: BTreeMap<String, usize>,
    pub opening: Vec<DigestEntry>,
    pub milestones: Vec<DigestEntry>,
    pub latest_state: LatestState,
    pub path: String,
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
            "message" => count_message(&event, &mut counts),
            "compaction" => {
                counts.compactions += 1;
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

fn duration_ms(start: &str, end: &str) -> Option<u64> {
    let start = OffsetDateTime::parse(start, &Rfc3339).ok()?;
    let end = OffsetDateTime::parse(end, &Rfc3339).ok()?;
    u64::try_from((end - start).whole_milliseconds()).ok()
}

pub fn session_digest(
    file: &SessionFile,
    opening_limit: usize,
    milestone_limit: usize,
) -> Result<SessionDigest> {
    let mut counts = BTreeMap::new();
    let mut opening = Vec::new();
    let mut snapshots = Vec::new();
    let mut ended_at = file.started_at.clone();
    let mut latest_state = LatestState::default();
    let Some(map) = map_file(&file.path)? else {
        return Ok(SessionDigest {
            project: file.project.clone(),
            cwd: file.cwd.clone(),
            session: file.session.clone(),
            started_at: file.started_at.clone(),
            ended_at,
            bytes: file.bytes,
            counts,
            opening,
            milestones: snapshots,
            latest_state,
            path: file.path.to_string_lossy().into_owned(),
        });
    };

    for (index, line) in map.split(|byte| *byte == b'\n').enumerate() {
        let Ok(records) = records_from_line(line, file, index + 1) else {
            continue;
        };
        for record in records {
            ended_at = record.timestamp.clone();
            *counts.entry(record.kind.label().to_owned()).or_insert(0) += 1;
            if record.kind == Kind::User && opening.len() < opening_limit {
                opening.push(DigestEntry {
                    timestamp: record.timestamp,
                    text: clipped_compact(&record.text, 600),
                });
            } else if record.kind == Kind::Summary {
                let state = summary_state(&record.text);
                if !state.goal.is_empty() {
                    snapshots.push(DigestEntry {
                        timestamp: record.timestamp,
                        text: state.goal.clone(),
                    });
                }
                latest_state = state;
            }
        }
    }

    Ok(SessionDigest {
        project: file.project.clone(),
        cwd: file.cwd.clone(),
        session: file.session.clone(),
        started_at: file.started_at.clone(),
        ended_at,
        bytes: file.bytes,
        counts,
        opening,
        milestones: sampled_entries(snapshots, milestone_limit),
        latest_state,
        path: file.path.to_string_lossy().into_owned(),
    })
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

pub fn select_session<'a>(files: &'a [SessionFile], selector: &str) -> Result<&'a SessionFile> {
    if selector.eq_ignore_ascii_case("latest") {
        return files.first().context("no sessions found");
    }

    let mut matches = files
        .iter()
        .filter(|file| session_matches(file, selector))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("no session matched '{selector}'");
    }
    if matches.len() > 1 {
        matches.sort_unstable_by(|left, right| right.started_at.cmp(&left.started_at));
        let choices = matches
            .iter()
            .take(6)
            .map(|file| format!("{} ({})", short_id(&file.session), file.started_at))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("session selector is ambiguous: {choices}");
    }
    Ok(matches[0])
}

pub fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
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
    let event = serde_json::from_slice::<Value>(line)?;
    let timestamp = event
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or(&file.started_at)
        .to_owned();

    if event.get("type").and_then(Value::as_str) == Some("compaction") {
        return Ok(event
            .get("summary")
            .and_then(Value::as_str)
            .map(|text| {
                vec![make_record(
                    file,
                    &timestamp,
                    line_number,
                    Kind::Summary,
                    None,
                    text,
                )]
            })
            .unwrap_or_default());
    }

    let Some(message) = event.get("message") else {
        return Ok(Vec::new());
    };
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = message
        .get("toolName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    Ok(content
        .iter()
        .filter_map(|part| normalize_part(part, role, tool_name.as_deref()))
        .map(|(kind, tool, text)| make_record(file, &timestamp, line_number, kind, tool, &text))
        .collect())
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
    kind: Kind,
    tool: Option<String>,
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

fn within_bounds(timestamp: &str, since: Option<&str>, until: Option<&str>) -> bool {
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

fn validate_bound(bound: Option<&str>) -> Result<()> {
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
        DigestEntry, Matcher, SearchOptions, compare_timestamp, normalize_part, sampled_entries,
        search, session_matches, session_metrics, summary_state,
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
}
