use std::{
    env,
    io::{self, IsTerminal, Write},
};

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::{
    cli::Kind,
    engine::{Record, SessionDigest, SessionMetrics, compact, short_id},
};

pub struct DisplayOptions<'a> {
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
    let mut output = io::BufWriter::new(stdout.lock());
    if options.json {
        for record in records {
            serde_json::to_writer(&mut output, &json_record(record, options))?;
            writeln!(output)?;
        }
        return Ok(());
    }

    let color = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
    let matcher = build_matcher(options)?;
    for record in records {
        let timestamp = display_timestamp(&record.timestamp);
        let tool = record
            .tool
            .as_deref()
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        let header = format!(
            "{timestamp}  {}  {}:{}  {}{tool}",
            record.project,
            short_id(&record.session),
            record.line,
            record.kind.label(),
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
        text,
        text_bytes: record.text.len(),
        text_truncated: text.is_none_or(|text| text.len() < record.text.len()),
        path: &record.path,
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

pub fn print_sessions(rows: &[SessionRow<'_>], json: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    for row in rows {
        if json {
            serde_json::to_writer(&mut output, row)?;
            writeln!(output)?;
        } else {
            writeln!(
                output,
                "{}  {}  {:>9}  {}  {}",
                display_timestamp(row.started_at),
                short_id(row.session),
                human_bytes(row.bytes),
                row.project,
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
        "events: {} total  {} messages  {} compactions  {} lifecycle/config  {} unknown",
        counts.events,
        counts.message_events,
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
    writeln!(
        output,
        "lifecycle: {} session starts  {} model changes  {} thinking-level changes",
        counts.session_events, counts.model_changes, counts.thinking_level_changes,
    )?;
    writeln!(
        output,
        "compactions: {} total, {} listed",
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
        writeln!(
            output,
            "  #{:<3} {:<32} {:>11}  {:>11}  {:>4}  {}{}",
            compaction.number, compaction.finished_at, gap, tokens, hook, previous, reason,
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
    writeln!(
        output,
        "range: {} → {}  size: {}",
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
    ]
    .iter()
    .map(|kind| format!("{kind}={}", digest.counts.get(*kind).copied().unwrap_or(0)))
    .collect::<Vec<_>>()
    .join("  ");
    writeln!(output, "records: {counts}")?;

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
    Ok(())
}

pub fn print_projects(rows: &[ProjectRow<'_>], json: bool) -> Result<()> {
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
                row.name,
                row.cwd,
            )?;
        }
    }
    Ok(())
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

fn display_timestamp(timestamp: &str) -> String {
    timestamp.get(..19).unwrap_or(timestamp).replace('T', " ")
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
