//! Wire types for the PLE serve protocol, version 1 (`docs/protocol.md` in google-research/ple).
//!
//! Every frame is one UTF-8 JSON object of at most [`MAX_FRAME_BYTES`]. Requests carry an `id` the
//! harness picks and get exactly one [`Reply`]; one-way messages (`log`, `usage`, `done`, `error`)
//! get none and take no request slot.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PROTOCOL_VERSION: u32 = 1;
/// Largest frame either side may send.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Requests the evaluator runs at once; a 9th waits, and so does a `done` sent behind it.
pub const MAX_IN_FLIGHT: usize = 8;
/// Malformed frames tolerated per task; one more ends it.
pub const MAX_MALFORMED: u32 = 20;
/// Largest `write_file` payload and largest `read_file` result, in bytes.
pub const MAX_FILE_BYTES: usize = 10_000_000;
/// Longest request id, in characters.
pub const MAX_ID_CHARS: usize = 128;
/// Seconds the evaluator keeps the socket open after `cancel`.
pub const CANCEL_GRACE_SEC: u64 = 10;
/// Seconds within which the server must accept the WebSocket.
pub const ACCEPT_DEADLINE_SEC: u64 = 120;

/// Frames the evaluator sends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Task(Task),
    Result(Reply),
    Cancel {
        #[serde(default)]
        reason: Option<String>,
    },
}

/// The first frame of every connection.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Task {
    pub protocol: u32,
    pub session: String,
    pub instruction: String,
    /// Where shell commands start and relative paths resolve.
    pub workdir: String,
    #[serde(default)]
    pub limits: Limits,
    /// Usually empty; when set, the task's apps (documents, spreadsheets, email, CAD) as MCP servers.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Limits {
    /// Seconds the task had left when this message was sent; `None` when untimed.
    #[serde(default)]
    pub agent_timeout_sec: Option<u64>,
    #[serde(default)]
    pub exec_timeout_sec: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
    #[serde(default)]
    pub max_write_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub transport: Option<String>,
}

/// The evaluator's reply to one request. Its fields depend on the request (protocol.md §2), so they
/// stay JSON: `exit_code`/`output`/`truncated`/`timed_out` for `exec`, `content_b64`/`size` for
/// `read_file`, `status`/`headers`/`body_b64` for `http`, and `ok: false` with `error` on failure.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reply {
    pub id: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl Reply {
    /// `false` when the evaluator could not carry the request out. A command that ran and exited
    /// non-zero is still `ok`.
    pub fn ok(&self) -> bool {
        self.fields
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    pub fn error(&self) -> Option<&str> {
        self.fields.get("error").and_then(Value::as_str)
    }
}

/// Frames the harness sends.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    /// `bash -c` in the task container, in a fresh subshell (no `cd` or exports carry over).
    Exec {
        id: String,
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_sec: Option<u64>,
    },
    ReadFile {
        id: String,
        path: String,
    },
    WriteFile {
        id: String,
        path: String,
        content_b64: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        append: bool,
    },
    /// Only to one of the task's `mcp_servers` URLs.
    Http {
        id: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<Map<String, Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body_b64: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_sec: Option<u64>,
    },
    Log {
        text: String,
    },
    /// One per model call.
    Usage {
        model: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
    },
    /// Ends the task; graders read `final_message` next to the deliverables.
    Done {
        final_message: String,
    },
    /// Ends the task early; deliverables already written are still graded.
    Error {
        message: String,
    },
}

impl Outgoing {
    /// Requests take one of the [`MAX_IN_FLIGHT`] slots; one-way messages do not.
    pub fn is_request(&self) -> bool {
        matches!(
            self,
            Self::Exec { .. } | Self::ReadFile { .. } | Self::WriteFile { .. } | Self::Http { .. }
        )
    }

    pub fn to_frame(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_task_message_from_the_protocol_doc() {
        let frame = json!({
            "type": "task",
            "protocol": 1,
            "session": "3f2c9a1e0b7d4c55",
            "instruction": "<the task, verbatim>",
            "workdir": "/app",
            "limits": {
                "agent_timeout_sec": 21600,
                "exec_timeout_sec": 600,
                "max_output_bytes": 100000,
                "max_write_bytes": 10000000
            },
            "mcp_servers": [
                {"name": "apps", "url": "http://127.0.0.1:8000/mcp/", "transport": "streamable-http"}
            ]
        });
        let Incoming::Task(task) = serde_json::from_value(frame).unwrap() else {
            panic!("not a task");
        };
        assert_eq!(task.protocol, PROTOCOL_VERSION);
        assert_eq!(task.workdir, "/app");
        assert_eq!(task.limits.agent_timeout_sec, Some(21600));
        assert_eq!(task.mcp_servers[0].name, "apps");
    }

    #[test]
    fn untimed_task_and_empty_limits_parse() {
        let frame = r#"{"type":"task","protocol":1,"session":"s","instruction":"i","workdir":"/app",
                        "limits":{"agent_timeout_sec":null},"mcp_servers":[]}"#;
        let Incoming::Task(task) = serde_json::from_str(frame).unwrap() else {
            panic!("not a task");
        };
        assert_eq!(task.limits.agent_timeout_sec, None);
    }

    #[test]
    fn replies_keep_their_fields_and_report_failure() {
        let ran: Incoming = serde_json::from_value(json!({
            "type": "result", "id": "1", "exit_code": 2, "output": "boom", "truncated": false, "timed_out": false
        }))
        .unwrap();
        let Incoming::Result(reply) = ran else {
            panic!("not a result")
        };
        assert!(reply.ok(), "a non-zero exit is still a carried-out request");
        assert_eq!(reply.fields["exit_code"], 2);

        let failed: Incoming = serde_json::from_value(json!({
            "type": "result", "id": "2", "ok": false, "error": "no such file"
        }))
        .unwrap();
        let Incoming::Result(reply) = failed else {
            panic!("not a result")
        };
        assert!(!reply.ok());
        assert_eq!(reply.error(), Some("no such file"));
    }

    #[test]
    fn cancel_parses() {
        let cancel: Incoming =
            serde_json::from_str(r#"{"type":"cancel","reason":"timeout"}"#).unwrap();
        assert_eq!(
            cancel,
            Incoming::Cancel {
                reason: Some("timeout".into())
            }
        );
    }

    #[test]
    fn requests_serialize_as_the_protocol_doc_shows() {
        let exec = Outgoing::Exec {
            id: "1".into(),
            command: "ls -la".into(),
            timeout_sec: Some(120),
        };
        let frame: Value = serde_json::from_str(&exec.to_frame().unwrap()).unwrap();
        assert_eq!(
            frame,
            json!({"type": "exec", "id": "1", "command": "ls -la", "timeout_sec": 120})
        );

        let write = Outgoing::WriteFile {
            id: "3".into(),
            path: "/app/out/report.md".into(),
            content_b64: String::new(),
            append: false,
        };
        let frame: Value = serde_json::from_str(&write.to_frame().unwrap()).unwrap();
        assert_eq!(
            frame,
            json!({"type": "write_file", "id": "3", "path": "/app/out/report.md", "content_b64": ""})
        );
    }

    #[test]
    fn only_requests_take_a_slot() {
        assert!(
            Outgoing::ReadFile {
                id: "1".into(),
                path: "a".into()
            }
            .is_request()
        );
        assert!(
            !Outgoing::Done {
                final_message: "ok".into()
            }
            .is_request()
        );
    }
}
