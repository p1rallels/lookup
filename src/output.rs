use std::{
    env,
    io::{self, IsTerminal, Write},
};

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::{
    cli::Kind,
    custom::{Msg, Relay, msg_label, relay_label, reset_label},
    engine::{
        Failures, MsgTraffic, Outcome, Peer, Record, SessionDigest, SessionMetrics, Usage, compact,
    },
    naming::{Names, SHORT_ID, display_timestamp, short_id},
};

const FINAL_MESSAGE_BYTES: usize = 4000;
const FILES_SHOWN: usize = 20;

pub struct DisplayOptions<'a> {
    pub names: &'a Names,
    pub query: Option<&'a str>,
    pub regex: bool,
    pub case_sensitive: bool,
    pub context: usize,
    pub full: bool,
    pub json: bool,
    pub max_text: Option<usize>,
    pub metadata_only: bool,
}

#[derive(Serialize)]
struct JsonRecord<'a> {
    project: &'a str,
    cwd: &'a str,
    session: &'a str,
    started_at: &'a str,
    timestamp: &'a str,
    line: usize,
    kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    msg: Option<&'a Msg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay: Option<&'a Relay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    text_bytes: usize,
    text_truncated: bool,
    path: &'a str,
}

#[derive(Serialize)]
pub struct SessionRow<'a> {
    pub project: &'a str,
    pub cwd: &'a str,
    pub session: &'a str,
    pub started_at: &'a str,
    pub started_at_iso: Option<String>,
    pub bytes: u64,
    pub title: &'a str,
    pub path: String,
}

#[derive(Serialize)]
pub struct ProjectRow<'a> {
    pub name: &'a str,
    pub cwd: &'a str,
    pub sessions: usize,
    pub bytes: u64,
}

pub fn print_records(records: &[Record], options: &DisplayOptions<'_>) -> Result<()> {
    let stdout = io::stdout();
    let color = stdout.is_terminal() && env::var_os("NO_COLOR").is_none();
    write_records(
        &mut io::BufWriter::new(stdout.lock()),
        records,
        options,
        color,
    )
}

/// JSONL, or a header line per record and its text indented under it. Plain
/// text (no color) is what relay pastes into a fresh context.
pub fn write_records(
    output: &mut impl Write,
    records: &[Record],
    options: &DisplayOptions<'_>,
    color: bool,
) -> Result<()> {
    if options.json {
        for record in records {
            serde_json::to_writer(&mut *output, &json_record(record, options))?;
            writeln!(output)?;
        }
        return Ok(());
    }

    let matcher = build_matcher(options)?;
    for record in records {
        let timestamp = display_timestamp(&record.timestamp);
        let tool = record
            .tool
            .as_deref()
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        let error = match record.error {
            true => " [error]",
            false => "",
        };
        let header = format!(
            "{timestamp}  {}  {}:{}  {}{tool}{error}",
            options.names.label(&record.cwd, &record.project),
            options.names.id(&record.session),
            record.line,
            record_label(record, options.names),
        );
        if color {
            writeln!(output, "\x1b[2m{header}\x1b[0m")?;
        } else {
            writeln!(output, "{header}")?;
        }

        let body = render_body(
            &record.text,
            matcher.as_ref(),
            options.context,
            options.full,
            color,
        );
        for line in body.lines() {
            writeln!(output, "  {line}")?;
        }
    }
    Ok(())
}

fn json_record<'a>(record: &'a Record, options: &DisplayOptions<'_>) -> JsonRecord<'a> {
    let text = if options.metadata_only {
        None
    } else {
        Some(truncate_text(&record.text, options.max_text))
    };
    JsonRecord {
        project: &record.project,
        cwd: &record.cwd,
        session: &record.session,
        started_at: &record.started_at,
        timestamp: &record.timestamp,
        line: record.line,
        kind: record.kind,
        tool: record.tool.as_deref(),
        error: record.error,
        custom_type: record.custom_type.as_deref(),
        msg: record.msg.as_ref(),
        relay: record.relay.as_ref(),
        text,
        text_bytes: record.text.len(),
        text_truncated: text.is_none_or(|text| text.len() < record.text.len()),
        path: &record.path,
    }
}

