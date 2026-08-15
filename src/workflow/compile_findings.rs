//! Pure compilation of accepted review findings into a new kvdag definition.
//!
//! `phase4-retarget-plan.md` §3.5 "Accept and compile", §5 packet P11. The
//! review cycle records findings **wholesale** and applies them
//! **selectively**: a human accepts or declines each one, and only the
//! accepted set reaches this module. What comes out is a [`KvdagSpec`] the
//! adapter hands to `create_version_with_metadata` with
//! `origin: self_improvement` and the run's own version as the explicit
//! parent — karvex mints a new immutable revision, it never edits the one the
//! run executed.
//!
//! Four rules this module exists to hold, in the order they matter:
//!
//! 1. **A declined finding leaves no trace.** Only [`AcceptedFinding`]s are
//!    passed in, they are the only thing read, and the merge is a pure
//!    function of them — there is no path here that can consult a finding the
//!    human said no to.
//! 2. **All or nothing.** Every refusal is a `Result::Err` *before* the
//!    caller has anything to write, and the error names the finding that
//!    caused it. A half-applied version is not representable: the merged
//!    nodes only leave this module through a successful `Ok`.
//! 3. **A compiled version is held to the authoring standard.** The same
//!    `Kvdag::try_new` gate a hand-written document passes, plus
//!    [`crate::workflow::definition::worktree_isolation_rejection`] — the
//!    authoring rule the lead path cannot honour (P14, §6 D-6). A
//!    self-improved version can never be a definition a human would have been
//!    refused.
//! 4. **Attribution survives compilation.** [`FindingAttribution`] carries
//!    what `review::finding_seed` stamped onto the finding — whether a
//!    teammate actually said this in a resumed interview, or karvex inferred
//!    it from evidence alone — into the minted version's `change_summary`
//!    ([`change_summary`]), so the provenance is readable from the version
//!    chain and not only from the review rows.
//!
//! Pure in the sense every other `src/workflow` module is: values in, values
//! out. No store, no `App`, no filesystem, no clock. The adapter
//! (`app/api/workflow_review_apply.rs`) reads the rows, calls
//! [`apply_findings`], and writes the result.
//!
//! **No edge surgery** (`08` D14, restated by §3.5): findings change nodes,
//! never the graph's shape. A structural verdict that really means "these two
//! nodes should not be connected" is a human's authoring edit, not something
//! karvex infers from an interview.

use std::collections::{BTreeMap, BTreeSet};

use crate::workflow::definition::worktree_isolation_rejection;
use crate::workflow::model::{
    Demand, InterviewMode, Kvdag, KvdagError, KvdagNode, KvdagSpec, NodeKey,
};
use crate::workflow::review::{FindingLevel, FindingVerdict};

/// The node fields a `prompt`-level `improve` may merge (§3.5, §5 P11): the
/// node's brief and its role sentence. Nothing that changes what the node
/// *is*.
pub const PROMPT_FIELDS: [&str; 2] = ["prompt_template", "role"];

/// The node fields a `structural` `improve` may merge: the plan's knobs —
/// how demanding the work is, how long it may take, how many attempts it
/// gets. Not the node's identity (`key`), not its wiring (edges), not its
/// runner.
pub const STRUCTURAL_FIELDS: [&str; 3] = ["demand", "timeout_ms", "max_attempts"];

/// The pseudo-field a `replace` claims, so a replacement and a field merge on
/// the same node are detected as the conflict they are rather than silently
/// racing each other.
const WHOLE_NODE: &str = "the whole node";

// ── attribution ────────────────────────────────────────────────────────────

/// Where one finding's account came from, recovered from the stored row.
///
/// `review::finding_seed` wraps the synthesiser's own evidence as
/// `{"reported": …, "attribution": {"member", "interview_mode", "reason"?}}`
/// precisely so this distinction survives the round trip through the store:
/// a finding karvex inferred from evidence alone must stay distinguishable
/// from one a teammate actually said, forever, including in the version it
/// compiles into. [`Self::from_seed_evidence`] is the reader of that shape;
/// the test `attribution_round_trips_through_review_finding_seed` pins the two
/// halves together so the writer cannot drift away from the reader.
/// Deliberately not `Default`: there is no honest default attribution. Every
/// value is read from a stored finding through [`Self::from_seed_evidence`],
/// which takes the interview mode as an argument precisely because guessing it
/// is the failure this type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingAttribution {
    /// The member whose account this is, when the synthesiser named one. A
    /// finding about the run rather than about a teammate has none, and that
    /// is honest rather than missing.
    pub member: Option<String>,
    pub mode: InterviewMode,
    /// Why the interview was `evidence_only`, when the seed recorded one
    /// (`review::EvidenceOnlyReason`, carried as its string).
    pub reason: Option<String>,
}

