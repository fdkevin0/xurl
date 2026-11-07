use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::error::{Result, XurlError};
use crate::jsonl;
use crate::model::{MessageRole, ProviderKind, ThreadMessage};
use crate::uri::AgentsUri;

const TOOL_TYPES: &[&str] = &[
    "tool_call",
    "tool_result",
    "tool_use",
    "function_call",
    "function_result",
    "function_response",
];
const COMPACT_PLACEHOLDER: &str = "Context was compacted.";

enum TimelineEntry {
    Message(ThreadMessage),
    Compact { summary: Option<String> },
}

pub fn render_markdown(uri: &AgentsUri, source_path: &Path, raw_jsonl: &str) -> Result<String> {
    let entries = extract_timeline_entries(
        uri.provider,
        source_path,
        raw_jsonl,
        &uri.session_id,
        uri.agent_id.as_deref(),
    )?;

    let mut output = String::new();
    let thread_uri = uri.as_agents_string();
    let source = source_path.to_string_lossy();
    output.push_str("---\n");
    output.push_str(&format!("uri: '{}'\n", yaml_single_quoted(&thread_uri)));
    output.push_str(&format!(
        "thread_source: '{}'\n",
        yaml_single_quoted(source.as_ref())
    ));
    output.push_str("---\n\n");
    output.push_str("# Thread\n\n");
    output.push_str("## Timeline\n\n");

    if entries.is_empty() {
        output.push_str("_No user/assistant messages or compact events found._\n");
        return Ok(output);
    }

    for (idx, entry) in entries.iter().enumerate() {
        let title = match entry {
            TimelineEntry::Message(message) => match message.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
            },
            TimelineEntry::Compact { .. } => "Context Compacted",
        };

        output.push_str(&format!("## {}. {}\n\n", idx + 1, title));
        match entry {
            TimelineEntry::Message(message) => output.push_str(message.text.trim()),
            TimelineEntry::Compact { summary } => {
                let summary = summary.as_deref().unwrap_or(COMPACT_PLACEHOLDER);
                output.push_str(summary.trim());
            }
        }
        output.push_str("\n\n");
    }

    Ok(output)
}

fn yaml_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

pub fn extract_messages(
    provider: ProviderKind,
    path: &Path,
    raw_jsonl: &str,
) -> Result<Vec<ThreadMessage>> {
    Ok(
        extract_timeline_entries(provider, path, raw_jsonl, "", None)?
            .into_iter()
            .filter_map(|entry| match entry {
                TimelineEntry::Message(message) => Some(message),
                TimelineEntry::Compact { .. } => None,
            })
            .collect(),
    )
}

