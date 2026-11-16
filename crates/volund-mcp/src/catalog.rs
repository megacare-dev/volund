//! How the task's MCP tools are offered to the model.
//!
//! A small catalog (all functions together at most [`MAX_FUNCTIONS`], and at most
//! [`DIRECT_SCHEMA_CHARS`] of schema JSON) goes in as functions named `<server>__<tool>`. A larger
//! one would crowd every prompt, so the prompt lists the tool names and the model looks a tool up with
//! `mcp_find` and runs it with `mcp_call`.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::Server;

/// Most functions one request may offer (OpenAI's limit), the harness's own included.
pub const MAX_FUNCTIONS: usize = 128;
/// Most characters of MCP tool definitions offered directly.
pub const DIRECT_SCHEMA_CHARS: usize = 40_000;
const DESCRIPTION_CHARS: usize = 1_000;
const FIND_RESULTS: usize = 8;

struct Entry {
    server: usize,
    tool: usize,
    /// `<server>__<tool>`, made a valid and unique function name.
    name: String,
}

pub struct Catalog {
    pub servers: Vec<Server>,
    entries: Vec<Entry>,
    direct: bool,
}

impl Catalog {
    /// `reserved`: the harness's own function names, which MCP names must not take.
    pub fn new(servers: Vec<Server>, reserved: &[&str]) -> Self {
        let mut taken: HashSet<String> = reserved.iter().map(|s| (*s).to_owned()).collect();
        taken.extend(["mcp_find".to_owned(), "mcp_call".to_owned()]);
        let mut entries = Vec::new();
        for (s, server) in servers.iter().enumerate() {
            for (t, tool) in server.tools.iter().enumerate() {
                let wanted = format!(
                    "{}__{}",
                    sanitize(&server.name),
                    sanitize(tool["name"].as_str().unwrap_or("tool"))
                );
                let name = unique_name(&wanted, &taken);
                taken.insert(name.clone());
                entries.push(Entry {
                    server: s,
                    tool: t,
                    name,
                });
            }
        }
        let mut catalog = Self {
            servers,
            entries,
            direct: false,
        };
        let size = Value::Array(catalog.direct_functions())
            .to_string()
            .chars()
            .count();
        catalog.direct =
            catalog.entries.len() + reserved.len() <= MAX_FUNCTIONS && size <= DIRECT_SCHEMA_CHARS;
        catalog
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the tools are offered as functions (otherwise through `mcp_find`/`mcp_call`).
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The functions to offer next to the harness's own.
    pub fn functions(&self) -> Vec<Value> {
        if self.entries.is_empty() {
            Vec::new()
        } else if self.direct {
            self.direct_functions()
        } else {
            find_and_call()
        }
    }

    /// The part of the system prompt that introduces the task's tools.
    pub fn prompt(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\nThe task also runs its own applications, which you use through their tools. What you do \
             through them (send an email, close a case, file a document, ...) is part of the result: do \
             every action the task asks for, then read the application back to confirm it took effect.",
        );
        if self.direct {
            out.push_str(" Their functions are named <server>__<tool>.");
            return out;
        }
        out.push_str(
            " They are listed below by server. Call mcp_find to see a tool's description and parameters, then \
             mcp_call to run it.",
        );
        for (s, server) in self.servers.iter().enumerate() {
            let names: Vec<&str> = self
                .entries
                .iter()
                .filter(|e| e.server == s)
                .map(|e| e.name.as_str())
                .collect();
            if !names.is_empty() {
                out.push_str(&format!(
                    "\n{} ({} tools): {}",
                    server.name,
                    names.len(),
                    names.join(", ")
                ));
            }
        }
        out
    }

