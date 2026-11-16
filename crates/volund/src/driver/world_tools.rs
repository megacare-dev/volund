//! The tools the model records the task with. The record is shown at the top of every phase and
//! survives when old turns are dropped; the state machine's guards and the final message read it.

use serde_json::{Map, Value, json};
use volund_world::{
    ActionStatus, AppAction, Constraint, Deliverable, DeliverableStatus, Issue, IssueKind, Param,
    Severity, SourceRef, Verdict, World,
};

fn function(name: &str, description: &str, required: &[&str], properties: Value) -> Value {
    json!({"type": "function", "function": {
        "name": name,
        "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required},
    }})
}

fn source_properties() -> Map<String, Value> {
    json!({
        "source_file": {"type": "string", "description": "The file (or app record) it comes from."},
        "page": {"type": "integer"},
        "locator": {"type": "string", "description": "Clause, table, cell, drawing callout, email id, ..."},
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn with_source(mut properties: Value) -> Value {
    if let Some(map) = properties.as_object_mut() {
        map.extend(source_properties());
    }
    properties
}

/// What INTAKE records.
pub const RECORD: &[&str] = &[
    "world_deliverable",
    "world_param",
    "world_constraint",
    "world_action",
    "world_assumption",
];
pub const VERDICT: &str = "world_verdict";
pub const ISSUE: &str = "world_issue";
pub const RECORD_AND_VERDICT: [&str; 6] = [
    "world_deliverable",
    "world_param",
    "world_constraint",
    "world_action",
    "world_assumption",
    VERDICT,
];
pub const ALL: &[&str] = &[
    "world_deliverable",
    "world_param",
    "world_constraint",
    "world_action",
    "world_assumption",
    VERDICT,
    ISSUE,
];

pub fn definitions(names: &[&str]) -> Vec<Value> {
    let all = [
        function(
            "world_deliverable",
            "Record a file the task asks you to deliver (or update its record).",
            &["path"],
            json!({
                "path": {"type": "string", "description": "Exact path the task gives; end with / for a whole directory."},
                "required": {"type": "array", "items": {"type": "string"}, "description": "What it must contain, as the task says."},
                "status": {"type": "string", "enum": ["draft", "written"]},
            }),
        ),
        function(
            "world_param",
            "Record an input value you rely on, with where it comes from.",
            &["name", "value", "source_file"],
            with_source(json!({
                "name": {"type": "string"},
                "value": {"type": "string", "description": "As written in the source."},
                "unit": {"type": "string"},
            })),
        ),
        function(
            "world_constraint",
            "Record a requirement that can be checked as a number or yes/no. Hard: breaking it makes the design \
             unsafe, infeasible or non-compliant, or the task says must. Soft: anything else.",
            &["id", "text", "severity", "source_file"],
            with_source(json!({
                "id": {"type": "string", "description": "Short id such as C1."},
                "text": {"type": "string"},
                "severity": {"type": "string", "enum": ["hard", "soft"]},
            })),
        ),
        function(
            VERDICT,
            "Record the value computed for a constraint and whether it passes.",
            &["id", "verdict"],
            json!({
                "id": {"type": "string"},
                "computed": {"type": "string", "description": "The value with its unit."},
                "verdict": {"type": "string", "enum": ["pass", "fail", "unchecked"]},
            }),
        ),
        function(
            "world_action",
            "Record an action the task asks you to take in its applications, or its progress. Mark it verified \
             only after reading the application back.",
            &["id"],
            json!({
                "id": {"type": "string"},
                "description": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "done", "verified"]},
            }),
        ),
        function(
            "world_assumption",
            "Record an assumption you had to make because the task or its files do not settle it.",
            &["text"],
            json!({"text": {"type": "string"}}),
        ),
        function(
            ISSUE,
            "Record a problem with the deliverables or the app actions, or close one that is fixed.",
            &["text"],
            json!({
                "id": {"type": "string", "description": "Leave out for a new issue; give it to update one."},
                "kind": {"type": "string", "enum": ["hard_constraint", "format", "content"]},
                "text": {"type": "string"},
                "open": {"type": "boolean", "description": "false once it is fixed."},
            }),
        ),
    ];
    all.into_iter()
        .filter(|d| names.contains(&d["function"]["name"].as_str().unwrap_or("")))
        .collect()
}

fn text<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn need<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    text(args, name).ok_or_else(|| format!("error: {name} is required"))
}

