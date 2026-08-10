//! The authored definition document (`05-phase-plan.md` §4).
//!
//! `workflow.create` and `workflow.version.create` carry the definition as
//! opaque TOML or JSON text, because the wire contract deliberately does not
//! duplicate the kvdag types (`src/api/schema/workflows.rs` is self-contained).
//! This module is where that text becomes model types, and it is pure: parsing
//! and validation happen with no store, no engine, and no runtime.
//!
//! Identity fields (`version_id`, `workflow_id`, `version`, `parent`) are the
//! store's to assign, so a document never carries them; [`Definition::spec`]
//! takes them from the caller.

use serde::Deserialize;

use crate::workflow::model::{
    ArgSpec, EdgeKind, GrowthLimits, Kvdag, KvdagEdge, KvdagError, KvdagNode, KvdagSpec, NodeKey,
    WorkflowId,
};
use crate::workflow::tier::Tier;

/// The identity [`Definition::validate_graph`] binds the document to for its
/// throwaway construction. Never persisted and never compared: `Kvdag::try_new`
/// validates node/edge/arg/template *content* only, so the identity it is
/// handed cannot change the verdict. The store's own probe inside
/// `create_version_with_metadata` works the same way.
const VALIDATION_PROBE_WORKFLOW: &str = "workflow:validation_probe";

/// A parsed definition document, before it is bound to a workflow identity.
///
/// The `[[node]]` / `[[edge]]` / `[[arg]]` spelling is what TOML's array-of-
/// tables syntax produces and is the spelling `05-phase-plan.md` §4 documents;
/// the plural aliases exist so the same document reads naturally as JSON.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_tier: Option<Tier>,
    /// Prepended to every node's system prompt.
    #[serde(default)]
    pub contract: String,
    #[serde(default)]
    pub max_depth: Option<u16>,
    #[serde(default)]
    pub max_nodes: Option<u16>,
    #[serde(default, alias = "args")]
    pub arg: Vec<ArgSpec>,
    #[serde(default, alias = "nodes")]
    pub node: Vec<KvdagNode>,
    #[serde(default, alias = "edges")]
    pub edge: Vec<KvdagEdge>,
}

/// Why a definition document could not be turned into a [`KvdagSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionError {
    Parse(String),
    /// A document that names nothing cannot be looked up by name later, and a
    /// document with no nodes has no run to schedule.
    Empty(&'static str),
    /// `kind = "conditional"` with no `condition` — the report ("UX
    /// validation" §2.19) found this silently accepted, producing an edge
    /// whose fan-out never resolves to fired-or-not.
    ConditionalEdgeMissingCondition {
        from: NodeKey,
        to: NodeKey,
    },
    /// An edge names a `port` that never appears as `{{port}}` in the
    /// target's `prompt_template` — accepted silently before this check
    /// (§2.19: "a typo'd port silently produces a workflow whose data goes
    /// nowhere"). Ports that *do* resolve are still checked in the opposite
    /// direction by the graph-level `UnresolvedPlaceholder` validator
    /// (`workflow::model::Kvdag::try_new`); this is the missing other half.
    UnmatchedEdgePort {
        from: NodeKey,
        to: NodeKey,
        port: String,
    },
}

impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "invalid definition document: {message}"),
            Self::Empty(field) => write!(f, "definition document has no {field}"),
            Self::ConditionalEdgeMissingCondition { from, to } => write!(
                f,
                "edge {from} -> {to} declares kind \"conditional\" but has no condition"
            ),
            Self::UnmatchedEdgePort { from, to, port } => write!(
                f,
                "edge {from} -> {to} declares port \"{port}\", which does not appear as {{{{{port}}}}} in \"{to}\"'s prompt_template"
            ),
        }
    }
}

impl std::error::Error for DefinitionError {}

impl Definition {
    pub fn parse_toml(text: &str) -> Result<Self, DefinitionError> {
        match toml::from_str::<Self>(text) {
            Ok(parsed) => parsed.check(),
            Err(first_error) => match Self::parse_toml_with_authoring_defaults(text) {
                Some(parsed) => parsed.check(),
                None => Err(DefinitionError::Parse(first_error.to_string())),
            },
        }
    }

    pub fn parse_json(text: &str) -> Result<Self, DefinitionError> {
        match serde_json::from_str::<Self>(text) {
            Ok(parsed) => parsed.check(),
            Err(first_error) => match Self::parse_json_with_authoring_defaults(text) {
                Some(parsed) => parsed.check(),
                None => Err(DefinitionError::Parse(first_error.to_string())),
            },
        }
    }

