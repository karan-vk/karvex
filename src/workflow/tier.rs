//! Tier → per-node model and effort.
//!
//! Two axes: the run's tier, chosen by the user, and the node's declared
//! demand. One pure function resolves them, so any run can be explained after
//! the fact and replayed identically
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §7).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::workflow::model::{Demand, NodeKey};

/// The run's cost/quality tier. `workflow_run.tier` persists these strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Deterministic per-node policy driven by the node's own history.
    Auto,
    Max,
    High,
    Medium,
    Low,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Max => "max",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "max" => Some(Self::Max),
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every `Tier` except `Auto` has a fixed row in the §7.1/§7.2 tables.
/// `Auto` computes its own assignment from `NodeHistory` instead (§7.3), so it
/// carries no row and cannot appear here — that keeps `model_for`/`effort_for`
/// total functions with no "what does auto's row mean" case to panic on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rung {
    Max,
    High,
    Medium,
    Low,
}

impl Rung {
    fn from_tier(tier: Tier) -> Option<Self> {
        match tier {
            Tier::Max => Some(Self::Max),
            Tier::High => Some(Self::High),
            Tier::Medium => Some(Self::Medium),
            Tier::Low => Some(Self::Low),
            Tier::Auto => None,
        }
    }
}

/// Passed to `claude --model`. Declared low-to-high cost/capability
/// (`Sonnet < Opus < Fable`) so the `auto` policy's escalation step (§7.3
/// step 4) and max-row cap (step 6) can use ordinary `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAlias {
    Sonnet,
    Opus,
    Fable,
}

impl ModelAlias {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fable => "fable",
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
        }
    }

    /// One step up the escalation ladder (§7.3 step 4). Saturates at `fable`.
    fn escalate(self) -> Self {
        match self {
            Self::Sonnet => Self::Opus,
            Self::Opus | Self::Fable => Self::Fable,
        }
    }
}

impl fmt::Display for ModelAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Passed to `claude --effort`. Ordered `low < medium < high < xhigh < max`,
/// which is the ladder `auto` walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// One step up the ladder (§7.3 step 4). Saturates at `max`.
    fn escalate(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Xhigh,
            Self::Xhigh | Self::Max => Self::Max,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved per-node spend, bound at spawn as
/// `claude --model <alias> --effort <level>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Assignment {
    pub model: ModelAlias,
    pub effort: Effort,
}

/// A node key's measured record across the workflow's recent runs; the input
/// the `auto` tier's deterministic policy reads (§7.3). `runs` and the counts
/// below it are already windowed to "the workflow's last N runs" by whatever
/// builds this — `resolve` itself does no further windowing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NodeHistory {
    pub runs: u32,
    pub first_pass_successes: u32,
    pub schema_failures: u32,
    pub watchdog_interventions: u32,
    pub mean_tokens: u64,
    /// How many of the node's most recent two runs failed on the first pass
    /// (0, 1, or 2). Distinct from `first_pass_successes`, which has no
    /// ordering — §7.3 step 4 needs the last two specifically.
    pub recent_first_pass_failures: u8,
}

/// Every node key's measured record for one workflow, as of run start.
///
/// The alias lives in this pure module rather than in the store because
/// `graph::resolve_assignments` compiles unconditionally while
/// `store::queries` is behind `#[cfg(feature = "workflow")]` — a type declared
/// inside the gated store cannot appear in an unconditional signature
/// (`06-phase2-plan.md` §3 frozen interface 5). The store owns the *query* that
/// fills it (`queries::node_history`), not the type. An absent key behaves like
/// an all-zero record, which is what `resolve` already documents.
pub type HistoryIndex = BTreeMap<NodeKey, NodeHistory>;

impl NodeHistory {
    /// `pub(crate)` because the store's aggregation test asserts that what
    /// `queries::node_history` builds reads back the rate the runs actually
    /// describe (`06-phase2-plan.md` WS-C "Tested").
    pub(crate) fn first_pass_success_rate(&self) -> f64 {
        if self.runs == 0 {
            return 0.0;
        }
        f64::from(self.first_pass_successes) / f64::from(self.runs)
    }

    fn watchdog_interventions_per_run(&self) -> f64 {
        if self.runs == 0 {
            return 0.0;
        }
        f64::from(self.watchdog_interventions) / f64::from(self.runs)
    }
}

/// Resolves one node's `(model, effort)`.
///
/// The four fixed tiers are direct table lookups (§7.1/§7.2). `auto` runs the
/// deterministic history-driven policy instead (§7.3); `history` is ignored
/// for the fixed tiers.
pub fn resolve(tier: Tier, demand: Demand, history: Option<&NodeHistory>) -> Assignment {
    match Rung::from_tier(tier) {
        Some(rung) => Assignment {
            model: model_for(rung, demand),
            effort: effort_for(rung, demand),
        },
        None => resolve_auto(demand, history),
    }
}

