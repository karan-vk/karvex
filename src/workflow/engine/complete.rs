//! Completion evidence, output-schema validation, and succession.
//!
//! Implements `docs/design/workflow-builder/04-kvdag-and-execution.md` §3.3 and
//! §4.3. Two rules drive everything here: a result must validate against the
//! node's output schema before the node may succeed, and idle with no valid
//! result never completes a node.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::detect::AgentState;
use crate::workflow::engine::expand::ExpandProposal;
use crate::workflow::model::{
    Evidence, InstancePath, NodeKey, NodeResult, NodeStatus, OutputSchema, RawJson, RunGraph,
    RunNodeIdx, Runner, Succession,
};

/// `node_checkpoint.summary` budget (`03-storage-schema.md` §7). Over-budget
/// text is truncated with an explicit marker; the full text stays in the
/// payload.
pub const SUMMARY_BUDGET: usize = 1_200;

const TRUNCATION_MARKER: &str = "…[truncated]";

/// `AgentState::Idle` must hold for this many consecutive detector
/// observations before it is even considered as a completion signal (§4.3
/// precedence 3).
pub const SUSTAINED_IDLE_TICKS: u16 = 3;

/// The ordinal of a node's first reported result. The first invalid result
/// earns the single corrective re-prompt; every later one goes straight to
/// `NeedsAttention`.
///
/// Also the `NodeHistory` sense of "first pass" (`06-phase2-plan.md` §4 D8): a
/// node that succeeded on this ordinal succeeded without the correction.
pub const FIRST_REPORT: u8 = 1;

/// The one top-level key a reported result may carry that is **not** part of
/// the result (`06-phase2-plan.md` §4 D6, `04-kvdag-and-execution.md` §3.4).
///
/// `check` implements only `type`/`required`/`properties`/`items` and never
/// rejects unknown top-level keys, so a result carrying `expand` would validate
/// by accident and then flow into [`NodeResult::payload`], [`summarise`],
/// [`digest`], and the persisted checkpoint — and the digest is what Phase 3's
/// restore reads for cross-version compatibility. So the key is lifted out by
/// [`strip_expand`] **before** validation and **before** [`node_result`], and
/// the node prompt/output contract is unchanged: `expand` is an *optional
/// additional* key that never reaches the schema, the payload, or the digest.
pub const EXPAND_KEY: &str = "expand";

/// What the completion gate decided about one reported result.
#[derive(Debug, Clone, PartialEq)]
pub enum Completion {
    /// The result validated; the node may succeed.
    Accepted(Box<NodeResult>),
    /// First validation failure: one automatic corrective re-prompt quoting the
    /// errors, then no more.
    Reprompt { errors: Vec<SchemaViolation> },
    /// Still invalid after the corrective re-prompt, or no result at all.
    ///
    /// `resume_when` is not decoration: a `NeedsAttention` node stalls the run
    /// until a human acts, so the completion gate that decides a node is stuck
    /// also has to say what would unstick it. It travels beside `reason` rather
    /// than being defaulted at the call site so the code that knows *why* a node
    /// is blocked is the code that names the way out.
    NeedsAttention { reason: String, resume_when: String },
}

/// One output-schema violation, phrased so it can be quoted straight back to
/// the node in the corrective re-prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    pub at: String,
    pub message: String,
}

impl SchemaViolation {
    fn new(at: &str, message: impl Into<String>) -> Self {
        Self {
            at: at.to_string(),
            message: message.into(),
        }
    }

    /// Render for the corrective re-prompt and for the `NeedsAttention` reason.
    pub fn quote(&self) -> String {
        if self.at.is_empty() {
            self.message.clone()
        } else {
            format!("{}: {}", self.at, self.message)
        }
    }
}

/// The three completion signals of §4.3 in strict precedence order: a stronger
/// signal always wins, and a weaker later signal never downgrades the evidence
/// already recorded for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Signal {
    /// The node wrote `result.json` and called `kvx workflow node complete`.
    SelfReport,
    /// The bundled Claude hook reported the pane's turn ended.
    TurnEnd,
    /// `AgentState::Idle` held for [`SUSTAINED_IDLE_TICKS`] detector ticks.
    SustainedIdle,
}

impl Signal {
    pub fn evidence(self) -> Evidence {
        match self {
            Self::SelfReport => Evidence::SelfReport,
            Self::TurnEnd => Evidence::Hook,
            Self::SustainedIdle => Evidence::Detection,
        }
    }

    /// Signals 2 and 3 are the Claude hook and the manifest detector, so a
    /// `Runner::Command` node has signal 1 only (§4.3).
    pub fn available_for(self, runner: Runner) -> bool {
        match runner {
            Runner::Agent => true,
            Runner::Command => self == Self::SelfReport,
        }
    }

    /// True when `self` is the stronger of the two.
    pub fn outranks(self, other: Self) -> bool {
        self < other
    }
}

/// Per-node completion-signal bookkeeping: which signal has fired, and how long
/// the detector has reported `Idle`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SignalLedger {
    best: Option<Signal>,
    idle_ticks: u16,
}