    /// Retries a TOML document that failed direct typed deserialization by
    /// backfilling `output_schema`/`kind` (see [`apply_authoring_defaults`])
    /// and reparsing. Only called on the failure path, so a document that
    /// already deserializes cleanly keeps `toml`'s original positional
    /// caret diagnostics untouched; only reached when *some* field was
    /// missing does this trade that position info for a chance at reaching
    /// [`Definition::check`] and the graph validators beyond it.
    fn parse_toml_with_authoring_defaults(text: &str) -> Option<Self> {
        let raw: toml::Value = toml::from_str(text).ok()?;
        let mut value = serde_json::to_value(raw).ok()?;
        apply_authoring_defaults(&mut value);
        serde_json::from_value(value).ok()
    }

    /// JSON counterpart of [`Definition::parse_toml_with_authoring_defaults`].
    fn parse_json_with_authoring_defaults(text: &str) -> Option<Self> {
        let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
        apply_authoring_defaults(&mut value);
        serde_json::from_value(value).ok()
    }

    fn check(self) -> Result<Self, DefinitionError> {
        if self.name.trim().is_empty() {
            return Err(DefinitionError::Empty("name"));
        }
        if self.node.is_empty() {
            return Err(DefinitionError::Empty("nodes"));
        }
        self.check_conditional_edges_declare_a_condition()?;
        self.check_edge_ports_match_a_target_placeholder()?;
        Ok(self)
    }

    fn check_conditional_edges_declare_a_condition(&self) -> Result<(), DefinitionError> {
        for edge in &self.edge {
            if edge.kind == EdgeKind::Conditional && edge.condition.is_none() {
                return Err(DefinitionError::ConditionalEdgeMissingCondition {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                });
            }
        }
        Ok(())
    }

    fn check_edge_ports_match_a_target_placeholder(&self) -> Result<(), DefinitionError> {
        let templates: std::collections::HashMap<&str, &str> = self
            .node
            .iter()
            .map(|node| (node.key.as_str(), node.prompt_template.as_str()))
            .collect();
        for edge in &self.edge {
            let Some(port) = edge.port.as_deref() else {
                continue;
            };
            let Some(template) = templates.get(edge.to.as_str()) else {
                // An edge to an unknown node is a different, already-named
                // validator (`KvdagError::UnknownEdgeEndpoint`,
                // `workflow::model::Kvdag::try_new`); this check only has an
                // opinion about ports on edges whose target it can see.
                continue;
            };
            if !template_declares_placeholder(template, port) {
                return Err(DefinitionError::UnmatchedEdgePort {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    port: port.to_string(),
                });
            }
        }
        Ok(())
    }

    pub fn tier(&self) -> Tier {
        self.default_tier.unwrap_or(Tier::High)
    }

    pub fn growth(&self) -> GrowthLimits {
        let default = GrowthLimits::default();
        GrowthLimits {
            max_depth: self.max_depth.unwrap_or(default.max_depth),
            max_nodes: self.max_nodes.unwrap_or(default.max_nodes),
        }
    }

    /// Binds the document to a workflow identity. The store assigns the version
    /// number, the version id, and the parent pointer, so the placeholders here
    /// are overwritten before anything is persisted.
    pub fn spec(&self, workflow: &WorkflowId) -> KvdagSpec {
        KvdagSpec {
            version_id: crate::workflow::model::KvdagVersionId::new(String::new()),
            workflow_id: workflow.clone(),
            version: 0,
            parent: None,
            contract: self.contract.clone(),
            growth: self.growth(),
            args: self.arg.clone(),
            nodes: self.node.clone(),
            edges: self.edge.clone(),
        }
    }

    /// Runs the graph-level validators — the ones `Definition::check` does not
    /// own — against this document without writing anything.
    ///
    /// `create_version_with_metadata` already gates every write on
    /// `Kvdag::try_new`, but it does so *after* the caller has committed the
    /// `workflow` row, so a cycle / duplicate node key / unknown edge endpoint
    /// used to leave a version-less workflow behind that permanently squatted
    /// the name (there is no `workflow delete`). Hoisting the identical check
    /// in front of the first write is what makes `workflow.create` all-or-
    /// nothing: a rollback would still lose the race against a crash between
    /// the two writes, and this cannot, because nothing has been written yet.
    ///
    /// Deliberately the same validator rather than a re-implementation of it,
    /// so the pre-check and the store's gate cannot drift apart and let a
    /// document through here that the store then rejects.
    pub fn validate_graph(&self) -> Result<(), KvdagError> {
        let probe = WorkflowId::new(VALIDATION_PROBE_WORKFLOW.to_string());
        Kvdag::try_new(self.spec(&probe)).map(|_| ())
    }
}

