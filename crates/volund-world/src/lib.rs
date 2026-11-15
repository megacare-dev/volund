//! World model: what the harness knows to be true about the task right now.
//!
//! The conversation with the model is not the record. Everything a later phase
//! depends on lives here: the deliverables, parameters with their sources,
//! constraints and their verdicts, app actions, check results and open issues.
//! Prompts are rendered from it ([`World::brief`]), the state machine's guards
//! read it, and the final message is built from it when time runs out.
//!
//! When the world and the files disagree, the files win: the deliverables are
//! what gets graded, so the world is re-derived from them, never the reverse.

use serde::{Deserialize, Serialize};
use std::fmt::{self, Write as _};

/// Where a value or a requirement came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// Path in the task container.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// Clause, sheet cell, drawing callout, email id, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

impl SourceRef {
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            file: path.into(),
            ..Self::default()
        }
    }
}

impl fmt::Display for SourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.file)?;
        if let Some(page) = self.page {
            write!(f, " p.{page}")?;
        }
        if let Some(locator) = &self.locator {
            write!(f, " [{locator}]")?;
        }
        Ok(())
    }
}

/// File format of a deliverable, from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Markdown,
    Text,
    Csv,
    Json,
    Yaml,
    Html,
    Xml,
    Docx,
    Xlsx,
    Pptx,
    Pdf,
    Ifc,
    Zip,
    Python,
    /// A path ending in `/`: the grader reads the whole directory.
    Directory,
    Other,
}

impl Format {
    pub fn from_path(path: &str) -> Self {
        if path.ends_with('/') {
            return Self::Directory;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" => Self::Markdown,
            "txt" => Self::Text,
            "csv" => Self::Csv,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "html" | "htm" => Self::Html,
            "xml" => Self::Xml,
            "docx" => Self::Docx,
            "xlsx" => Self::Xlsx,
            "pptx" => Self::Pptx,
            "pdf" => Self::Pdf,
            "ifc" => Self::Ifc,
            "zip" => Self::Zip,
            "py" => Self::Python,
            _ => Self::Other,
        }
    }

    /// Formats the rubric judge reads directly (protocol.md §3); others reach it only as a name and a size.
    pub fn judge_readable(self) -> bool {
        !matches!(self, Self::Ifc | Self::Zip | Self::Directory | Self::Other)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableStatus {
    #[default]
    Missing,
    /// A skeleton exists, so a timeout still leaves something to grade.
    Draft,
    Written,
    /// Passed the deterministic checks.
    Checked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliverable {
    pub path: String,
    pub format: Format,
    /// What the task says it must contain.
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub status: DeliverableStatus,
}

impl Deliverable {
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            format: Format::from_path(&path),
            path,
            required: Vec::new(),
            status: DeliverableStatus::Missing,
        }
    }
}

/// A value read from the input documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    /// As written in the source; the executable model parses it.
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub source: SourceRef,
}

/// Hard constraints are safety and feasibility: breaking one caps a graded item at 0.4 (rules.md, "Scoring").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Hard,
    Soft,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    #[default]
    Unchecked,
    Pass,
    Fail,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unchecked => "unchecked",
            Self::Pass => "pass",
            Self::Fail => "fail",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub id: String,
    pub text: String,
    pub severity: Severity,
    pub source: SourceRef,
    /// The value the model computed for it, with its unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed: Option<String>,
    #[serde(default)]
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    #[default]
    Pending,
    /// The model says it did it.
    Done,
    /// A read back through the app's MCP tools confirmed it.
    Verified,
}

/// Something the task asks to be done in its apps: send a reply, close a case, open a work package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppAction {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: ActionStatus,
}

/// The executable model's tests, as the harness last ran them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStatus {
    pub tests_total: u32,
    pub tests_passed: u32,
}

impl ModelStatus {
    pub fn green(self) -> bool {
        self.tests_total > 0 && self.tests_passed == self.tests_total
    }
}

/// One deterministic check the harness ran on the deliverables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    HardConstraint,
    /// Wrong path, format or packaging.
    Format,
    Content,
}

impl IssueKind {
    /// Blocking issues keep the task from finishing while fix rounds remain.
    pub fn blocking(self) -> bool {
        matches!(self, Self::HardConstraint | Self::Format)
    }
}

fn open_by_default() -> bool {
    true
}

/// A problem the verifier reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub kind: IssueKind,
    pub text: String,
    #[serde(default = "open_by_default")]
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct World {
    pub workdir: String,
    /// Input files found by PROBE.
    pub files: Vec<FileEntry>,
    pub deliverables: Vec<Deliverable>,
    pub params: Vec<Param>,
    pub constraints: Vec<Constraint>,
    pub actions: Vec<AppAction>,
    pub assumptions: Vec<String>,
    pub model: ModelStatus,
    pub checks: Vec<CheckResult>,
    pub issues: Vec<Issue>,
}