    /// The server and tool name behind a listed name (`<server>__<tool>`), or behind a bare tool name
    /// that only one server has.
    pub fn resolve(&self, name: &str) -> Option<(&Server, &str)> {
        let entry = self.entries.iter().find(|e| e.name == name).or_else(|| {
            let mut bare = self.entries.iter().filter(|e| self.tool_name(e) == name);
            match (bare.next(), bare.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        })?;
        Some((&self.servers[entry.server], self.tool_name(entry)))
    }

    /// The tools that best match `query`, with their descriptions and parameters.
    pub fn find(&self, query: &str) -> String {
        let query = query.trim().to_lowercase();
        let words: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 1)
            .collect();
        let mut scored: Vec<(usize, &Entry)> = self
            .entries
            .iter()
            .map(|e| {
                let name = e.name.to_lowercase();
                let tool = self.tool_name(e).to_lowercase();
                let description = self.tool(e)["description"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();
                let mut score = 0;
                if name == query || tool == query {
                    score += 1000;
                }
                for w in &words {
                    if name.contains(w) {
                        score += 10;
                    }
                    if description.contains(w) {
                        score += 2;
                    }
                }
                (score, e)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        if scored.is_empty() {
            let names: Vec<&str> = self
                .entries
                .iter()
                .take(60)
                .map(|e| e.name.as_str())
                .collect();
            return format!(
                "No tool matches {query:?}. Some of the tools: {}",
                names.join(", ")
            );
        }
        scored
            .iter()
            .take(FIND_RESULTS)
            .map(|(_, e)| {
                let tool = self.tool(e);
                let schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                format!(
                    "{}\n  {}\n  parameters: {}",
                    e.name,
                    clip(
                        tool["description"].as_str().unwrap_or("(no description)"),
                        600
                    ),
                    clip(&schema.to_string(), 3000)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn tool(&self, e: &Entry) -> &Value {
        &self.servers[e.server].tools[e.tool]
    }

    fn tool_name(&self, e: &Entry) -> &str {
        self.tool(e)["name"].as_str().unwrap_or("")
    }

    fn direct_functions(&self) -> Vec<Value> {
        self.entries
            .iter()
            .map(|e| {
                let tool = self.tool(e);
                let mut parameters = tool
                    .get("inputSchema")
                    .filter(|s| s.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                if parameters.get("type").is_none() {
                    parameters["type"] = json!("object");
                }
                json!({
                    "type": "function",
                    "function": {
                        "name": e.name,
                        "description": clip(tool["description"].as_str().unwrap_or(""), DESCRIPTION_CHARS),
                        "parameters": parameters,
                    },
                })
            })
            .collect()
    }
}

fn find_and_call() -> Vec<Value> {
    vec![
        json!({"type": "function", "function": {
            "name": "mcp_find",
            "description": "Show the description and parameters of the task's own tools that match a query.",
            "parameters": {"type": "object", "properties": {
                "query": {"type": "string", "description": "A tool name, or words describing what you need."}
            }, "required": ["query"]},
        }}),
        json!({"type": "function", "function": {
            "name": "mcp_call",
            "description": "Run one of the task's own tools.",
            "parameters": {"type": "object", "properties": {
                "tool": {"type": "string", "description": "The tool's name as listed (<server>__<tool>)."},
                "arguments": {"type": "object"}
            }, "required": ["tool", "arguments"]},
        }}),
    ]
}

/// A function-name-safe form: letters, digits, `_` and `-`.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "tool".to_owned() } else { s }
}

/// At most 64 characters, with `_2`, `_3`, ... added until it is not taken.
fn unique_name(name: &str, taken: &HashSet<String>) -> String {
    let head = |n: usize| name.chars().take(n).collect::<String>();
    let mut candidate = head(64);
    let mut n = 1;
    while taken.contains(&candidate) {
        n += 1;
        let suffix = format!("_{n}");
        candidate = format!("{}{suffix}", head(64 - suffix.len()));
    }
    candidate
}

fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    fn server(name: &str, tools: Vec<Value>) -> Server {
        Server {
            name: name.into(),
            url: format!("http://127.0.0.1:8000/{name}/"),
            tools,
            session: Mutex::new(None),
            version: Mutex::new(crate::PROTOCOL_VERSION.into()),
            next_id: AtomicU64::new(1),
        }
    }

    fn tool(name: &str, description: &str) -> Value {
        json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}})
    }

    const BUILTIN: &[&str] = &["bash", "read_file", "write_file", "edit_file"];

    #[test]
    fn a_small_catalog_is_offered_as_functions() {
        let c = Catalog::new(
            vec![server(
                "apps",
                vec![tool("secret_word", "Returns the secret word.")],
            )],
            BUILTIN,
        );
        assert!(c.is_direct());
        let f = c.functions();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["function"]["name"], "apps__secret_word");
        assert_eq!(
            f[0]["function"]["parameters"]["properties"]["id"]["type"],
            "string"
        );
        let (s, t) = c.resolve("apps__secret_word").unwrap();
        assert_eq!((s.name.as_str(), t), ("apps", "secret_word"));
        assert!(c.prompt().contains("<server>__<tool>"));
    }

    #[test]
    fn a_large_catalog_goes_through_find_and_call() {
        let tools: Vec<Value> = (0..200)
            .map(|i| tool(&format!("op_{i}"), "Does something with cases."))
            .chain([tool("close_case", "Close a support case by its id.")])
            .collect();
        let c = Catalog::new(vec![server("crm", tools)], BUILTIN);
        assert!(!c.is_direct());
        let names: Vec<_> = c
            .functions()
            .iter()
            .map(|f| f["function"]["name"].clone())
            .collect();
        assert_eq!(names, ["mcp_find", "mcp_call"]);
        assert!(c.prompt().contains("crm (201 tools): crm__op_0"));
        let found = c.find("close case");
        assert!(found.starts_with("crm__close_case"), "{found}");
        assert!(
            c.resolve("close_case").is_some(),
            "a bare name one server has"
        );
        assert!(c.find("zzz").starts_with("No tool matches"));
    }

    #[test]
    fn names_are_valid_unique_and_avoid_the_harness_tools() {
        let c = Catalog::new(
            vec![
                server(
                    "my apps",
                    vec![tool("send.email", ""), tool("send email", "")],
                ),
                server("x", vec![tool(&"long".repeat(30), "")]),
            ],
            BUILTIN,
        );
        let names: Vec<String> = c.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names[0], "my_apps__send_email");
        assert_eq!(names[1], "my_apps__send_email_2");
        assert_eq!(names[2].len(), 64);
        let c = Catalog::new(vec![server("bash", vec![])], BUILTIN);
        assert!(c.is_empty() && c.functions().is_empty() && c.prompt().is_empty());
    }
}