/// Backfills the two mandatory-but-undocumented fields the kvdag model
/// requires so a document that omits them reaches [`Definition::check`] and
/// the graph validators beyond it (`workflow::model::Kvdag::try_new`'s
/// cycle/dangling-edge/duplicate-key/missing-command checks) instead of
/// failing on a raw serde "missing field" error naming a field
/// `workflows.mdx` never told the author was required — UX validation
/// report §2.17: "seven of eleven distinct error fixtures returned the
/// identical message `missing field output_schema`, hiding [every other]
/// validator entirely."
///
/// - `output_schema` defaults to `{}`, the empty (accept-anything) JSON
///   Schema: `OutputSchema::validate` accepts an object with no `type`,
///   `required`, or `properties` key.
/// - `kind` (on an edge) defaults to `"sequence"`, the edge kind with no
///   data-flow or condition requirements of its own.
///
/// Only called after a direct typed parse already failed
/// ([`Definition::parse_toml_with_authoring_defaults`] /
/// `parse_json_with_authoring_defaults`), so a document that already
/// declares both fields never goes through this path.
fn apply_authoring_defaults(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for key in ["node", "nodes"] {
        if let Some(serde_json::Value::Array(nodes)) = object.get_mut(key) {
            for node in nodes.iter_mut() {
                if let Some(node) = node.as_object_mut() {
                    node.entry("output_schema")
                        .or_insert_with(|| serde_json::json!({}));
                }
            }
        }
    }
    for key in ["edge", "edges"] {
        if let Some(serde_json::Value::Array(edges)) = object.get_mut(key) {
            for edge in edges.iter_mut() {
                if let Some(edge) = edge.as_object_mut() {
                    edge.entry("kind")
                        .or_insert_with(|| serde_json::Value::String("sequence".to_string()));
                }
            }
        }
    }
}

