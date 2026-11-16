//! The ablation ladder of DESIGN.md §9, chosen with `VOLUND_ABLATION`. Each level adds one part to
//! the level below it, so the runs of two neighbouring levels differ in that part alone. The entry
//! runs the top level.

use volund_fsm::Features;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// One tool-calling loop.
    A1,
    /// The loop opens with what PROBE found: the input files and the container's tools.
    A2,
    /// The loop also keeps the world: `world_*` tools, the record re-rendered when the conversation is
    /// cut, and a summary built from it on cancel.
    A3,
    /// The phase driver: PROBE → INTAKE → WRITE → CHECK ↺ FIX, finishing through the finish gate.
    A4,
    /// MODEL and SOLVE (the executable model) for tasks with constraints.
    A5,
    /// VERIFY in a fresh conversation, and the Δ≥0 commit gate on every FIX. The full harness.
    A6,
}

impl Level {
    pub const FULL: Self = Self::A6;
    const ALL: [Self; 6] = [Self::A1, Self::A2, Self::A3, Self::A4, Self::A5, Self::A6];

    /// Reads `VOLUND_ABLATION`: unset or empty is the full harness; anything but `A1`–`A6` is refused,
    /// so a typo never runs a different experiment than the one asked for.
    pub fn from_setting(value: Option<&str>) -> Result<Self, String> {
        let value = value.map(str::trim).unwrap_or("");
        if value.is_empty() {
            return Ok(Self::FULL);
        }
        Self::ALL
            .into_iter()
            .find(|l| l.name().eq_ignore_ascii_case(value))
            .ok_or_else(|| {
                format!("VOLUND_ABLATION is {value:?}; use one of A1 to A6, or leave it unset")
            })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::A1 => "A1",
            Self::A2 => "A2",
            Self::A3 => "A3",
            Self::A4 => "A4",
            Self::A5 => "A5",
            Self::A6 => "A6",
        }
    }

    /// The phase driver works the task; below this level, the single loop does.
    pub fn phases(self) -> bool {
        self >= Self::A4
    }

    /// The loop starts with PROBE's findings (the driver always probes).
    pub fn probe(self) -> bool {
        self >= Self::A2
    }

    /// The loop keeps the world (the driver always does).
    pub fn world(self) -> bool {
        self >= Self::A3
    }

    /// What the state machine runs at this level.
    pub fn features(self) -> Features {
        Features {
            model: self >= Self::A5,
            verify: self >= Self::A6,
            commit_gate: self >= Self::A6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_names_a_level_or_is_refused() {
        assert_eq!(Level::from_setting(None), Ok(Level::A6));
        assert_eq!(Level::from_setting(Some(" ")), Ok(Level::A6));
        assert_eq!(Level::from_setting(Some("a3")), Ok(Level::A3));
        assert_eq!(Level::from_setting(Some("A1")), Ok(Level::A1));
        let refused = Level::from_setting(Some("loop")).unwrap_err();
        assert!(refused.contains("\"loop\""), "{refused}");
        assert!(Level::from_setting(Some("A7")).is_err());
    }

    #[test]
    fn each_level_adds_one_part() {
        let parts = |l: Level| {
            let f = l.features();
            [
                l.probe(),
                l.world(),
                l.phases(),
                l.phases() && f.model,
                l.phases() && f.verify && f.commit_gate,
            ]
        };
        let on = |l: Level| parts(l).iter().filter(|&&p| p).count();
        for (lower, upper) in Level::ALL.iter().zip(&Level::ALL[1..]) {
            assert_eq!(on(*upper), on(*lower) + 1, "{lower:?} → {upper:?}");
            for (a, b) in parts(*lower).iter().zip(parts(*upper)) {
                assert!(!a || b, "{upper:?} keeps every part of {lower:?}");
            }
        }
        assert_eq!(on(Level::A1), 0);
        assert_eq!(Level::FULL.features(), Features::default());
    }
}
