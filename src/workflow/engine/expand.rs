//! Expansion proposals, guardrails, and commit/reject.
//!
//! Phase 2 (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.4). The
//! types land now because the journal, the events, and the growth-limit badge
//! are already part of the Phase 1 vocabulary. A node cannot create nodes; it
//! proposes, and a rejection is always surfaced, never silently truncated.

use std::collections::BTreeMap;

use crate::workflow::model::NodeKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandProposal {
    pub template: NodeKey,
    pub label: String,
    pub inputs: BTreeMap<String, String>,
    pub count: Option<u16>,
}

/// Why a proposal was refused. Each variant carries the exact limit hit so the
/// DAG view can render it on the proposing node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandRejection {
    NotAllowed { template: NodeKey },
    UnknownTemplate { template: NodeKey },
    NotATemplate { template: NodeKey },
    ExpandMaxReached { limit: u16 },
    MaxDepthReached { limit: u16 },
    MaxNodesReached { limit: u16 },
}