fn extract_timeline_entries(
    provider: ProviderKind,
    path: &Path,
    raw_jsonl: &str,
    session_id: &str,
    target_entry_id: Option<&str>,
) -> Result<Vec<TimelineEntry>> {
    if provider == ProviderKind::Amp {
        return Ok(messages_to_entries(extract_amp_messages(path, raw_jsonl)?));
    }
    if provider == ProviderKind::Antigravity {
        return extract_antigravity_entries(path, raw_jsonl);
    }
    if provider == ProviderKind::Copilot {
        return Ok(messages_to_entries(extract_copilot_messages(
            path, raw_jsonl,
        )?));
    }
    if provider == ProviderKind::Gemini {
        return Ok(messages_to_entries(extract_gemini_messages(
            path, raw_jsonl,
        )?));
    }
    if provider == ProviderKind::Kimi {
        return Ok(messages_to_entries(extract_kimi_messages(raw_jsonl)));
    }
    if provider == ProviderKind::Pi {
        return extract_pi_entries(path, raw_jsonl, session_id, target_entry_id);
    }

    let mut entries = Vec::new();

    for (line_idx, line) in raw_jsonl.lines().enumerate() {
        let line_no = line_idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some(value) = jsonl::parse_json_line(path, line_no, trimmed)? else {
            continue;
        };

        let extracted = match provider {
            ProviderKind::Amp | ProviderKind::Antigravity => None,
            ProviderKind::Copilot => extract_copilot_entry(&value),
            ProviderKind::Codex => extract_codex_entry(&value),
            ProviderKind::Claude => extract_claude_entry(&value),
            ProviderKind::Cursor => extract_cursor_message(&value).map(TimelineEntry::Message),
            ProviderKind::Gemini => None,
            ProviderKind::Kimi => None,
            ProviderKind::Pi => None,
            ProviderKind::Opencode => extract_opencode_message(&value).map(TimelineEntry::Message),
        };

        if let Some(entry) = extracted {
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn messages_to_entries(messages: Vec<ThreadMessage>) -> Vec<TimelineEntry> {
    messages.into_iter().map(TimelineEntry::Message).collect()
}

fn extract_antigravity_entries(path: &Path, raw_jsonl: &str) -> Result<Vec<TimelineEntry>> {
    let mut entries = Vec::new();
    let mut parent_sender_id = None::<String>;

    for (line_idx, line) in raw_jsonl.lines().enumerate() {
        let line_no = line_idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some(value) = jsonl::parse_json_line(path, line_no, trimmed)? else {
            continue;
        };

        let Some(step_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };

        match step_type {
            "USER_INPUT" => {
                if let Some(content) = value.get("content").and_then(Value::as_str)
                    && let Some(text) = extract_antigravity_user_request(content)
                {
                    entries.push(TimelineEntry::Message(ThreadMessage {
                        role: MessageRole::User,
                        text,
                    }));
                }
            }
            "SYSTEM_MESSAGE" => {
                if let Some(content) = value.get("content").and_then(Value::as_str)
                    && let Some((sender, body)) = parse_antigravity_system_message(content)
                    && crate::uri::is_uuid_session_id(&sender)
                    && !body.is_empty()
                {
                    if parent_sender_id.is_none()
                        && entries.is_empty()
                        && value.get("step_index").and_then(Value::as_u64).unwrap_or(0) == 0
                    {
                        parent_sender_id = Some(sender.to_ascii_lowercase());
                    }
                    if parent_sender_id
                        .as_deref()
                        .is_some_and(|parent| parent.eq_ignore_ascii_case(&sender))
                    {
                        entries.push(TimelineEntry::Message(ThreadMessage {
                            role: MessageRole::User,
                            text: body,
                        }));
                    }
                }
            }
            "PLANNER_RESPONSE" => {
                let content_text = value
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty());
                let send_msg_text =
                    extract_antigravity_send_message(&value, parent_sender_id.as_deref());
                let assistant_text = match (content_text, send_msg_text) {
                    (Some(c), Some(m)) if c != m => Some(format!("{c}\n\n{m}")),
                    (Some(c), _) => Some(c.to_string()),
                    (None, Some(m)) => Some(m),
                    (None, None) => None,
                };
                if let Some(text) = assistant_text {
                    entries.push(TimelineEntry::Message(ThreadMessage {
                        role: MessageRole::Assistant,
                        text,
                    }));
                }
            }
            "CHECKPOINT" => {
                let summary = value
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(ToString::to_string);
                entries.push(TimelineEntry::Compact { summary });
            }
            _ => {}
        }
    }

    Ok(entries)
}

fn extract_antigravity_user_request(content: &str) -> Option<String> {
    if let Some((_, after_open)) = content.split_once("<USER_REQUEST>")
        && let Some((inner, _)) = after_open.split_once("</USER_REQUEST>")
    {
        let trimmed = inner.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let without_meta = content
        .split("<ADDITIONAL_METADATA>")
        .next()
        .unwrap_or(content)
        .split("<USER_SETTINGS_CHANGE>")
        .next()
        .unwrap_or(content)
        .trim();
    if without_meta.is_empty() {
        None
    } else {
        Some(without_meta.to_string())
    }
}

pub(crate) fn unquote_json_string_arg(raw: &str) -> String {
    if raw.starts_with('"') {
        serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    }
}

pub(crate) fn parse_antigravity_system_message(content: &str) -> Option<(String, String)> {
    let without_preamble = content
        .trim()
        .strip_prefix(
            "The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.",
        )
        .unwrap_or(content)
        .trim();
    let inner = if let Some((_, after_open)) = without_preamble.split_once("<SYSTEM_MESSAGE>") {
        after_open
            .rsplit_once("</SYSTEM_MESSAGE>")
            .map_or(after_open, |(inside, _)| inside)
            .trim()
    } else {
        without_preamble
    };
    let message_body = inner.strip_prefix("[Message]")?.trim();
    let sender_idx = message_body.find("sender=")?;
    let after_sender = &message_body[sender_idx + "sender=".len()..];
    let sender_end = after_sender.find(char::is_whitespace)?;
    let sender = after_sender[..sender_end].trim().to_string();

    let content_marker = "content=";
    let content_idx = after_sender.find(content_marker)?;
    let text = after_sender[content_idx + content_marker.len()..]
        .trim()
        .to_string();
    Some((sender, text))
}

fn extract_antigravity_send_message(
    value: &Value,
    expected_recipient: Option<&str>,
) -> Option<String> {
    let expected_recipient = expected_recipient?;
    for call in value
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if call.get("name").and_then(Value::as_str) != Some("send_message") {
            continue;
        }
        let args = call.get("args");
        if let Some(recipient_raw) = args
            .and_then(|a| a.get("Recipient").or_else(|| a.get("recipient")))
            .and_then(Value::as_str)
        {
            let recipient = unquote_json_string_arg(recipient_raw);
            if !recipient.eq_ignore_ascii_case(expected_recipient) {
                continue;
            }
        }
        let Some(raw_msg) = args
            .and_then(|a| a.get("Message").or_else(|| a.get("message")))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let decoded = unquote_json_string_arg(raw_msg);
        let trimmed = decoded.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn extract_pi_entries(
    path: &Path,
    raw_jsonl: &str,
    session_id: &str,
    target_entry_id: Option<&str>,
) -> Result<Vec<TimelineEntry>> {
    let mut entries_by_id = HashMap::<String, Value>::new();
    let mut last_entry_id = None::<String>;

    for (line_idx, line) in raw_jsonl.lines().enumerate() {
        let line_no = line_idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some(value) = jsonl::parse_json_line(path, line_no, trimmed)? else {
            continue;
        };

        if value.get("type").and_then(Value::as_str) == Some("session") {
            continue;
        }

        let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };

        last_entry_id = Some(id.clone());
        entries_by_id.insert(id, value);
    }

    if entries_by_id.is_empty() {
        return Ok(Vec::new());
    }

    let leaf_id = target_entry_id
        .map(str::to_ascii_lowercase)
        .or(last_entry_id)
        .unwrap_or_default();

    if !entries_by_id.contains_key(&leaf_id) {
        return Err(XurlError::EntryNotFound {
            provider: ProviderKind::Pi.to_string(),
            session_id: session_id.to_string(),
            entry_id: leaf_id,
        });
    }

    let mut path_ids = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf_id);

    while let Some(entry_id) = current {
        if !seen.insert(entry_id.clone()) {
            break;
        }

        let Some(entry) = entries_by_id.get(&entry_id) else {
            break;
        };
        path_ids.push(entry_id);

        current = entry
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
    }

    path_ids.reverse();

    let mut entries = Vec::new();
    for entry_id in path_ids {
        let Some(entry) = entries_by_id.get(&entry_id) else {
            continue;
        };
        if let Some(timeline_entry) = extract_pi_entry(entry) {
            entries.push(timeline_entry);
        }
    }

    Ok(entries)
}

fn extract_pi_entry(value: &Value) -> Option<TimelineEntry> {
    let entry_type = value.get("type").and_then(Value::as_str)?;

    if entry_type == "message" {
        let message = value.get("message")?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .and_then(parse_role)?;
        let text = extract_text(message.get("content"));
        if text.trim().is_empty() {
            return None;
        }

        return Some(TimelineEntry::Message(ThreadMessage { role, text }));
    }

    if entry_type == "compaction" || entry_type == "branch_summary" {
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return Some(TimelineEntry::Compact { summary });
    }

    None
}

fn extract_amp_messages(path: &Path, raw_json: &str) -> Result<Vec<ThreadMessage>> {
    let value =
        serde_json::from_str::<Value>(raw_json).map_err(|source| XurlError::InvalidJsonLine {
            path: path.to_path_buf(),
            line: 1,
            source,
        })?;

    let mut messages = Vec::new();
    for message in value
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(role) = message
            .get("role")
            .and_then(Value::as_str)
            .and_then(parse_role)
        else {
            continue;
        };

        let text = extract_amp_text(message.get("content"));
        if text.trim().is_empty() {
            continue;
        }

        messages.push(ThreadMessage { role, text });
    }

    Ok(messages)
}

fn extract_copilot_messages(path: &Path, raw_jsonl: &str) -> Result<Vec<ThreadMessage>> {
    let mut messages = Vec::new();

    for (line_idx, line) in raw_jsonl.lines().enumerate() {
        let line_no = line_idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some(value) = jsonl::parse_json_line(path, line_no, trimmed)? else {
            continue;
        };

        if let Some(message) = extract_copilot_message(&value) {
            messages.push(message);
        }
    }

    Ok(messages)
}

fn extract_gemini_messages(path: &Path, raw_json: &str) -> Result<Vec<ThreadMessage>> {
    let value =
        serde_json::from_str::<Value>(raw_json).map_err(|source| XurlError::InvalidJsonLine {
            path: path.to_path_buf(),
            line: 1,
            source,
        })?;

    let mut messages = Vec::new();
    for message in value
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(role) = message
            .get("type")
            .and_then(Value::as_str)
            .and_then(parse_gemini_role)
        else {
            continue;
        };

        let text = extract_text(message.get("displayContent"));
        let text = if text.trim().is_empty() {
            extract_text(message.get("content"))
        } else {
            text
        };

        if text.trim().is_empty() {
            continue;
        }

        messages.push(ThreadMessage { role, text });
    }

    Ok(messages)
}

