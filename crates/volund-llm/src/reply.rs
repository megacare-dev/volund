//! Accumulates one streamed chat completion into an assistant message.
//!
//! Provider-specific fields stay JSON and go back with the message: Gemini refuses a tool call without
//! the thought signature that arrives in the call's `extra_content`, and Claude needs its signed
//! thinking blocks on the next turn. Mapping them into structs would drop them.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

/// Standard fields of a streamed delta, of a tool call and of its function; any other field an
/// endpoint sends is kept and returned with the message.
const DELTA_FIELDS: &[&str] = &[
    "role",
    "content",
    "tool_calls",
    "function_call",
    "refusal",
    "audio",
    "annotations",
    "reasoning_content",
    "reasoning",
];
const CALL_FIELDS: &[&str] = &["index", "id", "type", "function"];
const FUNCTION_FIELDS: &[&str] = &["name", "arguments"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The JSON text of the arguments, as streamed; canonical after [`Reply::parse_calls`].
    pub arguments: String,
    extra: Map<String, Value>,
    function_extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

impl Usage {
    fn from_json(usage: &Value) -> Self {
        let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
        let cached = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_u64))
            .unwrap_or(0);
        Self {
            input_tokens: n(usage.get("prompt_tokens")),
            output_tokens: n(usage.get("completion_tokens")),
            cache_read_tokens: cached,
        }
    }
}

#[derive(Debug, Default)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    /// `stop`, `tool_calls`, `length`, ...; `None` until the stream reports one.
    pub finish: Option<String>,
    extra: Map<String, Value>,
    by_index: HashMap<i64, usize>,
    by_id: HashMap<String, usize>,
    last: Option<usize>,
}

impl Reply {
    /// Adds one parsed stream chunk.
    pub fn add_chunk(&mut self, chunk: &Value) {
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(Usage::from_json(usage));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(finish.to_owned());
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
        }
        let reasoning = [delta.get("reasoning_content"), delta.get("reasoning")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|s| !s.is_empty());
        if let Some(reasoning) = reasoning {
            self.reasoning.push_str(reasoning);
        }
        merge_into(&mut self.extra, extra_fields(delta, DELTA_FIELDS));
        if let Some(parts) = delta.get("tool_calls").and_then(Value::as_array) {
            for part in parts {
                self.add_call(part);
            }
        }
    }

    /// Groups tool-call deltas by `index`, or by `id` when an endpoint omits or reuses `index`; a delta
    /// with neither continues the latest call unless it visibly starts another one.
    fn add_call(&mut self, part: &Value) {
        let Some(obj) = part.as_object() else {
            return;
        };
        let index = obj.get("index").and_then(Value::as_i64);
        let call_id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        let function = obj.get("function").and_then(Value::as_object);
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let mut key = index.and_then(|i| self.by_index.get(&i).copied());
        if let Some(k) = key
            && !call_id.is_empty()
            && !self.calls[k].id.is_empty()
            && self.calls[k].id != call_id
        {
            key = None;
        }
        if let Some(k) = key
            && call_id.is_empty()
            && !name.is_empty()
            && another_call(&self.calls[k], name)
        {
            key = None;
        }
        if key.is_none() && !call_id.is_empty() {
            key = self.by_id.get(call_id).copied();
        }
        if key.is_none()
            && index.is_none()
            && call_id.is_empty()
            && let Some(last) = self.last
            && !(!name.is_empty() && another_call(&self.calls[last], name))
        {
            key = Some(last);
        }
        let k = key.unwrap_or_else(|| {
            self.calls.push(ToolCall::default());
            self.calls.len() - 1
        });
        if let Some(i) = index {
            self.by_index.insert(i, k);
        }
        if !call_id.is_empty() {
            self.by_id.insert(call_id.to_owned(), k);
        }
        let call = &mut self.calls[k];
        if call.id.is_empty() {
            call.id = call_id.to_owned();
        }
        if call.name.is_empty() {
            call.name = name.to_owned();
        }
        if let Some(args) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            call.arguments.push_str(args);
        }
        merge_into(&mut call.extra, extra_fields(obj, CALL_FIELDS));
        if let Some(function) = function {
            merge_into(
                &mut call.function_extra,
                extra_fields(function, FUNCTION_FIELDS),
            );
        }
        self.last = Some(k);
    }

    /// Gives every call an id and a clean name once the stream has ended. `salt` keeps generated ids
    /// unique across turns.
    pub fn finalize(&mut self, salt: usize) {
        for (n, call) in self.calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{n}_{salt}");
            }
            // some open models leak chat-template tokens into the name
            if let Some((name, _)) = call.name.split_once("<|") {
                call.name = name.to_owned();
            }
            call.name = call.name.trim().to_owned();
        }
    }

    /// Parses each call's arguments into a JSON object, or an error for the model. Rewrites the stored
    /// arguments as canonical JSON (`{}` when invalid): endpoints refuse a conversation that holds
    /// malformed tool-call arguments.
    pub fn parse_calls(&mut self) -> Vec<Result<Map<String, Value>, String>> {
        self.calls
            .iter_mut()
            .map(|call| {
                let text = if call.arguments.trim().is_empty() {
                    "{}"
                } else {
                    call.arguments.as_str()
                };
                match serde_json::from_str::<Value>(text) {
                    Ok(Value::Object(args)) => {
                        call.arguments = Value::Object(args.clone()).to_string();
                        Ok(args)
                    }
                    Ok(_) => {
                        call.arguments = "{}".into();
                        Err("error: the arguments must be a JSON object; send the call again".into())
                    }
                    Err(e) => {
                        call.arguments = "{}".into();
                        Err(format!(
                            "error: the arguments are not a valid JSON object ({e}); send the call again"
                        ))
                    }
                }
            })
            .collect()
    }

    /// The assistant message for the conversation history. Unsigned thinking blocks are dropped:
    /// Claude refuses them on the next turn.
    pub fn message(&self) -> Value {
        let mut m = self.extra.clone();
        m.insert("role".into(), json!("assistant"));
        m.insert(
            "content".into(),
            if self.text.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !self.calls.is_empty() {
            let calls = self
                .calls
                .iter()
                .map(|c| {
                    let mut function = c.function_extra.clone();
                    function.insert("name".into(), json!(c.name));
                    function.insert("arguments".into(), json!(c.arguments));
                    let mut call = c.extra.clone();
                    call.insert("id".into(), json!(c.id));
                    call.insert("type".into(), json!("function"));
                    call.insert("function".into(), Value::Object(function));
                    Value::Object(call)
                })
                .collect();
            m.insert("tool_calls".into(), Value::Array(calls));
        }
        if !self.reasoning.is_empty() {
            m.insert("reasoning_content".into(), json!(self.reasoning));
        }
        drop_unsigned_thinking(&mut m);
        Value::Object(m)
    }
}