impl SignalLedger {
    /// Records a signal. Returns `true` when it becomes the node's strongest.
    pub fn observe(&mut self, signal: Signal) -> bool {
        match self.best {
            Some(current) if !signal.outranks(current) => false,
            _ => {
                self.best = Some(signal);
                true
            }
        }
    }

    /// Feeds one detector observation. Returns `true` on the tick where `Idle`
    /// first becomes sustained; any non-idle observation resets the streak, so
    /// a single stray idle frame is never a completion signal.
    pub fn observe_agent_state(&mut self, state: AgentState) -> bool {
        if state != AgentState::Idle {
            self.idle_ticks = 0;
            return false;
        }
        self.idle_ticks = self.idle_ticks.saturating_add(1);
        if self.idle_ticks == SUSTAINED_IDLE_TICKS {
            self.observe(Signal::SustainedIdle);
            return true;
        }
        false
    }

    pub fn best(&self) -> Option<Signal> {
        self.best
    }

    pub fn idle_ticks(&self) -> u16 {
        self.idle_ticks
    }

    /// Restarts the idle streak without touching the evidence recorded so far.
    /// The sustained edge fires once per streak, so a caller that acted on that
    /// edge and wants the *next* streak to fire again resets it here rather
    /// than clearing the ledger and losing the signals it holds.
    pub fn reset_idle_streak(&mut self) {
        self.idle_ticks = 0;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Validates a reported result against the node's output schema.
///
/// Evaluates the same JSON Schema subset `OutputSchema::parse` accepts —
/// `type`, `required`, `properties`, `items` — and reports every violation, not
/// just the first, so one corrective re-prompt can quote them all.
pub fn validate(schema: &OutputSchema, result: &RawJson) -> Result<(), Vec<SchemaViolation>> {
    let mut violations = Vec::new();
    check(schema.as_value(), &result.0, "", &mut violations);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Lifts the optional top-level [`EXPAND_KEY`] out of a reported result.
///
/// Returns the result the completion gate actually sees and the raw `expand`
/// value, if there was one. A non-object result (an array, a scalar, `null`)
/// cannot carry the key and is handed back untouched, so this is a no-op for
/// every result shape that existed before Phase 2.
pub fn strip_expand(result: &RawJson) -> (RawJson, Option<serde_json::Value>) {
    let Some(fields) = result.0.as_object() else {
        return (result.clone(), None);
    };
    if !fields.contains_key(EXPAND_KEY) {
        return (result.clone(), None);
    }
    let mut stripped = fields.clone();
    let lifted = stripped.remove(EXPAND_KEY);
    (RawJson(serde_json::Value::Object(stripped)), lifted)
}

/// Parses a lifted `expand` value into proposals.
///
/// A malformed value is a **schema-class** violation of the result — the node
/// said something the contract does not allow — so the violations come back in
/// the same [`SchemaViolation`] vocabulary the output schema uses, each naming
/// the offending field, and the caller spends the node's single corrective
/// re-prompt on them exactly as it would on a schema failure (§4 D6).
///
/// The accepted shape is an array of objects: a non-empty string `template`, an
/// optional string `label` (defaulting to the template key), an optional object
/// `inputs` whose values are strings, and an optional integer `count` of at
/// least 1. Unknown keys inside an entry are ignored, matching [`check`]'s own
/// tolerance of unknown keys.
pub fn parse_expand(
    value: &serde_json::Value,
) -> Result<Vec<ExpandProposal>, Vec<SchemaViolation>> {
    let Some(entries) = value.as_array() else {
        return Err(vec![SchemaViolation::new(
            EXPAND_KEY,
            format!("expected type array, found {}", type_name(value)),
        )]);
    };

    let mut violations = Vec::new();
    let mut proposals = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let at = index_path(EXPAND_KEY, index);
        let Some(fields) = entry.as_object() else {
            violations.push(SchemaViolation::new(
                &at,
                format!("expected type object, found {}", type_name(entry)),
            ));
            continue;
        };

        let template = match fields.get("template") {
            None => {
                violations.push(SchemaViolation::new(
                    &at,
                    "missing required field \"template\"",
                ));
                continue;
            }
            Some(serde_json::Value::String(key)) if !key.trim().is_empty() => {
                NodeKey::new(key.as_str())
            }
            Some(serde_json::Value::String(_)) => {
                violations.push(SchemaViolation::new(
                    &join(&at, "template"),
                    "expected a non-empty node key",
                ));
                continue;
            }
            Some(other) => {
                violations.push(SchemaViolation::new(
                    &join(&at, "template"),
                    format!("expected type string, found {}", type_name(other)),
                ));
                continue;
            }
        };

        let label = match fields.get("label") {
            None | Some(serde_json::Value::Null) => template.as_str().to_string(),
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(other) => {
                violations.push(SchemaViolation::new(
                    &join(&at, "label"),
                    format!("expected type string, found {}", type_name(other)),
                ));
                continue;
            }
        };

        let mut inputs = BTreeMap::new();
        match fields.get("inputs") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (name, supplied) in map {
                    match supplied.as_str() {
                        Some(text) => {
                            inputs.insert(name.clone(), text.to_string());
                        }
                        None => violations.push(SchemaViolation::new(
                            &join(&join(&at, "inputs"), name),
                            format!("expected type string, found {}", type_name(supplied)),
                        )),
                    }
                }
            }
            Some(other) => {
                violations.push(SchemaViolation::new(
                    &join(&at, "inputs"),
                    format!("expected type object, found {}", type_name(other)),
                ));
                continue;
            }
        }

        let count = match fields.get("count") {
            None | Some(serde_json::Value::Null) => None,
            Some(supplied) => match supplied.as_u64().filter(|count| *count >= 1) {
                Some(count) => match u16::try_from(count) {
                    Ok(count) => Some(count),
                    Err(_) => {
                        violations.push(SchemaViolation::new(
                            &join(&at, "count"),
                            format!("expected an integer of at most {}", u16::MAX),
                        ));
                        continue;
                    }
                },
                None => {
                    violations.push(SchemaViolation::new(
                        &join(&at, "count"),
                        "expected an integer of at least 1",
                    ));
                    continue;
                }
            },
        };

        proposals.push(ExpandProposal {
            template,
            label,
            inputs,
            count,
        });
    }