fn extract_codex_message(value: &Value) -> Option<ThreadMessage> {
    let record_type = value.get("type").and_then(Value::as_str)?;

    if record_type == "response_item" {
        let payload = value.get("payload")?;
        let payload_type = payload.get("type").and_then(Value::as_str)?;
        if payload_type != "message" {
            return None;
        }

        let role = payload.get("role").and_then(Value::as_str)?;
        let role = parse_role(role)?;
        let text = extract_text(payload.get("content"));
        if text.trim().is_empty() {
            return None;
        }

        return Some(ThreadMessage { role, text });
    }

    if record_type == "event_msg"
        && value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|t| t == "agent_message")
    {
        let text = value
            .get("payload")
            .and_then(|payload| payload.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if text.trim().is_empty() {
            return None;
        }

        return Some(ThreadMessage {
            role: MessageRole::Assistant,
            text,
        });
    }

    None
}

fn extract_copilot_message(value: &Value) -> Option<ThreadMessage> {
    match value.get("type").and_then(Value::as_str)? {
        "user.message" => {
            let text = value
                .get("data")
                .and_then(|data| data.get("content"))
                .and_then(Value::as_str)?;
            if text.trim().is_empty() {
                return None;
            }
            Some(ThreadMessage {
                role: MessageRole::User,
                text: text.to_string(),
            })
        }
        "assistant.message" => {
            let text = value
                .get("data")
                .and_then(|data| data.get("content"))
                .and_then(Value::as_str)?;
            if text.trim().is_empty() {
                return None;
            }
            Some(ThreadMessage {
                role: MessageRole::Assistant,
                text: text.to_string(),
            })
        }
        _ => None,
    }
}

