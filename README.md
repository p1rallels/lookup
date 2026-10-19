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

# select projects by path/name substring, a folder and everything under it, or an id globally
lookup search "position mismatch" -p my-project
lookup search "position mismatch" -p ~/code/my-project
lookup last latest -p ../other-checkout
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
lookup search 'exited with code' -p my-project --errors
lookup search 'cargo test' -p my-project -k tool-call,tool-result --errors --around 1
lookup search "inventory" -p my-project --full
lookup search "inventory" -p my-project --json | jq -r '.text'
lookup search "inventory" -p my-project --json --max-text 500
lookup search "inventory" -p my-project --json --metadata-only
lookup search "inventory" -p my-project --json --full | jq -r '.text'

# hand an agent recent user/assistant context from the current or named chat
lookup catchup
lookup catchup a1b2c3d4
lookup catchup --json | jq -r '[.timestamp, .kind, .text] | @tsv'

# count activity, tokens and cost, and inspect recent compaction timestamps/gaps
lookup stats
lookup stats a1b2c3d4
lookup stats a1b2c3d4 -n 0 --json | jq '.counts, .usage, .compactions'

# browse, inspect the final event, summarize, and render sessions
lookup sessions -p my-project
lookup sessions -p ~/code --since 2026-10-19
lookup last a1b2c3d4
lookup last a1b2c3d4 -n 5 -k user,assistant
lookup summarize a1b2c3d4
lookup summarize a1b2c3d4 --json | jq '.outcome, .files_changed, .final_message.text'
lookup show latest -p my-project
lookup show e5f6a7b8 --all-kinds
lookup projects my-project
```

with no `-p`, lookup reads the current folder's sessions, under its physical path and, when your shell came through a symlink, its logical one (`/tmp` and `/private/tmp`). a `-p` that looks like a path (`/…`, `~/…`, `./…`, `../…`, `.`, `..`) selects that folder and every folder inside it, compared component by component against each session's recorded working folder, so `-p ~/code/app` never picks up `~/code/app2` and works for folders that no longer exist. any other `-p` is a case-insensitive substring of the project path or name (`-p Desktop/code/app` keeps the old substring behavior). an explicit session id searches globally unless constrained with `-p`. `lookup catchup` resolves the current chat exactly from `PI_SESSION_FILE` or `PI_SESSION_ID`, then emits the latest 20 user and assistant records chronologically without summarizing them; pass a session selector to use it outside pi. fixed-string search is case-insensitive by default; use `--case-sensitive` when needed. `--exclude-active` removes the current pi session (from `PI_SESSION_ID`/`PI_SESSION_FILE`), or outside pi the current folder's most recently written session, preventing the lookup command itself from becoming a match.

session selectors match id prefixes first, then file-name substrings (a date, say). pi ids are uuidv7, so the first 8 characters are a clock that only changes every ~65s, and sessions started together share them. human output therefore prints each id's shortest unique prefix (at least 8 characters), which always pastes back into a selector, and an ambiguous selector lists the candidates with those prefixes, start times, folders and titles. folders are labeled by their shortest unique path suffix (`pi-web-live/repo`, `pfs-cli/.forks/18`); json keeps `project` as the folder name and `cwd` as the full path.

sessions are read from `$PI_CODING_AGENT_DIR/sessions` (default `~/.pi/agent/sessions`) and, when set, `$PI_CODING_AGENT_SESSION_DIR`, the way pi picks them; `--root DIR` reads only `DIR`. a root may hold pi's per-folder directories, session files filed flat (grouped by each file's recorded folder), or both.

`--errors` keeps only tool results pi marked as failed (non-zero exits, timeouts, rejected edits); it implies `-k tool-result` unless `-k` is given, and json records carry `"error": true`. `summarize` leads with how the session ended (`outcome`: the last stop reason, any error message, and whether the last turn was left open), the files changed through successful edit/write calls, the most recent failed tool calls, and the final assistant message, so a session that never compacted still summarizes to something useful. `sessions --since/--until` bound session start times, and `sessions --json` adds `started_at_iso` (rfc3339) next to the file-name stamp in `started_at`.

jsonl text is capped at 4096 bytes per record by default and includes `text_bytes` plus `text_truncated`. use `--max-text N` for a tighter bound, `--metadata-only` to omit text, or `--full` for lossless unbounded json. search defaults to newest-first; `--sort oldest` reverses it. `--around N` expands each limited match with neighboring records of the selected kinds.

`lookup stats` defines a turn as one user-led interaction and counts raw assistant/model message events separately from assistant text parts, thinking blocks, and tool calls. it also totals the tokens and cost pi recorded on assistant messages (`usage.cost_usd`, at the model's list price) and lists the models used. pi records a compaction completion timestamp but no explicit start timestamp, so `preceding_event_gap_ms` is reported as an upper bound—not falsely labeled as compaction duration. human output marks this as `prior gap ≤`.

## why not only jq?

jq is ideal for one-off schema exploration. lookup memory-maps files in parallel, prefilters fixed strings before json parsing, normalizes only meaningful content, supports project/session/date/kind/tool filters, renders readable snippets, and retains jsonl output for jq pipelines.