    if violations.is_empty() {
        Ok(proposals)
    } else {
        Err(violations)
    }
}

/// Whether a violation came from the [`EXPAND_KEY`] channel rather than from the
/// node's output schema. The two are reported together but phrased apart, so a
/// node told its `expand` is malformed is not also told its payload failed a
/// schema that never saw the key.
fn is_expand_violation(violation: &SchemaViolation) -> bool {
    violation.at == EXPAND_KEY
        || violation.at.starts_with(&format!("{EXPAND_KEY}["))
        || violation.at.starts_with(&format!("{EXPAND_KEY}."))
}

/// Applies the completion contract to one reported result.
///
/// `report` is the 1-based ordinal of this reported result for the node, not
/// `RunNode::attempt` (which counts pane respawns): the first invalid result
/// earns exactly one corrective re-prompt, and every later invalid result goes
/// to `NeedsAttention`.
pub fn accept(
    schema: &OutputSchema,
    result: &RawJson,
    evidence: Evidence,
    report: u8,
) -> Completion {
    accept_with(schema, result, evidence, report, Vec::new())
}

/// [`accept`] with the schema-class violations the output schema cannot see.
///
/// The one producer of `extra` is [`parse_expand`]: the schema never sees
/// `expand`, so a malformed proposal has to be carried in beside the schema's
/// own verdict rather than discovered by it. Both kinds settle the same way —
/// one corrective re-prompt, then `NeedsAttention` — because both are the node
/// failing to say what the contract requires.
pub fn accept_with(
    schema: &OutputSchema,
    result: &RawJson,
    evidence: Evidence,
    report: u8,
    extra: Vec<SchemaViolation>,
) -> Completion {
    let mut errors = validate(schema, result).err().unwrap_or_default();
    errors.extend(extra);
    if errors.is_empty() {
        return Completion::Accepted(Box::new(node_result(result, evidence)));
    }
    if report <= FIRST_REPORT {
        return Completion::Reprompt { errors };
    }
    Completion::NeedsAttention {
        reason: format!(
            "result.json still fails output-schema validation after one corrective re-prompt: {}",
            describe(&errors)
        ),
        resume_when: RESUME_BY_CORRECTING_THE_RESULT.to_string(),
    }
}

/// The completion path when a signal arrives with no `result.json` at all.
///
/// **Idle with no valid result never completes a node** (§4.3): every signal
/// without an artifact lands here, and this is the single rule that makes the
/// design robust to turn-state edge cases.
pub fn missing_result(signal: Signal) -> Completion {
    Completion::NeedsAttention {
        reason: format!(
            "{} arrived with no result.json; a node completes only on a validated result artifact",
            match signal {
                Signal::SelfReport => "a self-report",
                Signal::TurnEnd => "a turn-end hook",
                Signal::SustainedIdle => "sustained idle",
            }
        ),
        resume_when: RESUME_BY_CORRECTING_THE_RESULT.to_string(),
    }
}

/// Both completion-gate blockers — no artifact at all, and an artifact that
/// still fails its schema — resume the same way, because the node is still
/// alive and holding its pane: tell it what to write, or start the attempt over.
///
/// `kvx workflow node complete` is deliberately **not** named here: it reads its
/// credential from the node's own environment, so it is the node's command, not
/// the watching human's. A resume condition that names a command the reader
/// cannot run is worse than none.
const RESUME_BY_CORRECTING_THE_RESULT: &str = "the node writes a result.json that satisfies its \
     output_schema; steer it with `kvx workflow node steer <run_id> <path> <text>`, or start the \
     attempt over with `kvx workflow node restart <run_id> <path>`";