/// §7.1: model by tier row and demand column.
fn model_for(rung: Rung, demand: Demand) -> ModelAlias {
    use Demand::{Critical, Light, Peak, Standard};
    match (rung, demand) {
        (Rung::Max, Peak) => ModelAlias::Fable,
        (Rung::Max, Critical | Standard | Light) => ModelAlias::Opus,
        (Rung::High, Peak | Critical | Standard) => ModelAlias::Opus,
        (Rung::High, Light) => ModelAlias::Sonnet,
        (Rung::Medium, Peak | Critical) => ModelAlias::Opus,
        (Rung::Medium, Standard | Light) => ModelAlias::Sonnet,
        (Rung::Low, Peak | Critical | Standard | Light) => ModelAlias::Sonnet,
    }
}

/// §7.2: effort by tier row and demand column. `max` and `low` are pinned —
/// every demand column reads the same value in those two rows.
fn effort_for(rung: Rung, demand: Demand) -> Effort {
    use Demand::{Critical, Light, Peak, Standard};
    match (rung, demand) {
        (Rung::Max, Peak | Critical | Standard | Light) => Effort::Max,
        (Rung::High, Peak) => Effort::Xhigh,
        (Rung::High, Critical | Standard) => Effort::High,
        (Rung::High, Light) => Effort::Medium,
        (Rung::Medium, Peak | Critical) => Effort::High,
        (Rung::Medium, Standard) => Effort::Medium,
        (Rung::Medium, Light) => Effort::Low,
        (Rung::Low, Peak | Critical | Standard | Light) => Effort::Low,
    }
}

/// §7.3 step 3: a `Standard` node with a strong track record drops to sonnet.
///
/// Split out of [`resolve_auto`] so [`auto_reason`] can say *which* steps fired
/// without restating the policy — a second copy of these predicates would be
/// exactly the "two agreeing resolvers" `06-phase2-plan.md` §4 D9 removes.
fn downgrades_standard(demand: Demand, history: &NodeHistory) -> bool {
    demand == Demand::Standard && history.runs >= 3 && history.first_pass_success_rate() >= 0.8
}

/// §7.3 step 4: repeated first-pass failure or heavy watchdog use escalates one
/// model step and one effort step.
///
/// `watchdog_interventions` has no writer before Phase 4 (`06-phase2-plan.md`
/// §4 D8), so today this predicate is driven entirely by
/// `recent_first_pass_failures`.
fn escalates(history: &NodeHistory) -> bool {
    history.recent_first_pass_failures >= 2 || history.watchdog_interventions_per_run() >= 2.0
}

/// The `auto` policy's reason string, persisted to `run_node.assignment_reason`
/// so a finished run can still be explained (§7.3's closing paragraph,
/// `06-phase2-plan.md` §4 D9).
///
/// Reads the same predicates [`resolve_auto`] applies, so the reason cannot
/// describe a step the assignment did not take. The fixed tiers have no reason
/// string at all — their table row *is* the explanation — which is why this is
/// keyed on `demand`/`history` rather than on [`Tier`].
pub fn auto_reason(demand: Demand, history: Option<&NodeHistory>) -> &'static str {
    let history = history.copied().unwrap_or_default();
    match (downgrades_standard(demand, &history), escalates(&history)) {
        (true, true) => "auto/downgrade-standard+escalate",
        (true, false) => "auto/downgrade-standard",
        (false, true) => "auto/escalate",
        (false, false) => "auto/high-row",
    }
}