/// Minimal, deliberately duplicated `{{name}}` scanner — mirrors
/// `workflow::model::scan_placeholders`, which is private to that module and
/// returns richer error detail this check does not need: a yes/no "does this
/// port appear as a placeholder in this template" answer is enough to reject
/// a typo'd port at authoring time.
fn template_declares_placeholder(template: &str, name: &str) -> bool {
    let mut search_from = 0;
    while let Some(start) = template[search_from..].find("{{") {
        let body_start = search_from + start + 2;
        let Some(end_offset) = template[body_start..].find("}}") else {
            break;
        };
        let body = template[body_start..body_start + end_offset].trim();
        if body == name {
            return true;
        }
        search_from = body_start + end_offset + 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Demand, Runner};

    const DOCUMENT: &str = r#"
name = "ship-feature"
description = "plan then implement"
contract = "Reply only through result.json."
max_depth = 5
max_nodes = 30

[[arg]]
name = "goal"
required = true
description = "what to build"

[[node]]
key = "plan"
label = "Plan"
demand = "critical"
prompt_template = "Produce an implementation plan for: {{goal}}"
output_schema = { type = "object", required = ["plan"], properties = { plan = { type = "string" } } }

[[node]]
key = "implement"
label = "Implement"
runner = "command"
command = ["/bin/sh", "-c", "true"]
prompt_template = "Implement this plan:\n{{plan}}"
output_schema = { type = "object", required = ["report"], properties = { report = { type = "string" } } }

[[edge]]
from = "plan"
to = "implement"
kind = "data"
payload = "summary"
port = "plan"
"#;

    #[test]
    fn a_toml_document_parses_into_model_types() {
        let definition = Definition::parse_toml(DOCUMENT).expect("document parses");
        assert_eq!(definition.name, "ship-feature");
        assert_eq!(definition.arg.len(), 1);
        assert_eq!(definition.node.len(), 2);
        assert_eq!(definition.edge.len(), 1);
        assert_eq!(definition.node[0].demand, Demand::Critical);
        assert_eq!(definition.node[1].runner, Runner::Command);
        assert_eq!(definition.edge[0].kind, EdgeKind::Data);
        assert_eq!(definition.growth().max_depth, 5);
        assert_eq!(definition.growth().max_nodes, 30);
    }

    #[test]
    fn growth_limits_fall_back_to_the_model_defaults() {
        let text = r#"
name = "minimal"
[[node]]
key = "only"
label = "Only"
prompt_template = "do it"
output_schema = { type = "object" }
"#;
        let definition = Definition::parse_toml(text).expect("document parses");
        assert_eq!(definition.growth(), GrowthLimits::default());
        assert_eq!(definition.tier(), Tier::High);
    }

    #[test]
    fn the_same_document_parses_as_json_with_plural_aliases() {
        let text = r#"{
            "name": "minimal",
            "nodes": [
                {
                    "key": "only",
                    "label": "Only",
                    "prompt_template": "do it",
                    "output_schema": { "type": "object" }
                }
            ]
        }"#;
        let definition = Definition::parse_json(text).expect("document parses");
        assert_eq!(definition.node.len(), 1);
        assert!(definition.edge.is_empty());
    }

    #[test]
    fn a_document_without_a_name_is_rejected() {
        let text = r#"
name = "  "
[[node]]
key = "only"
label = "Only"
prompt_template = "do it"
output_schema = { type = "object" }
"#;
        assert_eq!(
            Definition::parse_toml(text),
            Err(DefinitionError::Empty("name"))
        );
    }

    #[test]
    fn a_document_without_nodes_is_rejected() {
        assert_eq!(
            Definition::parse_toml("name = \"empty\"\n"),
            Err(DefinitionError::Empty("nodes"))
        );
    }

    #[test]
    fn an_unknown_field_is_reported_rather_than_silently_dropped() {
        let text = r#"
name = "typo"
maxdepth = 4
[[node]]
key = "only"
label = "Only"
prompt_template = "do it"
output_schema = { type = "object" }
"#;
        assert!(matches!(
            Definition::parse_toml(text),
            Err(DefinitionError::Parse(_))
        ));
    }

    /// §2.17: `output_schema` was mandatory but undocumented, and omitting it
    /// masked every other validator behind a raw `missing field
    /// output_schema` serde error. Fails before the fix (the old
    /// `toml::from_str::<Self>(text)`-only implementation rejected this
    /// document outright); now it parses with the empty, accept-anything
    /// schema.
    #[test]
    fn a_node_missing_output_schema_defaults_to_an_empty_schema() {
        let text = r#"
name = "no-schema"
[[node]]
key = "only"
label = "Only"
prompt_template = "do it"
"#;
        let definition = Definition::parse_toml(text).expect("document parses via defaulting");
        assert_eq!(
            definition.node[0].output_schema.as_json(),
            &serde_json::json!({})
        );

        // Same defaulting applies to the JSON form.
        let json_text = r#"{
            "name": "no-schema",
            "node": [
                { "key": "only", "label": "Only", "prompt_template": "do it" }
            ]
        }"#;
        let definition = Definition::parse_json(json_text).expect("document parses via defaulting");
        assert_eq!(
            definition.node[0].output_schema.as_json(),
            &serde_json::json!({})
        );
    }

    /// §2.17's other undocumented mandatory field: an edge with no `kind`.
    /// Fails before the fix for the same reason as the `output_schema` case.
    #[test]
    fn an_edge_missing_kind_defaults_to_sequence() {
        let text = r#"
name = "no-edge-kind"
[[node]]
key = "a"
label = "A"
prompt_template = "do it"
output_schema = { type = "object" }

[[node]]
key = "b"
label = "B"
prompt_template = "then this"
output_schema = { type = "object" }

[[edge]]
from = "a"
to = "b"
"#;
        let definition = Definition::parse_toml(text).expect("document parses via defaulting");
        assert_eq!(definition.edge[0].kind, EdgeKind::Sequence);
    }

    /// Defaulting must not paper over a genuinely missing required field: a
    /// document missing `prompt_template` (which has no default) still
    /// fails, and the error still names the actual missing field rather than
    /// something the defaulting retry silently invented.
    #[test]
    fn a_node_missing_prompt_template_still_fails_with_that_fields_name() {
        let text = r#"
name = "missing-prompt"
[[node]]
key = "only"
label = "Only"
output_schema = { type = "object" }
"#;
        let error = Definition::parse_toml(text).expect_err("prompt_template is still required");
        let DefinitionError::Parse(message) = error else {
            panic!("expected a Parse error, got {error:?}");
        };
        assert!(
            message.contains("prompt_template"),
            "error should still name the real missing field: {message}"
        );
    }

    /// §2.19: `kind = "conditional"` with no `condition` was accepted
    /// silently, producing an edge whose branch never resolves.
    #[test]
    fn a_conditional_edge_without_a_condition_is_rejected() {
        let text = r#"
name = "dangling-conditional"
[[node]]
key = "a"
label = "A"
prompt_template = "do it"
output_schema = { type = "object" }

[[node]]
key = "b"
label = "B"
prompt_template = "then this"
output_schema = { type = "object" }

[[edge]]
from = "a"
to = "b"
kind = "conditional"
"#;
        assert_eq!(
            Definition::parse_toml(text),
            Err(DefinitionError::ConditionalEdgeMissingCondition {
                from: NodeKey::new("a"),
                to: NodeKey::new("b"),
            })
        );
    }

    /// §2.19: an edge could declare `port = "notaslot"` even when the
    /// target's `prompt_template` has no `{{notaslot}}` — a typo that
    /// silently produced a workflow whose data goes nowhere.
    #[test]
    fn an_edge_port_with_no_matching_placeholder_is_rejected() {
        let text = r#"
name = "bad-port"
[[node]]
key = "a"
label = "A"
prompt_template = "do it"
output_schema = { type = "object" }

[[node]]
key = "b"
label = "B"
prompt_template = "then this: {{summary}}"
output_schema = { type = "object" }

[[edge]]
from = "a"
to = "b"
kind = "data"
port = "notaslot"
"#;
        assert_eq!(
            Definition::parse_toml(text),
            Err(DefinitionError::UnmatchedEdgePort {
                from: NodeKey::new("a"),
                to: NodeKey::new("b"),
                port: "notaslot".to_string(),
            })
        );
    }

    /// A port that *does* resolve to a `{{name}}` slot in the target's
    /// template is accepted — this is the paired positive case for the
    /// `an_edge_port_with_no_matching_placeholder_is_rejected` rejection.
    #[test]
    fn an_edge_port_matching_a_placeholder_is_accepted() {
        let text = r#"
name = "good-port"
[[node]]
key = "a"
label = "A"
prompt_template = "do it"
output_schema = { type = "object" }

[[node]]
key = "b"
label = "B"
prompt_template = "then this: {{ summary }}"
output_schema = { type = "object" }

[[edge]]
from = "a"
to = "b"
kind = "data"
port = "summary"
"#;
        assert!(Definition::parse_toml(text).is_ok());
    }

    #[test]
    fn the_spec_carries_the_documents_content_and_no_identity() {
        let definition = Definition::parse_toml(DOCUMENT).expect("document parses");
        let spec = definition.spec(&WorkflowId::new("workflow:abc"));
        assert_eq!(spec.workflow_id, WorkflowId::new("workflow:abc"));
        assert_eq!(spec.version, 0);
        assert_eq!(spec.parent, None);
        assert_eq!(spec.nodes.len(), 2);
        assert_eq!(spec.edges.len(), 1);
        assert_eq!(spec.args.len(), 1);
        assert_eq!(spec.contract, "Reply only through result.json.");
    }

    /// The pre-write gate `workflow.create` relies on to stay all-or-nothing.
    /// Each of these passes [`Definition::check`] and used to be caught only
    /// once the `workflow` row had already been committed.
    #[test]
    fn validate_graph_catches_what_definition_check_does_not() {
        let good = Definition::parse_toml(DOCUMENT).expect("document parses");
        assert!(good.validate_graph().is_ok());

        let cycle = r#"
name = "cycle"
[[node]]
key = "a"
label = "A"
prompt_template = "a"
output_schema = { type = "object" }
[[node]]
key = "b"
label = "B"
prompt_template = "b"
output_schema = { type = "object" }
[[edge]]
from = "a"
to = "b"
kind = "sequence"
[[edge]]
from = "b"
to = "a"
kind = "sequence"
"#;
        assert!(Definition::parse_toml(cycle)
            .expect("a cycle still parses")
            .validate_graph()
            .is_err());

        let duplicate_key = r#"
name = "duplicate"
[[node]]
key = "a"
label = "A"
prompt_template = "a"
output_schema = { type = "object" }
[[node]]
key = "a"
label = "A again"
prompt_template = "a"
output_schema = { type = "object" }
"#;
        assert!(matches!(
            Definition::parse_toml(duplicate_key)
                .expect("a duplicate key still parses")
                .validate_graph(),
            Err(KvdagError::DuplicateNodeKey(_))
        ));

        let unknown_endpoint = r#"
name = "dangling"
[[node]]
key = "a"
label = "A"
prompt_template = "a"
output_schema = { type = "object" }
[[edge]]
from = "a"
to = "ghost"
kind = "sequence"
"#;
        assert!(matches!(
            Definition::parse_toml(unknown_endpoint)
                .expect("a dangling edge still parses")
                .validate_graph(),
            Err(KvdagError::UnknownEdgeEndpoint { .. })
        ));
    }
}