impl FindingAttribution {
    /// Reads the `attribution` object `review::finding_seed` stamps onto a
    /// finding's evidence.
    ///
    /// `mode` is the `review_finding.interview_mode` **column**, which is the
    /// store's own typed copy of the same decision and the authority here: a
    /// missing or malformed evidence blob degrades the member name and the
    /// reason, never the resumed/evidence-only fact itself.
    pub fn from_seed_evidence(evidence: &serde_json::Value, mode: InterviewMode) -> Self {
        let attribution = evidence.get("attribution");
        let member = attribution
            .and_then(|value| value.get("member"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|member| !member.is_empty())
            .map(str::to_string);
        let reason = attribution
            .and_then(|value| value.get("reason"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string);
        Self {
            member,
            mode,
            reason,
        }
    }

    /// One clause a human can read in a version's `change_summary`, e.g.
    /// `research's own account` or `evidence only: no_session_id`.
    ///
    /// Never says "said" for an `evidence_only` finding, whatever the
    /// synthesiser claimed: this is the same honesty rule
    /// `review::Attribution::resolve` enforces, restated at the last point
    /// where the fact is still visible.
    pub fn describe(&self) -> String {
        match (self.mode, self.member.as_deref(), self.reason.as_deref()) {
            (InterviewMode::Resumed, Some(member), _) => format!("{member}'s own account"),
            (InterviewMode::Resumed, None, _) => "an interviewed teammate's account".to_string(),
            (InterviewMode::EvidenceOnly, Some(member), Some(reason)) => {
                format!("evidence only for {member}: {reason}")
            }
            (InterviewMode::EvidenceOnly, Some(member), None) => {
                format!("evidence only for {member}")
            }
            (InterviewMode::EvidenceOnly, None, Some(reason)) => {
                format!("evidence only: {reason}")
            }
            (InterviewMode::EvidenceOnly, None, None) => "evidence only".to_string(),
        }
    }
}

// ── input ──────────────────────────────────────────────────────────────────

/// One finding the human accepted, reduced to what compiling it needs.
///
/// Deliberately not the store's `ReviewFindingRecord`: this layer has no
/// store types, and the record carries `level`/`verdict` as raw strings the
/// adapter has already had to parse into the vocabulary `workflow::review`
/// owns — so the parse failure is the adapter's to report, and everything
/// here is total over its inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptedFinding {
    pub node_key: NodeKey,
    pub level: FindingLevel,
    pub verdict: FindingVerdict,
    /// The concrete change, as the synthesiser wrote it: an object whose keys
    /// are node fields. `null` or `{}` means "nothing proposed".
    pub proposed_change: serde_json::Value,
    /// A whole node document, mandatory for [`FindingVerdict::Replace`].
    pub replacement: Option<serde_json::Value>,
    pub attribution: FindingAttribution,
}

impl AcceptedFinding {
    /// The `prompt/improve`-style label used in errors and change summaries.
    fn kind(&self) -> String {
        format!("{}/{}", self.level.as_str(), self.verdict.as_str())
    }
}

// ── failure ────────────────────────────────────────────────────────────────

/// Why an accepted set of findings could not become a definition.
///
/// Every variant names a `node_key`, because every variant is a refusal a
/// human has to act on by *changing which findings they accept* — and that
/// choice is per node key (the CLI's `--accept <node_key>`, the store's
/// `finding_mark_applied(cycle, keys, version)`).
#[derive(Debug, Clone, PartialEq)]
pub enum CompileError {
    /// The finding is about a node this definition does not have — an
    /// emergent `.task/…` the lead invented, a `.lead` finding, or a node key
    /// that only ever existed in a later version.
    UnknownNode { node_key: NodeKey },
    /// `verdict = "replace"` with no `replacement`. The store's own
    /// `review_finding_replace_requires_replacement` event refuses this at
    /// write time too; this is the copy that runs before anything is written.
    ReplaceWithoutReplacement { node_key: NodeKey },
    /// The replacement is not a node document.
    ReplacementNotANode { node_key: NodeKey, error: String },
    /// The replacement renames the node. Node keys are the definition's
    /// stable identity — edges, checkpoints, and every past run's history
    /// address them — and this compiler does no edge surgery, so a rename
    /// would dangle every edge that names the old key.
    ReplacementRenamesNode {
        node_key: NodeKey,
        replacement_key: NodeKey,
    },
    /// `proposed_change` is not a JSON object of node fields.
    ProposedChangeNotAnObject { node_key: NodeKey },
    /// An `improve` that proposes nothing. Accepting it would mint a version
    /// that claims to carry a change it does not.
    ImproveWithoutChange { node_key: NodeKey },
    /// A `keep` that proposes a change. `keep` means "we looked and it was
    /// fine"; applying nothing while the finding says otherwise would be a
    /// silent no-op, and dropping the change on the floor is exactly the
    /// dishonesty this cycle exists to remove.
    KeepProposesChange { node_key: NodeKey },
    /// The field is not one this `(level, verdict)` may merge.
    UnsupportedField {
        node_key: NodeKey,
        kind: String,
        field: String,
        allowed: Vec<&'static str>,
    },
    /// The field is allowed but the value is not.
    InvalidFieldValue {
        node_key: NodeKey,
        field: String,
        error: String,
    },
    /// Two accepted findings write the same node field (or one replaces a
    /// node another one edits). Whichever karvex applied last would decide
    /// the version, so it applies neither and says so.
    ConflictingFindings { node_key: NodeKey, field: String },
    /// The merged definition fails the same graph validation a hand-written
    /// one does (`Kvdag::try_new`).
    Invalid {
        /// The accepted finding that changed the node the validator names,
        /// when it was one — `None` when the parent version already carried
        /// the problem and no accepted finding is to blame.
        node_key: Option<NodeKey>,
        error: KvdagError,
    },
    /// The merged definition trips an authoring rule the lead path cannot
    /// honour (today: `isolation = "worktree"`, §6 D-6). Same `node_key:
    /// None` reading as [`Self::Invalid`].
    Unauthorable {
        node_key: Option<NodeKey>,
        message: String,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownNode { node_key } => write!(
                f,
                "the accepted finding for \"{node_key}\" names no node in this definition, \
                 so there is nothing to apply"
            ),
            Self::ReplaceWithoutReplacement { node_key } => write!(
                f,
                "the accepted finding for \"{node_key}\" says replace but carries no replacement"
            ),
            Self::ReplacementNotANode { node_key, error } => write!(
                f,
                "the accepted finding for \"{node_key}\" carries a replacement that is not a \
                 node document: {error}"
            ),
            Self::ReplacementRenamesNode {
                node_key,
                replacement_key,
            } => write!(
                f,
                "the accepted finding for \"{node_key}\" carries a replacement keyed \
                 \"{replacement_key}\"; a review may replace what a node does, never its key \
                 — every edge and every past run addresses the old one"
            ),
            Self::ProposedChangeNotAnObject { node_key } => write!(
                f,
                "the accepted finding for \"{node_key}\" carries a proposed_change that is not \
                 an object of node fields"
            ),
            Self::ImproveWithoutChange { node_key } => write!(
                f,
                "the accepted finding for \"{node_key}\" says improve but proposes no change"
            ),
            Self::KeepProposesChange { node_key } => write!(
                f,
                "the accepted finding for \"{node_key}\" says keep but proposes a change; \
                 keep means the node stays as it is"
            ),
            Self::UnsupportedField {
                node_key,
                kind,
                field,
                allowed,
            } => write!(
                f,
                "the accepted {kind} finding for \"{node_key}\" proposes \"{field}\", which it \
                 may not change (it may change: {})",
                allowed.join(", ")
            ),
            Self::InvalidFieldValue {
                node_key,
                field,
                error,
            } => write!(
                f,
                "the accepted finding for \"{node_key}\" proposes an invalid \"{field}\": {error}"
            ),
            Self::ConflictingFindings { node_key, field } => write!(
                f,
                "two accepted findings for \"{node_key}\" both change {field}; accept one of \
                 them, not both"
            ),
            Self::Invalid { node_key, error } => match node_key {
                Some(node_key) => write!(
                    f,
                    "applying the accepted finding for \"{node_key}\" produces a definition that \
                     does not validate: {error}"
                ),
                None => write!(
                    f,
                    "the compiled definition does not validate: {error} — this is inherited from \
                     the version the run executed, not from any accepted finding"
                ),
            },
            Self::Unauthorable { node_key, message } => match node_key {
                Some(node_key) => write!(
                    f,
                    "applying the accepted finding for \"{node_key}\" produces a definition \
                     karvex would refuse to author: {message}"
                ),
                None => write!(
                    f,
                    "the compiled definition is one karvex would refuse to author: {message} — \
                     this is inherited from the version the run executed, so fix it by editing \
                     the workflow, not by accepting fewer findings"
                ),
            },
        }
    }
}