fn share(passed: usize, total: usize) -> f64 {
    if total == 0 {
        1.0
    } else {
        passed as f64 / total as f64
    }
}

impl World {
    /// INTAKE's exit guard: the task named at least one deliverable and each has a path.
    pub fn intake_complete(&self) -> bool {
        !self.deliverables.is_empty() && self.deliverables.iter().all(|d| !d.path.trim().is_empty())
    }

    /// Tasks with constraints get an executable model; pure writing tasks skip MODEL and SOLVE.
    pub fn needs_model(&self) -> bool {
        !self.constraints.is_empty()
    }

    /// The deterministic checks ran and every one passed.
    pub fn checks_pass(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }

    pub fn hard_unresolved(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints
            .iter()
            .filter(|c| c.severity == Severity::Hard && c.verdict != Verdict::Pass)
    }

    pub fn open_blocking_issues(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.open && i.kind.blocking())
    }

    /// The finish gate: why the task may not finish yet. Empty means it may.
    pub fn finish_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.deliverables.is_empty() {
            out.push("no deliverable recorded".to_owned());
        }
        for d in &self.deliverables {
            if d.status == DeliverableStatus::Missing {
                out.push(format!("deliverable missing: {}", d.path));
            }
        }
        for c in self.hard_unresolved() {
            out.push(format!("hard constraint {} is {}", c.id, c.verdict));
        }
        for a in &self.actions {
            if a.status != ActionStatus::Verified {
                out.push(format!("app action {} not verified", a.id));
            }
        }
        for c in &self.checks {
            if !c.passed {
                out.push(format!("check failed: {}", c.name));
            }
        }
        for i in self.open_blocking_issues() {
            out.push(format!("open issue {}: {}", i.id, i.text));
        }
        out
    }

    /// An estimate of the grader's shape for the Δ≥0 commit gate: `checks · (0.4·p + 0.6·g)`, where `p` is the
    /// share of constraints that pass and `g` is 1 only when every hard constraint passes (rules.md, "Scoring").
    /// The objective term `O'` is left out: it needs the task's certified bound, which the harness never sees.
    pub fn quality(&self) -> f64 {
        let passed = self
            .constraints
            .iter()
            .filter(|c| c.verdict == Verdict::Pass)
            .count();
        let p = share(passed, self.constraints.len());
        let g = if self.hard_unresolved().next().is_none() {
            1.0
        } else {
            0.0
        };
        let checks_passed = self.checks.iter().filter(|c| c.passed).count();
        share(checks_passed, self.checks.len()) * (0.4 * p + 0.6 * g)
    }

    /// The world as the model sees it at the top of each phase. Compaction re-renders this instead of
    /// summarising old turns, so nothing recorded here is lost when the conversation is cut.
    pub fn brief(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Deliverables:");
        if self.deliverables.is_empty() {
            let _ = writeln!(out, "- (none recorded yet)");
        }
        for d in &self.deliverables {
            let _ = write!(out, "- [{:?}] {} ({:?})", d.status, d.path, d.format);
            if !d.required.is_empty() {
                let _ = write!(out, ": {}", d.required.join("; "));
            }
            out.push('\n');
        }
        if !self.constraints.is_empty() {
            let passed = self
                .constraints
                .iter()
                .filter(|c| c.verdict == Verdict::Pass)
                .count();
            let _ = writeln!(
                out,
                "Constraints: {passed} of {} pass; not yet passing:",
                self.constraints.len()
            );
            for c in self
                .constraints
                .iter()
                .filter(|c| c.verdict != Verdict::Pass)
            {
                let _ = write!(out, "- {} {:?} {}: {}", c.id, c.severity, c.verdict, c.text);
                if let Some(v) = &c.computed {
                    let _ = write!(out, " (computed {v})");
                }
                let _ = writeln!(out, " <- {}", c.source);
            }
        }
        if !self.params.is_empty() {
            let _ = writeln!(out, "Parameters:");
            for p in &self.params {
                let unit = p.unit.as_deref().unwrap_or("");
                let _ = writeln!(out, "- {} = {} {} <- {}", p.name, p.value, unit, p.source);
            }
        }
        if !self.actions.is_empty() {
            let _ = writeln!(out, "App actions:");
            for a in &self.actions {
                let _ = writeln!(out, "- {} [{:?}] {}", a.id, a.status, a.description);
            }
        }
        if self.needs_model() {
            let _ = writeln!(
                out,
                "Model tests: {}/{} pass",
                self.model.tests_passed, self.model.tests_total
            );
        }
        let open: Vec<&Issue> = self.issues.iter().filter(|i| i.open).collect();
        if !open.is_empty() {
            let _ = writeln!(out, "Open issues:");
            for i in open {
                let _ = writeln!(out, "- {} [{:?}] {}", i.id, i.kind, i.text);
            }
        }
        if !self.assumptions.is_empty() {
            let _ = writeln!(out, "Assumptions:");
            for a in &self.assumptions {
                let _ = writeln!(out, "- {a}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraint(id: &str, severity: Severity, verdict: Verdict) -> Constraint {
        Constraint {
            id: id.to_owned(),
            text: format!("{id} text"),
            severity,
            source: SourceRef::file("/app/files/spec.pdf"),
            computed: None,
            verdict,
        }
    }

    fn check(name: &str, passed: bool) -> CheckResult {
        CheckResult {
            name: name.to_owned(),
            passed,
            detail: String::new(),
        }
    }

    #[test]
    fn format_comes_from_the_file_name() {
        assert_eq!(Format::from_path("/app/out/Report.DOCX"), Format::Docx);
        assert_eq!(Format::from_path("/filesystem/"), Format::Directory);
        assert_eq!(Format::from_path("/a.b/notes"), Format::Other);
        assert_eq!(
            Format::from_path("/app/files/x/submission/model.ifc"),
            Format::Ifc
        );
        assert!(Format::Docx.judge_readable());
        assert!(!Format::Ifc.judge_readable());
    }

    #[test]
    fn empty_world_scores_full_and_cannot_finish() {
        let world = World::default();
        assert!((world.quality() - 1.0).abs() < 1e-12);
        assert_eq!(world.finish_blockers(), vec!["no deliverable recorded"]);
        assert!(!world.intake_complete());
    }

    #[test]
    fn a_failing_hard_constraint_caps_quality_at_the_constraint_share() {
        let mut world = World {
            constraints: vec![
                constraint("C1", Severity::Hard, Verdict::Fail),
                constraint("C2", Severity::Soft, Verdict::Pass),
            ],
            ..World::default()
        };
        // p = 0.5, g = 0 -> 0.4 * 0.5
        assert!((world.quality() - 0.2).abs() < 1e-12);
        world.checks = vec![check("zip opens", true), check("paths", false)];
        assert!((world.quality() - 0.1).abs() < 1e-12);
        world.constraints[0].verdict = Verdict::Pass;
        world.checks[1].passed = true;
        assert!((world.quality() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn finish_blockers_name_every_gap() {
        let world = World {
            deliverables: vec![Deliverable::new("/app/output/memo.docx")],
            constraints: vec![constraint("C1", Severity::Hard, Verdict::Unchecked)],
            actions: vec![AppAction {
                id: "A1".into(),
                description: "close the case".into(),
                status: ActionStatus::Done,
            }],
            issues: vec![Issue {
                id: "I1".into(),
                kind: IssueKind::Format,
                text: "wrong folder".into(),
                open: true,
            }],
            ..World::default()
        };
        let blockers = world.finish_blockers();
        assert_eq!(
            blockers,
            vec![
                "deliverable missing: /app/output/memo.docx",
                "hard constraint C1 is unchecked",
                "app action A1 not verified",
                "open issue I1: wrong folder",
            ]
        );
    }

    #[test]
    fn round_trips_through_json_and_defaults_missing_fields() {
        let world = World {
            deliverables: vec![Deliverable::new("/filesystem/x/output/submission.zip")],
            params: vec![Param {
                name: "roof snow load".into(),
                value: "25".into(),
                unit: Some("psf".into()),
                source: SourceRef {
                    file: "/filesystem/x/S-001.pdf".into(),
                    page: Some(2),
                    locator: Some("General Note 4".into()),
                },
            }],
            ..World::default()
        };
        let json = serde_json::to_string(&world).unwrap();
        assert_eq!(serde_json::from_str::<World>(&json).unwrap(), world);
        assert_eq!(
            serde_json::from_str::<World>("{}").unwrap(),
            World::default()
        );
    }

    #[test]
    fn brief_lists_what_is_still_open() {
        let mut world = World {
            deliverables: vec![Deliverable::new("/app/output/memo.docx")],
            constraints: vec![
                constraint("C1", Severity::Hard, Verdict::Fail),
                constraint("C2", Severity::Soft, Verdict::Pass),
            ],
            ..World::default()
        };
        world.constraints[0].computed = Some("1.32 in".into());
        let brief = world.brief();
        assert!(brief.contains("- [Missing] /app/output/memo.docx (Docx)"));
        assert!(brief.contains("Constraints: 1 of 2 pass"));
        assert!(
            brief.contains("- C1 Hard fail: C1 text (computed 1.32 in) <- /app/files/spec.pdf")
        );
        assert!(!brief.contains("C2 text"));
        assert!(brief.contains("Model tests: 0/0 pass"));
    }
}
