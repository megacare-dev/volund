//! The single tool-calling loop, for the lower rungs of the ablation ladder (DESIGN.md §9); the
//! phase driver of DESIGN.md §4 works the task from A4 up.
//!
//! - A1 is meant to match the organisers' `custom-openai` sample (A0).
//! - A2 opens the conversation with what PROBE found: the input files and the container's tools.
//! - A3 also keeps the world: the model records the task with the `world_*` tools, the record is
//!   re-rendered when old turns are dropped, and a cancel sends the summary built from it.
//! - Keeps time for the final summary: three minutes, or a quarter of a short task's time. Once in
//!   that window it stops working and asks for the summary without tools.
//! - Tracks the best summary so far on the [`Link`], so a cancel still sends one.
//! - Trims tool output the model has already read when the prompt grows past `COMPACT_AT_TOKENS`
//!   or the endpoint says the conversation no longer fits.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use volund_llm::{Call, Client, Error, Reply, ToolChoice, visible};

use volund_mcp::Catalog;
use volund_world::World;

use crate::ablation::Level;
use crate::driver::{probe, world_tools};
use crate::link::Link;
use crate::{mcp, tools};

const FINISH_MARGIN: Duration = Duration::from_secs(180);
/// Tool results kept whole however long the conversation grows.
const KEEP_RECENT_RESULTS: usize = 12;
/// What an older tool result keeps when trimmed.
const TRIMMED_KEEP_CHARS: usize = 600;
/// Consecutive replies with neither text nor tool calls before the run ends.
const MAX_NUDGES: u32 = 2;

const SYSTEM: &str = "You are an expert engineer completing a task inside a Linux container. Your tools \
act in that container: bash (each call is a new shell that starts in {workdir}), read_file, write_file \
and edit_file. The container has no internet access.{record}

How to work:
- Read the whole task first, then look at the files you were given before choosing an approach.
- Take every input value from the task or its files and know where each came from. Do not invent \
inputs; when you must assume something, say so.
- Compute numbers with code rather than by hand, and check units.
- Where the task says how the result is checked (a format, a limit, a test), check it that way yourself.
- Write each deliverable exactly where, under the name and in the format the task asks for. Write a \
first version early, then improve it.
- Make office and binary formats (docx, xlsx, pdf, ...) with code, so they are real files of that format.
- Keep scratch files out of the folders that hold deliverables.
- Before you finish, open every deliverable again and check that it is complete and readable.
- You have about {minutes} minutes in total; the time left is shown after the results of your tools.

When the work is done, answer without calling a tool: a short summary of what you delivered (the path \
of each file) and the key results. That answer is graded together with your files.";

/// A3: the world in the system prompt (A1 and A2 leave `{record}` empty).
const RECORD: &str = "

Keep a record of the task with the world_* tools: each deliverable (its exact path and what it must \
contain), the input values you rely on with their sources, the requirements with their computed values \
and verdicts, the actions to take in the task's applications, and your assumptions. When old turns are \
dropped to save room, the record is what survives.";

pub struct Agent {
    link: Arc<Link>,
    llm: Arc<Client>,
    level: Level,
    world: World,
    tools: Vec<Value>,
    catalog: Catalog,
    messages: Vec<Value>,
    /// Messages kept when old turns are dropped: the system prompt, the task, PROBE's findings.
    kept: usize,
    prompt_tokens: u64,
    compact_at: u64,
    finish_margin: Duration,
}

impl Agent {
    /// A loop for `level` (A1 to A3).
    pub fn new(link: Arc<Link>, llm: Arc<Client>, compact_at: u64, level: Level) -> Self {
        Self {
            world: World {
                workdir: link.task.workdir.clone(),
                ..World::default()
            },
            link,
            llm,
            level,
            tools: tools::definitions(),
            catalog: Catalog::new(Vec::new(), tools::NAMES),
            messages: Vec::new(),
            kept: 2,
            prompt_tokens: 0,
            compact_at,
            finish_margin: FINISH_MARGIN,
        }
    }