impl std::error::Error for CompileError {}

impl CompileError {
    /// The node key a human should look at, for the two variants that may not
    /// have one.
    pub fn node_key(&self) -> Option<&NodeKey> {
        match self {
            Self::UnknownNode { node_key }
            | Self::ReplaceWithoutReplacement { node_key }
            | Self::ReplacementNotANode { node_key, .. }
            | Self::ReplacementRenamesNode { node_key, .. }
            | Self::ProposedChangeNotAnObject { node_key }
            | Self::ImproveWithoutChange { node_key }
            | Self::KeepProposesChange { node_key }
            | Self::UnsupportedField { node_key, .. }
            | Self::InvalidFieldValue { node_key, .. }
            | Self::ConflictingFindings { node_key, .. } => Some(node_key),
            Self::Invalid { node_key, .. } | Self::Unauthorable { node_key, .. } => {
                node_key.as_ref()
            }
        }
    }
}

// ── compilation ────────────────────────────────────────────────────────────

/// The spec a stored [`Kvdag`] was built from.
///
/// `Kvdag::try_new` consumes a [`KvdagSpec`] and hands back the validated,
/// topologically sorted graph; this is the way back, so the compiler can be
/// handed the version the run executed and produce a sibling document. The
/// identity fields are carried through verbatim and are the caller's to
/// overwrite — `create_version_with_metadata` assigns the version number and
/// honours `parent` as the explicit non-linear parent (§3.5: the parent is the
/// **run's** version, never the head).
pub fn spec_of(kvdag: &Kvdag) -> KvdagSpec {
    KvdagSpec {
        version_id: kvdag.version_id.clone(),
        workflow_id: kvdag.workflow_id.clone(),
        version: kvdag.version,
        parent: kvdag.parent.clone(),
        contract: kvdag.contract.clone(),
        growth: kvdag.growth,
        args: kvdag.args.clone(),
        nodes: kvdag.nodes.clone(),
        edges: kvdag.edges.clone(),
    }
}

/// Folds every accepted finding into `spec` and validates the result.
///
/// The merge table, in full (§3.5, §5 P11):
///
/// | level | verdict | effect |
/// |---|---|---|
/// | `prompt` | `keep` | nothing |
/// | `prompt` | `improve` | merges [`PROMPT_FIELDS`] |
/// | `prompt` | `replace` | swaps the whole node |
/// | `structural` | `keep` | nothing |
/// | `structural` | `improve` | merges [`STRUCTURAL_FIELDS`] |
/// | `structural` | `replace` | swaps the whole node |
///
/// Findings are applied in `node_key` order (then level, then verdict) so two
/// identical accepts compile to the identical document — the digest the store
/// dedupes on is computed over exactly these nodes, and a compiler whose
/// output depended on row order would make "the same review, applied twice"
/// two different versions.
///
/// `spec.parent`, `spec.version`, and `spec.version_id` are passed through
/// untouched: identity is the store's, and the *caller* states the parent.
pub fn apply_findings(
    spec: KvdagSpec,
    accepted: &[AcceptedFinding],
) -> Result<KvdagSpec, CompileError> {
    let mut nodes = spec.nodes.clone();
    let index: BTreeMap<NodeKey, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.key.clone(), at))
        .collect();

    let mut order: Vec<&AcceptedFinding> = accepted.iter().collect();
    order.sort_by(|left, right| {
        left.node_key
            .cmp(&right.node_key)
            .then_with(|| left.level.cmp(&right.level))
            .then_with(|| left.verdict.cmp(&right.verdict))
    });

    let mut claimed: BTreeMap<NodeKey, BTreeSet<&'static str>> = BTreeMap::new();

    for finding in order {
        let Some(&at) = index.get(&finding.node_key) else {
            return Err(CompileError::UnknownNode {
                node_key: finding.node_key.clone(),
            });
        };
        match finding.verdict {
            FindingVerdict::Keep => check_keep_changes_nothing(finding)?,
            FindingVerdict::Improve => {
                let fields = merge_fields(finding)?;
                for (field, value) in fields {
                    claim(&mut claimed, &finding.node_key, field)?;
                    apply_field(&mut nodes[at], &finding.node_key, field, &value)?;
                }
            }
            FindingVerdict::Replace => {
                claim(&mut claimed, &finding.node_key, WHOLE_NODE)?;
                nodes[at] = replacement_node(finding)?;
            }
        }
    }

    let before = spec.nodes.clone();
    validate(KvdagSpec { nodes, ..spec }, &before)
}

/// The authoring gate, run over the compiled document before the caller can
/// write anything.
///
/// Deliberately the *same* validators a hand-written document passes —
/// `Kvdag::try_new` (the one `create_version_with_metadata` itself gates on)
/// and the worktree-isolation authoring rule — rather than a re-implementation
/// of them, for the reason `Definition::validate_graph` gives: two copies of a
/// validator drift, and a self-improved version that karvex would have refused
/// from a human is the one thing this feature must never mint.
fn validate(spec: KvdagSpec, before: &[KvdagNode]) -> Result<KvdagSpec, CompileError> {
    // The parent document, run through the same gate first, so a failure can
    // be classified as inherited or caused. This is the cheap half of
    // honesty: a version authored before a rule existed still fails the rule,
    // and telling the human "decline a finding" when no finding would help
    // would send them in a circle.
    let parent = KvdagSpec {
        nodes: before.to_vec(),
        ..spec.clone()
    };
    let parent_isolation_ok = worktree_isolation_rejection(before).is_none();

    if let Some(rejection) = worktree_isolation_rejection(&spec.nodes) {
        return Err(CompileError::Unauthorable {
            node_key: parent_isolation_ok
                .then(|| blame(before, &spec.nodes, Some(&rejection.node)))
                .flatten(),
            message: rejection.message,
        });
    }
    // A clone, because `Kvdag::try_new` consumes the spec and the caller needs
    // the spec back: this is a validation probe, and the graph it builds is
    // thrown away. The store builds its own from the same document moments
    // later, with the identity fields it assigns.
    if let Err(error) = Kvdag::try_new(spec.clone()) {
        let named = error_node(&error);
        let parent_valid = Kvdag::try_new(parent).is_ok();
        return Err(CompileError::Invalid {
            node_key: parent_valid
                .then(|| blame(before, &spec.nodes, named.as_ref()))
                .flatten(),
            error,
        });
    }
    Ok(spec)
}