fn extract_copilot_entry(value: &Value) -> Option<TimelineEntry> {
    extract_copilot_message(value).map(TimelineEntry::Message)
}

fn extract_codex_entry(value: &Value) -> Option<TimelineEntry> {
    if let Some(message) = extract_codex_message(value) {
        return Some(TimelineEntry::Message(message));
    }

    if is_codex_compact_event(value) {
        return Some(TimelineEntry::Compact { summary: None });
    }

    None
}

fn is_codex_compact_event(value: &Value) -> bool {
    let record_type = value.get("type").and_then(Value::as_str);

    if record_type == Some("compacted") {
        return true;
    }

    record_type == Some("event_msg")
        && value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|payload_type| payload_type == "context_compacted")
}

fn extract_claude_message(value: &Value) -> Option<ThreadMessage> {
    let record_type = value.get("type").and_then(Value::as_str)?;
    if record_type != "user" && record_type != "assistant" {
        return None;
    }

    let message = value.get("message")?;
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .or(Some(record_type))?;
    let role = parse_role(role)?;

    let text = extract_text(message.get("content"));
    if text.trim().is_empty() {
        return None;
    }

    Some(ThreadMessage { role, text })
}

fn extract_claude_entry(value: &Value) -> Option<TimelineEntry> {
    if is_claude_compact_boundary(value) {
        return Some(TimelineEntry::Compact { summary: None });
    }

    if is_claude_compact_summary(value) {
        let summary = extract_claude_message(value).map(|message| message.text);
        return Some(TimelineEntry::Compact { summary });
    }

    extract_claude_message(value).map(TimelineEntry::Message)
}

fn is_claude_compact_boundary(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("system")
        && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
}

