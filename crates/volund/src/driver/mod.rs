//! The phase driver (DESIGN.md §4): the state machine in `volund-fsm` decides which phase runs next;
//! the driver runs it, carries out the transition's effects, and keeps the world up to date.
//!
//! - PROBE and CHECK are scripted; INTAKE, MODEL, SOLVE, WRITE and FIX share one conversation;
//!   VERIFY starts a fresh one, so the model does not grade its own reasoning.
//! - Each phase has a share of the task's time. When it is spent the run moves on, whatever state
//!   the phase is in; cancel or the wrap-up time sends it to FINALIZE.
//! - Before a FIX the deliverables are copied to /tmp/volund/snapshot and the world cloned; if the
//!   fix lowers [`World::quality`], both are restored (the Δ≥0 commit gate).
//! - The best summary so far, built from the world, is kept on the link after every phase, so a
//!   cancel always has something to send.

mod check;
mod probe;
mod prompts;
mod world_tools;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::time::Instant;
use volund_fsm::{Effect, Event, Phase, Progress, next};
use volund_llm::{Call, Client, Error, Reply, ToolChoice, visible};
use volund_mcp::Catalog;
use volund_proto::Outgoing;
use volund_world::World;

use crate::link::Link;
use crate::{mcp, tools};

/// Time kept for FINALIZE (or a quarter of a short task's time).
const WRAP_UP: Duration = Duration::from_secs(240);
/// The time budgets are shares of when the task is untimed.
const NOMINAL_TOTAL: Duration = Duration::from_secs(6 * 3600);
const MAX_FIX_ROUNDS: u32 = 2;
/// A run never takes more transitions than this; past it, FINALIZE.
const MAX_TRANSITIONS: usize = 60;
/// A phase with less time left than this is skipped.
const MIN_PHASE_TIME: Duration = Duration::from_secs(2);
/// Replies with neither text nor tool calls before a phase is ended.
const MAX_NUDGES: u32 = 2;
const KEEP_RECENT_RESULTS: usize = 12;
const TRIMMED_KEEP_CHARS: usize = 600;
const FILES_SHOWN: usize = 150;
const SNAPSHOT_DIR: &str = "/tmp/volund/snapshot";
const MODEL_TESTS: &str =
    "cd /tmp/volund/model 2>/dev/null && timeout 300 python3 tests.py 2>&1 | tail -n 40";

/// The phase's share of the task's time. FIX's share covers all its rounds.
fn share(phase: Phase, needs_model: bool) -> f64 {
    match phase {
        Phase::Intake => 0.10,
        Phase::Model => 0.20,
        Phase::Solve => 0.15,
        // a task with nothing to model writes with MODEL's and SOLVE's time too
        Phase::Write if needs_model => 0.20,
        Phase::Write => 0.55,
        Phase::Verify => 0.10,
        Phase::Fix => 0.15,
        _ => 0.0,
    }
}

struct Snapshot {
    world: World,
    quality: f64,
    /// Where the deliverables were copied; `None` when copying failed (no rollback of files then).
    dir: Option<String>,
}

pub struct Driver {
    link: Arc<Link>,
    llm: Arc<Client>,
    compact_at: u64,
    world: World,
    catalog: Catalog,
    system: String,
    /// The working conversation (every phase but VERIFY).
    messages: Vec<Value>,
    total: Duration,
    wrap_up: Duration,
    spent: HashMap<Phase, Duration>,
    fix_rounds: u32,
    snapshot: Option<Snapshot>,
    prompt_tokens: u64,
    started: Instant,
    /// Why the run is in the current phase, shown in its opening message.
    reason: &'static str,
    test_output: String,
    /// Set when the model refuses outright (bad key, unknown model): no more model calls.
    model_failed: Option<String>,
}

impl Driver {
    pub fn new(link: Arc<Link>, llm: Arc<Client>, compact_at: u64) -> Self {
        let total = link.time_left().unwrap_or(NOMINAL_TOTAL);
        Self {
            catalog: Catalog::new(Vec::new(), tools::NAMES),
            world: World {
                workdir: link.task.workdir.clone(),
                ..World::default()
            },
            link,
            llm,
            compact_at,
            system: String::new(),
            messages: Vec::new(),
            wrap_up: WRAP_UP.min(total / 4),
            total,
            spent: HashMap::new(),
            fix_rounds: 0,
            snapshot: None,
            prompt_tokens: 0,
            started: Instant::now(),
            reason: "the run started",
            test_output: String::new(),
            model_failed: None,
        }
    }