/// Whether a validator's complaint can be laid at an accepted finding's door,
/// **given that the parent document passed the same check**.
///
/// The test is "did this node actually change", not "did a finding mention
/// it": a finding that merged a field into an unrelated node of the same
/// document is not to blame for a validator complaining about a different one.
/// The caller's own precondition covers the other half — a definition that was
/// already invalid or already unauthorable (a node carrying `isolation =
/// "worktree"` from before D-6 landed, say) is reported as inherited, because
/// the human's next move is an authoring edit either way and there is no
/// finding they could decline that would help.
fn blame(before: &[KvdagNode], after: &[KvdagNode], named: Option<&NodeKey>) -> Option<NodeKey> {
    let named = named?;
    let find = |nodes: &[KvdagNode]| nodes.iter().find(|node| &node.key == named).cloned();
    let compiled = find(after)?;
    match find(before) {
        Some(original) if original == compiled => None,
        _ => Some(named.clone()),
    }
}

/// The node a [`KvdagError`] is about, when it is about one.
fn error_node(error: &KvdagError) -> Option<NodeKey> {
    match error {
        KvdagError::DuplicateNodeKey(key)
        | KvdagError::ReservedNodeKey(key)
        | KvdagError::SelfEdge(key)
        | KvdagError::UnreachableNode(key)
        | KvdagError::MissingCommand(key)
        | KvdagError::UnexpectedCommand(key) => Some(key.clone()),
        KvdagError::DuplicatePort { node, .. }
        | KvdagError::UnresolvedPlaceholder { node, .. }
        | KvdagError::MalformedPlaceholder { node, .. }
        | KvdagError::UnknownExpandTemplate { node, .. }
        | KvdagError::ExpandTargetNotTemplate { node, .. }
        | KvdagError::InvalidOutputSchema { node, .. } => Some(node.clone()),
        KvdagError::UnknownEdgeEndpoint { missing, .. } => Some(missing.clone()),
        KvdagError::Cycle(keys) => keys.first().cloned(),
        KvdagError::EmptyGraph | KvdagError::NoRoot | KvdagError::DuplicateArg(_) => None,
        KvdagError::Digest(_) => None,
    }
}

fn check_keep_changes_nothing(finding: &AcceptedFinding) -> Result<(), CompileError> {
    let proposes_nothing = match &finding.proposed_change {
        serde_json::Value::Null => true,
        serde_json::Value::Object(fields) => fields.is_empty(),
        _ => false,
    };
    if proposes_nothing && finding.replacement.is_none() {
        Ok(())
    } else {
        Err(CompileError::KeepProposesChange {
            node_key: finding.node_key.clone(),
        })
    }
}

/// The `(field, value)` pairs an `improve` may merge, refusing anything the
/// finding's own `(level, verdict)` does not own.
fn merge_fields(
    finding: &AcceptedFinding,
) -> Result<Vec<(&'static str, serde_json::Value)>, CompileError> {
    let allowed: &[&'static str] = match finding.level {
        FindingLevel::Prompt => &PROMPT_FIELDS,
        FindingLevel::Structural => &STRUCTURAL_FIELDS,
    };
    let proposed = match &finding.proposed_change {
        serde_json::Value::Object(fields) => fields,
        serde_json::Value::Null => {
            return Err(CompileError::ImproveWithoutChange {
                node_key: finding.node_key.clone(),
            })
        }
        _ => {
            return Err(CompileError::ProposedChangeNotAnObject {
                node_key: finding.node_key.clone(),
            })
        }
    };
    if proposed.is_empty() {
        return Err(CompileError::ImproveWithoutChange {
            node_key: finding.node_key.clone(),
        });
    }
    let mut merged = Vec::with_capacity(proposed.len());
    for (field, value) in proposed {
        let Some(known) = allowed.iter().find(|candidate| *candidate == field) else {
            return Err(CompileError::UnsupportedField {
                node_key: finding.node_key.clone(),
                kind: finding.kind(),
                field: field.clone(),
                allowed: allowed.to_vec(),
            });
        };
        merged.push((*known, value.clone()));
    }
    Ok(merged)
}

/// Records that one accepted finding writes one node field, refusing a second
/// finding that writes the same one.
///
/// A `replace` claims [`WHOLE_NODE`], which conflicts with every other change
/// to that node in both directions: applying a field merge and a whole-node
/// swap in either order gives a different document, so karvex applies neither
/// rather than letting row order decide a version.
fn claim(
    claimed: &mut BTreeMap<NodeKey, BTreeSet<&'static str>>,
    node_key: &NodeKey,
    field: &'static str,
) -> Result<(), CompileError> {
    let fields = claimed.entry(node_key.clone()).or_default();
    if fields.contains(WHOLE_NODE) || (field == WHOLE_NODE && !fields.is_empty()) {
        return Err(CompileError::ConflictingFindings {
            node_key: node_key.clone(),
            field: WHOLE_NODE.to_string(),
        });
    }
    if !fields.insert(field) {
        return Err(CompileError::ConflictingFindings {
            node_key: node_key.clone(),
            field: format!("\"{field}\""),
        });
    }
    Ok(())
}

fn apply_field(
    node: &mut KvdagNode,
    node_key: &NodeKey,
    field: &'static str,
    value: &serde_json::Value,
) -> Result<(), CompileError> {
    let invalid = |error: String| CompileError::InvalidFieldValue {
        node_key: node_key.clone(),
        field: field.to_string(),
        error,
    };
    match field {
        "prompt_template" => {
            node.prompt_template =
                string_field(value).ok_or_else(|| invalid(NOT_A_STRING.into()))?;
        }
        "role" => {
            node.role = string_field(value).ok_or_else(|| invalid(NOT_A_STRING.into()))?;
        }
        "demand" => {
            node.demand = serde_json::from_value::<Demand>(value.clone())
                .map_err(|error| invalid(error.to_string()))?;
        }
        "timeout_ms" => {
            node.timeout_ms =
                match value {
                    // Explicitly `null` clears the budget: "this node should not
                    // be on a clock" is a real structural verdict, and it is not
                    // expressible any other way.
                    serde_json::Value::Null => None,
                    other => Some(other.as_u64().ok_or_else(|| {
                        invalid("expected a whole number of milliseconds".into())
                    })?),
                };
        }
        "max_attempts" => {
            node.max_attempts = serde_json::from_value::<u8>(value.clone())
                .map_err(|error| invalid(error.to_string()))?;
        }
        // `merge_fields` only ever yields a member of `PROMPT_FIELDS` or
        // `STRUCTURAL_FIELDS`, and both are matched above. A new field added
        // to either constant without an arm here lands in this branch rather
        // than being silently dropped.
        other => {
            return Err(invalid(format!(
                "\"{other}\" is declared mergeable but has no compiler support"
            )))
        }
    }
    Ok(())
}

const NOT_A_STRING: &str = "expected a string";