    /// Works on the task until the model answers with its summary, which is returned.
    pub async fn run(mut self) -> Result<String, String> {
        let left = self.link.time_left();
        self.finish_margin = left.map_or(FINISH_MARGIN, |l| FINISH_MARGIN.min(l / 4));
        let minutes = left.map_or_else(
            || "unlimited".to_owned(),
            |l| (l.as_secs() / 60).to_string(),
        );
        self.event(
            "start",
            json!({
                "level": self.level.name(),
                "model": self.llm.model(),
                "workdir": self.link.task.workdir,
                "timeout_sec": left.map(|d| d.as_secs()),
                "mcp_servers": self.link.task.mcp_servers.len(),
            }),
        );
        if self.level.probe() {
            let summary = probe::run(&self.link, &mut self.world).await;
            self.event("probe", json!({"summary": summary}));
        }
        let mut reserved = tools::NAMES.to_vec();
        if self.level.world() {
            reserved.extend(world_tools::ALL);
            self.tools
                .extend(world_tools::definitions(world_tools::ALL));
        }
        let (catalog, missing) = mcp::connect_all(&self.link, &reserved).await;
        self.catalog = catalog;
        self.tools.extend(self.catalog.functions());
        let mut system = SYSTEM
            .replace("{workdir}", &self.link.task.workdir)
            .replace("{minutes}", &minutes)
            .replace("{record}", if self.level.world() { RECORD } else { "" });
        system.push_str(&self.catalog.prompt());
        if !missing.is_empty() {
            system.push_str(&format!(
                "\n\nThe task's application server(s) {} did not answer when the run started.",
                missing.join(", ")
            ));
        }
        self.messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": self.link.task.instruction}),
        ];
        if self.level.probe() {
            self.messages
                .push(json!({"role": "user", "content": self.findings()}));
        }
        self.kept = self.messages.len();