fn is_claude_compact_summary(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("user")
        && value
            .get("isCompactSummary")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn extract_opencode_message(value: &Value) -> Option<ThreadMessage> {
    let record_type = value.get("type").and_then(Value::as_str)?;
    if record_type != "message" {
        return None;
    }

    let message = value.get("message")?;
    let role = message.get("role").and_then(Value::as_str)?;
    let role = parse_role(role)?;

    let mut chunks = Vec::new();
    for part in value
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(part_type) = part.get("type").and_then(Value::as_str) else {
            continue;
        };

        if part_type != "text" && part_type != "reasoning" {
            continue;
        }

        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            chunks.push(text.trim().to_string());
        }
    }

    if chunks.is_empty() {
        return None;
    }

    Some(ThreadMessage {
        role,
        text: chunks.join("\n\n"),
    })
}

fn extract_cursor_message(value: &Value) -> Option<ThreadMessage> {
    extract_opencode_message(value)
}

fn extract_amp_text(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };

    let mut chunks = Vec::new();
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };

        match item_type {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    chunks.push(text.trim().to_string());
                }
            }
            "thinking" => {
                if let Some(thinking) = item.get("thinking").and_then(Value::as_str)
                    && !thinking.trim().is_empty()
                {
                    chunks.push(thinking.trim().to_string());
                }
            }
            _ => {}
        }
    }

    chunks.join("\n\n")
}

fn parse_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

fn parse_gemini_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "gemini" => Some(MessageRole::Assistant),
        _ => None,
    }
}

fn extract_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };

    if let Some(text) = content.as_str() {
        return text.to_string();
    }

    let Some(items) = content.as_array() else {
        return String::new();
    };

    let mut chunks = Vec::new();

    for item in items {
        if let Some(text) = item.as_str()
            && !text.trim().is_empty()
        {
            chunks.push(text.trim().to_string());
            continue;
        }

        if let Some(item_type) = item.get("type").and_then(Value::as_str)
            && TOOL_TYPES.contains(&item_type)
        {
            continue;
        }

        if let Some(text) = item.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            chunks.push(text.trim().to_string());
            continue;
        }

        if let Some(text) = item.get("input_text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            chunks.push(text.trim().to_string());
            continue;
        }

        if let Some(text) = item.get("output_text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            chunks.push(text.trim().to_string());
        }
    }

    chunks.join("\n\n")
}

fn extract_kimi_messages(raw_jsonl: &str) -> Vec<ThreadMessage> {
    let mut messages = Vec::new();

    for line in raw_jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        let Some(role) = value
            .get("role")
            .and_then(Value::as_str)
            .and_then(parse_role)
        else {
            continue;
        };

        let text = extract_kimi_text(&value);
        if text.trim().is_empty() {
            continue;
        }

        messages.push(ThreadMessage { role, text });
    }

    messages
}

