//! The model's tools. Each runs in the task container through the protocol and returns text for the
//! model; failures come back as `error: …` text rather than ending the run.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value, json};
use volund_proto::{MAX_FILE_BYTES, Outgoing, Reply};

use crate::link::{Link, RequestError};

/// Characters of one tool result the model sees; the middle of a longer one is cut.
pub const RESULT_CHARS: usize = 30_000;
const DEFAULT_EXEC_SEC: u64 = 120;

fn tool(name: &str, description: &str, required: &[&str], properties: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {"type": "object", "properties": properties, "required": required},
        },
    })
}

/// Function definitions offered to the model.
pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "bash",
            "Run a bash command in the task container and return its exit code and output. Each call \
             starts a new shell in the working directory: only files persist between calls.",
            &["command"],
            json!({
                "command": {"type": "string"},
                "timeout_sec": {"type": "integer", "description": "Stop the command after this many seconds (default 120)."},
            }),
        ),
        tool(
            "read_file",
            "Read a text file from the task container.",
            &["path"],
            json!({"path": {"type": "string"}}),
        ),
        tool(
            "write_file",
            "Create or overwrite a file in the task container; parent directories are created.",
            &["path", "content"],
            json!({
                "path": {"type": "string"},
                "content": {"type": "string"},
                "append": {"type": "boolean", "description": "Add to the end of the file instead (write a large file in parts)."},
            }),
        ),
        tool(
            "edit_file",
            "Replace exact text in a file. old_text must occur exactly once unless replace_all is true.",
            &["path", "old_text", "new_text"],
            json!({
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean"},
            }),
        ),
    ]
}

/// Runs one tool call. `exec_cap` is the longest a command may run (the task's limit, shortened so it
/// ends before the time kept for the final summary).
pub async fn run(link: &Link, name: &str, args: &Map<String, Value>, exec_cap: Duration) -> String {
    let result = match name {
        "bash" => bash(link, args, exec_cap).await,
        "read_file" => read_file(link, args).await,
        "write_file" => write_file(link, args).await,
        "edit_file" => edit_file(link, args).await,
        _ => Err(format!("there is no tool named {name}")),
    };
    clip(
        &result.unwrap_or_else(|e| format!("error: {e}")),
        RESULT_CHARS,
    )
}

fn text_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the argument {name} is missing or not a string"))
}