fn string_field(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// Parses a `replace` finding's replacement into a node.
///
/// Held to the same standard as a hand-authored node: the same
/// `output_schema` backfill `Definition`'s parser applies (so a synthesiser
/// that omits it is not punished for a field `workflows.mdx` never told it was
/// required), then the same typed deserialization, then the key check.
fn replacement_node(finding: &AcceptedFinding) -> Result<KvdagNode, CompileError> {
    let Some(replacement) = finding.replacement.as_ref() else {
        return Err(CompileError::ReplaceWithoutReplacement {
            node_key: finding.node_key.clone(),
        });
    };
    let mut document = replacement.clone();
    crate::workflow::definition::apply_node_authoring_defaults(&mut document);
    let node: KvdagNode =
        serde_json::from_value(document).map_err(|error| CompileError::ReplacementNotANode {
            node_key: finding.node_key.clone(),
            error: error.to_string(),
        })?;
    if node.key != finding.node_key {
        return Err(CompileError::ReplacementRenamesNode {
            node_key: finding.node_key.clone(),
            replacement_key: node.key,
        });
    }
    Ok(node)
}

// ── the version's own record of what it is ─────────────────────────────────

/// How many findings a change summary names before it stops listing them. A
/// `kvdag_version.change_summary` is read in a list, a DAG header, and a
/// version chain; an unbounded one would make all three unreadable.
const SUMMARY_MAX_ENTRIES: usize = 8;

/// The `change_summary` the compiled version carries.
///
/// This is where attribution stops being a review-cycle fact and becomes part
/// of the definition's own permanent history: a reader of
/// `kvx workflow get --json` sees which findings minted this version and
/// whether each one is a teammate's own account or karvex's inference from
/// evidence. Ordered like the merge, so it describes the document that was
/// actually built.
pub fn change_summary(accepted: &[AcceptedFinding]) -> String {
    if accepted.is_empty() {
        return "self-improvement: no findings accepted".to_string();
    }
    let mut order: Vec<&AcceptedFinding> = accepted.iter().collect();
    order.sort_by(|left, right| {
        left.node_key
            .cmp(&right.node_key)
            .then_with(|| left.level.cmp(&right.level))
            .then_with(|| left.verdict.cmp(&right.verdict))
    });
    let count = order.len();
    let noun = if count == 1 { "finding" } else { "findings" };
    let listed: Vec<String> = order
        .iter()
        .take(SUMMARY_MAX_ENTRIES)
        .map(|finding| {
            format!(
                "\"{}\" {} ({})",
                finding.node_key,
                finding.kind(),
                finding.attribution.describe()
            )
        })
        .collect();
    let mut summary = format!(
        "self-improvement: {count} accepted {noun} — {}",
        listed.join(", ")
    );
    if count > SUMMARY_MAX_ENTRIES {
        summary.push_str(&format!(", +{} more", count - SUMMARY_MAX_ENTRIES));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{
        ArgSpec, EdgeKind, EdgePayload, GrowthLimits, InterrogationId, Isolation, KvdagEdge,
        KvdagVersionId, NodeKind, OutputSchema, ReviewFindingSeed, Runner, WorkflowId,
    };
    use crate::workflow::review::{
        finding_seed, Attribution, EvidenceOnlyReason, MemberAttribution, ParsedFinding,
    };

    // ── fixtures ───────────────────────────────────────────────────────────

    fn node(key: &str, prompt: &str) -> KvdagNode {
        KvdagNode {
            key: NodeKey::new(key),
            label: key.to_string(),
            role: String::new(),
            kind: NodeKind::Agent,
            demand: Demand::Standard,
            runner: Runner::Agent,
            command: None,
            prompt_template: prompt.to_string(),
            system_contract: None,
            output_schema: OutputSchema::parse(serde_json::json!({"type": "object"}))
                .expect("valid schema"),
            max_attempts: 2,
            timeout_ms: None,
            isolation: Isolation::None,
            is_template: false,
            expand_allow: Vec::new(),
            expand_max: 0,
        }
    }

    /// `plan -> implement`, one arg, the smallest graph that still has an
    /// edge and a placeholder to break.
    fn spec() -> KvdagSpec {
        KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:1"),
            workflow_id: WorkflowId::new("workflow:1"),
            version: 1,
            parent: None,
            contract: "reply only through result.json".to_string(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: true,
                default: None,
                description: "what to build".to_string(),
            }],
            nodes: vec![
                node("plan", "Plan for: {{goal}}"),
                node("implement", "Implement {{goal}}"),
            ],
            edges: vec![KvdagEdge {
                from: NodeKey::new("plan"),
                to: NodeKey::new("implement"),
                kind: EdgeKind::Sequence,
                condition: None,
                payload: EdgePayload::Summary,
                port: None,
            }],
        }
    }

    fn resumed(member: &str) -> FindingAttribution {
        FindingAttribution {
            member: Some(member.to_string()),
            mode: InterviewMode::Resumed,
            reason: None,
        }
    }

    fn evidence_only(member: Option<&str>, reason: Option<&str>) -> FindingAttribution {
        FindingAttribution {
            member: member.map(str::to_string),
            mode: InterviewMode::EvidenceOnly,
            reason: reason.map(str::to_string),
        }
    }

    fn finding(
        node_key: &str,
        level: FindingLevel,
        verdict: FindingVerdict,
        proposed_change: serde_json::Value,
    ) -> AcceptedFinding {
        AcceptedFinding {
            node_key: NodeKey::new(node_key),
            level,
            verdict,
            proposed_change,
            replacement: None,
            attribution: resumed("research"),
        }
    }

    fn compiled(accepted: Vec<AcceptedFinding>) -> KvdagSpec {
        apply_findings(spec(), &accepted).expect("the accepted findings compile")
    }

    fn refused(accepted: Vec<AcceptedFinding>) -> CompileError {
        apply_findings(spec(), &accepted).expect_err("the accepted findings are refused")
    }

    fn node_of<'a>(spec: &'a KvdagSpec, key: &str) -> &'a KvdagNode {
        spec.nodes
            .iter()
            .find(|node| node.key.as_str() == key)
            .expect("the node is still in the compiled document")
    }

    // ── the merge table ────────────────────────────────────────────────────

    #[test]
    fn a_prompt_improve_merges_the_brief_and_the_role() {
        let out = compiled(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({
                "prompt_template": "Plan for {{goal}}, and name the risks",
                "role": "planner",
            }),
        )]);
        let plan = node_of(&out, "plan");
        assert_eq!(
            plan.prompt_template,
            "Plan for {{goal}}, and name the risks"
        );
        assert_eq!(plan.role, "planner");
        // Everything else on the node, and every other node, is untouched.
        assert_eq!(plan.demand, Demand::Standard);
        assert_eq!(plan.max_attempts, 2);
        assert_eq!(node_of(&out, "implement"), node_of(&spec(), "implement"));
        assert_eq!(out.edges, spec().edges, "no edge surgery");
        assert_eq!(out.args, spec().args);
        assert_eq!(out.contract, spec().contract);
    }

    #[test]
    fn a_structural_improve_merges_the_three_plan_knobs() {
        let out = compiled(vec![finding(
            "implement",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"demand": "critical", "timeout_ms": 900000, "max_attempts": 3}),
        )]);
        let implement = node_of(&out, "implement");
        assert_eq!(implement.demand, Demand::Critical);
        assert_eq!(implement.timeout_ms, Some(900_000));
        assert_eq!(implement.max_attempts, 3);
        assert_eq!(
            implement.prompt_template,
            node_of(&spec(), "implement").prompt_template,
            "a structural finding never rewrites the brief"
        );
    }

    /// An explicit `null` is the only way to say "take this node off the
    /// clock", so it is a value rather than a parse failure.
    #[test]
    fn a_null_timeout_clears_the_budget() {
        let base = spec();
        let with_budget = KvdagSpec {
            nodes: base
                .nodes
                .iter()
                .map(|node| KvdagNode {
                    timeout_ms: Some(60_000),
                    ..node.clone()
                })
                .collect(),
            ..base
        };
        let out = apply_findings(
            with_budget,
            &[finding(
                "plan",
                FindingLevel::Structural,
                FindingVerdict::Improve,
                serde_json::json!({"timeout_ms": null}),
            )],
        )
        .expect("clearing a budget compiles");
        assert_eq!(node_of(&out, "plan").timeout_ms, None);
        assert_eq!(node_of(&out, "implement").timeout_ms, Some(60_000));
    }

    #[test]
    fn a_replace_swaps_the_whole_node() {
        let mut replace = finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({
            "key": "plan",
            "label": "Plan, properly",
            "role": "planner",
            "demand": "peak",
            "prompt_template": "Plan for: {{goal}}",
            "max_attempts": 1,
        }));
        let out = apply_findings(spec(), &[replace]).expect("a replacement compiles");
        let plan = node_of(&out, "plan");
        assert_eq!(plan.label, "Plan, properly");
        assert_eq!(plan.demand, Demand::Peak);
        assert_eq!(plan.max_attempts, 1);
        assert_eq!(plan.role, "planner");
        assert_eq!(out.edges, spec().edges, "no edge surgery");
    }

    /// The synthesiser is not punished for omitting the field
    /// `workflows.mdx` never told it was required — the same backfill a
    /// hand-authored document gets (`definition::apply_node_authoring_defaults`).
    #[test]
    fn a_replacement_may_omit_the_output_schema() {
        let mut replace = finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({
            "key": "plan",
            "label": "Plan",
            "prompt_template": "Plan for: {{goal}}",
        }));
        let out = apply_findings(spec(), &[replace]).expect("a replacement compiles");
        assert_eq!(node_of(&out, "plan").label, "Plan");
    }

    #[test]
    fn a_keep_changes_nothing_at_all() {
        for level in FindingLevel::ALL {
            let out = compiled(vec![finding(
                "plan",
                level,
                FindingVerdict::Keep,
                serde_json::json!({}),
            )]);
            assert_eq!(out.nodes, spec().nodes, "{level} keep changed a node");
        }
        // `null` is the other spelling of "nothing proposed".
        let out = compiled(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Keep,
            serde_json::Value::Null,
        )]);
        assert_eq!(out.nodes, spec().nodes);
    }

    #[test]
    fn two_findings_on_two_nodes_both_land() {
        let out = compiled(vec![
            finding(
                "implement",
                FindingLevel::Structural,
                FindingVerdict::Improve,
                serde_json::json!({"max_attempts": 4}),
            ),
            finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"role": "planner"}),
            ),
        ]);
        assert_eq!(node_of(&out, "implement").max_attempts, 4);
        assert_eq!(node_of(&out, "plan").role, "planner");
    }

    /// Two accepts of the same set must compile to the same document, whatever
    /// order the store handed the rows back in: the version's `spec_digest` is
    /// computed over exactly these nodes.
    #[test]
    fn the_merge_is_independent_of_the_order_the_findings_arrive_in() {
        let one = finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"role": "planner"}),
        );
        let two = finding(
            "implement",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"demand": "light"}),
        );
        assert_eq!(
            compiled(vec![one.clone(), two.clone()]),
            compiled(vec![two, one])
        );
    }

    /// A prompt-level and a structural finding about the same node touch
    /// disjoint fields, so both are applicable and neither is a conflict.
    #[test]
    fn two_findings_on_one_node_may_merge_when_they_touch_different_fields() {
        let out = compiled(vec![
            finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"prompt_template": "Plan {{goal}} in three steps"}),
            ),
            finding(
                "plan",
                FindingLevel::Structural,
                FindingVerdict::Improve,
                serde_json::json!({"demand": "peak"}),
            ),
        ]);
        assert_eq!(
            node_of(&out, "plan").prompt_template,
            "Plan {{goal}} in three steps"
        );
        assert_eq!(node_of(&out, "plan").demand, Demand::Peak);
    }

    // ── refusals ───────────────────────────────────────────────────────────

    /// A finding about `.lead`, an emergent `.task/…`, or a node key that only
    /// exists in some other version has nothing to apply to. Refused rather
    /// than skipped: the human accepted it, and a silently-dropped accept is
    /// the dishonesty this whole cycle exists to remove.
    #[test]
    fn a_finding_about_a_node_this_definition_does_not_have_is_refused() {
        let error = refused(vec![finding(
            ".lead",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"role": "lead"}),
        )]);
        assert_eq!(
            error,
            CompileError::UnknownNode {
                node_key: NodeKey::new(".lead")
            }
        );
        assert!(error.to_string().contains(".lead"), "{error}");
    }

    #[test]
    fn a_replace_without_a_replacement_never_reaches_the_store() {
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Replace,
            serde_json::json!({}),
        )]);
        assert_eq!(
            error,
            CompileError::ReplaceWithoutReplacement {
                node_key: NodeKey::new("plan")
            }
        );
    }

    #[test]
    fn a_replacement_that_is_not_a_node_is_refused() {
        let mut replace = finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({"key": "plan"}));
        let error = apply_findings(spec(), &[replace]).expect_err("no label, no prompt template");
        assert!(
            matches!(error, CompileError::ReplacementNotANode { ref node_key, .. }
                if node_key.as_str() == "plan"),
            "{error:?}"
        );
    }

    #[test]
    fn a_replacement_may_not_rename_the_node() {
        let mut replace = finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({
            "key": "planning",
            "label": "Plan",
            "prompt_template": "Plan for: {{goal}}",
        }));
        let error = apply_findings(spec(), &[replace]).expect_err("a rename is refused");
        assert_eq!(
            error,
            CompileError::ReplacementRenamesNode {
                node_key: NodeKey::new("plan"),
                replacement_key: NodeKey::new("planning"),
            }
        );
    }

    #[test]
    fn a_prompt_finding_may_not_change_a_structural_field_and_the_other_way_round() {
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"timeout_ms": 1000}),
        )]);
        assert!(
            matches!(&error, CompileError::UnsupportedField { field, kind, .. }
                if field == "timeout_ms" && kind == "prompt/improve"),
            "{error:?}"
        );
        assert!(error.to_string().contains("prompt_template"), "{error}");

        let error = refused(vec![finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"prompt_template": "do better"}),
        )]);
        assert!(
            matches!(&error, CompileError::UnsupportedField { field, .. }
                if field == "prompt_template"),
            "{error:?}"
        );
    }

    /// Not even a field that exists on the node: the merge is a closed list,
    /// so "the synthesiser invented a key" is a refusal rather than a silent
    /// drop.
    #[test]
    fn a_field_outside_the_merge_table_is_refused() {
        for field in ["key", "runner", "command", "output_schema", "isolation"] {
            let error = refused(vec![finding(
                "plan",
                FindingLevel::Structural,
                FindingVerdict::Improve,
                serde_json::json!({field: "whatever"}),
            )]);
            assert!(
                matches!(&error, CompileError::UnsupportedField { field: named, .. }
                    if named == field),
                "{field}: {error:?}"
            );
        }
    }

    #[test]
    fn an_unusable_field_value_is_refused_with_the_field_named() {
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"demand": "urgent"}),
        )]);
        assert!(
            matches!(&error, CompileError::InvalidFieldValue { field, .. } if field == "demand"),
            "{error:?}"
        );

        let error = refused(vec![finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"timeout_ms": "ten minutes"}),
        )]);
        assert!(
            matches!(&error, CompileError::InvalidFieldValue { field, .. } if field == "timeout_ms"),
            "{error:?}"
        );

        let error = refused(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"prompt_template": 42}),
        )]);
        assert!(
            matches!(&error, CompileError::InvalidFieldValue { field, .. }
                if field == "prompt_template"),
            "{error:?}"
        );
    }

    #[test]
    fn an_improve_that_proposes_nothing_is_refused() {
        for proposed in [serde_json::json!({}), serde_json::Value::Null] {
            let error = refused(vec![finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                proposed,
            )]);
            assert_eq!(
                error,
                CompileError::ImproveWithoutChange {
                    node_key: NodeKey::new("plan")
                }
            );
        }
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!("rewrite it"),
        )]);
        assert_eq!(
            error,
            CompileError::ProposedChangeNotAnObject {
                node_key: NodeKey::new("plan")
            }
        );
    }

    #[test]
    fn a_keep_that_proposes_a_change_is_refused_rather_than_silently_dropped() {
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Keep,
            serde_json::json!({"role": "planner"}),
        )]);
        assert_eq!(
            error,
            CompileError::KeepProposesChange {
                node_key: NodeKey::new("plan")
            }
        );
    }

    #[test]
    fn two_findings_that_change_the_same_field_are_refused_rather_than_raced() {
        let error = refused(vec![
            finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"role": "planner"}),
            ),
            finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"role": "architect"}),
            ),
        ]);
        assert_eq!(
            error,
            CompileError::ConflictingFindings {
                node_key: NodeKey::new("plan"),
                field: "\"role\"".to_string(),
            }
        );
    }

    #[test]
    fn a_replacement_and_a_field_merge_on_one_node_are_refused_in_either_order() {
        let mut replace = finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({
            "key": "plan",
            "label": "Plan",
            "prompt_template": "Plan for: {{goal}}",
        }));
        let merge = finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"role": "planner"}),
        );
        for accepted in [vec![replace.clone(), merge.clone()], vec![merge, replace]] {
            let error = apply_findings(spec(), &accepted).expect_err("a conflict is refused");
            assert_eq!(
                error,
                CompileError::ConflictingFindings {
                    node_key: NodeKey::new("plan"),
                    field: WHOLE_NODE.to_string(),
                }
            );
        }
    }

    // ── the authoring gate ─────────────────────────────────────────────────

    /// The compiled document goes through the same `Kvdag::try_new` a
    /// hand-written one does, and the refusal names the finding that broke it.
    #[test]
    fn a_finding_that_breaks_graph_validation_is_refused_and_names_itself() {
        let error = refused(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"prompt_template": "Plan for {{nowhere}}"}),
        )]);
        assert!(
            matches!(&error, CompileError::Invalid { node_key: Some(key), error }
                if key.as_str() == "plan"
                    && matches!(error, KvdagError::UnresolvedPlaceholder { .. })),
            "{error:?}"
        );
        assert!(error.to_string().contains("plan"), "{error}");
    }

    /// P14's authoring rule (D-6), applied to a compiled version: a review may
    /// not mint a definition karvex would have refused from a human.
    #[test]
    fn a_replacement_that_demands_worktree_isolation_is_refused() {
        let mut replace = finding(
            "plan",
            FindingLevel::Structural,
            FindingVerdict::Replace,
            serde_json::json!({}),
        );
        replace.replacement = Some(serde_json::json!({
            "key": "plan",
            "label": "Plan",
            "prompt_template": "Plan for: {{goal}}",
            "isolation": "worktree",
        }));
        let error = apply_findings(spec(), &[replace]).expect_err("worktree isolation is refused");
        assert!(
            matches!(&error, CompileError::Unauthorable { node_key: Some(key), .. }
                if key.as_str() == "plan"),
            "{error:?}"
        );
        assert!(error.to_string().contains("worktree"), "{error}");
    }

    /// The other half of the same rule: a version authored before D-6 landed
    /// can still carry `isolation = "worktree"`, and the human must not be
    /// sent looking for a finding to decline that would not help.
    #[test]
    fn a_problem_inherited_from_the_parent_version_is_not_blamed_on_a_finding() {
        let base = spec();
        let inherited = KvdagSpec {
            nodes: base
                .nodes
                .iter()
                .map(|node| KvdagNode {
                    isolation: Isolation::Worktree,
                    ..node.clone()
                })
                .collect(),
            ..base
        };
        let error = apply_findings(
            inherited,
            &[finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"role": "planner"}),
            )],
        )
        .expect_err("the compiled document is unauthorable");
        assert!(
            matches!(&error, CompileError::Unauthorable { node_key: None, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("inherited"), "{error}");
    }

    /// Nothing accepted means nothing changes — the identity case the handler
    /// short-circuits, pinned here so the pure layer agrees with it.
    #[test]
    fn an_empty_accept_set_compiles_to_the_same_document() {
        assert_eq!(compiled(Vec::new()), spec());
    }

    #[test]
    fn identity_fields_are_carried_through_untouched_for_the_store_to_assign() {
        let out = compiled(vec![finding(
            "plan",
            FindingLevel::Prompt,
            FindingVerdict::Improve,
            serde_json::json!({"role": "planner"}),
        )]);
        assert_eq!(out.version_id, spec().version_id);
        assert_eq!(out.workflow_id, spec().workflow_id);
        assert_eq!(out.version, spec().version);
        assert_eq!(out.parent, spec().parent);
    }

    #[test]
    fn spec_of_round_trips_a_validated_graph() {
        let kvdag = Kvdag::try_new(spec()).expect("the fixture validates");
        let round_tripped = spec_of(&kvdag);
        assert_eq!(round_tripped.nodes, kvdag.nodes);
        assert_eq!(round_tripped.edges, kvdag.edges);
        assert_eq!(round_tripped.args, kvdag.args);
        assert_eq!(round_tripped.contract, kvdag.contract);
        assert_eq!(round_tripped.growth, kvdag.growth);
        assert_eq!(
            Kvdag::try_new(round_tripped)
                .expect("and validates again")
                .spec_digest,
            kvdag.spec_digest,
            "the same document, so the same digest"
        );
    }

    // ── attribution ────────────────────────────────────────────────────────

    /// The coupling `review::finding_seed` and this module share, pinned in a
    /// test rather than in prose: P5 writes `{"reported", "attribution"}` onto
    /// the finding's evidence so a compiled version can still say where the
    /// finding came from, and this is the reader of that shape.
    #[test]
    fn attribution_round_trips_through_review_finding_seed() {
        let parsed = ParsedFinding {
            node_key: NodeKey::new("plan"),
            source_member: Some("research".to_string()),
            level: FindingLevel::Prompt,
            verdict: FindingVerdict::Improve,
            rationale: "the brief never said what done looks like".to_string(),
            evidence: serde_json::json!({"idle_ms": 900000}),
            proposed_change: serde_json::json!({"role": "planner"}),
            replacement: None,
        };
        let interviewed = Attribution::new(BTreeMap::from([(
            "research".to_string(),
            MemberAttribution::resumed(InterrogationId::new("interrogation:1")),
        )]));
        let seed: ReviewFindingSeed = finding_seed(&parsed, &interviewed, &BTreeMap::new());
        let attribution =
            FindingAttribution::from_seed_evidence(&seed.evidence, seed.interview_mode);
        assert_eq!(attribution, resumed("research"));
        assert_eq!(attribution.describe(), "research's own account");

        // The same finding out of an evidence-only cycle: the member is still
        // named, but it is never presented as something the teammate said.
        let degraded = Attribution::new(BTreeMap::from([(
            "research".to_string(),
            MemberAttribution::evidence_only(EvidenceOnlyReason::InterviewTimedOut),
        )]));
        let seed = finding_seed(&parsed, &degraded, &BTreeMap::new());
        let attribution =
            FindingAttribution::from_seed_evidence(&seed.evidence, seed.interview_mode);
        assert_eq!(
            attribution,
            evidence_only(Some("research"), Some("interview_timed_out"))
        );
        assert!(
            !attribution.describe().contains("own account"),
            "{}",
            attribution.describe()
        );
        // And the reported evidence itself is still where P5 put it, unwrapped
        // by nobody along the way.
        assert_eq!(seed.evidence["reported"], parsed.evidence);
    }

    /// The interview mode is taken from the store's own typed column, so a
    /// mangled evidence blob degrades the member name and never the
    /// resumed-versus-inferred fact.
    #[test]
    fn a_missing_evidence_blob_degrades_the_member_not_the_mode() {
        let attribution = FindingAttribution::from_seed_evidence(
            &serde_json::json!({"reported": {}}),
            InterviewMode::Resumed,
        );
        assert_eq!(attribution.member, None);
        assert_eq!(attribution.mode, InterviewMode::Resumed);
        assert_eq!(attribution.describe(), "an interviewed teammate's account");

        let attribution = FindingAttribution::from_seed_evidence(
            &serde_json::json!({"attribution": {"member": "   "}}),
            InterviewMode::EvidenceOnly,
        );
        assert_eq!(attribution.member, None, "a blank member is no member");
        assert_eq!(attribution.describe(), "evidence only");
    }

    #[test]
    fn the_change_summary_names_every_finding_and_where_it_came_from() {
        let mut structural = finding(
            "implement",
            FindingLevel::Structural,
            FindingVerdict::Improve,
            serde_json::json!({"max_attempts": 3}),
        );
        structural.attribution = evidence_only(Some("builder"), Some("no_session_id"));
        let summary = change_summary(&[
            structural,
            finding(
                "plan",
                FindingLevel::Prompt,
                FindingVerdict::Improve,
                serde_json::json!({"role": "planner"}),
            ),
        ]);
        assert_eq!(
            summary,
            "self-improvement: 2 accepted findings — \
             \"implement\" structural/improve (evidence only for builder: no_session_id), \
             \"plan\" prompt/improve (research's own account)"
        );
    }

    #[test]
    fn a_long_accept_still_produces_a_readable_summary() {
        let accepted: Vec<AcceptedFinding> = (0..12)
            .map(|index| {
                finding(
                    &format!("node{index:02}"),
                    FindingLevel::Prompt,
                    FindingVerdict::Keep,
                    serde_json::json!({}),
                )
            })
            .collect();
        let summary = change_summary(&accepted);
        assert!(
            summary.starts_with("self-improvement: 12 accepted findings — "),
            "{summary}"
        );
        assert!(summary.ends_with(", +4 more"), "{summary}");
        assert!(summary.contains("\"node00\""), "{summary}");
        assert!(!summary.contains("\"node08\""), "{summary}");
    }

    #[test]
    fn an_empty_accept_says_so_rather_than_claiming_a_change() {
        assert_eq!(
            change_summary(&[]),
            "self-improvement: no findings accepted"
        );
    }

    /// Every cell of the merge table compiles: a level or verdict added to
    /// `workflow::review` without a rule here fails this test rather than
    /// being silently ignored by the compiler.
    #[test]
    fn every_level_and_verdict_has_a_merge_rule() {
        for level in FindingLevel::ALL {
            for verdict in FindingVerdict::ALL {
                let mut accepted = finding("plan", level, verdict, serde_json::json!({}));
                match verdict {
                    FindingVerdict::Keep => {}
                    FindingVerdict::Improve => {
                        accepted.proposed_change = match level {
                            FindingLevel::Prompt => serde_json::json!({"role": "planner"}),
                            FindingLevel::Structural => serde_json::json!({"max_attempts": 3}),
                        };
                    }
                    FindingVerdict::Replace => {
                        accepted.replacement = Some(serde_json::json!({
                            "key": "plan",
                            "label": "Plan",
                            "prompt_template": "Plan for: {{goal}}",
                        }));
                    }
                }
                apply_findings(spec(), &[accepted])
                    .unwrap_or_else(|error| panic!("{level}/{verdict} does not compile: {error}"));
            }
        }
    }
}