    /// Runs the phases until FINALIZE, and returns the final message.
    pub async fn run(mut self) -> Result<String, String> {
        let mut phase = Phase::Probe;
        for _ in 0..MAX_TRANSITIONS {
            if matches!(phase, Phase::Finalize | Phase::Done) {
                break;
            }
            let began = Instant::now();
            let event = if self.link.is_cancelled() {
                Event::Cancelled
            } else {
                match phase {
                    Phase::Probe => self.probe().await,
                    Phase::Check => self.check().await,
                    Phase::Verify => self.verify().await,
                    working => self.work(working).await,
                }
            };
            *self.spent.entry(phase).or_default() += began.elapsed();
            let event = if self.link.is_cancelled() {
                Event::Cancelled
            } else {
                event
            };
            self.link.set_final_message(&self.world.report());
            let t = next(phase, event, &self.world, &self.progress());
            self.log(
                phase,
                "transition",
                json!({"event": event, "to": prompts::name(t.to), "reason": t.reason, "effects": t.effects}),
            );
            for effect in &t.effects {
                self.apply(*effect).await;
            }
            self.reason = t.reason;
            phase = t.to;
        }
        let message = self.finalize().await;
        self.link.set_final_message(&message);
        Ok(message)
    }

    fn progress(&self) -> Progress {
        Progress {
            time_left_sec: self.link.time_left().map(|d| d.as_secs()),
            wrap_up_sec: self.wrap_up.as_secs(),
            fix_rounds: self.fix_rounds,
            max_fix_rounds: MAX_FIX_ROUNDS,
            quality_at_snapshot: self.snapshot.as_ref().map(|s| s.quality),
        }
    }

    async fn probe(&mut self) -> Event {
        let summary = probe::run(&self.link, &mut self.world).await;
        self.log(Phase::Probe, "probe", json!({"summary": summary}));
        let reserved: Vec<&str> = tools::NAMES
            .iter()
            .chain(world_tools::ALL)
            .copied()
            .collect();
        let (catalog, missing) = mcp::connect_all(&self.link, &reserved).await;
        self.catalog = catalog;
        if !missing.is_empty() {
            self.world.environment.push(format!(
                "application server(s) not answering: {}",
                missing.join(", ")
            ));
        }
        self.system =
            prompts::SYSTEM.replace("{workdir}", &self.link.task.workdir) + &self.catalog.prompt();
        self.messages = vec![
            json!({"role": "system", "content": self.system}),
            json!({"role": "user", "content": self.link.task.instruction}),
        ];
        Event::PhaseEnded
    }

    async fn check(&mut self) -> Event {
        check::run(&self.link, &mut self.world).await;
        self.log(Phase::Check, "check", json!({"checks": self.world.checks}));
        Event::PhaseEnded
    }

    /// A phase of the working conversation.
    async fn work(&mut self, phase: Phase) -> Event {
        if self.model_failed.is_some() {
            return Event::BudgetSpent;
        }
        let deadline = self.phase_deadline(phase);
        if deadline.saturating_duration_since(Instant::now()) < MIN_PHASE_TIME {
            return Event::BudgetSpent;
        }
        let opening = self.opening(phase, deadline);
        let mut messages = std::mem::take(&mut self.messages);
        messages.push(json!({"role": "user", "content": opening}));
        let tools = self.tools_for(phase);
        let event = self.converse(phase, &mut messages, &tools, deadline).await;
        self.messages = messages;
        if phase == Phase::Model && !self.link.is_cancelled() {
            self.run_model_tests().await;
        }
        event
    }