fn source(args: &Map<String, Value>) -> SourceRef {
    SourceRef {
        file: text(args, "source_file")
            .unwrap_or("(task text)")
            .to_owned(),
        page: args
            .get("page")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok()),
        locator: text(args, "locator").map(str::to_owned),
    }
}

/// Applies a world tool call; `None` when `name` is not one. The text tells the model what changed.
pub fn apply(world: &mut World, name: &str, args: &Map<String, Value>) -> Option<String> {
    let result = match name {
        "world_deliverable" => deliverable(world, args),
        "world_param" => param(world, args),
        "world_constraint" => constraint(world, args),
        VERDICT => verdict(world, args),
        "world_action" => action(world, args),
        "world_assumption" => need(args, "text").map(|t| {
            if !world.assumptions.iter().any(|a| a == t) {
                world.assumptions.push(t.to_owned());
            }
            format!("recorded; {} assumption(s)", world.assumptions.len())
        }),
        ISSUE => issue(world, args),
        _ => return None,
    };
    Some(result.unwrap_or_else(|e| e))
}

fn deliverable(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let path = need(args, "path")?;
    let index = match world.deliverables.iter().position(|d| d.path == path) {
        Some(i) => i,
        None => {
            world.deliverables.push(Deliverable::new(path));
            world.deliverables.len() - 1
        }
    };
    let d = &mut world.deliverables[index];
    if let Some(items) = args.get("required").and_then(Value::as_array) {
        d.required = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    match text(args, "status") {
        Some("draft") if d.status == DeliverableStatus::Missing => {
            d.status = DeliverableStatus::Draft
        }
        Some("written") if d.status != DeliverableStatus::Checked => {
            d.status = DeliverableStatus::Written
        }
        _ => {}
    }
    let status = d.status;
    Ok(format!(
        "recorded {path} ({status:?}); {} deliverable(s)",
        world.deliverables.len()
    ))
}

fn param(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let name = need(args, "name")?;
    let p = Param {
        name: name.to_owned(),
        value: need(args, "value")?.to_owned(),
        unit: text(args, "unit").map(str::to_owned),
        source: source(args),
    };
    match world.params.iter_mut().find(|q| q.name == name) {
        Some(existing) => *existing = p,
        None => world.params.push(p),
    }
    Ok(format!(
        "recorded {name}; {} parameter(s)",
        world.params.len()
    ))
}

fn constraint(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let id = need(args, "id")?;
    let severity = match need(args, "severity")? {
        "hard" => Severity::Hard,
        "soft" => Severity::Soft,
        other => return Err(format!("error: severity must be hard or soft, not {other}")),
    };
    let c = Constraint {
        id: id.to_owned(),
        text: need(args, "text")?.to_owned(),
        severity,
        source: source(args),
        computed: None,
        verdict: Verdict::Unchecked,
    };
    match world.constraints.iter_mut().find(|k| k.id == id) {
        Some(existing) => {
            let (computed, verdict) = (existing.computed.take(), existing.verdict);
            *existing = Constraint {
                computed,
                verdict,
                ..c
            };
        }
        None => world.constraints.push(c),
    }
    Ok(format!(
        "recorded {id}; {} constraint(s)",
        world.constraints.len()
    ))
}

fn verdict(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let id = need(args, "id")?;
    let verdict = match need(args, "verdict")? {
        "pass" => Verdict::Pass,
        "fail" => Verdict::Fail,
        "unchecked" => Verdict::Unchecked,
        other => {
            return Err(format!(
                "error: verdict must be pass, fail or unchecked, not {other}"
            ));
        }
    };
    let c = world
        .constraints
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| {
            format!("error: no constraint {id}; record it with world_constraint first")
        })?;
    c.verdict = verdict;
    if let Some(v) = text(args, "computed") {
        c.computed = Some(v.to_owned());
    }
    let passing = world
        .constraints
        .iter()
        .filter(|c| c.verdict == Verdict::Pass)
        .count();
    Ok(format!(
        "{id} {verdict}; {passing} of {} pass",
        world.constraints.len()
    ))
}

fn action(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let id = need(args, "id")?;
    let status = match text(args, "status") {
        None | Some("pending") => ActionStatus::Pending,
        Some("done") => ActionStatus::Done,
        Some("verified") => ActionStatus::Verified,
        Some(other) => {
            return Err(format!(
                "error: status must be pending, done or verified, not {other}"
            ));
        }
    };
    match world.actions.iter_mut().find(|a| a.id == id) {
        Some(a) => {
            if args.contains_key("status") {
                a.status = status;
            }
            if let Some(d) = text(args, "description") {
                a.description = d.to_owned();
            }
        }
        None => world.actions.push(AppAction {
            id: id.to_owned(),
            description: need(args, "description")?.to_owned(),
            status,
        }),
    }
    Ok(format!("{id} recorded; {} action(s)", world.actions.len()))
}

