use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::error::{Result, XurlError};
use crate::model::{ProviderKind, ResolutionMeta, ResolvedThread, WriteRequest, WriteResult};
use crate::provider::{
    Provider, WriteEventSink, append_passthrough_args, append_passthrough_args_excluding,
};

#[derive(Debug, Clone)]
pub struct AntigravityProvider {
    root: PathBuf,
}

impl AntigravityProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn brain_root(&self) -> PathBuf {
        self.root.join("brain")
    }

    pub fn full_transcript_path(brain_root: &Path, session_id: &str) -> PathBuf {
        brain_root
            .join(session_id)
            .join(".system_generated/logs/transcript_full.jsonl")
    }

    pub fn compact_transcript_path(brain_root: &Path, session_id: &str) -> PathBuf {
        brain_root
            .join(session_id)
            .join(".system_generated/logs/transcript.jsonl")
    }

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

    fn agy_bin() -> String {
        std::env::var("XURL_ANTIGRAVITY_BIN")
            .ok()
            .filter(|val| !val.is_empty())
            .or_else(|| {
                std::env::var("XURL_AGY_BIN")
                    .ok()
                    .filter(|val| !val.is_empty())
            })
            .unwrap_or_else(|| "agy".to_string())
    }

    fn spawn_agy_command(args: &[String]) -> Result<std::process::Child> {
        let bin = Self::agy_bin();
        let mut command = Command::new(&bin);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                XurlError::CommandNotFound { command: bin }
            } else {
                XurlError::Io {
                    path: PathBuf::from(bin),
                    source,
                }
            }
        })
    }

    fn session_id_from_event(value: &Value) -> Option<&str> {
        value
            .get("conversation_id")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("step_update")
                    .and_then(|step| step.get("conversation_id"))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .get("result")
                    .and_then(|result| result.get("conversation_id"))
                    .and_then(Value::as_str)
            })
    }

    fn assistant_delta(value: &Value) -> Option<&str> {
        if value.get("event").and_then(Value::as_str) != Some("step_update") {
            return None;
        }
        let step_update = value.get("step_update")?;
        if step_update.get("step_type").and_then(Value::as_str) != Some("agent_response") {
            return None;
        }
        step_update.get("text_delta").and_then(Value::as_str)
    }

    fn result_response_text(value: &Value) -> Option<&str> {
        if value.get("event").and_then(Value::as_str) != Some("result") {
            return None;
        }
        value
            .get("result")
            .and_then(|res| res.get("response"))
            .and_then(Value::as_str)
    }

    fn result_error_text(value: &Value) -> Option<String> {
        if value.get("event").and_then(Value::as_str) != Some("result") {
            return None;
        }
        let result = value.get("result")?;
        let status = result.get("status").and_then(Value::as_str)?;
        if status.eq_ignore_ascii_case("SUCCESS") {
            return None;
        }
        let err_msg = result
            .get("error")
            .and_then(Value::as_str)
            .filter(|msg| !msg.trim().is_empty())
            .unwrap_or("antigravity result status is not SUCCESS");
        Some(format!("{status}: {err_msg}"))
    }

    fn run_write(
        &self,
        args: &[String],
        req: &WriteRequest,
        sink: &mut dyn WriteEventSink,
        warnings: Vec<String>,
    ) -> Result<WriteResult> {
        let mut child = Self::spawn_agy_command(args)?;
        let stdout = child.stdout.take().ok_or_else(|| {
            XurlError::WriteProtocol("antigravity stdout pipe is unavailable".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            XurlError::WriteProtocol("antigravity stderr pipe is unavailable".to_string())
        })?;
        let stderr_handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut content = String::new();
            let _ = reader.read_to_string(&mut content);
            content
        });

        let stream_path = Path::new("<antigravity:stdout>");
        let mut session_id = req.session_id.clone();
        let mut final_text = None::<String>;
        let mut streamed_text = String::new();
        let mut saw_json_event = false;
        let mut stream_error = None::<String>;
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line.map_err(|source| XurlError::Io {
                path: stream_path.to_path_buf(),
                source,
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            saw_json_event = true;

            if let Some(current_session_id) = Self::session_id_from_event(&value)
                && session_id.as_deref() != Some(current_session_id)
            {
                sink.on_session_ready(ProviderKind::Antigravity, current_session_id)?;
                session_id = Some(current_session_id.to_string());
            }

            if let Some(delta) = Self::assistant_delta(&value)
                && !delta.is_empty()
            {
                sink.on_text_delta(delta)?;
                streamed_text.push_str(delta);
            }

            if let Some(text) = Self::result_response_text(&value)
                && !text.is_empty()
                && streamed_text.is_empty()
            {
                final_text = Some(text.to_string());
            }

            if let Some(error_text) = Self::result_error_text(&value) {
                stream_error = Some(error_text);
            }
        }

        if !streamed_text.is_empty() {
            final_text = Some(streamed_text);
        }

        let status = child.wait().map_err(|source| XurlError::Io {
            path: PathBuf::from(Self::agy_bin()),
            source,
        })?;
        let stderr_content = stderr_handle.join().unwrap_or_default();
        if !status.success() {
            return Err(XurlError::CommandFailed {
                command: format!("{} {}", Self::agy_bin(), args.join(" ")),
                code: status.code(),
                stderr: stderr_content.trim().to_string(),
            });
        }

        if let Some(error_text) = stream_error {
            return Err(XurlError::WriteProtocol(format!(
                "antigravity stream reported failure ({error_text})"
            )));
        }

        if !saw_json_event {
            return Err(XurlError::WriteProtocol(
                "antigravity output does not contain JSON events".to_string(),
            ));
        }

        let session_id = if let Some(session_id) = session_id {
            session_id
        } else {
            return Err(XurlError::WriteProtocol(
                "missing conversation_id in antigravity event stream".to_string(),
            ));
        };

        Ok(WriteResult {
            provider: ProviderKind::Antigravity,
            session_id,
            final_text,
            warnings,
        })
    }
}