/// The single corrective re-prompt, quoting the violations and the schema's own
/// required fields so the node is told exactly what to fill in.
///
/// A malformed `expand` is named as its own fault rather than folded into the
/// output-schema sentence: the schema never sees the key (§4 D6), so telling a
/// node its payload failed `./output_schema.json` when only `expand` is wrong
/// would send it looking in the wrong file.
pub fn corrective_prompt(schema: &OutputSchema, errors: &[SchemaViolation]) -> String {
    let expand_failed = errors.iter().any(is_expand_violation);
    let schema_failed = errors.iter().any(|error| !is_expand_violation(error));

    let mut text = String::new();
    if schema_failed {
        text.push_str("Your result.json does not validate against ./output_schema.json. ");
    }
    if expand_failed {
        text.push_str("Your result.json's `expand` field is malformed. ");
    }
    text.push_str("Fix these and re-run `kvx workflow node complete`:\n");
    for error in errors {
        text.push_str("- ");
        text.push_str(&error.quote());
        text.push('\n');
    }
    let required = schema.required_fields();
    if !required.is_empty() {
        text.push_str("Required fields: ");
        text.push_str(&required.join(", "));
        text.push('\n');
    }
    if expand_failed {
        text.push_str(
            "`expand` is optional. When present it must be an array of objects, each with a \
             non-empty string \"template\", an optional string \"label\", an optional object \
             \"inputs\" whose values are strings, and an optional integer \"count\" of at least 1.\n",
        );
    }
    text
}

/// A node reached a terminal status with nothing to record (§3.3). The engine
/// transitions it to `NeedsAttention` rather than letting the branch evaporate
/// while the run reports success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessionGap {
    pub path: InstancePath,
    pub reason: String,
}

/// Resolves the succession a closing node must record (§3.3).
///
/// An explicitly recorded succession — the blocker the engine writes when a
/// node fails or a run is cancelled — always wins; everything else is derived
/// from the node's status, its validated result, and whether any outbound edge
/// fired.
pub fn resolve_succession(graph: &RunGraph, idx: RunNodeIdx) -> Result<Succession, SuccessionGap> {
    let Some(node) = graph.node(idx) else {
        return Err(SuccessionGap {
            path: InstancePath::new(""),
            reason: "no such node in the run graph".to_string(),
        });
    };
    let gap = |reason: &str| SuccessionGap {
        path: node.path.clone(),
        reason: reason.to_string(),
    };

    if let Some(existing) = &node.succession {
        return Ok(existing.clone());
    }

    match node.status {
        NodeStatus::Skipped => Ok(Succession::NoFollowup {
            evidence: "every inbound edge was dead; the branch was not taken".to_string(),
        }),
        NodeStatus::Succeeded | NodeStatus::Restored => {
            if node.result.is_none() {
                return Err(gap("closed with no validated result"));
            }
            let outbound: Vec<usize> = graph.outbound(idx).collect();
            let dead = outbound
                .iter()
                .filter_map(|index| graph.edges.get(*index))
                .filter(|edge| edge.condition_result == Some(false))
                .count();
            if !outbound.is_empty() && dead == outbound.len() {
                Ok(Succession::NoFollowup {
                    evidence: format!("all {dead} outbound edges evaluated false"),
                })
            } else {
                Ok(Succession::Satisfied)
            }
        }
        _ => Err(gap("closed without recording a succession")),
    }
}

/// Builds the checkpointed result from a validated payload.
fn node_result(result: &RawJson, evidence: Evidence) -> NodeResult {
    NodeResult {
        payload: result.0.clone(),
        summary: summarise(&result.0),
        artifact_paths: artifact_paths(&result.0),
        digest: digest(&result.0),
        evidence,
    }
}

/// Token-lean handoff text: the payload's own `summary` when it has one, else a
/// canonical rendering of the payload, always inside [`SUMMARY_BUDGET`].
pub fn summarise(payload: &serde_json::Value) -> String {
    let raw = payload
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| canonical(payload).to_string(), str::to_string);
    truncate(&raw)
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= SUMMARY_BUDGET {
        return text.to_string();
    }
    let keep = SUMMARY_BUDGET.saturating_sub(TRUNCATION_MARKER.chars().count());
    let mut truncated: String = text.chars().take(keep).collect();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

/// Artifact paths the node indexed into its result, so the checkpoint records
/// where the large output actually lives (`04` §4.1 `artifacts/`).
fn artifact_paths(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// SHA-256 over the canonical rendering, so two runs producing the same result
/// produce the same digest regardless of key order.
pub fn digest(payload: &serde_json::Value) -> String {
    format!("{:x}", Sha256::digest(canonical(payload).to_string()))
}

/// Rebuilds a value with object keys in sorted order. `serde_json::Map` is a
/// `BTreeMap` unless `preserve_order` is enabled somewhere in the dependency
/// graph, so the sort is done explicitly rather than assumed.
fn canonical(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::with_capacity(keys.len());
            for key in keys {
                if let Some(entry) = map.get(key) {
                    sorted.insert(key.clone(), canonical(entry));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical).collect())
        }
        other => other.clone(),
    }
}

