use std::{
    collections::{BTreeSet, HashMap},
    path::{Component, Path},
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const SHORT_ID: usize = 8;

/// How human output names sessions and projects: ids cut to their shortest
/// unique prefix, folders to their shortest unique path suffix.
#[derive(Debug, Default)]
pub struct Names {
    id_lengths: HashMap<String, usize>,
    labels: HashMap<String, String>,
}

impl Names {
    pub fn new(ids: &[String], cwds: &[String]) -> Self {
        Self {
            id_lengths: unique_prefix_lengths(ids, SHORT_ID),
            labels: unique_labels(cwds),
        }
    }

    pub fn id<'a>(&self, id: &'a str) -> &'a str {
        short_id(id, self.id_lengths.get(id).copied().unwrap_or(SHORT_ID))
    }

    pub fn label<'a>(&'a self, cwd: &str, fallback: &'a str) -> &'a str {
        self.labels.get(cwd).map_or(fallback, String::as_str)
    }
}

pub fn short_id(id: &str, length: usize) -> &str {
    id.get(..length).unwrap_or(id)
}

/// The shortest prefix of each id that no other id shares, never shorter
/// than `minimum`. Pi ids are UUIDv7: their first 8 hex digits are the top of
/// a millisecond clock, so sessions started within ~65s of each other share
/// them.
pub fn unique_prefix_lengths(ids: &[String], minimum: usize) -> HashMap<String, usize> {
    let mut sorted = ids
        .iter()
        .map(|id| (id.to_lowercase(), id))
        .collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted.dedup_by(|left, right| left.0 == right.0);
    sorted
        .iter()
        .enumerate()
        .map(|(index, (lower, original))| {
            let shared = [index.checked_sub(1), index.checked_add(1)]
                .into_iter()
                .flatten()
                .filter_map(|neighbor| sorted.get(neighbor))
                .map(|(other, _)| common_prefix(lower, other))
                .max()
                .unwrap_or(0);
            let length = shared.saturating_add(1).max(minimum).min(lower.len());
            ((*original).clone(), length)
        })
        .collect()
}

fn common_prefix(left: &str, right: &str) -> usize {
    left.bytes()
        .zip(right.bytes())
        .take_while(|(left, right)| left == right)
        .count()
}

/// The shortest path suffix that names each folder uniquely. A numeric
/// folder (`…/18`) keeps its parent, and a label never starts with a hidden
/// folder (`.forks/18` becomes `repo/.forks/18`). A folder that is the tail
/// of another keeps its full path.
pub fn unique_labels(cwds: &[String]) -> HashMap<String, String> {
    let folders = cwds
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|cwd| (cwd, components(cwd)))
        .collect::<Vec<_>>();
    let counts = folders
        .iter()
        .flat_map(|(_, parts)| (1..=parts.len()).map(|size| suffix(parts, size)))
        .fold(HashMap::<String, usize>::new(), |mut counts, tail| {
            *counts.entry(tail).or_default() += 1;
            counts
        });
    folders
        .iter()
        .map(|(cwd, parts)| ((*cwd).clone(), label(cwd, parts, &counts)))
        .collect()
}

fn label(cwd: &str, parts: &[&str], counts: &HashMap<String, usize>) -> String {
    let unique = (1..=parts.len()).find(|size| counts.get(&suffix(parts, *size)) == Some(&1));
    match unique.map(|size| widened(parts, size)) {
        Some(size) if size < parts.len() => suffix(parts, size),
        _ => cwd.to_owned(),
    }
}

fn widened(parts: &[&str], size: usize) -> usize {
    let numeric = parts
        .last()
        .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()));
    let size = if numeric { size.max(2) } else { size };
    (size..parts.len())
        .find(|size| !parts[parts.len() - size].starts_with('.'))
        .unwrap_or(parts.len())
}

fn suffix(parts: &[&str], size: usize) -> String {
    parts[parts.len().saturating_sub(size)..].join("/")
}

fn components(cwd: &str) -> Vec<&str> {
    Path::new(cwd)
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect()
}

/// `2026-10-19T00:53:32.662Z` and pi's file stamp `2026-10-19T00-53-32-662Z`
/// both read `2026-10-19 00:53:32`.
pub fn display_timestamp(timestamp: &str) -> String {
    let head = timestamp.get(..19).unwrap_or(timestamp);
    let bytes = head.as_bytes();
    let clock = |at: usize| matches!(bytes.get(at), Some(b':' | b'-'));
    match (
        head.get(..10),
        head.get(11..13),
        head.get(14..16),
        head.get(17..19),
    ) {
        (Some(date), Some(hours), Some(minutes), Some(seconds))
            if bytes[10] == b'T' && clock(13) && clock(16) =>
        {
            format!("{date} {hours}:{minutes}:{seconds}")
        }
        _ => head.replace('T', " "),
    }
}

