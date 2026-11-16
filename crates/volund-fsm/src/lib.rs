//! Phase state machine: code decides when the run moves on; the model only works inside a phase.
//!
//! [`next`] is a pure function of the current phase, the event that ended the step, the world and
//! the run's progress, so every transition can be tested without a model. Two rules hold in every
//! phase: a cancel or the wrap-up deadline sends the run to FINALIZE, and a spent phase budget always
//! moves the run forward, so no phase can stall a task.

use serde::{Deserialize, Serialize};
use volund_world::World;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Inventory files, tools and MCP catalogs with scripted commands (no model call).
    Probe,
    /// Read the task; record deliverables, parameters, constraints and app actions in the world.
    Intake,
    /// Write the executable model of the project and the tests that tie it to the input documents.
    Model,
    /// Search the model for a design that meets every hard constraint, then improve the objective.
    Solve,
    /// Produce the deliverables and carry out the app actions.
    Write,
    /// Deterministic checks of the deliverables (no model call).
    Check,
    /// A fresh conversation recomputes the key results and reports issues.
    Verify,
    /// Fix the reported issues; the fix is kept only if quality does not drop.
    Fix,
    /// Send `done` with a summary built from the world.
    Finalize,
    /// Terminal.
    Done,
}

impl Phase {
    /// Whether the phase runs a model conversation; the others are scripted.
    pub fn uses_model(self) -> bool {
        !matches!(
            self,
            Self::Probe | Self::Check | Self::Finalize | Self::Done
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The phase finished its work: the model stopped or called `finish`, or a scripted phase completed.
    PhaseEnded,
    /// The phase used up its share of time or tokens.
    BudgetSpent,
    /// The evaluator sent `cancel`.
    Cancelled,
    /// FINALIZE sent `done`.
    DoneSent,
}

/// The parts of the run that can be switched off, for the ablation ladder (DESIGN.md §9). All are on
/// by default; with all off the run is PROBE → INTAKE → WRITE → CHECK ↺ FIX → FINALIZE, finishing
/// through the finish gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Features {
    /// MODEL and SOLVE run when the world has constraints.
    pub model: bool,
    /// VERIFY runs once the checks pass; without it the finish gate is read straight after CHECK.
    pub verify: bool,
    /// Each FIX starts from a snapshot that is restored if the fix lowers quality (Δ≥0).
    pub commit_gate: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            model: true,
            verify: true,
            commit_gate: true,
        }
    }
}

/// What the driver tracks across phases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Seconds left before the task deadline; `None` when untimed.
    pub time_left_sec: Option<u64>,
    /// Seconds kept for FINALIZE.
    pub wrap_up_sec: u64,
    /// FIX rounds started so far: the driver counts each transition into FIX.
    pub fix_rounds: u32,
    pub max_fix_rounds: u32,
    /// World quality when the pending FIX took its snapshot; `None` when no fix awaits its check.
    pub quality_at_snapshot: Option<f64>,
    pub features: Features,
}

impl Progress {
    pub fn wrap_up_reached(&self) -> bool {
        self.time_left_sec
            .is_some_and(|left| left <= self.wrap_up_sec)
    }

    pub fn fix_round_left(&self) -> bool {
        self.fix_rounds < self.max_fix_rounds
    }
}

/// Side effects the driver carries out when it takes a transition, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Entering FIX with the commit gate on: copy the deliverables aside, keep a clone of the world
    /// and record its quality.
    Snapshot,
    /// The fix kept or raised quality: drop the snapshot.
    Commit,
    /// The fix lowered quality: restore the deliverables and the world from the snapshot.
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Transition {
    pub to: Phase,
    /// Logged with every transition; the failure breakdown groups runs by the last one.
    pub reason: &'static str,
    pub effects: Vec<Effect>,
}

fn go(to: Phase, reason: &'static str) -> Transition {
    Transition {
        to,
        reason,
        effects: Vec::new(),
    }
}

fn start_fix(reason: &'static str, features: Features) -> Transition {
    Transition {
        to: Phase::Fix,
        reason,
        effects: if features.commit_gate {
            vec![Effect::Snapshot]
        } else {
            Vec::new()
        },
    }
}