fn describe(errors: &[SchemaViolation]) -> String {
    errors
        .iter()
        .map(SchemaViolation::quote)
        .collect::<Vec<String>>()
        .join("; ")
}

// ── JSON Schema subset evaluator ────────────────────────────────────────────

fn check(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    at: &str,
    out: &mut Vec<SchemaViolation>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };

    if let Some(declared) = object.get("type") {
        let names = type_names(declared);
        if !names.is_empty() && !names.iter().any(|name| matches_type(name, value)) {
            out.push(SchemaViolation::new(
                at,
                format!(
                    "expected type {}, found {}",
                    names.join(" or "),
                    type_name(value)
                ),
            ));
            // Reporting every child of a wrong-typed value would bury the one
            // violation the node actually has to fix.
            return;
        }
    }

    if let Some(required) = object.get("required").and_then(serde_json::Value::as_array) {
        match value.as_object() {
            Some(fields) => {
                for name in required.iter().filter_map(serde_json::Value::as_str) {
                    if !fields.contains_key(name) {
                        out.push(SchemaViolation::new(
                            at,
                            format!("missing required field \"{name}\""),
                        ));
                    }
                }
            }
            None => out.push(SchemaViolation::new(
                at,
                "expected an object carrying the required fields",
            )),
        }
    }

    if let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    {
        if let Some(fields) = value.as_object() {
            for (name, property) in properties {
                if let Some(found) = fields.get(name) {
                    check(property, found, &join(at, name), out);
                }
            }
        }
    }

    if let Some(items) = object.get("items") {
        if let Some(entries) = value.as_array() {
            match items {
                serde_json::Value::Array(schemas) => {
                    for (index, (item_schema, entry)) in schemas.iter().zip(entries).enumerate() {
                        check(item_schema, entry, &index_path(at, index), out);
                    }
                }
                single => {
                    for (index, entry) in entries.iter().enumerate() {
                        check(single, entry, &index_path(at, index), out);
                    }
                }
            }
        }
    }
}

fn type_names(declared: &serde_json::Value) -> Vec<&str> {
    match declared {
        serde_json::Value::Array(entries) => entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect(),
        other => other.as_str().into_iter().collect(),
    }
}

fn matches_type(name: &str, value: &serde_json::Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => true,
    }
}

fn type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn join(at: &str, segment: &str) -> String {
    if at.is_empty() {
        segment.to_string()
    } else {
        format!("{at}.{segment}")
    }
}