/// RFC 3339 for a timestamp or a pi file stamp (`2026-10-19T00-53-32-662Z`,
/// `2026-02-09T01-07-39-386303+00-00`), when it is one.
pub fn iso_timestamp(stamp: &str) -> Option<String> {
    let valid = |text: &str| OffsetDateTime::parse(text, &Rfc3339).is_ok();
    if valid(stamp) {
        return Some(stamp.to_owned());
    }
    let date = stamp.get(..10)?;
    let clock = stamp.get(11..19)?.replace('-', ":");
    let rest = stamp.get(19..)?;
    let fraction_end = rest
        .char_indices()
        .skip(1)
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(rest.len(), |(at, _)| at);
    let (fraction, zone) = match rest.strip_prefix(['-', '.']) {
        Some(_) if fraction_end > 1 => (
            format!(".{}", &rest[1..fraction_end]),
            &rest[fraction_end..],
        ),
        _ => (String::new(), rest),
    };
    let zone = match zone {
        "Z" | "z" => "Z".to_owned(),
        _ => zone
            .get(..3)
            .zip(zone.get(4..6))
            .filter(|_| zone.len() == 6 && zone.starts_with(['+', '-']))
            .map(|(hours, minutes)| format!("{hours}:{minutes}"))?,
    };
    let candidate = format!("{date}T{clock}{fraction}{zone}");
    valid(&candidate).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::{
        Names, display_timestamp, iso_timestamp, short_id, unique_labels, unique_prefix_lengths,
    };

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn sibling_ids_grow_past_their_shared_clock_prefix() {
        let ids = owned(&[
            "01a151a6-4047-76d7-9678-aac290e56167",
            "01a151a6-4175-71a6-8f37-8e46e3570d2c",
            "01a13c8e-b55d-7f7f-830b-38f10a92af7a",
        ]);
        let lengths = unique_prefix_lengths(&ids, 8);
        assert_eq!(lengths[&ids[0]], 11);
        assert_eq!(lengths[&ids[1]], 11);
        assert_eq!(lengths[&ids[2]], 8);
        let names = Names::new(&ids, &[]);
        assert_eq!(names.id(&ids[0]), "01a151a6-40");
        assert_eq!(names.id(&ids[1]), "01a151a6-41");
        assert_eq!(names.id("unknown-id-123"), "unknown-");
    }

    #[test]
    fn duplicate_ids_count_once_and_short_ids_never_overrun() {
        let ids = owned(&["abcdef12-1", "abcdef12-1", "abc"]);
        let lengths = unique_prefix_lengths(&ids, 8);
        assert_eq!(lengths[&ids[0]], 8);
        assert_eq!(lengths[&ids[2]], 3);
        assert_eq!(short_id("αβγδ", 3), "αβγδ");
    }

    #[test]
    fn labels_are_the_shortest_unique_path_suffix() {
        let cwds = owned(&[
            "/a/pfs-cli/.forks/18",
            "/a/pfs-cli/.forks/19",
            "/a/pfs-demo/.forks/19",
            "/a/pfs-cli",
            "/b/x/repo",
            "/c/y/repo",
            "/d/repo",
            "/x/api",
            "/h/.pfs/forks/pfs-cli/8",
            "/tmp",
            "/private/tmp",
            "/",
        ]);
        let labels = unique_labels(&cwds);
        let label = |cwd: &str| labels[cwd].as_str();
        assert_eq!(label("/a/pfs-cli/.forks/18"), "pfs-cli/.forks/18");
        assert_eq!(label("/a/pfs-cli/.forks/19"), "pfs-cli/.forks/19");
        assert_eq!(label("/a/pfs-demo/.forks/19"), "pfs-demo/.forks/19");
        assert_eq!(label("/a/pfs-cli"), "pfs-cli");
        assert_eq!(label("/b/x/repo"), "x/repo");
        assert_eq!(label("/c/y/repo"), "y/repo");
        assert_eq!(label("/d/repo"), "/d/repo");
        assert_eq!(label("/x/api"), "api");
        assert_eq!(label("/h/.pfs/forks/pfs-cli/8"), "pfs-cli/8");
        assert_eq!(label("/tmp"), "/tmp");
        assert_eq!(label("/private/tmp"), "/private/tmp");
        assert_eq!(label("/"), "/");
    }

    #[test]
    fn displays_rfc3339_and_file_stamps_alike() {
        assert_eq!(
            display_timestamp("2026-10-19T00:53:32.662Z"),
            "2026-10-19 00:53:32"
        );
        assert_eq!(
            display_timestamp("2026-10-19T00-53-32-662Z"),
            "2026-10-19 00:53:32"
        );
        assert_eq!(display_timestamp("2026-10-19"), "2026-10-19");
    }

    #[test]
    fn converts_file_stamps_to_rfc3339() {
        assert_eq!(
            iso_timestamp("2026-10-19T00-53-32-662Z").as_deref(),
            Some("2026-10-19T00:53:32.662Z")
        );
        assert_eq!(
            iso_timestamp("2026-02-09T01-07-39-386303+00-00").as_deref(),
            Some("2026-02-09T01:07:39.386303+00:00")
        );
        assert_eq!(
            iso_timestamp("2026-10-19T00:53:32.662Z").as_deref(),
            Some("2026-10-19T00:53:32.662Z")
        );
        assert_eq!(
            iso_timestamp("2026-10-19T00-53-32Z").as_deref(),
            Some("2026-10-19T00:53:32Z")
        );
        assert_eq!(iso_timestamp("not-a-stamp"), None);
        assert_eq!(iso_timestamp("2026-13-45T99-99-99Z"), None);
    }
}
