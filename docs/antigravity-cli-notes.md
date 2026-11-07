# Antigravity CLI Provider & Refactoring Specification

*   **Author:** xurl-team
*   **Status:** Approved
*   **Date:** 2026-11-07

--------------------------------------------------------------------------------

## Overview

`xurl` provides a unified `agents://` URI interface to read, discover, inspect subagent hierarchies, and non-interactively write conversations across local coding agent CLIs (`codex`, `claude`, `gemini`, `opencode`, `pi`, `amp`, `copilot`, `cursor`, `kimi`). This specification defines the protocol support and lean refactoring plan for Google Antigravity CLI (`antigravity`, with CLI shorthand alias `agy`).

## The Problem

Local Antigravity CLI (`agy`) stores conversation transcripts under `~/.gemini/antigravity-cli/brain/<session_id>/.system_generated/logs/` (`transcript_full.jsonl` and `transcript.jsonl`) and indexes session metadata, workspace URIs, and parent-child subagent links in `~/.gemini/antigravity-cli/conversation_summaries.db`.

While the initial `antigravity` provider implementation in `xurl` works for basic read, query, subagent index, and write flows, our code review and Goldfish audit identified five structural and correctness gaps:
1. `<SYSTEM_MESSAGE>` tag extraction in `xurl-core/src/render.rs` uses chained `.strip_prefix(...).unwrap_or(content).strip_suffix(...).unwrap_or(content)`, which resets the string back to the raw `<SYSTEM_MESSAGE>`-prefixed input whenever trailing metadata blocks (such as `<ADDITIONAL_METADATA>`) follow `</SYSTEM_MESSAGE>`.
2. Multi-turn subagent transcripts lose chronological ordering or drop messages because `extract_antigravity_entries` only inspects `SYSTEM_MESSAGE` at `step_index == 0`, defers `send_message` tool-call replies into a separate `fallback_assistant_messages` buffer appended at the end of the timeline, and skips `send_message` whenever a brief companion `content` string appears in the same `PLANNER_RESPONSE` step.
3. Thread list queries (`agents://antigravity?limit=N`) perform $1 + N$ full SQLite table scans on `conversation_summaries` because `collect_antigravity_query_thread_metadata` reloads the entire table per matched item; additionally, subagent detail drill-down (`agents://antigravity/<main_id>/<child_id>`) unconditionally loads the full SQLite table and scans all `brain/*` directories on disk even when the target `child_id` is already known.
4. Subagent list item construction (`build_antigravity_subagent_list_item`) and subagent detail construction (`build_antigravity_subagent_detail_view`) duplicate ~140 lines of relation validation, file reading, and status inference, open the child transcript twice (once for step 0 check and once for full read), parse the full child JSONL twice (`infer_antigravity_child_status` followed by `render::extract_messages`), and misreport multi-turn active subagents as `completed`.
5. Transcript path existence checks, metadata field stripping, and JSON string unquoting are duplicated across `xurl-core/src/provider/antigravity.rs`, `xurl-core/src/render.rs`, and `xurl-core/src/service.rs`.

## Goals

*   Support `agents://antigravity/<session_id>` and alias `agents://agy/<session_id>` for reading conversations, frontmatter metadata (`-I/--head`), subagent discovery, subagent drill-down (`agents://antigravity/<main_id>/<child_id>`), provider/path queries, and non-interactive writes (`-d`).
*   Parse `<SYSTEM_MESSAGE>` blocks deterministically using tag boundary splits (`split_once("<SYSTEM_MESSAGE>")` and `split_once("</SYSTEM_MESSAGE>")`) so trailing metadata blocks never corrupt or drop subagent prompts.
*   Preserve strict chronological step order in `extract_antigravity_entries` across multi-turn parent-subagent conversations and `CHECKPOINT` compactions without deferred fallback buffers, and preserve `send_message` replies even when companion `content` is present on the same step.
*   Eliminate $O(N)$ redundant full-table SQLite scans during thread metadata collection and subagent detail drill-down by querying single session rows with `WHERE conversation_id = ?1 COLLATE NOCASE LIMIT 1`, and skip `brain/` directory scans during single-subagent detail drill-down.
*   Consolidate subagent child analysis, transcript path resolution, metadata key stripping, and JSON string unquoting to remove ~100 lines of duplicated code across `xurl-core`.