/// The phase to run after `phase` ended with `event`.
pub fn next(phase: Phase, event: Event, world: &World, progress: &Progress) -> Transition {
    use Phase::*;
    let features = progress.features;
    match (phase, event) {
        (Done, _) => go(Done, "finished"),
        (Finalize, Event::DoneSent) => go(Done, "done sent"),
        (Finalize, _) => go(Finalize, "sending done"),
        (_, Event::Cancelled) => go(Finalize, "cancelled by the evaluator"),
        _ if progress.wrap_up_reached() => go(Finalize, "wrap-up time reached"),

        (Probe, _) => go(Intake, "probe complete"),

        (Intake, Event::PhaseEnded) if !world.intake_complete() => {
            go(Intake, "deliverables not specified yet")
        }
        (Intake, _) if world.needs_model() && features.model => go(Model, "constraints recorded"),
        (Intake, _) if world.needs_model() => go(Write, "constraints recorded; MODEL is off"),
        (Intake, _) => go(Write, "no constraints: nothing to model"),

        (Model, _) if world.model.green() => go(Solve, "model tests pass"),
        (Model, Event::PhaseEnded) => go(Model, "model tests not green yet"),
        (Model, _) => go(Write, "model budget spent: write with what holds"),

        (Solve, _) => go(Write, "solve complete"),
        (Write, _) => go(Check, "deliverables written"),
        (Check, _) => after_check(world, progress),

        (Verify, _) if world.finish_blockers().is_empty() => go(Finalize, "verified"),
        (Verify, _) if progress.fix_round_left() => start_fix("verifier found gaps", features),
        (Verify, _) => go(Finalize, "gaps remain; no fix round left"),

        (Fix, _) => go(Check, "fix applied"),
    }
}