/// §7.3's deterministic `auto` policy, numbered to match the doc's steps.
fn resolve_auto(demand: Demand, history: Option<&NodeHistory>) -> Assignment {
    // 1. Start from the high row.
    let mut model = model_for(Rung::High, demand);
    let mut effort = effort_for(Rung::High, demand);

    // 2. Look up history (already windowed by the caller — see NodeHistory's
    // doc comment). No history behaves like an all-zero record: none of the
    // steps below fire, so the assignment stays the high row.
    let history = history.copied().unwrap_or_default();

    // 3. Downgrade Standard -> sonnet (effort high) on a strong sonnet track
    // record: >= 3 prior runs at >= 80% first-pass success.
    if downgrades_standard(demand, &history) {
        model = ModelAlias::Sonnet;
        effort = Effort::High;
    }

    // 4. Upgrade one model step and one effort step on repeated first-pass
    // failure or heavy watchdog use, evaluated against the current
    // (possibly already-downgraded) assignment.
    if escalates(&history) {
        model = model.escalate();
        effort = effort.escalate();
    }

    // 5. Peak/Critical never drop below opus. Nothing above downgrades a
    // Peak/Critical node, so this never actually binds today; it stays as a
    // guardrail so a future change to step 3/4 can't silently violate it.
    if matches!(demand, Demand::Peak | Demand::Critical) {
        model = model.max(ModelAlias::Opus);
    }

    // 6. Never exceed the max row: model capped per demand, effort capped at
    // the single `max` ceiling (the max row is pinned to `max` for every
    // demand — see effort_for).
    model = model.min(model_for(Rung::Max, demand));
    effort = effort.min(Effort::Max);

    // 7. Never below the low row. `sonnet`/`low` are already each ladder's
    // minimum variant, so this holds by construction — nothing above this
    // point can produce a value lower than those.

    Assignment { model, effort }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_strings_round_trip() {
        for tier in [Tier::Auto, Tier::Max, Tier::High, Tier::Medium, Tier::Low] {
            assert_eq!(Tier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(Tier::parse("cheap"), None);
    }

    #[test]
    fn effort_ladder_is_ordered() {
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::Medium < Effort::High);
        assert!(Effort::High < Effort::Xhigh);
        assert!(Effort::Xhigh < Effort::Max);
    }

    #[test]
    fn model_ladder_is_ordered() {
        assert!(ModelAlias::Sonnet < ModelAlias::Opus);
        assert!(ModelAlias::Opus < ModelAlias::Fable);
    }

    /// Exhaustive (tier, demand) table test: every cell of §7.1 and §7.2 for
    /// the four fixed tiers, read straight off the doc's tables.
    #[test]
    fn resolve_matches_the_documented_table_for_every_fixed_tier_and_demand() {
        use Demand::{Critical, Light, Peak, Standard};
        use Effort::{High as EHigh, Low as ELow, Max as EMax, Medium as EMedium, Xhigh};
        use ModelAlias::{Fable, Opus, Sonnet};
        use Tier::{High, Low, Max, Medium};

        let cases: &[(Tier, Demand, ModelAlias, Effort)] = &[
            // max row: fable on Peak, opus elsewhere; max effort everywhere.
            (Max, Peak, Fable, EMax),
            (Max, Critical, Opus, EMax),
            (Max, Standard, Opus, EMax),
            (Max, Light, Opus, EMax),
            // high row: opus except Light; xhigh/high/high/medium effort.
            (High, Peak, Opus, Xhigh),
            (High, Critical, Opus, EHigh),
            (High, Standard, Opus, EHigh),
            (High, Light, Sonnet, EMedium),
            // medium row: opus only Peak/Critical; high/high/medium/low effort.
            (Medium, Peak, Opus, EHigh),
            (Medium, Critical, Opus, EHigh),
            (Medium, Standard, Sonnet, EMedium),
            (Medium, Light, Sonnet, ELow),
            // low row: sonnet everywhere; low effort everywhere.
            (Low, Peak, Sonnet, ELow),
            (Low, Critical, Sonnet, ELow),
            (Low, Standard, Sonnet, ELow),
            (Low, Light, Sonnet, ELow),
        ];

        assert_eq!(cases.len(), 16, "4 tiers x 4 demands");
        for &(tier, demand, model, effort) in cases {
            let got = resolve(tier, demand, None);
            assert_eq!(
                got,
                Assignment { model, effort },
                "tier={tier:?} demand={demand:?}"
            );
            // history is documented as ignored for fixed tiers.
            let with_history = resolve(
                tier,
                demand,
                Some(&NodeHistory {
                    runs: 10,
                    first_pass_successes: 0,
                    recent_first_pass_failures: 2,
                    watchdog_interventions: 20,
                    ..Default::default()
                }),
            );
            assert_eq!(with_history, got, "fixed tiers must ignore history");
        }
    }

    #[test]
    fn auto_with_no_history_matches_the_high_row() {
        for demand in [
            Demand::Peak,
            Demand::Critical,
            Demand::Standard,
            Demand::Light,
        ] {
            assert_eq!(
                resolve(Tier::Auto, demand, None),
                resolve(Tier::High, demand, None),
                "demand={demand:?}"
            );
        }
    }

    #[test]
    fn auto_downgrades_standard_at_or_above_the_success_threshold() {
        let strong = NodeHistory {
            runs: 5,
            first_pass_successes: 4, // exactly 0.8
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Standard, Some(&strong)),
            Assignment {
                model: ModelAlias::Sonnet,
                effort: Effort::High,
            }
        );
    }

    #[test]
    fn auto_does_not_downgrade_standard_below_the_success_threshold() {
        let weak = NodeHistory {
            runs: 5,
            first_pass_successes: 3, // 0.6, below 0.8
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Standard, Some(&weak)),
            resolve(Tier::High, Demand::Standard, None),
        );
    }

    #[test]
    fn auto_does_not_downgrade_standard_below_three_runs() {
        let too_few_runs = NodeHistory {
            runs: 2,
            first_pass_successes: 2, // 1.0, but runs < 3
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Standard, Some(&too_few_runs)),
            resolve(Tier::High, Demand::Standard, None),
        );
    }

    #[test]
    fn auto_upgrades_on_two_recent_first_pass_failures() {
        let failing = NodeHistory {
            runs: 5,
            recent_first_pass_failures: 2,
            ..Default::default()
        };
        // Light's high row is (sonnet, medium); one escalation step each.
        assert_eq!(
            resolve(Tier::Auto, Demand::Light, Some(&failing)),
            Assignment {
                model: ModelAlias::Opus,
                effort: Effort::High,
            }
        );
    }

    #[test]
    fn auto_does_not_upgrade_on_one_recent_first_pass_failure() {
        let one_failure = NodeHistory {
            runs: 5,
            recent_first_pass_failures: 1,
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Light, Some(&one_failure)),
            resolve(Tier::High, Demand::Light, None),
        );
    }

    #[test]
    fn auto_upgrades_on_heavy_watchdog_average() {
        let watchdog_heavy = NodeHistory {
            runs: 2,
            watchdog_interventions: 4, // average 2.0
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Light, Some(&watchdog_heavy)),
            Assignment {
                model: ModelAlias::Opus,
                effort: Effort::High,
            }
        );
    }

    #[test]
    fn auto_does_not_upgrade_below_the_watchdog_average_threshold() {
        let watchdog_light = NodeHistory {
            runs: 3,
            watchdog_interventions: 5, // average ~1.67
            ..Default::default()
        };
        assert_eq!(
            resolve(Tier::Auto, Demand::Light, Some(&watchdog_light)),
            resolve(Tier::High, Demand::Light, None),
        );
    }

    #[test]
    fn auto_caps_an_upgraded_critical_node_model_at_the_max_row() {
        let failing = NodeHistory {
            runs: 5,
            recent_first_pass_failures: 2,
            ..Default::default()
        };
        // Critical's high row is (opus, high); model would escalate to
        // fable, but critical's max-row model is opus, so it caps there.
        // Effort still escalates: high -> xhigh, under the max-effort ceiling.
        assert_eq!(
            resolve(Tier::Auto, Demand::Critical, Some(&failing)),
            Assignment {
                model: ModelAlias::Opus,
                effort: Effort::Xhigh,
            }
        );
    }

    #[test]
    fn auto_upgraded_peak_node_lands_exactly_on_the_max_row() {
        let failing = NodeHistory {
            runs: 5,
            recent_first_pass_failures: 2,
            ..Default::default()
        };
        // Peak's high row is (opus, xhigh); escalating one step each lands
        // exactly on peak's max row (fable, max) rather than exceeding it.
        assert_eq!(
            resolve(Tier::Auto, Demand::Peak, Some(&failing)),
            Assignment {
                model: ModelAlias::Fable,
                effort: Effort::Max,
            }
        );
    }

    #[test]
    fn the_auto_reason_names_exactly_the_steps_that_fired() {
        let none = NodeHistory::default();
        assert_eq!(auto_reason(Demand::Standard, None), "auto/high-row");
        assert_eq!(auto_reason(Demand::Standard, Some(&none)), "auto/high-row");

        let strong = NodeHistory {
            runs: 4,
            first_pass_successes: 4,
            ..Default::default()
        };
        assert_eq!(
            auto_reason(Demand::Standard, Some(&strong)),
            "auto/downgrade-standard"
        );
        assert_eq!(
            auto_reason(Demand::Peak, Some(&strong)),
            "auto/high-row",
            "step 3 is Standard-only"
        );

        let failing = NodeHistory {
            runs: 5,
            recent_first_pass_failures: 2,
            ..Default::default()
        };
        assert_eq!(
            auto_reason(Demand::Critical, Some(&failing)),
            "auto/escalate"
        );

        let both = NodeHistory {
            runs: 4,
            first_pass_successes: 4,
            recent_first_pass_failures: 2,
            ..Default::default()
        };
        assert_eq!(
            auto_reason(Demand::Standard, Some(&both)),
            "auto/downgrade-standard+escalate"
        );
    }

    #[test]
    fn the_auto_reason_never_describes_a_step_the_assignment_did_not_take() {
        let watchdogged = NodeHistory {
            runs: 2,
            watchdog_interventions: 4,
            ..Default::default()
        };
        assert_eq!(
            auto_reason(Demand::Standard, Some(&watchdogged)),
            "auto/escalate"
        );
        assert_eq!(
            resolve(Tier::Auto, Demand::Standard, Some(&watchdogged)),
            Assignment {
                // Standard's high row is (opus, high); escalation raises both
                // and step 6 caps the model at Standard's max row, opus.
                model: ModelAlias::Opus,
                effort: Effort::Xhigh,
            },
            "the escalation the reason claims is the one resolve applied"
        );
    }
}