/// The kind, or for custom records and relay's compactions what they are:
/// `msg scout → you`, `relay 45% nudge`, `relay reset #2 at 48%`.
fn record_label(record: &Record, names: &Names) -> String {
    match (record.kind, &record.msg, &record.relay) {
        (Kind::Custom, Some(msg), _) => msg_label(msg, &record.session, names),
        (Kind::Custom, None, Some(relay)) => relay_label(relay),
        (Kind::Custom, None, None) => record
            .custom_type
            .clone()
            .unwrap_or_else(|| Kind::Custom.label().to_owned()),
        (Kind::Summary, _, Some(relay)) => reset_label(relay),
        (kind, _, _) => kind.label().to_owned(),
    }
}

fn truncate_text(text: &str, maximum: Option<usize>) -> &str {
    let Some(maximum) = maximum else {
        return text;
    };
    if text.len() <= maximum {
        return text;
    }
    let mut end = maximum;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn print_sessions(rows: &[SessionRow<'_>], json: bool, names: &Names) -> Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    let id_width = rows
        .iter()
        .map(|row| names.id(row.session).len())
        .max()
        .unwrap_or_default();
    for row in rows {
        if json {
            serde_json::to_writer(&mut output, row)?;
            writeln!(output)?;
        } else {
            writeln!(
                output,
                "{}  {:<id_width$}  {:>9}  {}  {}",
                display_timestamp(row.started_at),
                names.id(row.session),
                human_bytes(row.bytes),
                names.label(row.cwd, row.project),
                excerpt_without_match(row.title, 100),
            )?;
        }
    }
    Ok(())
}

pub fn print_metrics(metrics: &SessionMetrics, json: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if json {
        serde_json::to_writer(&mut output, metrics)?;
        writeln!(output)?;
        return Ok(());
    }

    let counts = &metrics.counts;
    writeln!(output, "session: {}", metrics.session)?;
    writeln!(output, "project: {} ({})", metrics.project, metrics.cwd)?;
    writeln!(
        output,
        "range: {} → {}  elapsed: {}  size: {}",
        metrics.started_at,
        metrics.ended_at,
        metrics
            .elapsed_ms
            .map_or_else(|| "unknown".to_owned(), human_duration),
        human_bytes(metrics.bytes),
    )?;
    let lifecycle_events = counts
        .session_events
        .saturating_add(counts.model_changes)
        .saturating_add(counts.thinking_level_changes);
    writeln!(
        output,
        "events: {} total  {} messages  {} custom messages  {} compactions  {} lifecycle/config  {} unknown",
        counts.events,
        counts.message_events,
        counts.custom_messages,
        counts.compactions,
        lifecycle_events,
        counts.other_events,
    )?;
    writeln!(output, "turns: {} user-led", counts.turns)?;
    writeln!(
        output,
        "messages: {} user  {} assistant/model  {} tool-result",
        counts.user_messages, counts.assistant_messages, counts.tool_result_messages,
    )?;
    writeln!(
        output,
        "content: {} assistant text  {} thinking  {} tool calls",
        counts.assistant_text_parts, counts.thinking_blocks, counts.tool_calls,
    )?;
    if !counts.custom_types.is_empty() {
        let types = counts
            .custom_types
            .iter()
            .map(|(custom_type, count)| format!("{count} {custom_type}"))
            .collect::<Vec<_>>()
            .join("  ");
        writeln!(output, "custom messages: {types}")?;
    }
    writeln!(
        output,
        "lifecycle: {} session starts  {} model changes  {} thinking-level changes",
        counts.session_events, counts.model_changes, counts.thinking_level_changes,
    )?;
    writeln!(output, "usage: {}", describe_usage(&metrics.usage))?;
    if !metrics.models.is_empty() {
        writeln!(output, "models: {}", metrics.models.join(", "))?;
    }
    let by_relay = match counts.relay_resets {
        0 => String::new(),
        1 => " (1 relay reset)".to_owned(),
        resets => format!(" ({resets} relay resets)"),
    };
    writeln!(
        output,
        "compactions: {} total{by_relay}, {} listed",
        counts.compactions, metrics.compactions_listed,
    )?;
    if !metrics.compactions.is_empty() {
        writeln!(
            output,
            "  completion timestamp                    prior gap ≤  tokens       hook  previous"
        )?;
    }
    for compaction in &metrics.compactions {
        let gap = compaction
            .preceding_event_gap_ms
            .map_or_else(|| "unknown".to_owned(), human_duration);
        let tokens = compaction
            .tokens_before
            .map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string());
        let hook = compaction
            .from_hook
            .map_or("?", |hook| if hook { "yes" } else { "no" });
        let previous = compaction.previous_event.as_deref().unwrap_or("unknown");
        let reason = compaction
            .previous_stop_reason
            .as_deref()
            .map(|reason| format!("/{reason}"))
            .unwrap_or_default();
        let relay = compaction
            .relay
            .as_ref()
            .map(|relay| format!("  {}", reset_label(relay)))
            .unwrap_or_default();
        writeln!(
            output,
            "  #{:<3} {:<32} {:>11}  {:>11}  {:>4}  {}{}{}",
            compaction.number, compaction.finished_at, gap, tokens, hook, previous, reason, relay,
        )?;
    }
    if !metrics.compactions.is_empty() {
        writeln!(output, "note: {}", metrics.compaction_timing_note)?;
    }
    Ok(())
}

pub fn print_digest(digest: &SessionDigest, json: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if json {
        serde_json::to_writer(&mut output, digest)?;
        writeln!(output)?;
        return Ok(());
    }

    writeln!(output, "session: {}", digest.session)?;
    writeln!(output, "project: {} ({})", digest.project, digest.cwd)?;
    let elapsed = digest
        .elapsed_ms
        .map(|milliseconds| format!("  elapsed: {}", human_duration(milliseconds)))
        .unwrap_or_default();
    writeln!(
        output,
        "range: {} → {}{elapsed}  size: {}",
        display_timestamp(&digest.started_at),
        display_timestamp(&digest.ended_at),
        human_bytes(digest.bytes)
    )?;
    let counts = [
        "user",
        "assistant",
        "thinking",
        "tool-call",
        "tool-result",
        "summary",
        "custom",
    ]
    .iter()
    .map(|kind| format!("{kind}={}", digest.counts.get(*kind).copied().unwrap_or(0)))
    .collect::<Vec<_>>()
    .join("  ");
    writeln!(output, "records: {counts}")?;
    writeln!(output, "outcome: {}", describe_outcome(&digest.outcome))?;
    if let Some(traffic) = &digest.msg {
        writeln!(output, "msg: {}", describe_traffic(traffic))?;
    }

    if !digest.opening.is_empty() {
        writeln!(output, "\nopening:")?;
        for entry in &digest.opening {
            writeln!(
                output,
                "  {}  {}",
                display_timestamp(&entry.timestamp),
                entry.text
            )?;
        }
    }
    if !digest.milestones.is_empty() {
        writeln!(output, "\nmilestones:")?;
        for entry in &digest.milestones {
            writeln!(
                output,
                "  {}  {}",
                display_timestamp(&entry.timestamp),
                entry.text
            )?;
        }
    }

    let state = &digest.latest_state;
    if [
        &state.goal,
        &state.in_progress,
        &state.blocked,
        &state.next_steps,
    ]
    .iter()
    .any(|section| !section.is_empty())
    {
        writeln!(output, "\nlatest state:")?;
        for (label, value) in [
            ("goal", &state.goal),
            ("in progress", &state.in_progress),
            ("blocked", &state.blocked),
            ("next steps", &state.next_steps),
        ] {
            if !value.is_empty() {
                writeln!(output, "  {label}: {value}")?;
            }
        }
    }
    print_changes(&mut output, digest)?;
    print_failures(&mut output, &digest.failed_tools)?;
    print_final_message(&mut output, digest)?;
    Ok(())
}

fn print_changes(output: &mut impl Write, digest: &SessionDigest) -> io::Result<()> {
    if digest.files_changed.is_empty() {
        return Ok(());
    }
    writeln!(output, "\nfiles changed: {}", digest.files_changed.len())?;
    let inside = format!("{}/", digest.cwd.trim_end_matches('/'));
    for change in digest.files_changed.iter().take(FILES_SHOWN) {
        let path = change.path.strip_prefix(&inside).unwrap_or(&change.path);
        let counts = [(change.writes, "write"), (change.edits, "edit")]
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{label} ×{count}"))
            .collect::<Vec<_>>()
            .join("  ");
        writeln!(output, "  {path}  {counts}")?;
    }
    let hidden = digest.files_changed.len().saturating_sub(FILES_SHOWN);
    if hidden > 0 {
        writeln!(output, "  … and {hidden} more")?;
    }
    Ok(())
}

fn print_failures(output: &mut impl Write, failures: &Failures) -> io::Result<()> {
    if failures.count == 0 {
        return Ok(());
    }
    let shown = match failures.count > failures.recent.len() {
        true => format!(" (last {})", failures.recent.len()),
        false => String::new(),
    };
    writeln!(output, "\nfailed tool calls: {}{shown}", failures.count)?;
    for failure in &failures.recent {
        writeln!(
            output,
            "  {}  {}  {}",
            display_timestamp(&failure.timestamp),
            failure.tool.as_deref().unwrap_or("tool"),
            failure.text,
        )?;
    }
    Ok(())
}

fn print_final_message(output: &mut impl Write, digest: &SessionDigest) -> io::Result<()> {
    let Some(entry) = &digest.final_message else {
        return Ok(());
    };
    writeln!(
        output,
        "\nfinal message ({}):",
        display_timestamp(&entry.timestamp)
    )?;
    let text = entry.text.trim();
    let shown = truncate_text(text, Some(FINAL_MESSAGE_BYTES));
    for line in shown.lines() {
        writeln!(output, "  {line}")?;
    }
    let rest = text.len().saturating_sub(shown.len());
    if rest > 0 {
        writeln!(
            output,
            "  … {} more: lookup last {} -k assistant --full",
            human_bytes(rest as u64),
            digest.session,
        )?;
    }
    Ok(())
}

pub fn print_projects(rows: &[ProjectRow<'_>], json: bool, names: &Names) -> Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    for row in rows {
        if json {
            serde_json::to_writer(&mut output, row)?;
            writeln!(output)?;
        } else {
            writeln!(
                output,
                "{:>5}  {:>9}  {:<28}  {}",
                row.sessions,
                human_bytes(row.bytes),
                names.label(row.cwd, row.name),
                row.cwd,
            )?;
        }
    }
    Ok(())
}

fn describe_usage(usage: &Usage) -> String {
    if usage.total_tokens == 0 && usage.cost_usd == 0.0 {
        return "not recorded".to_owned();
    }
    format!(
        "{}  {} tokens  ({} input  {} output  {} cache read  {} cache write)",
        human_cost(usage.cost_usd),
        human_count(usage.total_tokens),
        human_count(usage.input),
        human_count(usage.output),
        human_count(usage.cache_read),
        human_count(usage.cache_write),
    )
}

fn describe_outcome(outcome: &Outcome) -> String {
    match (
        outcome.open_turn,
        outcome.stop_reason.as_deref(),
        outcome.error_message.as_deref(),
    ) {
        (Some("running-tools"), _, _) => {
            "mid-turn, tools were running (still going, or it stopped there)".to_owned()
        }
        (Some(_), _, _) => {
            "mid-turn, the model hadn't answered (still going, or it stopped there)".to_owned()
        }
        (None, Some(reason), Some(message)) => {
            format!(
                "{reason}: {}",
                excerpt_without_match(&compact(message), 300)
            )
        }
        (None, Some(reason), None) => reason.to_owned(),
        (None, None, _) => "no assistant reply".to_owned(),
    }
}

/// `sent 2 to csv, db; got 3 from csv, user; csv ended`.
fn describe_traffic(traffic: &MsgTraffic) -> String {
    let name = |peer: &Peer| {
        peer.name
            .clone()
            .unwrap_or_else(|| short_id(&peer.id, SHORT_ID).to_owned())
    };
    let sent_to = traffic
        .peers
        .iter()
        .filter(|peer| peer.sent > 0)
        .map(name)
        .collect::<Vec<_>>();
    let got_from = traffic
        .peers
        .iter()
        .filter(|peer| peer.got > 0)
        .map(name)
        .chain((traffic.from_user > 0).then(|| "user".to_owned()))
        .collect::<Vec<_>>();
    let exits = traffic
        .peers
        .iter()
        .filter_map(|peer| {
            peer.exit
                .as_ref()
                .map(|exit| format!("{} {exit}", name(peer)))
        })
        .collect::<Vec<_>>();
    [
        (traffic.sent > 0).then(|| format!("sent {} to {}", traffic.sent, sent_to.join(", "))),
        (traffic.got > 0).then(|| format!("got {} from {}", traffic.got, got_from.join(", "))),
        (!exits.is_empty()).then(|| exits.join(", ")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ")
}

fn human_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1e3),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

fn human_cost(dollars: f64) -> String {
    match dollars < 1.0 {
        true => format!("${dollars:.3}"),
        false => format!("${dollars:.2}"),
    }
}

fn human_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    match (days, hours, minutes, seconds) {
        (days, _, _, _) if days > 0 => format!("{}d {}h", days, hours % 24),
        (_, hours, _, _) if hours > 0 => format!("{}h {}m", hours, minutes % 60),
        (_, _, minutes, _) if minutes > 0 => format!("{}m {}s", minutes, seconds % 60),
        (_, _, _, seconds) if seconds > 0 => {
            format!("{}.{:03}s", seconds, milliseconds % 1_000)
        }
        _ => format!("{milliseconds}ms"),
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn build_matcher(options: &DisplayOptions<'_>) -> Result<Option<Regex>> {
    options
        .query
        .map(|query| {
            let pattern = if options.regex {
                query.to_owned()
            } else {
                regex::escape(query)
            };
            RegexBuilder::new(&pattern)
                .case_insensitive(!options.case_sensitive)
                .build()
                .context("failed to construct output matcher")
        })
        .transpose()
}

fn render_body(
    text: &str,
    matcher: Option<&Regex>,
    context: usize,
    full: bool,
    color: bool,
) -> String {
    let rendered = if full {
        text.to_owned()
    } else {
        let compacted = compact(text);
        match matcher.and_then(|matcher| matcher.find(&compacted)) {
            Some(found) => excerpt(&compacted, found.start(), found.end(), context),
            None => excerpt_without_match(&compacted, context.saturating_mul(2)),
        }
    };
    if !color {
        return rendered;
    }
    matcher.map_or(rendered.clone(), |matcher| {
        matcher
            .replace_all(&rendered, "\x1b[1;33m$0\x1b[0m")
            .into_owned()
    })
}

fn excerpt(text: &str, match_start: usize, match_end: usize, context: usize) -> String {
    let mut start = match_start.saturating_sub(context);
    let mut end = match_end.saturating_add(context).min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        &text[start..end],
        if end < text.len() { "…" } else { "" }
    )
}

fn excerpt_without_match(text: &str, width: usize) -> String {
    if text.len() <= width {
        return text.to_owned();
    }
    let mut end = width;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::{excerpt, human_bytes, human_duration, truncate_text};

    #[test]
    fn excerpt_preserves_utf8_boundaries() {
        assert_eq!(excerpt("αβγ needle δεζ", 7, 13, 3), "…γ needle δ…");
    }

    #[test]
    fn formats_binary_sizes() {
        assert_eq!(human_bytes(1_048_576), "1.0 MiB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(106_306), "1m 46s");
        assert_eq!(human_duration(5_230), "5.230s");
        assert_eq!(human_duration(90_000_000), "1d 1h");
    }

    #[test]
    fn truncates_json_text_on_utf8_boundaries() {
        assert_eq!(truncate_text("αβγ", Some(3)), "α");
        assert_eq!(truncate_text("αβγ", Some(4)), "αβ");
        assert_eq!(truncate_text("αβγ", None), "αβγ");
    }
}