    /// VERIFY: a fresh conversation that reports issues and changes nothing.
    async fn verify(&mut self) -> Event {
        if self.model_failed.is_some() {
            return Event::BudgetSpent;
        }
        let deadline = self.phase_deadline(Phase::Verify);
        if deadline.saturating_duration_since(Instant::now()) < MIN_PHASE_TIME {
            return Event::BudgetSpent;
        }
        let system = prompts::VERIFY_SYSTEM.replace("{workdir}", &self.link.task.workdir)
            + &self.catalog.prompt();
        let mut messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": self.link.task.instruction}),
            json!({"role": "user", "content": self.opening(Phase::Verify, deadline)}),
        ];
        let tools = self.tools_for(Phase::Verify);
        self.converse(Phase::Verify, &mut messages, &tools, deadline)
            .await
    }

    /// The model works until it answers in words (the phase is done), the phase's time is spent, or
    /// the task is cancelled.
    async fn converse(
        &mut self,
        phase: Phase,
        messages: &mut Vec<Value>,
        tools: &[Value],
        deadline: Instant,
    ) -> Event {
        let offered: Vec<String> = tools
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
            .collect();
        let mut nudges = 0;
        loop {
            if self.link.is_cancelled() {
                return Event::Cancelled;
            }
            if deadline.saturating_duration_since(Instant::now()) < MIN_PHASE_TIME {
                return Event::BudgetSpent;
            }
            let mut reply = match self
                .complete(messages, tools, ToolChoice::Auto, Some(deadline))
                .await
            {
                Ok(reply) => reply,
                Err(Error::CutOff { .. } | Error::OutOfTime(_)) => return Event::BudgetSpent,
                Err(Error::ContextFull(why)) => {
                    if self.shrink(phase, messages, deadline, true) {
                        continue;
                    }
                    self.log(phase, "llm", json!({"error": why}));
                    return Event::BudgetSpent;
                }
                Err(e) => {
                    self.log(phase, "llm", json!({"error": e.to_string()}));
                    self.model_failed = Some(e.to_string());
                    return Event::BudgetSpent;
                }
            };
            reply.finalize(messages.len());
            let arguments = reply.parse_calls();
            if !reply.text.is_empty() || !reply.calls.is_empty() {
                messages.push(reply.message());
            }
            if reply.calls.is_empty() {
                if !visible(&reply.text).is_empty() || nudges >= MAX_NUDGES {
                    return Event::PhaseEnded;
                }
                nudges += 1;
                messages.push(json!({"role": "user", "content":
                    "Continue this phase; when its work is done, say so in one or two sentences."}));
                continue;
            }
            nudges = 0;
            let cut_off = reply.finish.as_deref() == Some("length");
            for (call, args) in reply.calls.iter().zip(arguments) {
                let result = match args {
                    Ok(_) if !offered.contains(&call.name) => {
                        format!("error: {} is not available in this phase", call.name)
                    }
                    Ok(args) => self.run_tool(phase, &call.name, &args, deadline).await,
                    Err(e) if cut_off => {
                        format!("{e} Your reply hit the output limit; write large files in parts.")
                    }
                    Err(e) => e,
                };
                messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": result}));
            }
            let minutes = deadline.saturating_duration_since(Instant::now()).as_secs() / 60;
            if let Some(Value::Object(last)) = messages.last_mut()
                && let Some(Value::String(content)) = last.get_mut("content")
            {
                content.push_str(&format!("\n[{minutes} minutes left in this phase]"));
            }
            if self.prompt_tokens > self.compact_at {
                self.shrink(phase, messages, deadline, false);
            }
        }
    }

    async fn run_tool(
        &mut self,
        phase: Phase,
        name: &str,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> String {
        let shown: String = Value::Object(args.clone())
            .to_string()
            .chars()
            .take(300)
            .collect();
        self.log(phase, "tool", json!({"name": name, "args": shown}));
        if let Some(result) = world_tools::apply(&mut self.world, name, args) {
            return result;
        }
        if let Some(result) = mcp::run(&self.link, &self.catalog, name, args).await {
            return tools::clip(&result, tools::RESULT_CHARS);
        }
        let limit = Duration::from_secs(self.link.task.limits.exec_timeout_sec.unwrap_or(600));
        let cap = limit
            .min(deadline.saturating_duration_since(Instant::now()))
            .max(Duration::from_secs(1));
        tools::run(&self.link, name, args, cap).await
    }

    async fn complete(
        &mut self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        deadline: Option<Instant>,
    ) -> Result<Reply, Error> {
        let link = self.link.clone();
        let log = move |line: String| link.log(line);
        let call = Call {
            messages,
            tools,
            tool_choice,
            deadline,
        };
        let reply = self.llm.complete(call, &log).await?;
        match &reply.usage {
            Some(usage) => {
                self.prompt_tokens = usage.input_tokens;
                self.link.usage(self.llm.model(), usage);
            }
            None => self.prompt_tokens = estimate_tokens(messages),
        }
        Ok(reply)
    }

    /// Makes the conversation smaller: first by trimming tool output the model has read (all of it
    /// when `all`), then, if that is not enough, by dropping every turn after the task and opening the
    /// phase again from the world. Returns whether anything changed.
    fn shrink(
        &mut self,
        phase: Phase,
        messages: &mut Vec<Value>,
        deadline: Instant,
        all: bool,
    ) -> bool {
        let trimmed = trim_read_results(messages, if all { 0 } else { KEEP_RECENT_RESULTS });
        let size = estimate_tokens(messages);
        if trimmed && !all && size <= self.compact_at {
            self.prompt_tokens = size;
            self.log(phase, "context", json!({"trimmed": true, "tokens": size}));
            return true;
        }
        let head = messages.len().min(2);
        if messages.len() <= head + 1 {
            return trimmed;
        }
        messages.truncate(head);
        let reopened = format!(
            "{}\n\nEarlier turns were dropped to save room; the record above holds what was learned. Continue \
             this phase from there.",
            self.opening(phase, deadline)
        );
        messages.push(json!({"role": "user", "content": reopened}));
        self.prompt_tokens = estimate_tokens(messages);
        self.log(
            phase,
            "context",
            json!({"reopened": true, "tokens": self.prompt_tokens}),
        );
        true
    }

    fn tools_for(&self, phase: Phase) -> Vec<Value> {
        let mut out = tools::definitions();
        let world: &[&str] = match phase {
            Phase::Intake => world_tools::RECORD,
            Phase::Model | Phase::Solve | Phase::Write => &world_tools::RECORD_AND_VERDICT,
            Phase::Fix => world_tools::ALL,
            Phase::Verify => &[world_tools::ISSUE, world_tools::VERDICT],
            _ => &[],
        };
        if phase == Phase::Verify {
            out.retain(|t| matches!(t["function"]["name"].as_str(), Some("bash" | "read_file")));
        }
        out.extend(world_tools::definitions(world));
        out.extend(self.catalog.functions());
        out
    }

    /// When the phase must end: its share of the task's time (less what it already used), and never
    /// past the time kept for FINALIZE.
    fn phase_deadline(&self, phase: Phase) -> Instant {
        let budget = self.total.mul_f64(share(phase, self.world.needs_model()));
        let left = budget.saturating_sub(self.spent.get(&phase).copied().unwrap_or_default());
        let mut deadline = Instant::now() + left;
        if let Some(end) = self
            .link
            .deadline()
            .and_then(|d| d.checked_sub(self.wrap_up))
        {
            deadline = deadline.min(end);
        }
        deadline
    }

    /// The message that opens a phase: what to do, why the run is here, the time, and the record.
    fn opening(&self, phase: Phase, deadline: Instant) -> String {
        let mut out = format!(
            "Phase: {}\n{}\n",
            prompts::name(phase),
            prompts::instructions(phase)
        );
        out.push_str(&format!(
            "\n(The harness is here because: {}.)\n",
            self.reason
        ));
        let phase_min = deadline.saturating_duration_since(Instant::now()).as_secs() / 60;
        match self.link.time_left() {
            Some(left) => out.push_str(&format!(
                "Time: about {phase_min} minutes for this phase, {} for the whole task.\n",
                left.as_secs() / 60
            )),
            None => out.push_str(&format!(
                "Time: about {phase_min} minutes for this phase.\n"
            )),
        }
        out.push_str("\nThe record so far:\n");
        out.push_str(&self.world.brief());
        if phase == Phase::Intake && !self.world.files.is_empty() {
            out.push_str("\nInput files:\n");
            for f in self.world.files.iter().take(FILES_SHOWN) {
                out.push_str(&format!("- {} ({} bytes)\n", f.path, f.bytes));
            }
            if self.world.files.len() > FILES_SHOWN {
                out.push_str(&format!(
                    "- ... and {} more\n",
                    self.world.files.len() - FILES_SHOWN
                ));
            }
        }
        if phase == Phase::Model && !self.test_output.is_empty() {
            out.push_str(&format!("\nLast run of tests.py:\n{}\n", self.test_output));
        }
        if matches!(phase, Phase::Fix | Phase::Verify) {
            let failed: Vec<String> = self
                .world
                .checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| format!("- {}: {}", c.name, c.detail))
                .collect();
            if !failed.is_empty() {
                out.push_str(&format!("\nFailed checks:\n{}\n", failed.join("\n")));
            }
        }
        out
    }

    /// Runs the model's tests itself and records the result: the model's word is not the result.
    async fn run_model_tests(&mut self) {
        let reply = self
            .link
            .request(|id| Outgoing::Exec {
                id,
                command: MODEL_TESTS.to_owned(),
                timeout_sec: Some(330),
            })
            .await;
        let output = match reply {
            Ok(r) => r
                .fields
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Err(_) => return,
        };
        if let Some((total, passed)) = test_counts(&output) {
            self.world.model.tests_total = total;
            self.world.model.tests_passed = passed;
        }
        self.test_output = tools::clip(&output, 3000);
        self.log(
            Phase::Model,
            "model_tests",
            json!({"total": self.world.model.tests_total, "passed": self.world.model.tests_passed}),
        );
    }

    async fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Snapshot => {
                self.fix_rounds += 1;
                let dir = self.copy_deliverables().await;
                self.snapshot = Some(Snapshot {
                    world: self.world.clone(),
                    quality: self.world.quality(),
                    dir,
                });
            }
            Effect::Commit => self.snapshot = None,
            Effect::Rollback => {
                if let Some(snapshot) = self.snapshot.take() {
                    if let Some(dir) = &snapshot.dir {
                        let restored = self
                            .exec_ok(&format!("cp -a '{dir}'/. / && echo restored"))
                            .await;
                        self.log(
                            Phase::Check,
                            "rollback",
                            json!({"files_restored": restored}),
                        );
                    }
                    self.world = snapshot.world;
                }
            }
        }
    }

    /// Copies the deliverables (with their full paths) under the snapshot directory.
    async fn copy_deliverables(&self) -> Option<String> {
        let dir = format!("{SNAPSHOT_DIR}/{}", self.fix_rounds);
        let paths: Vec<String> = self
            .world
            .deliverables
            .iter()
            .map(|d| {
                let p = if d.path.starts_with('/') {
                    d.path.trim_end_matches('/').to_owned()
                } else {
                    format!(
                        "{}/{}",
                        self.link.task.workdir.trim_end_matches('/'),
                        d.path.trim_end_matches('/')
                    )
                };
                format!("'{}'", p.replace('\'', r"'\''"))
            })
            .collect();
        let command = format!(
            "rm -rf '{dir}' && mkdir -p '{dir}' && for p in {}; do if [ -e \"$p\" ]; then cp -a --parents \"$p\" '{dir}'/ || exit 1; fi; done && echo copied",
            if paths.is_empty() {
                "''".to_owned()
            } else {
                paths.join(" ")
            }
        );
        self.exec_ok(&command).await.then_some(dir)
    }

    /// Runs a scripted command; true when it printed its last word as the final line.
    async fn exec_ok(&self, command: &str) -> bool {
        let word = command
            .rsplit("echo ")
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        match self
            .link
            .request(|id| Outgoing::Exec {
                id,
                command: command.to_owned(),
                timeout_sec: Some(120),
            })
            .await
        {
            Ok(r) => {
                r.fields.get("exit_code").and_then(Value::as_i64) == Some(0)
                    && r.fields
                        .get("output")
                        .and_then(Value::as_str)
                        .is_some_and(|o| o.trim_end().ends_with(&word))
            }
            Err(_) => false,
        }
    }

    /// The final message: the model's summary when there is time and a model to write it, else the
    /// world's report.
    async fn finalize(&mut self) -> String {
        let report = self.world.report();
        let left = self.link.time_left();
        if self.link.is_cancelled()
            || self.model_failed.is_some()
            || self.messages.is_empty()
            || left.is_some_and(|l| l < Duration::from_secs(20))
        {
            return report;
        }
        let spend = left.map_or(Duration::from_secs(300), |l| {
            l.saturating_sub(Duration::from_secs(10))
                .min(Duration::from_secs(300))
        });
        let mut messages = std::mem::take(&mut self.messages);
        let opening = self.opening(Phase::Finalize, Instant::now() + spend);
        messages.push(json!({"role": "user", "content": opening}));
        let tools = self.tools_for(Phase::Write);
        let text = match self
            .complete(
                &messages,
                &tools,
                ToolChoice::None,
                Some(Instant::now() + spend),
            )
            .await
        {
            Ok(reply) => visible(&reply.text),
            Err(Error::CutOff { partial }) => visible(&partial),
            Err(e) => {
                self.log(Phase::Finalize, "llm", json!({"error": e.to_string()}));
                String::new()
            }
        };
        self.messages = messages;
        if text.is_empty() { report } else { text }
    }

    /// One JSON line per event in the run's log (DESIGN.md §9).
    fn log(&self, phase: Phase, kind: &str, data: Value) {
        let mut line = json!({"t": self.started.elapsed().as_secs(), "phase": prompts::name(phase), "kind": kind});
        if let (Value::Object(l), Value::Object(d)) = (&mut line, data) {
            l.extend(d);
        }
        self.link.log(line.to_string());
    }
}