fn bool_arg(args: &Map<String, Value>, name: &str) -> bool {
    match args.get(name) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// A positive whole number of seconds from a model's argument, rounded up and capped; zero,
/// negative or unreadable values give `default` (a model's `0` for "no limit" must not become 1).
pub fn positive_secs(value: Option<&Value>, default: u64, cap: u64) -> u64 {
    let number = match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    let secs = if number > 0.0 && number.is_finite() {
        (number.ceil() as u64).max(1)
    } else {
        default
    };
    secs.min(cap).max(1)
}

fn checked(reply: Result<Reply, RequestError>) -> Result<Reply, String> {
    let reply = reply.map_err(|e| e.to_string())?;
    if !reply.ok() {
        return Err(reply.error().unwrap_or("the request failed").to_owned());
    }
    Ok(reply)
}

async fn bash(link: &Link, args: &Map<String, Value>, cap: Duration) -> Result<String, String> {
    let command = text_arg(args, "command")?;
    let cap = cap.as_secs().max(1);
    let timeout = positive_secs(args.get("timeout_sec"), DEFAULT_EXEC_SEC, cap);
    let reply = checked(
        link.request(|id| Outgoing::Exec {
            id,
            command: command.to_owned(),
            timeout_sec: Some(timeout),
        })
        .await,
    )?;
    let code = reply
        .fields
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let output = reply
        .fields
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let timed_out = reply.fields.get("timed_out").and_then(Value::as_bool) == Some(true);
    Ok(format!(
        "exit code {code}{}\n{}",
        if timed_out { " (timed out)" } else { "" },
        if output.is_empty() {
            "(no output)"
        } else {
            output
        }
    ))
}

async fn read_bytes(link: &Link, path: &str) -> Result<(Vec<u8>, bool), String> {
    let reply = checked(
        link.request(|id| Outgoing::ReadFile {
            id,
            path: path.to_owned(),
        })
        .await,
    )?;
    let b64 = reply
        .fields
        .get("content_b64")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bytes = B64
        .decode(b64)
        .map_err(|e| format!("the file came back undecodable: {e}"))?;
    let truncated = reply.fields.get("truncated").and_then(Value::as_bool) == Some(true);
    Ok((bytes, truncated))
}

async fn read_file(link: &Link, args: &Map<String, Value>) -> Result<String, String> {
    let path = text_arg(args, "path")?;
    let (bytes, truncated) = read_bytes(link, path).await?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(format!(
            "{path} is a binary file ({} bytes); inspect it with bash (python3, unzip -l, ...)",
            bytes.len()
        ));
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.is_empty() {
        text = "(empty file)".into();
    }
    if truncated {
        text.push_str("\n[the file is larger than one read returns; only its start is shown]");
    }
    Ok(text)
}

/// Writes `bytes`, in parts when they exceed what one request carries.
async fn write_bytes(link: &Link, path: &str, bytes: &[u8], append: bool) -> Result<u64, String> {
    // an empty write still creates (or empties) the file
    let parts: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(MAX_FILE_BYTES).collect()
    };
    let mut size = 0;
    for (i, part) in parts.into_iter().enumerate() {
        let reply = checked(
            link.request(|id| Outgoing::WriteFile {
                id,
                path: path.to_owned(),
                content_b64: B64.encode(part),
                append: append || i > 0,
            })
            .await,
        )?;
        size = reply
            .fields
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    Ok(size)
}

async fn write_file(link: &Link, args: &Map<String, Value>) -> Result<String, String> {
    let path = text_arg(args, "path")?;
    let content = text_arg(args, "content")?;
    let size = write_bytes(link, path, content.as_bytes(), bool_arg(args, "append")).await?;
    Ok(format!("{path} now holds {size} bytes"))
}

async fn edit_file(link: &Link, args: &Map<String, Value>) -> Result<String, String> {
    let path = text_arg(args, "path")?;
    let old = text_arg(args, "old_text")?;
    let new = text_arg(args, "new_text")?;
    let all = bool_arg(args, "replace_all");
    let (bytes, truncated) = read_bytes(link, path).await?;
    if truncated {
        return Err(format!("{path} is too large to edit this way; use bash"));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text; use bash"))?;
    let count = if old.is_empty() {
        0
    } else {
        text.matches(old).count()
    };
    if count == 0 {
        return Err(format!("old_text was not found in {path}"));
    }
    if count > 1 && !all {
        return Err(format!(
            "old_text occurs {count} times in {path}; add context to make it unique, or set replace_all"
        ));
    }
    let edited = if all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    write_bytes(link, path, edited.as_bytes(), false).await?;
    Ok(format!(
        "replaced {} occurrence(s) in {path}",
        if all { count } else { 1 }
    ))
}

/// Cuts the middle out of `text` beyond `limit` characters, keeping two thirds of it from the start.
pub fn clip(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_owned();
    }
    let head = limit * 2 / 3;
    let tail = limit - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(total - tail).collect();
    format!("{start}\n[... {} characters cut ...]\n{end}", total - limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_from_model_arguments() {
        assert_eq!(positive_secs(None, 120, 600), 120);
        assert_eq!(
            positive_secs(Some(&json!(0)), 120, 600),
            120,
            "0 is not 'no limit'"
        );
        assert_eq!(positive_secs(Some(&json!(-5)), 120, 600), 120);
        assert_eq!(positive_secs(Some(&json!(2.1)), 120, 600), 3);
        assert_eq!(positive_secs(Some(&json!("30")), 120, 600), 30);
        assert_eq!(positive_secs(Some(&json!(9000)), 120, 600), 600);
        assert_eq!(positive_secs(Some(&json!(true)), 120, 50), 50);
    }

    #[test]
    fn clipping_keeps_head_and_tail() {
        assert_eq!(clip("short", 10), "short");
        let long = "a".repeat(20) + &"b".repeat(10);
        let c = clip(&long, 15);
        assert!(c.starts_with(&"a".repeat(10)), "{c}");
        assert!(c.ends_with(&"b".repeat(5)), "{c}");
        assert!(c.contains("15 characters cut"));
        let thai = "ก".repeat(40);
        assert_eq!(clip(&thai, 30).chars().filter(|&c| c == 'ก').count(), 30);
    }

    #[test]
    fn every_tool_has_a_name_and_required_fields() {
        let names: Vec<_> = definitions()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["bash", "read_file", "write_file", "edit_file"]);
    }
}