/// CHECK settles a pending fix first (the Δ≥0 commit gate), then routes on the checks; without
/// VERIFY, passing checks finish through the finish gate.
fn after_check(world: &World, progress: &Progress) -> Transition {
    let features = progress.features;
    let mut effects = Vec::new();
    if let Some(before) = progress.quality_at_snapshot {
        if world.quality() < before {
            return Transition {
                to: Phase::Finalize,
                reason: "fix lowered quality: rolled back",
                effects: vec![Effect::Rollback],
            };
        }
        effects.push(Effect::Commit);
    }
    let passing = world.checks_pass();
    let (to, reason) = if passing && features.verify {
        (Phase::Verify, "checks pass")
    } else if passing && world.finish_blockers().is_empty() {
        (Phase::Finalize, "checks pass; nothing blocks finishing")
    } else if progress.fix_round_left() {
        if features.commit_gate {
            effects.push(Effect::Snapshot);
        }
        let why = if passing {
            "checks pass; finish blockers remain"
        } else {
            "checks failing"
        };
        (Phase::Fix, why)
    } else if passing {
        (Phase::Finalize, "finish blockers remain; no fix round left")
    } else {
        (Phase::Finalize, "checks failing; no fix round left")
    };
    Transition {
        to,
        reason,
        effects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volund_world::{
        CheckResult, Constraint, Deliverable, DeliverableStatus, Severity, SourceRef, Verdict,
    };

    fn progress() -> Progress {
        Progress {
            time_left_sec: Some(3600),
            wrap_up_sec: 240,
            fix_rounds: 0,
            max_fix_rounds: 2,
            quality_at_snapshot: None,
            features: Features::default(),
        }
    }

    fn without(f: impl Fn(&mut Features)) -> Progress {
        let mut p = progress();
        f(&mut p.features);
        p
    }

    fn with_deliverable() -> World {
        World {
            deliverables: vec![Deliverable::new("/app/output/memo.docx")],
            ..World::default()
        }
    }

    fn with_constraint(verdict: Verdict) -> World {
        let mut world = with_deliverable();
        world.constraints = vec![Constraint {
            id: "C1".into(),
            text: "voltage drop <= 3%".into(),
            severity: Severity::Hard,
            source: SourceRef::file("/filesystem/x/spec.pdf"),
            computed: None,
            verdict,
        }];
        world
    }

    fn checks(world: &mut World, passed: bool) {
        world.checks = vec![CheckResult {
            name: "deliverable opens".into(),
            passed,
            detail: String::new(),
        }];
    }

    #[test]
    fn probe_goes_to_intake() {
        let t = next(
            Phase::Probe,
            Event::PhaseEnded,
            &World::default(),
            &progress(),
        );
        assert_eq!(t.to, Phase::Intake);
    }

    #[test]
    fn intake_holds_until_deliverables_are_named_but_never_past_its_budget() {
        let empty = World::default();
        let stay = next(Phase::Intake, Event::PhaseEnded, &empty, &progress());
        assert_eq!(stay.to, Phase::Intake);
        let forced = next(Phase::Intake, Event::BudgetSpent, &empty, &progress());
        assert_eq!(forced.to, Phase::Write);
    }

    #[test]
    fn intake_skips_the_model_when_there_is_nothing_to_compute() {
        let t = next(
            Phase::Intake,
            Event::PhaseEnded,
            &with_deliverable(),
            &progress(),
        );
        assert_eq!(t.to, Phase::Write);
        let t = next(
            Phase::Intake,
            Event::PhaseEnded,
            &with_constraint(Verdict::Unchecked),
            &progress(),
        );
        assert_eq!(t.to, Phase::Model);
    }

    #[test]
    fn model_needs_green_tests_to_reach_solve() {
        let mut world = with_constraint(Verdict::Unchecked);
        world.model.tests_total = 4;
        world.model.tests_passed = 3;
        assert_eq!(
            next(Phase::Model, Event::PhaseEnded, &world, &progress()).to,
            Phase::Model
        );
        assert_eq!(
            next(Phase::Model, Event::BudgetSpent, &world, &progress()).to,
            Phase::Write
        );
        world.model.tests_passed = 4;
        assert_eq!(
            next(Phase::Model, Event::PhaseEnded, &world, &progress()).to,
            Phase::Solve
        );
    }

    #[test]
    fn failing_checks_start_a_fix_with_a_snapshot() {
        let mut world = with_deliverable();
        checks(&mut world, false);
        let t = next(Phase::Check, Event::PhaseEnded, &world, &progress());
        assert_eq!(t.to, Phase::Fix);
        assert_eq!(t.effects, vec![Effect::Snapshot]);
    }

    #[test]
    fn a_fix_that_lowers_quality_is_rolled_back() {
        let mut world = with_constraint(Verdict::Fail);
        checks(&mut world, true);
        let mut p = progress();
        p.fix_rounds = 1;
        p.quality_at_snapshot = Some(1.0);
        let t = next(Phase::Check, Event::PhaseEnded, &world, &p);
        assert_eq!(t.to, Phase::Finalize);
        assert_eq!(t.effects, vec![Effect::Rollback]);
    }

    #[test]
    fn a_fix_that_keeps_quality_is_committed_and_verified_again() {
        let mut world = with_constraint(Verdict::Pass);
        checks(&mut world, true);
        let mut p = progress();
        p.fix_rounds = 1;
        p.quality_at_snapshot = Some(world.quality());
        let t = next(Phase::Check, Event::PhaseEnded, &world, &p);
        assert_eq!(t.to, Phase::Verify);
        assert_eq!(t.effects, vec![Effect::Commit]);
    }

    #[test]
    fn verify_finishes_only_without_blockers_or_rounds() {
        let mut world = with_constraint(Verdict::Fail);
        world.deliverables[0].status = DeliverableStatus::Written;
        checks(&mut world, true);
        assert_eq!(
            next(Phase::Verify, Event::PhaseEnded, &world, &progress()).to,
            Phase::Fix
        );
        let mut spent = progress();
        spent.fix_rounds = 2;
        let t = next(Phase::Verify, Event::PhaseEnded, &world, &spent);
        assert_eq!(
            (t.to, t.reason),
            (Phase::Finalize, "gaps remain; no fix round left")
        );
        world.constraints[0].verdict = Verdict::Pass;
        let t = next(Phase::Verify, Event::PhaseEnded, &world, &progress());
        assert_eq!((t.to, t.reason), (Phase::Finalize, "verified"));
    }

    #[test]
    fn cancel_and_wrap_up_reach_finalize_from_any_working_phase() {
        let mut late = progress();
        late.time_left_sec = Some(200);
        let world = with_constraint(Verdict::Unchecked);
        for phase in [
            Phase::Probe,
            Phase::Intake,
            Phase::Model,
            Phase::Solve,
            Phase::Write,
            Phase::Check,
            Phase::Verify,
            Phase::Fix,
        ] {
            assert_eq!(
                next(phase, Event::Cancelled, &world, &progress()).to,
                Phase::Finalize
            );
            assert_eq!(
                next(phase, Event::PhaseEnded, &world, &late).to,
                Phase::Finalize
            );
        }
        assert_eq!(
            next(Phase::Finalize, Event::DoneSent, &world, &late).to,
            Phase::Done
        );
    }

    #[test]
    fn without_the_model_phases_constraints_go_straight_to_write() {
        let world = with_constraint(Verdict::Unchecked);
        let t = next(
            Phase::Intake,
            Event::PhaseEnded,
            &world,
            &without(|f| f.model = false),
        );
        assert_eq!(
            (t.to, t.reason),
            (Phase::Write, "constraints recorded; MODEL is off")
        );
    }

    #[test]
    fn without_verify_passing_checks_meet_the_finish_gate() {
        let off = without(|f| f.verify = false);
        let mut done = with_constraint(Verdict::Pass);
        done.deliverables[0].status = DeliverableStatus::Checked;
        checks(&mut done, true);
        let t = next(Phase::Check, Event::PhaseEnded, &done, &off);
        assert_eq!(
            (t.to, t.reason),
            (Phase::Finalize, "checks pass; nothing blocks finishing")
        );

        let mut open = with_constraint(Verdict::Unchecked);
        open.deliverables[0].status = DeliverableStatus::Checked;
        checks(&mut open, true);
        let t = next(Phase::Check, Event::PhaseEnded, &open, &off);
        assert_eq!(
            (t.to, t.reason),
            (Phase::Fix, "checks pass; finish blockers remain")
        );
        assert_eq!(t.effects, vec![Effect::Snapshot]);
        let mut spent = off;
        spent.fix_rounds = 2;
        assert_eq!(
            next(Phase::Check, Event::PhaseEnded, &open, &spent).reason,
            "finish blockers remain; no fix round left"
        );
    }

    #[test]
    fn without_the_commit_gate_a_fix_takes_no_snapshot() {
        let off = without(|f| f.commit_gate = false);
        let mut failing = with_deliverable();
        checks(&mut failing, false);
        let t = next(Phase::Check, Event::PhaseEnded, &failing, &off);
        assert_eq!((t.to, t.effects), (Phase::Fix, vec![]));

        let mut gaps = with_constraint(Verdict::Fail);
        checks(&mut gaps, true);
        let t = next(Phase::Verify, Event::PhaseEnded, &gaps, &off);
        assert_eq!((t.to, t.effects), (Phase::Fix, vec![]));
    }

    /// Plays the driver's part: applies effects, counts FIX rounds and keeps spending budgets until
    /// the run is done.
    fn run_to_done(world: &World, mut p: Progress, max_steps: usize) -> Option<usize> {
        let mut phase = Phase::Probe;
        for step in 0..max_steps {
            if phase == Phase::Done {
                return Some(step);
            }
            let event = if phase == Phase::Finalize {
                Event::DoneSent
            } else {
                Event::BudgetSpent
            };
            let t = next(phase, event, world, &p);
            if t.to == Phase::Fix {
                p.fix_rounds += 1;
            }
            for effect in &t.effects {
                match effect {
                    Effect::Snapshot => p.quality_at_snapshot = Some(world.quality()),
                    Effect::Commit | Effect::Rollback => p.quality_at_snapshot = None,
                }
            }
            phase = t.to;
        }
        None
    }

    #[test]
    fn spent_budgets_always_end_the_run() {
        let mut failing = with_constraint(Verdict::Fail);
        checks(&mut failing, false);
        let mut blocked = with_constraint(Verdict::Unchecked);
        checks(&mut blocked, true);
        for bits in 0..8u8 {
            let p = without(|f| {
                f.model = bits & 1 == 0;
                f.verify = bits & 2 == 0;
                f.commit_gate = bits & 4 == 0;
            });
            for world in [
                World::default(),
                with_deliverable(),
                failing.clone(),
                blocked.clone(),
            ] {
                assert!(
                    run_to_done(&world, p, 40).is_some(),
                    "stalled on {world:?} with {:?}",
                    p.features
                );
            }
        }
    }
}