/// The `{"total": N, "passed": M}` line tests.py ends with (the last such line).
fn test_counts(output: &str) -> Option<(u32, u32)> {
    output.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let total = u32::try_from(v.get("total")?.as_u64()?).ok()?;
        let passed = u32::try_from(v.get("passed")?.as_u64()?).ok()?;
        Some((total, passed.min(total)))
    })
}

/// Shortens tool results the model has already read, keeping the newest `keep` whole.
fn trim_read_results(messages: &mut [Value], keep: usize) -> bool {
    let Some(last_assistant) = messages.iter().rposition(|m| m["role"] == "assistant") else {
        return false;
    };
    let read: Vec<usize> = (0..last_assistant)
        .filter(|&i| messages[i]["role"] == "tool")
        .collect();
    let mut changed = false;
    for &i in &read[..read.len().saturating_sub(keep)] {
        let Some(content) = messages[i]["content"].as_str() else {
            continue;
        };
        if content.chars().count() <= TRIMMED_KEEP_CHARS + 100 {
            continue;
        }
        let start: String = content.chars().take(TRIMMED_KEEP_CHARS).collect();
        messages[i]["content"] = json!(format!("{start}\n[older output removed to save room]"));
        changed = true;
    }
    changed
}

fn estimate_tokens(messages: &[Value]) -> u64 {
    messages
        .iter()
        .map(|m| m.to_string().len() as u64)
        .sum::<u64>()
        / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counts_come_from_the_last_json_line() {
        assert_eq!(
            test_counts("ok\n{\"total\": 5, \"passed\": 4}\n"),
            Some((5, 4))
        );
        assert_eq!(test_counts("{\"total\": 2, \"passed\": 9}"), Some((2, 2)));
        assert_eq!(test_counts("Traceback ...\nNameError"), None);
    }

    #[test]
    fn writing_tasks_get_the_modelling_time() {
        let with: f64 = [
            Phase::Intake,
            Phase::Model,
            Phase::Solve,
            Phase::Write,
            Phase::Verify,
            Phase::Fix,
        ]
        .iter()
        .map(|&p| share(p, true))
        .sum();
        let without: f64 = [Phase::Intake, Phase::Write, Phase::Verify, Phase::Fix]
            .iter()
            .map(|&p| share(p, false))
            .sum();
        assert!((with - 0.9).abs() < 1e-9 && (without - 0.9).abs() < 1e-9);
    }

    #[test]
    fn trimming_keeps_unread_and_recent_results() {
        let long = "x".repeat(2000);
        let mut m = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "tool", "content": long}),
            json!({"role": "tool", "content": long}),
            json!({"role": "assistant", "content": "a"}),
            json!({"role": "tool", "content": long}),
        ];
        assert!(trim_read_results(&mut m, 1));
        assert!(
            m[1]["content"]
                .as_str()
                .unwrap()
                .ends_with("[older output removed to save room]")
        );
        assert_eq!(
            m[2]["content"].as_str().unwrap().len(),
            2000,
            "the newest read result stays"
        );
        assert_eq!(
            m[4]["content"].as_str().unwrap().len(),
            2000,
            "unread results stay"
        );
    }
}