        // `head`: the text of an answer cut off at the output limit, continued in the next turn
        let (mut nudges, mut head) = (0, String::new());
        loop {
            if self.link.is_cancelled() {
                // the task is over: no more model calls, the session sends the best summary
                return Ok(self.link.final_message());
            }
            if self.in_finish_window() {
                return Ok(self.finish("Time is almost up.").await);
            }
            let mut reply = match self.complete(ToolChoice::Auto, self.work_deadline()).await {
                Ok(reply) => reply,
                Err(Error::OutOfTime(_)) => {
                    return Ok(self
                        .finish("Time is almost up, and the model is not answering.")
                        .await);
                }
                Err(Error::CutOff { partial }) => {
                    self.link.set_final_message(&visible(&(head + &partial)));
                    return Ok(self.finish("Time is almost up.").await);
                }
                Err(Error::ContextFull(why)) => {
                    if self.shrink(true) {
                        continue;
                    }
                    return Err(format!("the conversation no longer fits the model: {why}"));
                }
                Err(e) => return Err(e.to_string()),
            };
            reply.finalize(self.messages.len());
            let arguments = reply.parse_calls();
            if !reply.text.is_empty() || !reply.calls.is_empty() {
                // a turn with neither is not sent back: endpoints refuse an assistant message with
                // null content and no tool calls
                self.messages.push(reply.message());
            }
            let text = visible(&format!("{head}{}", reply.text));
            self.link.set_final_message(&text);

            if reply.calls.is_empty() {
                let cut_off = reply.finish.as_deref() == Some("length");
                if cut_off && !visible(&reply.text).is_empty() {
                    nudges = 0;
                    head.push_str(&reply.text);
                    self.messages.push(json!({"role": "user", "content":
                        "Your reply was cut off at the output limit. Continue where it stopped, and put \
                         long content in files with write_file rather than in a reply."}));
                    continue;
                }
                if !visible(&reply.text).is_empty() || nudges >= MAX_NUDGES {
                    return Ok(if text.is_empty() {
                        self.link.final_message()
                    } else {
                        text
                    });
                }
                nudges += 1;
                self.messages.push(json!({"role": "user", "content":
                    "Continue the task; when it is done, answer with your summary of the deliverables."}));
                continue;
            }

            nudges = 0;
            head.clear();
            let cut_off = reply.finish.as_deref() == Some("length");
            for (call, args) in reply.calls.iter().zip(arguments) {
                let result = match args {
                    Ok(args) => {
                        let shown: String = Value::Object(args.clone())
                            .to_string()
                            .chars()
                            .take(400)
                            .collect();
                        self.event("tool", json!({"name": call.name, "args": shown}));
                        let recorded = if self.level.world() {
                            world_tools::apply(&mut self.world, &call.name, &args)
                        } else {
                            None
                        };
                        match recorded {
                            Some(result) => result,
                            None => match mcp::run(&self.link, &self.catalog, &call.name, &args)
                                .await
                            {
                                Some(result) => tools::clip(&result, tools::RESULT_CHARS),
                                None => {
                                    tools::run(&self.link, &call.name, &args, self.exec_cap()).await
                                }
                            },
                        }
                    }
                    // arguments cut off at the output limit are the usual cause
                    Err(e) if cut_off => {
                        format!("{e} Your reply hit the output limit; write large files in parts.")
                    }
                    Err(e) => e,
                };
                self.messages
                    .push(json!({"role": "tool", "tool_call_id": call.id, "content": result}));
            }
            if let (Some(left), Some(Value::Object(last))) =
                (self.link.time_left(), self.messages.last_mut())
                && let Some(Value::String(content)) = last.get_mut("content")
            {
                content.push_str(&format!("\n[{} minutes left]", left.as_secs() / 60));
            }
            if self.level.world() && !self.world.deliverables.is_empty() {
                // a cancel now sends what the record holds rather than the model's last remark
                self.link.set_final_message(&self.world.report());
            }
            if self.prompt_tokens > self.compact_at {
                self.shrink(false);
            }
        }
    }

    /// PROBE's findings, as the message after the task: with the world (A3), the record they start.
    fn findings(&self) -> String {
        let mut out = if self.level.world() {
            format!(
                "The record so far (the world_* tools add to it):\n{}",
                self.world.brief()
            )
        } else if self.world.environment.is_empty() {
            // the probe did not run, so it says nothing about the files either
            return "Before you started, the harness tried to look around the container, but the \
                    probe did not run.\n"
                .to_owned();
        } else {
            format!(
                "Before you started, the harness looked around the container.\nContainer: {}\n",
                self.world.environment.join("; ")
            )
        };
        if self.world.files.is_empty() {
            out.push_str("\nNo input files were found outside hidden folders.\n");
        } else {
            out.push('\n');
            out.push_str(&probe::input_files(&self.world));
        }
        out
    }

    /// Makes the conversation smaller: trims tool output the model has read (all of it when `all`),
    /// and with the world (A3), drops the turns after the opening and re-renders the record when
    /// trimming is not enough. Returns whether anything changed.
    fn shrink(&mut self, all: bool) -> bool {
        let trimmed = self.trim_read_results(all);
        if !self.level.world() || (trimmed && !all && self.prompt_tokens <= self.compact_at) {
            return trimmed;
        }
        if self.messages.len() <= self.kept + 1 {
            return trimmed;
        }
        self.messages.truncate(self.kept);
        self.messages.push(json!({"role": "user", "content": format!(
            "Earlier turns were dropped to save room. The record so far:\n{}\nContinue the task from \
             there; when it is done, answer with your summary of the deliverables.",
            self.world.brief()
        )}));
        self.prompt_tokens = estimate_tokens(&self.messages);
        self.event(
            "context",
            json!({"reopened": true, "tokens": self.prompt_tokens}),
        );
        true
    }

    /// One JSON line of the run's log; the loop is one phase, `LOOP`.
    fn event(&self, kind: &str, data: Value) {
        self.link.event("LOOP", kind, data);
    }

    /// Asks for the final summary without tools, keeping a little time in hand.
    async fn finish(&mut self, why: &str) -> String {
        if self.link.is_cancelled() {
            return self.link.final_message();
        }
        self.messages.push(json!({"role": "user", "content": format!(
            "{why} Stop working now and answer with your final summary: what you delivered, where, and \
             the key results."
        )}));
        let deadline = self.link.time_left().map(|left| {
            let keep = Duration::from_secs(30).min(left / 4);
            Instant::now() + left.saturating_sub(keep).max(Duration::from_secs(5))
        });
        let text = match self.complete(ToolChoice::None, deadline).await {
            Ok(reply) => visible(&reply.text),
            Err(Error::CutOff { partial }) => visible(&partial),
            Err(e) => {
                self.event("llm", json!({"error": e.to_string()}));
                String::new()
            }
        };
        if text.is_empty() {
            self.link.final_message()
        } else {
            text
        }
    }

    async fn complete(
        &mut self,
        tool_choice: ToolChoice,
        deadline: Option<Instant>,
    ) -> Result<Reply, Error> {
        let link = self.link.clone();
        let log = move |line: String| link.log(line);
        let reply = self
            .llm
            .complete(
                Call {
                    messages: &self.messages,
                    tools: &self.tools,
                    tool_choice,
                    deadline,
                },
                &log,
            )
            .await?;
        match &reply.usage {
            Some(usage) => {
                self.prompt_tokens = usage.input_tokens;
                self.link.usage(self.llm.model(), usage);
                self.event(
                    "llm",
                    json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cache_read_tokens": usage.cache_read_tokens,
                    }),
                );
            }
            None => {
                self.prompt_tokens = estimate_tokens(&self.messages);
                self.event("llm", json!({"usage": null}));
            }
        }
        Ok(reply)
    }

    fn in_finish_window(&self) -> bool {
        self.link
            .time_left()
            .is_some_and(|l| l < self.finish_margin)
    }

    /// Working calls must end before the time kept for the summary.
    fn work_deadline(&self) -> Option<Instant> {
        self.link.deadline().map(|d| d - self.finish_margin)
    }

    /// The longest a command may run: the task's limit, ended before the summary window.
    fn exec_cap(&self) -> Duration {
        let limit = Duration::from_secs(self.link.task.limits.exec_timeout_sec.unwrap_or(600));
        match self.link.time_left() {
            Some(left) => limit
                .min(left.saturating_sub(self.finish_margin))
                .max(Duration::from_secs(1)),
            None => limit,
        }
    }

    /// Shortens tool results the model has already read, oldest first, keeping the newest
    /// [`KEEP_RECENT_RESULTS`] whole (all of them when `all`). Returns whether anything changed.
    fn trim_read_results(&mut self, all: bool) -> bool {
        let Some(last_assistant) = self.messages.iter().rposition(|m| m["role"] == "assistant")
        else {
            return false;
        };
        let read: Vec<usize> = (0..last_assistant)
            .filter(|&i| self.messages[i]["role"] == "tool")
            .collect();
        let keep = if all { 0 } else { KEEP_RECENT_RESULTS };
        let mut changed = false;
        for &i in &read[..read.len().saturating_sub(keep)] {
            let Some(content) = self.messages[i]["content"].as_str() else {
                continue;
            };
            if content.chars().count() <= TRIMMED_KEEP_CHARS + 100 {
                continue;
            }
            let start: String = content.chars().take(TRIMMED_KEEP_CHARS).collect();
            self.messages[i]["content"] = json!(format!(
                "{start}\n[older output removed to save room; run the command again if you need it]"
            ));
            changed = true;
        }
        if changed {
            self.prompt_tokens = estimate_tokens(&self.messages);
            self.event(
                "context",
                json!({"trimmed": true, "tokens": self.prompt_tokens}),
            );
        }
        changed
    }
}

/// Four characters of JSON per token.
fn estimate_tokens(messages: &[Value]) -> u64 {
    messages
        .iter()
        .map(|m| m.to_string().len() as u64)
        .sum::<u64>()
        / 4
}