fn extract_kimi_text(value: &Value) -> String {
    if let Some(text) = value.get("content").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }

    let Some(items) = value.get("content").and_then(Value::as_array) else {
        return String::new();
    };

    let mut chunks = Vec::new();
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };

        match item_type {
            "think" | "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        chunks.push(text.trim().to_string());
                    }
                }
            }
            _ => {}
        }
    }

    chunks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::model::ProviderKind;
    use crate::render::{extract_messages, render_markdown};
    use crate::uri::AgentsUri;

    #[test]
    fn render_outputs_frontmatter() {
        let raw = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;
        let uri =
            AgentsUri::parse("codex://019c871c-b1f9-7f60-9c4f-87ed09f13592").expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.starts_with("---\n"));
        assert!(output.contains("uri: 'agents://codex/019c871c-b1f9-7f60-9c4f-87ed09f13592'"));
        assert!(output.contains("thread_source: '/tmp/mock'"));
        assert!(output.contains("## Timeline"));
    }

    #[test]
    fn codex_filters_function_calls() {
        let raw = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"ls"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"world"}]}}"#;

        let messages =
            extract_messages(ProviderKind::Codex, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "world");
    }

    #[test]
    fn claude_filters_tool_use() {
        let raw = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"search"},{"type":"text","text":"done"}]}}"#;

        let messages =
            extract_messages(ProviderKind::Claude, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text, "done");
    }

    #[test]
    fn opencode_extracts_text_and_reasoning_parts() {
        let raw = r#"{"type":"session","sessionId":"ses_43a90e3adffejRgrTdlJa48CtE"}
{"type":"message","id":"msg_1","sessionId":"ses_43a90e3adffejRgrTdlJa48CtE","message":{"role":"user","time":{"created":1}},"parts":[{"type":"text","text":"hello"}]}
{"type":"message","id":"msg_2","sessionId":"ses_43a90e3adffejRgrTdlJa48CtE","message":{"role":"assistant","time":{"created":2}},"parts":[{"type":"reasoning","text":"thinking"},{"type":"tool","tool":"read"},{"type":"text","text":"world"}]}"#;

        let messages =
            extract_messages(ProviderKind::Opencode, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "thinking\n\nworld");
    }

    #[test]
    fn amp_extracts_text_and_thinking_content() {
        let raw = r#"{"id":"T-019c0797-c402-7389-bd80-d785c98df295","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"step by step"},{"type":"tool_use","name":"finder"},{"type":"text","text":"done"}]},{"role":"user","content":[{"type":"tool_result","toolUseID":"tool_1","run":{"status":"done","result":"ignored"}}]}]}"#;

        let messages =
            extract_messages(ProviderKind::Amp, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "step by step\n\ndone");
    }

    #[test]
    fn gemini_extracts_user_and_assistant_messages() {
        let raw = r#"{"sessionId":"29d207db-ca7e-40ba-87f7-e14c9de60613","messages":[{"type":"info","content":"ignored"},{"type":"user","content":"hello"},{"type":"gemini","content":"world"},{"type":"gemini","content":[{"type":"thinking","text":"step by step"},{"type":"tool_call","name":"list_directory"},{"type":"text","text":"done"}]}]}"#;

        let messages =
            extract_messages(ProviderKind::Gemini, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "world");
        assert_eq!(messages[2].text, "step by step\n\ndone");
    }

    #[test]
    fn pi_default_leaf_renders_latest_branch() {
        let raw = r#"{"type":"session","version":3,"id":"12cb4c19-2774-4de4-a0d0-9fa32fbae29f","timestamp":"2026-02-23T13:00:12.780Z","cwd":"/tmp/project"}
{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-02-23T13:00:13.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}
{"type":"message","id":"b1b2c3d4","parentId":"a1b2c3d4","timestamp":"2026-02-23T13:00:14.000Z","message":{"role":"assistant","content":[{"type":"text","text":"root done"}]}}
{"type":"message","id":"c1b2c3d4","parentId":"b1b2c3d4","timestamp":"2026-02-23T13:00:15.000Z","message":{"role":"user","content":[{"type":"text","text":"branch one"}]}}
{"type":"message","id":"d1b2c3d4","parentId":"c1b2c3d4","timestamp":"2026-02-23T13:00:16.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch one done"}]}}
{"type":"message","id":"e1b2c3d4","parentId":"b1b2c3d4","timestamp":"2026-02-23T13:00:17.000Z","message":{"role":"user","content":[{"type":"text","text":"branch two"}]}}
{"type":"compaction","id":"f1b2c3d4","parentId":"e1b2c3d4","timestamp":"2026-02-23T13:00:18.000Z","summary":"compact summary","firstKeptEntryId":"b1b2c3d4","tokensBefore":128}
{"type":"message","id":"g1b2c3d4","parentId":"f1b2c3d4","timestamp":"2026-02-23T13:00:19.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch two done"}]}}"#;

        let uri = AgentsUri::parse("pi://12cb4c19-2774-4de4-a0d0-9fa32fbae29f").expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("root"));
        assert!(output.contains("branch two"));
        assert!(output.contains("compact summary"));
        assert!(!output.contains("branch one done"));
    }

    #[test]
    fn pi_entry_leaf_renders_requested_branch() {
        let raw = r#"{"type":"session","version":3,"id":"12cb4c19-2774-4de4-a0d0-9fa32fbae29f","timestamp":"2026-02-23T13:00:12.780Z","cwd":"/tmp/project"}
{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-02-23T13:00:13.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}
{"type":"message","id":"b1b2c3d4","parentId":"a1b2c3d4","timestamp":"2026-02-23T13:00:14.000Z","message":{"role":"assistant","content":[{"type":"text","text":"root done"}]}}
{"type":"message","id":"c1b2c3d4","parentId":"b1b2c3d4","timestamp":"2026-02-23T13:00:15.000Z","message":{"role":"user","content":[{"type":"text","text":"branch one"}]}}
{"type":"message","id":"d1b2c3d4","parentId":"c1b2c3d4","timestamp":"2026-02-23T13:00:16.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch one done"}]}}
{"type":"message","id":"e1b2c3d4","parentId":"b1b2c3d4","timestamp":"2026-02-23T13:00:17.000Z","message":{"role":"user","content":[{"type":"text","text":"branch two"}]}}
{"type":"message","id":"f1b2c3d4","parentId":"e1b2c3d4","timestamp":"2026-02-23T13:00:18.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch two done"}]}}"#;

        let uri = AgentsUri::parse("pi://12cb4c19-2774-4de4-a0d0-9fa32fbae29f/d1b2c3d4")
            .expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("branch one done"));
        assert!(!output.contains("branch two done"));
    }

    #[test]
    fn pi_entry_leaf_renders_requested_branch_with_uppercase_ids() {
        let raw = r#"{"type":"session","version":3,"id":"12cb4c19-2774-4de4-a0d0-9fa32fbae29f","timestamp":"2026-02-23T13:00:12.780Z","cwd":"/tmp/project"}
{"type":"message","id":"A1B2C3D4","parentId":null,"timestamp":"2026-02-23T13:00:13.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}
{"type":"message","id":"B1B2C3D4","parentId":"A1B2C3D4","timestamp":"2026-02-23T13:00:14.000Z","message":{"role":"assistant","content":[{"type":"text","text":"root done"}]}}
{"type":"message","id":"C1B2C3D4","parentId":"B1B2C3D4","timestamp":"2026-02-23T13:00:15.000Z","message":{"role":"user","content":[{"type":"text","text":"branch one"}]}}
{"type":"message","id":"D1B2C3D4","parentId":"C1B2C3D4","timestamp":"2026-02-23T13:00:16.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch one done"}]}}
{"type":"message","id":"E1B2C3D4","parentId":"B1B2C3D4","timestamp":"2026-02-23T13:00:17.000Z","message":{"role":"user","content":[{"type":"text","text":"branch two"}]}}
{"type":"message","id":"F1B2C3D4","parentId":"E1B2C3D4","timestamp":"2026-02-23T13:00:18.000Z","message":{"role":"assistant","content":[{"type":"text","text":"branch two done"}]}}"#;

        let uri = AgentsUri::parse("pi://12cb4c19-2774-4de4-a0d0-9fa32fbae29f/d1b2c3d4")
            .expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("branch one done"));
        assert!(!output.contains("branch two done"));
    }

    #[test]
    fn pi_entry_leaf_reports_not_found() {
        let raw = r#"{"type":"session","version":3,"id":"12cb4c19-2774-4de4-a0d0-9fa32fbae29f","timestamp":"2026-02-23T13:00:12.780Z","cwd":"/tmp/project"}
{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-02-23T13:00:13.000Z","message":{"role":"user","content":[{"type":"text","text":"root"}]}}"#;

        let uri = AgentsUri::parse("pi://12cb4c19-2774-4de4-a0d0-9fa32fbae29f/deadbeef")
            .expect("parse uri");
        let err = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect_err("must fail");
        assert!(format!("{err}").contains("entry not found"));
    }

    #[test]
    fn codex_renders_compact_events_in_timeline() {
        let raw = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"type":"event_msg","payload":{"type":"context_compacted"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"world"}]}}"#;

        let uri =
            AgentsUri::parse("codex://019c871c-b1f9-7f60-9c4f-87ed09f13592").expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("## 1. User"));
        assert!(output.contains("## 2. Context Compacted"));
        assert!(output.contains("Context was compacted."));
        assert!(output.contains("## 3. Assistant"));
    }

    #[test]
    fn claude_compact_summary_renders_as_compact_entry() {
        let raw = r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":[{"type":"text","text":"Summary: old conversation"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"New answer"}]}}"#;

        let uri =
            AgentsUri::parse("claude://2823d1df-720a-4c31-ac55-ae8ba726721f").expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("## 1. Context Compacted"));
        assert!(output.contains("Summary: old conversation"));
        assert!(!output.contains("## 1. User"));
        assert!(output.contains("## 2. Assistant"));
    }

    #[test]
    fn kimi_extracts_string_content_messages() {
        let raw = r#"{"role":"user","content":"hello"}
{"role":"assistant","content":"world"}"#;

        let messages =
            extract_messages(ProviderKind::Kimi, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "world");
    }

    #[test]
    fn kimi_extracts_think_and_text_content_types() {
        let raw = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}
{"role":"assistant","content":[{"type":"think","text":"reasoning"},{"type":"tool_call","name":"read_file"},{"type":"text","text":"done"}]}"#;

        let messages =
            extract_messages(ProviderKind::Kimi, Path::new("/tmp/mock"), raw).expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "reasoning\n\ndone");
    }

    #[test]
    fn antigravity_extracts_user_request_planner_response_and_checkpoint() {
        let raw = r##"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-04-06T03:07:38.587966376Z","content":"<USER_REQUEST>\ninspect antigravity support\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-04-06\n</ADDITIONAL_METADATA>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-04-06T03:07:40Z","thinking":"thinking...","tool_calls":[{"id":"1","name":"read_file","args":{"Path":"/tmp/a"}}]}
{"step_index":2,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-04-06T03:07:41Z","content":"# Resuming from a compaction\nSummary of work"}
{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-04-06T03:07:42Z","content":"Antigravity support is ready."}"##;

        let uri = AgentsUri::parse("agents://antigravity/7f636947-513f-4eef-b737-c3be296f6b48")
            .expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        assert!(output.contains("## 1. User"));
        assert!(output.contains("inspect antigravity support"));
        assert!(!output.contains("ADDITIONAL_METADATA"));
        assert!(output.contains("## 2. Context Compacted"));
        assert!(output.contains("# Resuming from a compaction"));
        assert!(output.contains("## 3. Assistant"));
        assert!(output.contains("Antigravity support is ready."));
    }

    #[test]
    fn antigravity_subagent_extracts_system_prompt_and_send_message_fallback() {
        let raw = r#"{"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-04-06T03:08:30Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-04-06T03:08:30Z sender=7f636947-513f-4eef-b737-c3be296f6b48 priority=NORMAL content=Audit the module and reply.\n</SYSTEM_MESSAGE>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-04-06T03:08:40Z","tool_calls":[{"id":"call_1","name":"send_message","args":{"Message":"\"Audit complete: no issues found.\"","Recipient":"\"7f636947-513f-4eef-b737-c3be296f6b48\""}}]}"#;

        let messages = extract_messages(ProviderKind::Antigravity, Path::new("/tmp/mock"), raw)
            .expect("extract");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "Audit the module and reply.");
        assert_eq!(messages[1].text, "Audit complete: no issues found.");
    }

    #[test]
    fn antigravity_system_message_with_trailing_metadata_parses_cleanly() {
        let raw = r#"{"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-04-06T03:08:30Z","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-04-06T03:08:30Z sender=7f636947-513f-4eef-b737-c3be296f6b48 priority=NORMAL content=Audit the module.\n</SYSTEM_MESSAGE>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-04-06\n</ADDITIONAL_METADATA>"}"#;

        let messages = extract_messages(ProviderKind::Antigravity, Path::new("/tmp/mock"), raw)
            .expect("extract");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Audit the module.");
    }

    #[test]
    fn antigravity_subagent_multi_turn_preserves_chronological_order() {
        let raw = r##"{"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-04-06T03:08:30Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-04-06T03:08:30Z sender=7f636947-513f-4eef-b737-c3be296f6b48 priority=NORMAL content=First task.\n</SYSTEM_MESSAGE>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-04-06T03:08:40Z","content":"Sending report.","tool_calls":[{"id":"call_1","name":"send_message","args":{"Message":"\"Turn 1 report.\"","Recipient":"\"7f636947-513f-4eef-b737-c3be296f6b48\""}}]}
{"step_index":2,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-04-06T03:08:50Z","content":"# Compacted"}
{"step_index":3,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-04-06T03:09:00Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-04-06T03:09:00Z sender=7f636947-513f-4eef-b737-c3be296f6b48 priority=NORMAL content=Second task.\n</SYSTEM_MESSAGE>"}
{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-04-06T03:09:10Z","content":"Turn 2 final answer."}"##;

        let uri = AgentsUri::parse("agents://antigravity/37fa3489-df9f-4e82-9234-43963c27d1c6")
            .expect("parse uri");
        let output = render_markdown(&uri, Path::new("/tmp/mock"), raw).expect("render");

        let idx1 = output.find("First task.").expect("turn 1 user");
        let idx2 = output
            .find("Sending report.\n\nTurn 1 report.")
            .expect("turn 1 assistant");
        let idx3 = output.find("## 3. Context Compacted").expect("checkpoint");
        let idx4 = output.find("Second task.").expect("turn 2 user");
        let idx5 = output
            .find("Turn 2 final answer.")
            .expect("turn 2 assistant");
        assert!(idx1 < idx2 && idx2 < idx3 && idx3 < idx4 && idx4 < idx5);
    }
}