impl Provider for AntigravityProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Antigravity
    }

    fn resolve(&self, session_id: &str) -> Result<ResolvedThread> {
        let brain_root = self.brain_root();
        if let Some(selected) = Self::resolve_transcript_path(&brain_root, session_id) {
            let metadata = ResolutionMeta {
                source: "antigravity:brain".to_string(),
                candidate_count: 1,
                warnings: Vec::new(),
            };

            return Ok(ResolvedThread {
                provider: ProviderKind::Antigravity,
                session_id: session_id.to_string(),
                path: selected,
                metadata,
            });
        }

        Err(XurlError::ThreadNotFound {
            provider: ProviderKind::Antigravity.to_string(),
            session_id: session_id.to_string(),
            searched_roots: vec![
                Self::full_transcript_path(&brain_root, session_id),
                Self::compact_transcript_path(&brain_root, session_id),
            ],
        })
    }

    fn write(&self, req: &WriteRequest, sink: &mut dyn WriteEventSink) -> Result<WriteResult> {
        let mut warnings = Vec::new();
        let mut args = vec![
            "-p".to_string(),
            req.prompt.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];

        if let Some(role) = req.options.role.as_deref() {
            args.push("--agent".to_string());
            args.push(role.to_string());
            let ignored =
                append_passthrough_args_excluding(&mut args, &req.options.params, &["agent"]);
            if !ignored.is_empty() {
                warnings.push(
                    "ignored query parameter `agent` because URI role is already set".to_string(),
                );
            }
        } else {
            append_passthrough_args(&mut args, &req.options.params);
        }

        if let Some(session_id) = req.session_id.as_deref() {
            args.push("--conversation".to_string());
            args.push(session_id.to_string());
        }

        self.run_write(&args, req, sink, warnings)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::provider::Provider;
    use crate::provider::antigravity::AntigravityProvider;

    #[test]
    fn resolves_prefers_full_transcript_over_compact() {
        let temp = tempdir().expect("tempdir");
        let full_file = temp
            .path()
            .join("brain/688628a1-407a-4b4e-b24a-1a250ebf864f/.system_generated/logs/transcript_full.jsonl");
        let compact_file = temp.path().join(
            "brain/688628a1-407a-4b4e-b24a-1a250ebf864f/.system_generated/logs/transcript.jsonl",
        );
        fs::create_dir_all(full_file.parent().expect("parent")).expect("mkdir");
        fs::write(&full_file, "{}\n").expect("write");
        fs::write(&compact_file, "{}\n").expect("write");

        let provider = AntigravityProvider::new(temp.path());
        let resolved = provider
            .resolve("688628a1-407a-4b4e-b24a-1a250ebf864f")
            .expect("resolve should succeed");

        assert_eq!(resolved.path, full_file);
        assert_eq!(resolved.metadata.source, "antigravity:brain");
    }

    #[test]
    fn resolves_falls_back_to_compact_transcript() {
        let temp = tempdir().expect("tempdir");
        let compact_file = temp.path().join(
            "brain/688628a1-407a-4b4e-b24a-1a250ebf864f/.system_generated/logs/transcript.jsonl",
        );
        fs::create_dir_all(compact_file.parent().expect("parent")).expect("mkdir");
        fs::write(&compact_file, "{}\n").expect("write");

        let provider = AntigravityProvider::new(temp.path());
        let resolved = provider
            .resolve("688628a1-407a-4b4e-b24a-1a250ebf864f")
            .expect("resolve should succeed");

        assert_eq!(resolved.path, compact_file);
    }
}