fn issue(world: &mut World, args: &Map<String, Value>) -> Result<String, String> {
    let open = args.get("open").and_then(Value::as_bool).unwrap_or(true);
    let kind = match text(args, "kind") {
        Some("hard_constraint") => Some(IssueKind::HardConstraint),
        Some("format") => Some(IssueKind::Format),
        Some("content") | None => None,
        Some(other) => {
            return Err(format!(
                "error: kind must be hard_constraint, format or content, not {other}"
            ));
        }
    };
    if let Some(id) = text(args, "id")
        && let Some(existing) = world.issues.iter_mut().find(|i| i.id == id)
    {
        existing.open = open;
        if let Some(k) = kind {
            existing.kind = k;
        }
        if let Some(t) = text(args, "text") {
            existing.text = t.to_owned();
        }
        return Ok(format!("{id} {}", if open { "open" } else { "closed" }));
    }
    let id = text(args, "id")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("I{}", world.issues.len() + 1));
    world.issues.push(Issue {
        id: id.clone(),
        kind: kind.unwrap_or(IssueKind::Content),
        text: need(args, "text")?.to_owned(),
        open,
    });
    let open_count = world.issues.iter().filter(|i| i.open).count();
    Ok(format!("recorded {id}; {open_count} open issue(s)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_model_builds_the_record() {
        let mut w = World::default();
        assert!(apply(&mut w, "bash", &Map::new()).is_none());
        apply(
            &mut w,
            "world_deliverable",
            &args(json!({"path": "/app/out/report.docx", "required": ["sizing table"]})),
        );
        apply(
            &mut w,
            "world_deliverable",
            &args(json!({"path": "/app/out/report.docx", "status": "written"})),
        );
        assert_eq!(w.deliverables.len(), 1);
        assert_eq!(w.deliverables[0].status, DeliverableStatus::Written);
        assert_eq!(w.deliverables[0].required, ["sizing table"]);

        let r = apply(
            &mut w,
            "world_constraint",
            &args(
                json!({"id": "C1", "text": "Vd ≤ 3 %", "severity": "hard", "source_file": "/app/spec.pdf", "page": 4}),
            ),
        );
        assert_eq!(r.unwrap(), "recorded C1; 1 constraint(s)");
        assert_eq!(w.constraints[0].source.to_string(), "/app/spec.pdf p.4");
        apply(
            &mut w,
            VERDICT,
            &args(json!({"id": "C1", "verdict": "pass", "computed": "2.1 %"})),
        );
        assert_eq!(w.constraints[0].verdict, Verdict::Pass);
        // re-recording the text keeps the verdict
        apply(
            &mut w,
            "world_constraint",
            &args(
                json!({"id": "C1", "text": "Vd ≤ 3 % (feeder)", "severity": "hard", "source_file": "/app/spec.pdf"}),
            ),
        );
        assert_eq!(w.constraints[0].verdict, Verdict::Pass);
        assert!(
            apply(
                &mut w,
                VERDICT,
                &args(json!({"id": "C9", "verdict": "pass"}))
            )
            .unwrap()
            .starts_with("error")
        );

        apply(
            &mut w,
            "world_action",
            &args(json!({"id": "A1", "description": "Reply to the client"})),
        );
        apply(
            &mut w,
            "world_action",
            &args(json!({"id": "A1", "status": "verified"})),
        );
        assert_eq!(w.actions[0].status, ActionStatus::Verified);

        apply(
            &mut w,
            ISSUE,
            &args(json!({"kind": "format", "text": "zip has an extra folder"})),
        );
        assert_eq!(w.open_blocking_issues().count(), 1);
        apply(&mut w, ISSUE, &args(json!({"id": "I1", "open": false})));
        assert_eq!(w.open_blocking_issues().count(), 0);

        assert!(
            apply(&mut w, "world_param", &args(json!({"name": "L"})))
                .unwrap()
                .starts_with("error")
        );
        assert!(w.finish_blockers().is_empty(), "{:?}", w.finish_blockers());
    }

    #[test]
    fn each_phase_gets_the_tools_it_names() {
        let d = definitions(RECORD);
        assert_eq!(d.len(), 5);
        let d = definitions(&[ISSUE]);
        assert_eq!(d[0]["function"]["name"], ISSUE);
    }
}