/// True when a nameless-id delta naming `name` must start a new call: the current call already has
/// another name, or its arguments already form a complete JSON object.
fn another_call(call: &ToolCall, name: &str) -> bool {
    if call.name.is_empty() {
        return false;
    }
    if name != call.name {
        return true;
    }
    matches!(
        serde_json::from_str::<Value>(&call.arguments),
        Ok(Value::Object(_))
    )
}

fn extra_fields(obj: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    obj.iter()
        .filter(|(k, v)| !known.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn merge_into(old: &mut Map<String, Value>, new: Map<String, Value>) {
    for (k, v) in new {
        let merged = merged(old.get(&k), v, &k);
        old.insert(k, merged);
    }
}

/// Merges one delta's value into what has accumulated: objects key by key, lists by concatenation,
/// anything else replaced. Claude's `thinking_blocks` join into whole blocks.
fn merged(old: Option<&Value>, new: Value, key: &str) -> Value {
    if key == "thinking_blocks"
        && let Value::Array(new_blocks) = &new
    {
        let old_blocks = old.and_then(Value::as_array).cloned().unwrap_or_default();
        return Value::Array(thinking_blocks(old_blocks, new_blocks));
    }
    match (old, new) {
        (Some(Value::Object(o)), Value::Object(n)) => {
            let mut out = o.clone();
            merge_into(&mut out, n);
            Value::Object(out)
        }
        (Some(Value::Array(o)), Value::Array(n)) => {
            let mut out = o.clone();
            out.extend(n);
            Value::Array(out)
        }
        (_, n) => n,
    }
}

/// Joins consecutive thinking deltas into the current block until a `signature` closes it; redacted
/// blocks (with `data`) arrive whole and stay separate.
fn thinking_blocks(mut out: Vec<Value>, new: &[Value]) -> Vec<Value> {
    for block in new {
        let Some(b) = block.as_object() else { continue };
        let starts_new = match out.last().and_then(Value::as_object) {
            None => true,
            Some(last) => {
                last.get("signature").is_some_and(truthy)
                    || last.get("data").is_some_and(truthy)
                    || b.get("data").is_some_and(truthy)
                    || last.get("type") != b.get("type")
            }
        };
        if starts_new {
            out.push(block.clone());
            continue;
        }
        let Some(Value::Object(last)) = out.last_mut() else {
            continue;
        };
        let joined = format!(
            "{}{}",
            last.get("thinking").and_then(Value::as_str).unwrap_or(""),
            b.get("thinking").and_then(Value::as_str).unwrap_or("")
        );
        last.insert("thinking".into(), json!(joined));
        if let Some(sig) = b.get("signature").filter(|s| truthy(s)) {
            last.insert("signature".into(), sig.clone());
        }
    }
    out
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(_) => true,
    }
}

fn signed(block: &Value) -> bool {
    block.get("signature").is_some_and(truthy) || block.get("data").is_some_and(truthy)
}

fn drop_unsigned_thinking(m: &mut Map<String, Value>) {
    if let Some(Value::Array(blocks)) = m.get_mut("thinking_blocks") {
        blocks.retain(signed);
    }
    if let Some(Value::Object(fields)) = m.get_mut("provider_specific_fields")
        && let Some(Value::Array(blocks)) = fields.get_mut("thinking_blocks")
    {
        blocks.retain(signed);
    }
}

/// The answer text without inline `<think>…</think>` reasoning, trimmed.
pub fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("</think>") {
            Some(end) => &rest[start + end + "</think>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(delta: Value) -> Value {
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
    }

    #[test]
    fn text_reasoning_usage_and_finish() {
        let mut r = Reply::default();
        r.add_chunk(&chunk(json!({"role": "assistant", "content": "Hel"})));
        r.add_chunk(&chunk(
            json!({"content": "lo", "reasoning_content": "think"}),
        ));
        r.add_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
        r.add_chunk(
            &json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 8}}}),
        );
        assert_eq!(r.text, "Hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.finish.as_deref(), Some("stop"));
        assert_eq!(
            r.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: 8
            })
        );
        let m = r.message();
        assert_eq!(m["content"], "Hello");
        assert_eq!(m["reasoning_content"], "think");
        assert!(m.get("tool_calls").is_none());
    }

    #[test]
    fn tool_calls_split_by_index_keep_gemini_signatures() {
        let mut r = Reply::default();
        r.add_chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "a", "type": "function",
            "function": {"name": "bash", "arguments": "{\"comm"}, "extra_content": {"google": {"thought_signature": "SIG"}}}]})));
        r.add_chunk(&chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "and\":\"ls\"}"}}]}),
        ));
        r.add_chunk(&chunk(json!({"tool_calls": [{"index": 1, "id": "b", "function": {"name": "read_file", "arguments": "{}"}}]})));
        r.finalize(0);
        let args = r.parse_calls();
        assert_eq!(r.calls.len(), 2);
        assert_eq!(args[0].as_ref().unwrap()["command"], "ls");
        let m = r.message();
        assert_eq!(m["content"], Value::Null);
        assert_eq!(m["tool_calls"][0]["id"], "a");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            m["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "SIG",
            "Gemini refuses the next turn without its thought signature"
        );
        assert_eq!(m["tool_calls"][1]["function"]["name"], "read_file");
    }

    #[test]
    fn calls_without_index_or_id_are_told_apart_by_complete_arguments() {
        let mut r = Reply::default();
        r.add_chunk(&chunk(
            json!({"tool_calls": [{"function": {"name": "bash", "arguments": "{\"command\":"}}]}),
        ));
        r.add_chunk(&chunk(
            json!({"tool_calls": [{"function": {"arguments": "\"ls\"}"}}]}),
        ));
        r.add_chunk(&chunk(json!({"tool_calls": [{"function": {"name": "bash", "arguments": "{\"command\":\"pwd\"}"}}]})));
        r.finalize(7);
        assert_eq!(r.calls.len(), 2);
        assert_eq!(r.calls[0].id, "call_0_7");
        assert_eq!(r.calls[1].arguments, "{\"command\":\"pwd\"}");
    }

    #[test]
    fn invalid_arguments_become_an_error_and_canonical_json() {
        let mut r = Reply::default();
        r.add_chunk(&chunk(json!({"tool_calls": [{"index": 0, "id": "x", "function": {"name": "bash<|call|>", "arguments": "{not json"}}]})));
        r.add_chunk(&chunk(json!({"tool_calls": [{"index": 1, "id": "y", "function": {"name": "bash", "arguments": ""}}]})));
        r.finalize(0);
        let args = r.parse_calls();
        assert!(args[0].is_err());
        assert_eq!(r.calls[0].arguments, "{}");
        assert_eq!(r.calls[0].name, "bash");
        assert!(
            args[1].as_ref().unwrap().is_empty(),
            "no arguments means {{}}"
        );
    }

    #[test]
    fn claude_thinking_blocks_join_and_unsigned_ones_drop() {
        let mut r = Reply::default();
        r.add_chunk(&chunk(
            json!({"thinking_blocks": [{"type": "thinking", "thinking": "a"}]}),
        ));
        r.add_chunk(&chunk(
            json!({"thinking_blocks": [{"type": "thinking", "thinking": "b", "signature": "S"}]}),
        ));
        r.add_chunk(&chunk(
            json!({"thinking_blocks": [{"type": "thinking", "thinking": "unsigned"}]}),
        ));
        r.add_chunk(&chunk(json!({"content": "ok"})));
        let m = r.message();
        assert_eq!(
            m["thinking_blocks"],
            json!([{"type": "thinking", "thinking": "ab", "signature": "S"}])
        );
    }

    #[test]
    fn visible_drops_think_blocks() {
        assert_eq!(visible("<think>x</think> Done. "), "Done.");
        assert_eq!(visible("A<think>open"), "A");
        assert_eq!(visible("plain"), "plain");
    }
}