fn index_path(at: &str, index: usize) -> String {
    format!("{at}[{index}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine::tests_support::{edge, graph_of, node_at, set_result, TestNode};
    use crate::workflow::model::{Condition, EdgeKind, FieldPath, JsonScalar};

    fn json(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("test json parses")
    }

    fn schema(raw: &str) -> OutputSchema {
        OutputSchema::parse(json(raw)).expect("test schema parses")
    }

    fn report(raw: &str) -> RawJson {
        RawJson(json(raw))
    }

    #[test]
    fn a_valid_result_is_accepted_and_checkpointed() {
        let schema = schema(
            r#"{"type":"object","required":["plan"],"properties":{"plan":{"type":"string"}}}"#,
        );
        let result = report(r#"{"plan":"ship it","summary":"planned","artifacts":["out/a.md"]}"#);

        let Completion::Accepted(accepted) = accept(&schema, &result, Evidence::SelfReport, 1)
        else {
            panic!("a schema-valid result must be accepted");
        };
        assert_eq!(accepted.summary, "planned");
        assert_eq!(accepted.artifact_paths, vec!["out/a.md".to_string()]);
        assert_eq!(accepted.evidence, Evidence::SelfReport);
        assert_eq!(accepted.digest.len(), 64);
    }

    #[test]
    fn an_invalid_result_reprompts_exactly_once_then_needs_attention() {
        let schema = schema(
            r#"{"type":"object","required":["plan"],"properties":{"plan":{"type":"string"}}}"#,
        );
        let result = report(r#"{"notes":"forgot the plan"}"#);

        let Completion::Reprompt { errors } = accept(&schema, &result, Evidence::SelfReport, 1)
        else {
            panic!("the first invalid result earns one corrective re-prompt");
        };
        assert_eq!(
            errors,
            vec![SchemaViolation::new("", "missing required field \"plan\"")]
        );
        let prompt = corrective_prompt(&schema, &errors);
        assert!(prompt.contains("missing required field \"plan\""));
        assert!(prompt.contains("Required fields: plan"));

        let Completion::NeedsAttention {
            reason,
            resume_when,
        } = accept(&schema, &result, Evidence::SelfReport, 2)
        else {
            panic!("the second invalid result must not earn another re-prompt");
        };
        assert!(reason.contains("missing required field \"plan\""));
        assert!(
            resume_when.contains("kvx workflow node restart"),
            "giving up on a result names how the user resumes: {resume_when}"
        );
    }

    #[test]
    fn validation_covers_the_declared_json_schema_subset() {
        let schema = schema(
            r#"{
                "type":"object",
                "required":["changed_files","report","count"],
                "properties":{
                    "changed_files":{"type":"array","items":{"type":"string"}},
                    "report":{"type":"string"},
                    "count":{"type":"integer"},
                    "nested":{"type":"object","required":["inner"]}
                }
            }"#,
        );

        assert!(validate(
            &schema,
            &report(r#"{"changed_files":["a.rs"],"report":"done","count":2}"#)
        )
        .is_ok());

        let errors = validate(
            &schema,
            &report(
                r#"{"changed_files":["a.rs",7],"report":5,"count":1.5,"nested":{"other":true}}"#,
            ),
        )
        .expect_err("every violation is reported");
        let quoted: Vec<String> = errors.iter().map(SchemaViolation::quote).collect();
        assert!(quoted
            .iter()
            .any(|line| line.starts_with("changed_files[1]")));
        assert!(quoted.iter().any(|line| line.starts_with("report")));
        assert!(quoted.iter().any(|line| line.starts_with("count")));
        assert!(quoted
            .iter()
            .any(|line| line.contains("missing required field \"inner\"")));
    }

    #[test]
    fn a_multi_type_declaration_accepts_any_listed_type() {
        let schema =
            schema(r#"{"type":"object","properties":{"note":{"type":["string","null"]}}}"#);
        assert!(validate(&schema, &report(r#"{"note":null}"#)).is_ok());
        assert!(validate(&schema, &report(r#"{"note":"hi"}"#)).is_ok());
        assert!(validate(&schema, &report(r#"{"note":3}"#)).is_err());
    }

    #[test]
    fn a_signal_with_no_result_never_completes_a_node() {
        for signal in [Signal::SelfReport, Signal::TurnEnd, Signal::SustainedIdle] {
            let Completion::NeedsAttention {
                reason,
                resume_when,
            } = missing_result(signal)
            else {
                panic!("no artifact can ever be a completion");
            };
            assert!(reason.contains("no result.json"));
            assert!(
                resume_when.contains("kvx workflow node steer")
                    && resume_when.contains("kvx workflow node restart"),
                "the blocker names commands the watching human can actually run: {resume_when}"
            );
            assert!(
                !resume_when.contains("kvx workflow node complete"),
                "`node complete` reads the node's own env credential, so it is not the \
                 human's command: {resume_when}"
            );
        }
    }

    #[test]
    fn signals_rank_in_the_documented_precedence() {
        assert!(Signal::SelfReport.outranks(Signal::TurnEnd));
        assert!(Signal::TurnEnd.outranks(Signal::SustainedIdle));
        assert!(!Signal::SustainedIdle.outranks(Signal::SelfReport));
        assert_eq!(Signal::SelfReport.evidence(), Evidence::SelfReport);
        assert_eq!(Signal::TurnEnd.evidence(), Evidence::Hook);
        assert_eq!(Signal::SustainedIdle.evidence(), Evidence::Detection);

        assert!(Signal::SelfReport.available_for(Runner::Command));
        assert!(!Signal::TurnEnd.available_for(Runner::Command));
        assert!(!Signal::SustainedIdle.available_for(Runner::Command));
        assert!(Signal::SustainedIdle.available_for(Runner::Agent));
    }

    #[test]
    fn a_weaker_signal_never_downgrades_the_recorded_evidence() {
        let mut ledger = SignalLedger::default();
        assert!(ledger.observe(Signal::TurnEnd));
        assert_eq!(ledger.best(), Some(Signal::TurnEnd));
        assert!(!ledger.observe(Signal::SustainedIdle));
        assert_eq!(ledger.best(), Some(Signal::TurnEnd));
        assert!(ledger.observe(Signal::SelfReport));
        assert_eq!(ledger.best(), Some(Signal::SelfReport));
    }

    #[test]
    fn idle_becomes_a_signal_only_once_it_is_sustained() {
        let mut ledger = SignalLedger::default();
        assert!(!ledger.observe_agent_state(AgentState::Idle));
        assert!(!ledger.observe_agent_state(AgentState::Idle));
        assert!(!ledger.observe_agent_state(AgentState::Working));
        assert_eq!(ledger.idle_ticks(), 0);
        assert_eq!(ledger.best(), None);

        assert!(!ledger.observe_agent_state(AgentState::Idle));
        assert!(!ledger.observe_agent_state(AgentState::Idle));
        assert!(ledger.observe_agent_state(AgentState::Idle));
        assert_eq!(ledger.best(), Some(Signal::SustainedIdle));
        assert!(
            !ledger.observe_agent_state(AgentState::Idle),
            "the sustained edge fires once, not on every later tick"
        );
    }

    #[test]
    fn a_succeeded_node_with_a_live_outbound_edge_is_satisfied() {
        let mut graph = graph_of(
            &[TestNode::new("plan"), TestNode::new("implement")],
            &[edge(0, 1, EdgeKind::Data)],
        );
        crate::workflow::engine::schedule::propagate(&mut graph);
        set_result(&mut graph, "plan", json(r#"{"plan":"x"}"#));
        crate::workflow::engine::schedule::propagate(&mut graph);

        assert_eq!(
            resolve_succession(&graph, node_at(&graph, "plan").idx),
            Ok(Succession::Satisfied)
        );
    }

    #[test]
    fn a_leaf_with_a_validated_result_is_satisfied() {
        let mut graph = graph_of(&[TestNode::new("only")], &[]);
        crate::workflow::engine::schedule::propagate(&mut graph);
        set_result(&mut graph, "only", json(r#"{"done":true}"#));

        assert_eq!(
            resolve_succession(&graph, node_at(&graph, "only").idx),
            Ok(Succession::Satisfied)
        );
    }

    #[test]
    fn a_node_whose_every_outbound_edge_died_records_no_followup() {
        let mut graph = graph_of(
            &[TestNode::new("gate"), TestNode::new("hotfix")],
            &[
                edge(0, 1, EdgeKind::Conditional).with_condition(Condition::Eq {
                    path: FieldPath("verdict".to_string()),
                    value: JsonScalar::String("fail".to_string()),
                }),
            ],
        );
        crate::workflow::engine::schedule::propagate(&mut graph);
        set_result(&mut graph, "gate", json(r#"{"verdict":"pass"}"#));
        crate::workflow::engine::schedule::propagate(&mut graph);

        let Ok(Succession::NoFollowup { evidence }) =
            resolve_succession(&graph, node_at(&graph, "gate").idx)
        else {
            panic!("a branch that legitimately produced nothing records no_followup");
        };
        assert!(evidence.contains("outbound edges evaluated false"));
    }

    #[test]
    fn a_skipped_node_records_no_followup() {
        let mut graph = graph_of(&[TestNode::new("only")], &[]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Skipped;
        }
        assert!(matches!(
            resolve_succession(&graph, idx),
            Ok(Succession::NoFollowup { .. })
        ));
    }

    #[test]
    fn a_terminal_node_with_nothing_to_record_is_a_succession_gap() {
        let mut graph = graph_of(&[TestNode::new("only")], &[]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Succeeded;
        }
        let gap = resolve_succession(&graph, idx).expect_err("no result means no succession");
        assert_eq!(gap.path, InstancePath::new("only"));

        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Failed;
        }
        assert!(resolve_succession(&graph, idx).is_err());
    }

    #[test]
    fn an_explicitly_recorded_succession_wins() {
        let mut graph = graph_of(&[TestNode::new("only")], &[]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Failed;
            node.succession = Some(Succession::Blocked {
                reason: "pane exited with code 1".to_string(),
                resume_when: "restart the node".to_string(),
            });
        }
        assert!(matches!(
            resolve_succession(&graph, idx),
            Ok(Succession::Blocked { .. })
        ));
    }

    #[test]
    fn a_summary_is_derived_and_budgeted() {
        assert_eq!(summarise(&json(r#"{"summary":"short"}"#)), "short");
        assert_eq!(
            summarise(&json(r#"{"b":1,"a":2}"#)),
            r#"{"a":2,"b":1}"#,
            "a payload without a summary is rendered canonically"
        );

        let long = "x".repeat(SUMMARY_BUDGET + 500);
        let payload = serde_json::json!({ "summary": long });
        let summary = summarise(&payload);
        assert_eq!(summary.chars().count(), SUMMARY_BUDGET);
        assert!(summary.ends_with(TRUNCATION_MARKER));
    }

    /// §4 D6: the gate must never see the key, and every result shape that
    /// existed before Phase 2 must come through untouched.
    #[test]
    fn strip_expand_lifts_only_the_expand_key() {
        let (stripped, lifted) = strip_expand(&report(r#"{"plan":"x","expand":[{"a":1}]}"#));
        assert_eq!(stripped.0, json(r#"{"plan":"x"}"#));
        assert_eq!(lifted, Some(json(r#"[{"a":1}]"#)));

        for untouched in [
            r#"{"plan":"x"}"#,
            r#"{}"#,
            r#"[1,2]"#,
            r#""a string""#,
            r#"null"#,
        ] {
            let (stripped, lifted) = strip_expand(&report(untouched));
            assert_eq!(stripped.0, json(untouched), "unchanged for {untouched}");
            assert_eq!(lifted, None, "nothing lifted from {untouched}");
        }

        // An explicit null still counts as the key being present: the node said
        // something about `expand`, and what it said has to be judged.
        let (stripped, lifted) = strip_expand(&report(r#"{"plan":"x","expand":null}"#));
        assert_eq!(stripped.0, json(r#"{"plan":"x"}"#));
        assert_eq!(lifted, Some(serde_json::Value::Null));
    }

    #[test]
    fn parse_expand_reads_the_documented_shape_with_its_defaults() {
        let proposals = parse_expand(&json(
            r#"[
                {"template":"worker"},
                {"template":"worker","label":"deep dive","inputs":{"focus":"api"},"count":3}
            ]"#,
        ))
        .expect("a well-formed expand parses");

        assert_eq!(proposals.len(), 2);
        assert_eq!(proposals[0].template, NodeKey::new("worker"));
        assert_eq!(
            proposals[0].label, "worker",
            "an omitted label falls back to the template key"
        );
        assert!(proposals[0].inputs.is_empty());
        assert_eq!(proposals[0].count, None);

        assert_eq!(proposals[1].label, "deep dive");
        assert_eq!(
            proposals[1].inputs.get("focus").map(String::as_str),
            Some("api")
        );
        assert_eq!(proposals[1].count, Some(3));

        assert_eq!(
            parse_expand(&json("[]")).expect("an empty array is well formed"),
            Vec::new()
        );
    }

    /// A malformed `expand` is answered in the schema vocabulary, and every
    /// violation names the field it is about — otherwise the node's one
    /// corrective re-prompt is spent telling it nothing it can act on.
    #[test]
    fn every_malformed_expand_names_the_field_it_is_about() {
        let cases = [
            (r#"{"template":"w"}"#, "expand"),
            (r#"[3]"#, "expand[0]"),
            (r#"[{}]"#, "expand[0]"),
            (r#"[{"template":7}]"#, "expand[0].template"),
            (r#"[{"template":"  "}]"#, "expand[0].template"),
            (r#"[{"template":"w","label":7}]"#, "expand[0].label"),
            (r#"[{"template":"w","inputs":[]}]"#, "expand[0].inputs"),
            (
                r#"[{"template":"w","inputs":{"k":7}}]"#,
                "expand[0].inputs.k",
            ),
            (r#"[{"template":"w","count":0}]"#, "expand[0].count"),
            (r#"[{"template":"w","count":-1}]"#, "expand[0].count"),
            (r#"[{"template":"w","count":99999}]"#, "expand[0].count"),
            (r#"[{"template":"w"},{"label":"x"}]"#, "expand[1]"),
        ];
        for (raw, at) in cases {
            let errors = parse_expand(&json(raw)).expect_err("malformed: {raw}");
            assert!(
                errors.iter().any(|error| error.at == at),
                "{raw} must report a violation at {at}, got {errors:?}"
            );
            assert!(
                errors.iter().all(|error| !error.message.is_empty()),
                "{raw} must say what is wrong"
            );
        }
    }

    /// The schema never sees `expand`, so a node whose only fault is the
    /// proposal must not be sent looking in `./output_schema.json`.
    #[test]
    fn the_corrective_prompt_blames_expand_only_when_expand_is_what_failed() {
        let schema = schema(
            r#"{"type":"object","required":["plan"],"properties":{"plan":{"type":"string"}}}"#,
        );

        let expand_only = parse_expand(&json(r#""nope""#)).expect_err("malformed");
        let text = corrective_prompt(&schema, &expand_only);
        assert!(text.contains("`expand` field is malformed"));
        assert!(
            !text.contains("does not validate against ./output_schema.json"),
            "the payload did not fail a schema that never saw the key"
        );
        assert!(text.contains("\"template\""), "the contract is restated");

        let schema_only = validate(&schema, &report(r#"{"notes":"oops"}"#)).expect_err("invalid");
        let text = corrective_prompt(&schema, &schema_only);
        assert!(text.contains("does not validate against ./output_schema.json"));
        assert!(!text.contains("`expand`"));

        let mut both = schema_only;
        both.extend(expand_only);
        let text = corrective_prompt(&schema, &both);
        assert!(text.contains("does not validate against ./output_schema.json"));
        assert!(text.contains("`expand` field is malformed"));
    }

    #[test]
    fn accept_with_settles_a_non_schema_violation_exactly_like_a_schema_one() {
        let schema = schema(r#"{"type":"object","required":["plan"]}"#);
        let valid = report(r#"{"plan":"ship it"}"#);
        let extra = vec![SchemaViolation::new(
            "expand",
            "expected type array, found string",
        )];

        assert!(matches!(
            accept_with(&schema, &valid, Evidence::SelfReport, 1, extra.clone()),
            Completion::Reprompt { .. }
        ));
        assert!(matches!(
            accept_with(&schema, &valid, Evidence::SelfReport, 2, extra),
            Completion::NeedsAttention { .. }
        ));
        assert!(matches!(
            accept_with(&schema, &valid, Evidence::SelfReport, 1, Vec::new()),
            Completion::Accepted(_)
        ));
    }

    #[test]
    fn digests_ignore_key_order() {
        assert_eq!(
            digest(&json(r#"{"a":1,"b":[{"y":2,"x":3}]}"#)),
            digest(&json(r#"{"b":[{"x":3,"y":2}],"a":1}"#))
        );
        assert_ne!(digest(&json(r#"{"a":1}"#)), digest(&json(r#"{"a":2}"#)));
    }
}
