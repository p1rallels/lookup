# lookup

fast, focused search for pi agent session jsonl. `lookup` normalizes user messages, assistant messages, thinking, tool calls, tool results, and compaction summaries while ignoring model/config/accounting records.

## install

```sh
cargo install --path . --force
```

## usage

```sh
# search the current project's sessions (user + assistant + tool calls)
lookup search "position mismatch"

# select projects by path/name substring, or target an id globally
lookup search "position mismatch" -p my-project
lookup search "position mismatch" --session a1b2c3d4

# prevent transcript self-echoes in broad searches
lookup search "a1b2c3d4" --all-projects --exclude-active
lookup search "inventory" --all-projects --exclude-session a1b2c3d4

# search tool results, thinking, or every useful record kind
lookup search "timeout" -p my-project -k tool-result
lookup search "race condition" -p my-project -k thinking,summary
lookup search "order" -p my-project --all-kinds

# regex, dates, ordering, neighboring records, and tool filters
lookup search 'fill(ed|s)' -p my-project -e --since 2026-04-01
lookup search "journey" --session a1b2c3d4 --sort oldest --around 2
lookup search 'rg ' -p my-project -k tool-call --tool bash
lookup search "inventory" -p my-project --full
lookup search "inventory" -p my-project --json | jq -r '.text'
lookup search "inventory" -p my-project --json --max-text 500
lookup search "inventory" -p my-project --json --metadata-only
lookup search "inventory" -p my-project --json --full | jq -r '.text'

# hand an agent recent user/assistant context from the current or named chat
lookup catchup
lookup catchup a1b2c3d4
lookup catchup --json | jq -r '[.timestamp, .kind, .text] | @tsv'

# count activity and inspect recent compaction timestamps/gaps
lookup stats
lookup stats a1b2c3d4
lookup stats a1b2c3d4 -n 0 --json | jq '.counts, .compactions'

# browse, inspect the final event, summarize, and render sessions
lookup sessions -p my-project
lookup last a1b2c3d4
lookup last a1b2c3d4 -n 5 -k user,assistant
lookup summarize a1b2c3d4
lookup summarize a1b2c3d4 --json | jq '.latest_state'
lookup show latest -p my-project
lookup show e5f6a7b8 --all-kinds
lookup projects my-project
```

project and session matching are case-insensitive and prefix/substring-based. an explicit session id searches globally unless constrained with `-p`; otherwise lookup uses the current working directory's session folder. `lookup catchup` resolves the current chat exactly from `PI_SESSION_FILE` or `PI_SESSION_ID`, then emits the latest 20 user and assistant records chronologically without summarizing them; pass a session selector to use it outside pi. fixed-string search is case-insensitive by default; use `--case-sensitive` when needed. `--exclude-active` removes the most recently modified session for the current working directory, preventing the lookup command itself from becoming a match.

jsonl text is capped at 4096 bytes per record by default and includes `text_bytes` plus `text_truncated`. use `--max-text N` for a tighter bound, `--metadata-only` to omit text, or `--full` for lossless unbounded json. search defaults to newest-first; `--sort oldest` reverses it. `--around N` expands each limited match with neighboring records of the selected kinds.

`lookup stats` defines a turn as one user-led interaction and counts raw assistant/model message events separately from assistant text parts, thinking blocks, and tool calls. pi records a compaction completion timestamp but no explicit start timestamp, so `preceding_event_gap_ms` is reported as an upper bound—not falsely labeled as compaction duration. human output marks this as `prior gap ≤`.

## why not only jq?

jq is ideal for one-off schema exploration. lookup memory-maps files in parallel, prefilters fixed strings before json parsing, normalizes only meaningful content, supports project/session/date/kind/tool filters, renders readable snippets, and retains jsonl output for jq pipelines.