## Non-Goals

*   Supporting desktop Antigravity IDE protobuf/LevelDB state databases outside `~/.gemini/antigravity-cli`.
*   Adding background daemon processes, persistent caches, or new third-party Rust dependencies.
*   Changing the CLI surface or YAML frontmatter schema used by other providers.

## Critical User Journeys (CUJs)

### CUJ 1: Read Main or Subagent Conversation Timeline
*   **Given**: A local Antigravity root at `ANTIGRAVITY_CLI_HOME` (or `~/.gemini/antigravity-cli`) containing `brain/<session_id>/.system_generated/logs/transcript_full.jsonl` (or fallback `transcript.jsonl`).
*   **When**: A caller runs `xurl agents://antigravity/<session_id>` or `xurl agents://agy/<session_id>`.
*   **Then**: `xurl` resolves `transcript_full.jsonl` (preferred over `transcript.jsonl`) and renders `# Thread` / `## Timeline` containing user requests (stripped of `<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, and `<USER_SETTINGS_CHANGE>` wrappers), chronological assistant responses (including inline subagent `send_message` replies to the parent session), and `Context Compacted` (`CHECKPOINT`) entries.

### CUJ 2: Inspect Metadata and Subagent Index (`-I/--head`)
*   **Given**: A main session that spawned subagents via `invoke_subagent`.
*   **When**: A caller runs `xurl -I agents://antigravity/<main_id>`.
*   **Then**: `xurl` outputs YAML frontmatter with `mode: 'subagent_index'`, `thread_metadata` merged from `conversation_summaries.db` and step 0 metadata, and `subagents` entries showing each child's `agent_id`, `uri`, `status`, `status_source`, `last_update`, and `thread_source`.

### CUJ 3: Drill Down into a Subagent Conversation
*   **Given**: A main session `<main_id>` and child subagent `<child_id>`.
*   **When**: A caller runs `xurl agents://antigravity/<main_id>/<child_id>`.
*   **Then**: `xurl` validates the parent-child relationship (via parent `invoke_subagent` output, `conversation_summaries.db` `parent_conversation_id`, or child step 0 `SYSTEM_MESSAGE` `sender=<main_id>`), reads and parses the child transcript once, and renders `# Subagent Thread` with relation evidence, parent lifecycle events, and the last 3 messages in `## Thread Excerpt (Child Thread)`.

### CUJ 4: Discover Conversations by Provider or Workspace Path
*   **Given**: Multiple conversations under `<root>/brain/` and workspace mappings in `<root>/conversation_summaries.db`.
*   **When**: A caller runs `xurl 'agents://antigravity?q=keyword&limit=5'` or `xurl 'agents:///path/to/workspace?providers=antigravity'`.
*   **Then**: `xurl` filters and sorts matching sessions by modification timestamp. For path-scoped queries (`need_scope_path == true`), child subagent workspace paths are resolved from their parent session in `conversation_summaries.db` (or via child step 0 `sender` fallback when the child row is absent from `conversation_summaries.db`).

### CUJ 5: Non-Interactive Write (`-d`)
*   **Given**: `agy` CLI (or `XURL_ANTIGRAVITY_BIN` / `XURL_AGY_BIN`) installed in `PATH`.
*   **When**: A caller runs `xurl agents://antigravity -d "prompt"`, `xurl agents://antigravity/coding -d "prompt"`, or `xurl agents://antigravity/<session_id> -d "prompt"`.
*   **Then**: `xurl` invokes `agy -p <prompt> --output-format stream-json --dangerously-skip-permissions` (with `--agent <role>` or `--conversation <session_id>`), streams `step_update` deltas to stdout, and emits `created: agents://antigravity/<session_id>` or `updated: agents://antigravity/<session_id>` to stderr.

## Technical Plan (Architecture)

```text
xurl-cli (src/main.rs)
  │
  ▼
xurl-core::uri (src/uri.rs)
  ├── "antigravity" | "agy" -> ProviderKind::Antigravity
  └── UUID validation & lowercase normalization for session_id / agent_id
  │
  ▼
xurl-core::provider::antigravity (src/provider/antigravity.rs)
  ├── AntigravityProvider::resolve_transcript_path(brain_root, session_id)
  │     └── prefers transcript_full.jsonl over transcript.jsonl
  └── AntigravityProvider::write(req, sink) -> spawns `agy` with stream-json
  │
  ▼
xurl-core::render (src/render.rs)
  ├── unquote_json_string_arg(raw)
  ├── parse_antigravity_system_message(content)
  └── extract_antigravity_entries(path, raw_jsonl)
        ├── USER_INPUT -> extract_antigravity_user_request
        ├── SYSTEM_MESSAGE -> parse_antigravity_system_message (tracks parent_sender_id)
        ├── PLANNER_RESPONSE -> content + inline send_message to parent_sender_id
        └── CHECKPOINT -> TimelineEntry::Compact
  │
  ▼
xurl-core::service (src/service.rs)
  ├── load_antigravity_summaries(root) & load_antigravity_summary_row(root, session_id)
  ├── collect_antigravity_thread_metadata / collect_antigravity_query_thread_metadata
  ├── collect_antigravity_query_candidates(roots, warnings, need_scope_path)
  └── resolve_antigravity_subagent_view -> analyze_antigravity_child
```

### Key Architectural Refinements

1. **Deterministic `<SYSTEM_MESSAGE>` Parsing & Multi-Turn Inline Subagent Rendering (`xurl-core/src/render.rs`)**:
   - `parse_antigravity_system_message` splits on `<SYSTEM_MESSAGE>` and `</SYSTEM_MESSAGE>` boundaries before extracting `[Message]`, `sender`, and `content`, preventing trailing `<ADDITIONAL_METADATA>` blocks from breaking parsing or leaking into message bodies.
   - `extract_antigravity_entries` binds `parent_sender_id` from the initial `SYSTEM_MESSAGE` (`entries.is_empty()` and `step_index.unwrap_or(0) == 0`) whose `sender` is a valid UUID. Subsequent `SYSTEM_MESSAGE` records from `parent_sender_id` are emitted inline as `MessageRole::User`, and `send_message` tool calls targeting `parent_sender_id` in `PLANNER_RESPONSE` steps are combined with any step `content` and emitted inline as `MessageRole::Assistant`.
   - `unquote_json_string_arg` lives as `pub(crate) fn` in `xurl-core/src/render.rs` alongside `parse_antigravity_system_message`, keeping `render.rs` decoupled from `crate::provider::*`.

2. **Single-Row SQLite Summary Lookup & Shared Metadata Stripping (`xurl-core/src/service.rs`)**:
   - `load_antigravity_summary_row` queries a single row using `WHERE conversation_id = ?1 COLLATE NOCASE LIMIT 1`.
   - `collect_antigravity_thread_metadata`, `collect_antigravity_query_thread_metadata`, and detail mode of `resolve_antigravity_subagent_view` all use `load_antigravity_summary_row` instead of scanning the entire `conversation_summaries` table.
   - A shared `strip_antigravity_step_payload` removes `content`, `thinking`, `tool_calls`, `media`, and `truncated_fields` from step metadata objects.

3. **Unified Subagent Child Analysis & Targeted Discovery (`xurl-core/src/service.rs`)**:
   - `analyze_antigravity_child` resolves and reads the child transcript once, checks step 0 `SYSTEM_MESSAGE` on the in-memory `child_raw` string (falling back to a 1-line file read only if `read_thread_raw` fails), extracts messages once, and computes multi-turn-safe status (`CASCADE_RUN_STATUS_RUNNING` checked first; `STATUS_COMPLETED` requires the latest message to be `MessageRole::Assistant`).
   - `resolve_antigravity_subagent_view` only scans `conversation_summaries` and `brain/` directories when `list == true`.
   - `collect_antigravity_query_candidates` only reads child step 0 from disk for parent `scope_path` inheritance when `need_scope_path == true` (`query_threads_by_path`).

4. **Shared Transcript Resolution & Linear Streaming Write (`xurl-core/src/provider/antigravity.rs`)**:
   - Expose `AntigravityProvider::resolve_transcript_path(brain_root: &Path, session_id: &str) -> Option<PathBuf>` and assign `final_text` once after the streaming loop in `run_write`.

## Alternatives Considered

### Alternative A: Read Only `conversation_summaries.db` for Subagent Discovery
*   **Pros**: Avoids scanning `brain/<id>` directories when listing subagents.
*   **Cons**: `conversation_summaries.db` can be missing, vacuumed, or omit subagent rows while transcript files still exist under `brain/<id>/.system_generated/logs/`.
*   **Reason for Rejection**: Relying exclusively on SQLite loses subagents when the summary database is incomplete. Combining parent transcript `invoke_subagent` events, `conversation_summaries.db`, and a fallback `brain/` scan in list mode guarantees discovery even without SQLite.

### Alternative B: Parse `transcript.jsonl` Instead of `transcript_full.jsonl`
*   **Pros**: Smaller file size on disk because large fields are truncated.
*   **Cons**: `transcript.jsonl` truncates `content` and `tool_calls` when `truncated_fields` is non-empty, which corrupts rendered user/assistant messages and hides `invoke_subagent` / `send_message` arguments.
*   **Reason for Rejection**: `transcript_full.jsonl` is the canonical untruncated log in `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/`; `transcript.jsonl` should only be used as a fallback when `transcript_full.jsonl` is absent.

### Alternative C: Retain Deferred `fallback_assistant_messages` Buffer in `render.rs`
*   **Pros**: Avoids checking `send_message` recipient against the parent session ID.
*   **Cons**: Appends `send_message` replies at the end of the timeline after `CHECKPOINT` entries and drops earlier `send_message` replies whenever a later turn emits normal `content`.
*   **Reason for Rejection**: Matching `send_message` against `parent_sender_id` (discovered from step 0 `SYSTEM_MESSAGE`) allows inline emission in exact chronological order with less state.

--------------------------------------------------------------------------------

## Non-Functional Requirements

*   **Performance & Latency**:
    *   Single-session metadata lookup (`xurl -I agents://antigravity/<id>`), per-item query metadata collection, and subagent detail drill-down (`list == false`) must query at most 1 row from `conversation_summaries.db` via `WHERE conversation_id = ?1 COLLATE NOCASE LIMIT 1`.
    *   Subagent detail drill-down (`xurl agents://antigravity/<main_id>/<child_id>`) must not scan unrelated `brain/*` directories and must read and parse the child transcript file at most once.
*   **Security & Permissions**:
    *   SQLite connections to `conversation_summaries.db` must always use `OpenFlags::SQLITE_OPEN_READ_ONLY` and parameterized queries (`rusqlite::params!`).
    *   Non-interactive writes pass `--dangerously-skip-permissions` to `agy` to match the non-interactive execution contract of other `xurl` providers (`codex`, `claude`, `copilot`, `cursor`, `gemini`, `amp`).
*   **Compatibility**:
    *   Zero new external crate dependencies in `Cargo.toml`.
    *   Must pass `cargo clippy --all-targets` (`all = warn`, `pedantic = warn`) and `cargo test`.

## Risks & Mitigations

*   **SQLite Lock Contention or Missing Summary Rows**: While `agy` is actively writing a session, `conversation_summaries.db` may be locked (`SQLITE_BUSY`), absent, or missing rows for newly spawned subagents.
    *   *Mitigation*: Open `conversation_summaries.db` with `OpenFlags::SQLITE_OPEN_READ_ONLY`, degrade gracefully to `None` / empty map on SQLite errors without failing transcript reads, and fall back to step 0 JSONL metadata and `SYSTEM_MESSAGE` parent links.
*   **Truncated Fields in `transcript.jsonl` Fallback**: If `transcript_full.jsonl` was removed and only `transcript.jsonl` remains, large `content` or `tool_calls` fields may be truncated.
    *   *Mitigation*: Always prefer `transcript_full.jsonl` in `AntigravityProvider::resolve_transcript_path` and tolerate partial/missing tool call arguments without failing timeline extraction.
*   **Large `brain/` Directory Scan Overhead**: A developer machine may accumulate thousands of `brain/<uuid>` directories over time.
    *   *Mitigation*: Skip `brain/` directory scans during subagent detail drill-down (`list == false`), and during non-path provider queries (`need_scope_path == false`) avoid opening child transcript files until after `updated_epoch` sorting and `limit` filtering.
*   **Non-Interactive Write Permissions (`--dangerously-skip-permissions`)**: Passing `--dangerously-skip-permissions` allows `agy` to execute tool calls without interactive prompts.
    *   *Mitigation*: Only invoke `agy` when the caller explicitly passes `-d/--data` to `xurl`, matching the documented write contract across all `xurl` providers.

--------------------------------------------------------------------------------

## Detailed Implementation Plan

### Target Files to Create/Modify

#### 1. `xurl-core/src/provider/antigravity.rs` (Modify)
*   **Type / Scope**: Provider implementation (`xurl-core`)
*   **Proposed Changes**:
    1. Replace `candidate_paths` with `pub fn resolve_transcript_path(brain_root: &Path, session_id: &str) -> Option<PathBuf>`:
       ```rust
       pub fn resolve_transcript_path(brain_root: &Path, session_id: &str) -> Option<PathBuf> {
           let full = Self::full_transcript_path(brain_root, session_id);
           if full.exists() {
               return Some(full);
           }
           let compact = Self::compact_transcript_path(brain_root, session_id);
           if compact.exists() {
               return Some(compact);
           }
           None
       }
       ```
    2. Update `AntigravityProvider::resolve(&self, session_id: &str)` to call `Self::resolve_transcript_path(&brain_root, session_id)`.
    3. In `AntigravityProvider::run_write`, remove `final_text = Some(streamed_text.clone());` from inside the `for line in reader.lines()` loop, and after the loop set:
       ```rust
       if !streamed_text.is_empty() {
           final_text = Some(streamed_text);
       }
       ```

#### 2. `xurl-core/src/render.rs` (Modify)
*   **Type / Scope**: Transcript timeline extractor (`xurl-core`)
*   **Proposed Changes**:
    1. Add `pub(crate) fn unquote_json_string_arg(raw: &str) -> String`:
       ```rust
       pub(crate) fn unquote_json_string_arg(raw: &str) -> String {
           if raw.starts_with('"') {
               serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
           } else {
               raw.to_string()
           }
       }
       ```
    2. Update `parse_antigravity_system_message(content: &str) -> Option<(String, String)>` to strip the optional `"The following is a <SYSTEM_MESSAGE> not actually sent by the user..."` preamble first, then split on `<SYSTEM_MESSAGE>` and `</SYSTEM_MESSAGE>` (using `rsplit_once("</SYSTEM_MESSAGE>")`) before parsing `[Message]`, `sender=`, and `content=`.
    3. Refactor `extract_antigravity_entries(path: &Path, raw_jsonl: &str) -> Result<Vec<TimelineEntry>>`:
       - Remove `fallback_assistant_messages` and `has_assistant_content`; track `let mut parent_sender_id = None::<String>;`.
       - On `"SYSTEM_MESSAGE"`: parse `(sender, body)` via `parse_antigravity_system_message(content)`. If `crate::uri::is_uuid_session_id(&sender)` and `!body.is_empty()`:
         - If `parent_sender_id.is_none()` and `entries.is_empty()` and `value.get("step_index").and_then(Value::as_u64).unwrap_or(0) == 0`, set `parent_sender_id = Some(sender.to_ascii_lowercase())`.
         - If `parent_sender_id.as_deref().is_some_and(|parent| parent.eq_ignore_ascii_case(&sender))`, push `TimelineEntry::Message(ThreadMessage { role: MessageRole::User, text: body })`.
       - On `"PLANNER_RESPONSE"`:
         - Extract `content_text = value.get("content").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())`.
         - Extract `send_msg_text = extract_antigravity_send_message(&value, parent_sender_id.as_deref())`.
         - Combine into `assistant_text`: if both `content_text` and `send_msg_text` are `Some(c)` and `Some(m)` (with `c != m`), join as `format!("{c}\n\n{m}")`; else take whichever is `Some`. Push `TimelineEntry::Message(ThreadMessage { role: MessageRole::Assistant, text: assistant_text })`.
    4. Update `extract_antigravity_send_message(value: &Value, expected_recipient: Option<&str>) -> Option<String>`:
       - Require `let expected_recipient = expected_recipient?;`.
       - Iterate through `tool_calls` where `name == "send_message"`. If `Recipient` / `recipient` is present in `args`, unquote via `unquote_json_string_arg` and `continue` if `!recipient.eq_ignore_ascii_case(expected_recipient)`.
       - Unquote `Message` / `message` via `unquote_json_string_arg` and return `Some(trimmed.to_string())` when non-empty.
    5. Add unit tests in `xurl-core/src/render.rs`:
       - `antigravity_system_message_with_trailing_metadata_parses_cleanly`
       - `antigravity_subagent_multi_turn_preserves_chronological_order`

#### 3. `xurl-core/src/service.rs` (Modify)
*   **Type / Scope**: Query, metadata, and subagent service layer (`xurl-core`)
*   **Proposed Changes**:
    1. Extract `fn map_antigravity_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AntigravitySummaryRow>` and add `fn load_antigravity_summary_row(root: &Path, session_id: &str) -> Option<AntigravitySummaryRow>` executing:
       ```sql
       SELECT conversation_id, title, step_count, last_modified_time, workspace_uris,
              status, source, agent_name, parent_conversation_id, killed
       FROM conversation_summaries
       WHERE conversation_id = ?1 COLLATE NOCASE
       LIMIT 1
       ```
    2. Add `fn strip_antigravity_step_payload(value: &mut Value)` removing `["content", "thinking", "tool_calls", "media", "truncated_fields"]`, and update `collect_antigravity_thread_metadata` and `collect_antigravity_query_thread_metadata` to use `load_antigravity_summary_row` and `strip_antigravity_step_payload`.
    3. Replace `antigravity_child_first_step_points_to_parent` with:
       - `fn antigravity_child_parent_id_from_raw(raw: &str) -> Option<String>`: parses the first non-empty line within 5 lines of `raw`, checks `type == "SYSTEM_MESSAGE"`, calls `render::parse_antigravity_system_message`, verifies `is_uuid_session_id(&sender)`, and returns `Some(sender.to_ascii_lowercase())`.
       - `fn antigravity_child_parent_id(path: &Path) -> Option<String>`: opens `path`, reads up to 5 lines into a small string, and delegates to `antigravity_child_parent_id_from_raw`.
    4. Update `collect_antigravity_query_candidates(roots: &ProviderRoots, warnings: &mut Vec<String>, need_scope_path: bool) -> Vec<QueryCandidate>` (passing `false` from `query_threads` and `true` from `collect_candidates_for_provider`) to use `AntigravityProvider::resolve_transcript_path(&brain_root, &session_id)` and, when `need_scope_path && scope_path.is_none()`, fall back to `antigravity_child_parent_id(&transcript_path).and_then(|pid| summaries.get(&pid)).and_then(|row| parse_antigravity_workspace_scope_path(row.workspace_uris.as_deref()))`.
    5. Replace `unquote_json_string_arg` in `service.rs` with `render::unquote_json_string_arg`.
    6. In `resolve_antigravity_subagent_view(uri, roots, list)`:
       - When `list == true`: call `load_antigravity_summaries(&roots.antigravity_root)`, populate `timelines` from `summaries`, call `discover_antigravity_children_from_brain`, and build list items via `build_antigravity_subagent_list_item`.
       - When `list == false`: extract `agent_id`, call `let summary_row = load_antigravity_summary_row(&roots.antigravity_root, &agent_id);`, and call `build_antigravity_subagent_detail_view(uri, roots, &agent_id, &timelines, summary_row.as_ref(), warnings)`.
    7. Define `struct AntigravityChildAnalysis`:
       ```rust
       struct AntigravityChildAnalysis {
           resolved_path: PathBuf,
           points_to_parent: bool,
           thread: Option<SubagentThreadRef>,
           status: Option<(String, String)>,
           excerpt: Vec<SubagentExcerptMessage>,
       }
       ```
       and `fn analyze_antigravity_child(agent_id: &str, main_thread_id: &str, summary_row: Option<&AntigravitySummaryRow>, roots: &ProviderRoots, include_excerpt: bool, read_error_label: &str, warnings: &mut Vec<String>) -> Option<AntigravityChildAnalysis>`:
       - Resolves `agent_id` via `AntigravityProvider::new(&roots.antigravity_root).resolve(agent_id).ok()?`.
       - Reads `read_thread_raw(&resolved.path)`. On `Ok(child_raw)`, computes `points_to_parent` via `antigravity_child_parent_id_from_raw(&child_raw).is_some_and(|pid| pid == main_thread_id)`, extracts `messages = render::extract_messages(ProviderKind::Antigravity, &resolved.path, &child_raw).unwrap_or_default()`, computes `status = Some(infer_antigravity_child_status(&messages, summary_row))`, builds `thread = Some(SubagentThreadRef { ... })`, and (when `include_excerpt`) populates `excerpt` with the last 3 messages.
       - On `Err(err)`, pushes `format!("failed reading antigravity {read_error_label} for agent_id={agent_id}: {err}")` to `warnings` and sets `points_to_parent = antigravity_child_parent_id(&resolved.path).is_some_and(|pid| pid == main_thread_id)`.
       - Update `infer_antigravity_child_status(messages: &[ crate::model::ThreadMessage ], summary_row: Option<&AntigravitySummaryRow>) -> (String, String)`:
         1. If `summary_row.and_then(|r| r.killed).is_some_and(|k| k != 0)` -> `(STATUS_SHUTDOWN, "conversation_summaries")`.
         2. If `summary_row.and_then(|r| r.status.as_deref()) == Some("CASCADE_RUN_STATUS_RUNNING")` -> `(STATUS_RUNNING, "conversation_summaries")`.
         3. If `messages.last().is_some_and(|m| m.role == MessageRole::Assistant)` -> `(STATUS_COMPLETED, "child_rollout")`.
         4. If `!messages.is_empty()` -> `(STATUS_RUNNING, "child_rollout")`.
         5. Otherwise -> `(STATUS_PENDING_INIT, "child_rollout")`.
       - Share `fn build_antigravity_relation(main_thread_id: &str, timeline: &AgentTimeline, summary_row: Option<&AntigravitySummaryRow>, points_to_parent: bool) -> SubagentRelation` between `build_antigravity_subagent_list_item` and `build_antigravity_subagent_detail_view`.

#### 4. `xurl-cli/tests/cli.rs` (Modify)
*   **Type / Scope**: CLI integration tests (`xurl-cli`)
*   **Proposed Changes**:
    1. Keep existing `antigravity_query_and_path_query_return_fixture_threads` passing and add `antigravity_path_query_inherits_parent_workspace_without_child_summary_row`, which sets up a `tempdir` with a parent row in `conversation_summaries.db` (`workspace_uris = '["file:///workspace/demo"]'`), omits the child row from `conversation_summaries.db`, writes a child transcript whose step 0 `SYSTEM_MESSAGE` has `sender=<parent_id>`, and asserts `xurl 'agents:///workspace/demo?providers=antigravity'` returns both parent and child threads.

### Phasing & Checkpoints

1.  **Phase 1: Provider & Render Refinements (`xurl-core/src/provider/antigravity.rs`, `xurl-core/src/render.rs`)**
    *   Implement `AntigravityProvider::resolve_transcript_path`, linear `final_text` assignment in `run_write`, `render::unquote_json_string_arg`, tag-split `parse_antigravity_system_message`, and inline multi-turn `extract_antigravity_entries` + unit tests.
    *   **Checkpoint**: Run `cargo test -p xurl-core -- render::tests` and `cargo test -p xurl-core -- provider::antigravity::tests`.
2.  **Phase 2: Service Layer SQLite & Subagent Consolidation (`xurl-core/src/service.rs`)**
    *   Implement `load_antigravity_summary_row`, `strip_antigravity_step_payload`, `antigravity_child_parent_id_from_raw`, `analyze_antigravity_child`, and `build_antigravity_relation`.
    *   **Checkpoint**: Run `cargo clippy --all-targets` and `cargo test -p xurl-core`.
3.  **Phase 3: CLI Integration Test Verification (`xurl-cli/tests/cli.rs`)**
    *   Add `antigravity_path_query_inherits_parent_workspace_without_child_summary_row` and run full test suite.
    *   **Checkpoint**: Run `cargo clippy --all-targets && cargo test`.

### Verification Plan

*   **Compilation & Linting**: `cargo clippy --all-targets` must exit with code 0 and zero warnings.
*   **Unit Tests**: `cargo test -p xurl-core` must pass all unit tests, including the new `render.rs` tests.
*   **Integration Tests**: `cargo test -p xurl-cli --test cli` must pass all CLI integration tests, including `antigravity_path_query_inherits_parent_workspace_without_child_summary_row`.

--------------------------------------------------------------------------------

*Drafted with EGM*
