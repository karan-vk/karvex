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

use crate::workflow::model::{ArgSpec, GrowthLimits, KvdagEdge, KvdagNode, KvdagSpec, WorkflowId};
use crate::workflow::tier::Tier;

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
}

impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "invalid definition document: {message}"),
            Self::Empty(field) => write!(f, "definition document has no {field}"),
        }
    }
}

impl std::error::Error for DefinitionError {}

impl Definition {
    pub fn parse_toml(text: &str) -> Result<Self, DefinitionError> {
        let parsed: Self =
            toml::from_str(text).map_err(|error| DefinitionError::Parse(error.to_string()))?;
        parsed.check()
    }

    pub fn parse_json(text: &str) -> Result<Self, DefinitionError> {
        let parsed: Self = serde_json::from_str(text)
            .map_err(|error| DefinitionError::Parse(error.to_string()))?;
        parsed.check()
    }

    fn check(self) -> Result<Self, DefinitionError> {
        if self.name.trim().is_empty() {
            return Err(DefinitionError::Empty("name"));
        }
        if self.node.is_empty() {
            return Err(DefinitionError::Empty("nodes"));
        }
        Ok(self)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Demand, EdgeKind, Runner};

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
}
