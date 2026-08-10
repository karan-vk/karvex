//! The render contract: definition version + args → the team lead's prompt.
//!
//! `09-agent-teams-rework.md` §3.2. Karvex no longer executes a workflow; it
//! hands one Claude Code team lead a plan and records what the team actually
//! does. This rendered text is therefore the *entire* influence karvex has on
//! execution, which is why it lives here as a pure, versioned, unit-tested
//! function rather than as string building scattered through the launch path.
//!
//! Pure by construction, in the sense `workflow::model` and `workflow::tier`
//! are: definition + args + tier resolution in, `String` out. No store, no
//! runtime, no filesystem.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::workflow::model::{Demand, Kvdag, KvdagNode, NodeKey, RunId};
use crate::workflow::tier::{self, Assignment, HistoryIndex, Tier};

/// Bumped whenever the rendered text changes in a way a lead would behave
/// differently for. Recorded in the prompt itself so a stored run says which
/// contract produced it, and asserted by the render tests so a change to the
/// template is never silent.
pub const LEAD_PROMPT_VERSION: u32 = 1;

/// The separator between a node key and its label in a task subject. The
/// projection (§3.4) matches Claude Code tasks back to definition nodes by
/// this prefix, so the two halves of the contract share one constant.
pub const SUBJECT_SEPARATOR: &str = ": ";

/// Everything the render needs. Borrowed, so the launch path can build one
/// from the graph it already loaded without cloning it.
#[derive(Debug)]
pub struct LeadPromptInput<'a> {
    pub run_id: &'a RunId,
    /// The workflow's display name (`workflow.name`), not its id.
    pub workflow_name: &'a str,
    pub kvdag: &'a Kvdag,
    pub tier: Tier,
    /// The run's resolved arguments, already defaulted and validated.
    pub args: &'a BTreeMap<String, String>,
    /// Per-node history for `Tier::Auto`'s resolution. Empty is legal and
    /// makes `auto` behave like the `high` row, which is what a first run of a
    /// definition gets anyway.
    pub history: &'a HistoryIndex,
    /// Where the lead is told to write its run summary, as an absolute path.
    /// The launch path points this inside the run directory.
    pub summary_path: &'a str,
}

/// The task subject karvex asks the lead to use for a definition node, and
/// which [`subject_node_key`] parses back.
pub fn task_subject(node: &KvdagNode) -> String {
    if node.label.trim().is_empty() {
        node.key.as_str().to_string()
    } else {
        format!("{}{}{}", node.key.as_str(), SUBJECT_SEPARATOR, node.label)
    }
}

/// The inverse of [`task_subject`]: which definition node, if any, an observed
/// Claude Code task subject belongs to.
///
/// Deliberately prefix-based and forgiving — the lead is allowed to reword a
/// subject's tail (§3.2's "loose" paragraph), and only the key prefix is
/// contractual. A subject that matches no key is an *emergent* task and is
/// recorded as such rather than being forced onto a node.
pub fn subject_node_key(subject: &str, keys: &[NodeKey]) -> Option<NodeKey> {
    let trimmed = subject.trim();
    // Longest key first, so `review-notes` never loses to `review`.
    let mut candidates: Vec<&NodeKey> = keys.iter().collect();
    candidates.sort_by_key(|key| std::cmp::Reverse(key.as_str().len()));
    candidates
        .into_iter()
        .find(|key| {
            let key = key.as_str();
            match trimmed.strip_prefix(key) {
                // Bare key, or key followed by the separator's first char.
                Some(rest) => rest.is_empty() || rest.starts_with(':'),
                None => false,
            }
        })
        .cloned()
}

/// Renders the lead prompt. See the module doc for why this is pure.
pub fn render_lead_prompt(input: &LeadPromptInput<'_>) -> String {
    let mut out = String::with_capacity(4096);
    let kvdag = input.kvdag;

    let _ = writeln!(out, "# Workflow run: {}", input.workflow_name);
    out.push('\n');
    out.push_str(
        "You are the **team lead** for a karvex workflow run. Karvex authored the plan \
         below and launched you to carry it out with a Claude Code agent team. Karvex \
         does not schedule, claim, retry, or validate anything: you do, through the \
         shared task list and your own judgment. Karvex watches your team's task list \
         and records what actually happens.\n",
    );
    out.push('\n');

    let _ = writeln!(out, "- Workflow: `{}`", input.workflow_name);
    let _ = writeln!(out, "- Run id: `{}`", input.run_id.as_str());
    let _ = writeln!(out, "- Definition version: `v{}`", kvdag.version);
    let _ = writeln!(out, "- Tier: `{}`", input.tier.as_str());
    let _ = writeln!(out, "- Lead prompt contract: `v{LEAD_PROMPT_VERSION}`");
    out.push('\n');

    render_args(&mut out, input);
    render_contract(&mut out, kvdag);
    render_plan(&mut out, input);
    render_teammates(&mut out, input);
    render_finish(&mut out, input);
    render_loose(&mut out);

    out
}

fn render_args(out: &mut String, input: &LeadPromptInput<'_>) {
    out.push_str("## Arguments\n\n");
    if input.args.is_empty() {
        out.push_str("This run was started with no arguments.\n\n");
        return;
    }
    for arg in &input.kvdag.args {
        let value = input.args.get(&arg.name);
        let described = if arg.description.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", arg.description.trim())
        };
        match value {
            Some(value) => {
                let _ = writeln!(out, "- `{}` = {}{}", arg.name, quoted(value), described);
            }
            None => {
                let _ = writeln!(out, "- `{}` = (unset){}", arg.name, described);
            }
        }
    }
    // Arguments the run carries that the definition never declared are still
    // shown: they were accepted at launch, so hiding them here would make the
    // prompt disagree with the run record.
    let declared: Vec<&str> = input.kvdag.args.iter().map(|a| a.name.as_str()).collect();
    for (name, value) in input.args {
        if !declared.contains(&name.as_str()) {
            let _ = writeln!(out, "- `{name}` = {}", quoted(value));
        }
    }
    out.push('\n');
}

fn render_contract(out: &mut String, kvdag: &Kvdag) {
    let contract = kvdag.contract.trim();
    if contract.is_empty() {
        return;
    }
    out.push_str("## Standing contract\n\n");
    out.push_str(
        "Every teammate on this run inherits the following. Repeat it in the prompts \
         you spawn teammates with.\n\n",
    );
    for line in contract.lines() {
        let _ = writeln!(out, "> {line}");
    }
    out.push('\n');
}

fn render_plan(out: &mut String, input: &LeadPromptInput<'_>) {
    let kvdag = input.kvdag;
    out.push_str("## The plan\n\n");
    out.push_str(
        "Create one task per item below, in this order, using your task tool. Use the \
         **exact subject** given — karvex matches your tasks back to the plan by the \
         `node-id:` prefix, and a reworded prefix is recorded as new, unplanned work. \
         Set `blockedBy` exactly as listed so the team's dependency graph is the \
         plan's graph.\n\n",
    );

    let planned: Vec<&KvdagNode> = kvdag
        .nodes
        .iter()
        .filter(|node| !node.is_template)
        .collect();
    for (index, node) in planned.iter().enumerate() {
        let _ = writeln!(out, "### {}. {}", index + 1, task_subject(node));
        out.push('\n');
        let _ = writeln!(out, "- Subject (verbatim): `{}`", task_subject(node));

        let blocked_by: Vec<String> = kvdag
            .inbound_edges(&node.key)
            .filter_map(|edge| kvdag.node(&edge.from))
            .filter(|from| !from.is_template)
            .map(|from| format!("`{}`", task_subject(from)))
            .collect();
        if blocked_by.is_empty() {
            out.push_str("- Blocked by: nothing — this one can start immediately.\n");
        } else {
            let _ = writeln!(out, "- Blocked by: {}", blocked_by.join(", "));
        }

        let assignment = resolve_for(input, node);
        let _ = writeln!(
            out,
            "- Suggested model: `{}` at `{}` effort (declared demand: `{}`)",
            assignment.model.as_str(),
            assignment.effort.as_str(),
            demand_str(node.demand),
        );
        if !node.role.trim().is_empty() {
            let _ = writeln!(out, "- Role: {}", node.role.trim());
        }

        out.push_str("- Description to give the teammate:\n\n");
        let body = interpolate(&node.prompt_template, input.args);
        for line in body.trim_end().lines() {
            let _ = writeln!(out, "  > {line}");
        }
        out.push('\n');
    }

    let unresolved_note = planned
        .iter()
        .any(|node| !remaining_placeholders(&node.prompt_template, input.args).is_empty());
    if unresolved_note {
        out.push_str(
            "Descriptions still containing `{{slot}}` markers take that slot from the \
             upstream task's result: hand the blocking task's output to the teammate in \
             place of the marker when you spawn it.\n\n",
        );
    }
}

fn render_teammates(out: &mut String, input: &LeadPromptInput<'_>) {
    let kvdag = input.kvdag;
    let planned: Vec<&KvdagNode> = kvdag
        .nodes
        .iter()
        .filter(|node| !node.is_template)
        .collect();
    let suggested = suggested_teammates(&planned, kvdag);

    out.push_str("## Teammates\n\n");
    let _ = writeln!(
        out,
        "This plan has {} task{} and about {} of them can run at once, so around **{} \
         teammate{}** is the right size to start with. Spawn more if the work fans out; \
         karvex records whatever you actually spawn.",
        planned.len(),
        plural(planned.len()),
        widest_ready_set(&planned, kvdag),
        suggested,
        plural(suggested),
    );
    out.push('\n');
    out.push_str(
        "**Name each teammate after the node it owns** — use the bare node id (the part \
         before the `:` in the subject) as the teammate's name. Karvex labels the \
         teammate's pane with that name, so a well-named team makes the run readable \
         at a glance.\n\n",
    );

    let mut by_model: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for node in &planned {
        let assignment = resolve_for(input, node);
        by_model
            .entry(assignment.model.as_str())
            .or_default()
            .push(format!("`{}`", node.key.as_str()));
    }
    let _ = writeln!(
        out,
        "Model hints for tier `{}` (hints, not rules — spend more if a task turns out \
         to deserve it):",
        input.tier.as_str(),
    );
    out.push('\n');
    for (model, keys) in &by_model {
        let _ = writeln!(out, "- `{model}` for {}", keys.join(", "));
    }
    out.push('\n');
}

fn render_finish(out: &mut String, input: &LeadPromptInput<'_>) {
    out.push_str("## Finishing the run\n\n");
    out.push_str(
        "When every task is complete (or you have concluded the run cannot go \
         further), do exactly this, in order:\n\n",
    );
    let _ = writeln!(
        out,
        "1. Write the run summary as markdown to `{}`. Say what was done, what the \
         outcome was, what is still open, and one line per task.",
        input.summary_path,
    );
    let _ = writeln!(
        out,
        "2. Run `kvx workflow run finish --summary-file {}` from your own pane.",
        input.summary_path,
    );
    out.push('\n');
    out.push_str(
        "`kvx` is on your PATH and `KARVEX_WORKFLOW_RUN_ID` is already exported in this \
         pane, so the command identifies this run by itself. That single call is what \
         closes the run: karvex will not decide on its own that you are finished. If \
         the run failed, still call it — say so in the summary.\n\n",
    );
    out.push_str(
        "Add a final task to the list for this (subject `finish: write the run summary \
         and report`), blocked by every other task, so it cannot be forgotten.\n\n",
    );
}

fn render_loose(out: &mut String) {
    out.push_str("## The plan is a plan, not a cage\n\n");
    out.push_str(
        "Split a task that turned out to be two. Merge two that turned out to be one. \
         Add work the plan did not anticipate. Skip work that turned out to be \
         unnecessary — say why in the summary. Karvex records what the team actually \
         did, including tasks that were never in the plan, rather than enforcing what \
         it asked for. Keep the `node-id:` prefixes on the tasks that *do* correspond \
         to plan items, and everything else is yours to decide.\n",
    );
}

// ── helpers ────────────────────────────────────────────────────────────────

fn resolve_for(input: &LeadPromptInput<'_>, node: &KvdagNode) -> Assignment {
    tier::resolve(input.tier, node.demand, input.history.get(&node.key))
}

fn demand_str(demand: Demand) -> &'static str {
    match demand {
        Demand::Peak => "peak",
        Demand::Critical => "critical",
        Demand::Standard => "standard",
        Demand::Light => "light",
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// How many planned nodes have no planned blocker — the run's opening ready
/// set, and the honest lower bound on useful parallelism.
fn widest_ready_set(planned: &[&KvdagNode], kvdag: &Kvdag) -> usize {
    planned
        .iter()
        .filter(|node| {
            kvdag
                .inbound_edges(&node.key)
                .filter_map(|edge| kvdag.node(&edge.from))
                .all(|from| from.is_template)
        })
        .count()
        .max(1)
}

/// A teammate count hint, not a limit. Bounded above so a 40-node definition
/// does not suggest a 40-way fan-out the machine cannot host.
fn suggested_teammates(planned: &[&KvdagNode], kvdag: &Kvdag) -> usize {
    widest_ready_set(planned, kvdag)
        .clamp(1, 6)
        .min(planned.len().max(1))
}

fn quoted(value: &str) -> String {
    if value.contains('\n') {
        format!("\n\n```\n{}\n```\n", value.trim_end())
    } else {
        format!("`{value}`")
    }
}

/// Fills `{{name}}` slots from the run's args, leaving unknown slots intact:
/// those are edge ports, filled from an upstream task's result, and the lead
/// is told as much.
fn interpolate(template: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = after[..close].trim();
        match args.get(name) {
            Some(value) => out.push_str(value),
            None => {
                out.push_str("{{");
                out.push_str(&after[..close]);
                out.push_str("}}");
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

fn remaining_placeholders(template: &str, args: &BTreeMap<String, String>) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let name = after[..close].trim();
        if !args.contains_key(name) {
            names.push(name.to_string());
        }
        rest = &after[close + 2..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{
        ArgSpec, EdgeKind, EdgePayload, GrowthLimits, Isolation, KvdagEdge, KvdagSpec,
        KvdagVersionId, NodeKind, OutputSchema, Runner, WorkflowId,
    };

    fn schema() -> OutputSchema {
        OutputSchema::parse(serde_json::json!({
            "type": "object",
            "required": ["report"],
        }))
        .expect("fixture schema is valid")
    }

    fn node(key: &str, label: &str, demand: Demand, template: &str) -> KvdagNode {
        KvdagNode {
            key: NodeKey::new(key),
            label: label.to_string(),
            role: String::new(),
            kind: NodeKind::Agent,
            demand,
            runner: Runner::Agent,
            command: None,
            prompt_template: template.to_string(),
            system_contract: None,
            output_schema: schema(),
            max_attempts: 2,
            timeout_ms: None,
            isolation: Isolation::None,
            is_template: false,
            expand_allow: Vec::new(),
            expand_max: 0,
        }
    }

    fn edge(from: &str, to: &str) -> KvdagEdge {
        KvdagEdge {
            from: NodeKey::new(from),
            to: NodeKey::new(to),
            kind: EdgeKind::Sequence,
            condition: None,
            payload: EdgePayload::Summary,
            port: None,
        }
    }

    /// research → build → review, with one arg and one edge port.
    fn fixture() -> Kvdag {
        let spec = KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:v1"),
            workflow_id: WorkflowId::new("workflow:1"),
            version: 3,
            parent: None,
            contract: "Never push to master.".to_string(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: true,
                default: None,
                description: "what to build".to_string(),
            }],
            nodes: vec![
                node(
                    "research",
                    "Survey the options",
                    Demand::Light,
                    "Research how to {{goal}}.",
                ),
                node(
                    "build",
                    "Implement it",
                    Demand::Peak,
                    "Build {{goal}} using {{findings}}.",
                ),
                node(
                    "review",
                    "Review the change",
                    Demand::Standard,
                    "Review it.",
                ),
            ],
            edges: vec![
                KvdagEdge {
                    port: Some("findings".to_string()),
                    kind: EdgeKind::Data,
                    ..edge("research", "build")
                },
                edge("build", "review"),
            ],
        };
        Kvdag::try_new(spec).expect("fixture graph is valid")
    }

    fn args() -> BTreeMap<String, String> {
        BTreeMap::from([("goal".to_string(), "a faster parser".to_string())])
    }

    fn render(tier: Tier) -> String {
        let kvdag = fixture();
        let args = args();
        let history = HistoryIndex::new();
        let run = RunId::new("workflow_run:abc123");
        render_lead_prompt(&LeadPromptInput {
            run_id: &run,
            workflow_name: "parser-work",
            kvdag: &kvdag,
            tier,
            args: &args,
            history: &history,
            summary_path: "/runs/abc123/summary.md",
        })
    }

    #[test]
    fn the_header_pins_run_identity_and_the_contract_version() {
        let text = render(Tier::High);
        assert!(text.starts_with("# Workflow run: parser-work\n"));
        assert!(text.contains("- Run id: `workflow_run:abc123`"));
        assert!(text.contains("- Definition version: `v3`"));
        assert!(text.contains("- Tier: `high`"));
        assert!(text.contains(&format!("- Lead prompt contract: `v{LEAD_PROMPT_VERSION}`")));
    }

    #[test]
    fn every_planned_node_becomes_a_task_with_an_id_prefixed_subject() {
        let text = render(Tier::High);
        assert!(text.contains("- Subject (verbatim): `research: Survey the options`"));
        assert!(text.contains("- Subject (verbatim): `build: Implement it`"));
        assert!(text.contains("- Subject (verbatim): `review: Review the change`"));
    }

    #[test]
    fn edges_become_blocked_by_lines_and_roots_say_so() {
        let text = render(Tier::High);
        assert!(text.contains(
            "### 1. research: Survey the options\n\n- Subject (verbatim): \
             `research: Survey the options`\n- Blocked by: nothing"
        ));
        assert!(text.contains("- Blocked by: `research: Survey the options`"));
        assert!(text.contains("- Blocked by: `build: Implement it`"));
    }

    #[test]
    fn args_are_interpolated_into_descriptions_and_ports_are_left_for_the_lead() {
        let text = render(Tier::High);
        assert!(text.contains("> Research how to a faster parser."));
        assert!(text.contains("> Build a faster parser using {{findings}}."));
        assert!(text.contains("in place of the marker when you spawn it"));
    }

    #[test]
    fn tier_resolution_reaches_the_prompt_as_prose_per_node() {
        // `high` row: peak → opus/xhigh, standard → opus/high, light → sonnet/medium.
        let text = render(Tier::High);
        assert!(text
            .contains("- Suggested model: `sonnet` at `medium` effort (declared demand: `light`)"));
        assert!(
            text.contains("- Suggested model: `opus` at `xhigh` effort (declared demand: `peak`)")
        );
        assert!(text
            .contains("- Suggested model: `opus` at `high` effort (declared demand: `standard`)"));
        assert!(text.contains("- `sonnet` for `research`"));
        assert!(text.contains("- `opus` for `build`, `review`"));
    }

    #[test]
    fn a_lower_tier_moves_the_model_hints_without_changing_the_shape() {
        // `low` row is sonnet/low everywhere.
        let text = render(Tier::Low);
        assert!(text.contains("- `sonnet` for `research`, `build`, `review`"));
        assert!(!text.contains("- `opus` for"));
        assert!(text.contains("- Subject (verbatim): `build: Implement it`"));
    }

    #[test]
    fn the_naming_rule_and_teammate_count_are_stated() {
        let text = render(Tier::High);
        assert!(text.contains("**Name each teammate after the node it owns**"));
        // Only `research` is a root, so one teammate to start with.
        assert!(text.contains("around **1 teammate**"));
    }

    #[test]
    fn the_finish_rule_names_the_exact_command_and_summary_path() {
        let text = render(Tier::High);
        assert!(text.contains("kvx workflow run finish --summary-file /runs/abc123/summary.md"));
        assert!(text.contains("`KARVEX_WORKFLOW_RUN_ID` is already exported"));
        assert!(text.contains("subject `finish: write the run summary and report`"));
    }

    #[test]
    fn the_loose_paragraph_is_present() {
        let text = render(Tier::High);
        assert!(text.contains("## The plan is a plan, not a cage"));
        assert!(text.contains("Split a task that turned out to be two."));
    }

    #[test]
    fn the_standing_contract_is_quoted_for_the_lead_to_pass_on() {
        let text = render(Tier::High);
        assert!(text.contains("## Standing contract"));
        assert!(text.contains("> Never push to master."));
    }

    #[test]
    fn a_definition_with_no_args_says_so_rather_than_rendering_an_empty_list() {
        let kvdag = fixture();
        let empty = BTreeMap::new();
        let history = HistoryIndex::new();
        let run = RunId::new("workflow_run:abc123");
        let text = render_lead_prompt(&LeadPromptInput {
            run_id: &run,
            workflow_name: "parser-work",
            kvdag: &kvdag,
            tier: Tier::Medium,
            args: &empty,
            history: &history,
            summary_path: "/runs/abc123/summary.md",
        });
        assert!(text.contains("This run was started with no arguments."));
        // The unfilled arg slot stays visible in the description.
        assert!(text.contains("> Research how to {{goal}}."));
    }

    #[test]
    fn template_nodes_are_not_planned_as_tasks() {
        let mut spec_nodes = fixture().nodes;
        spec_nodes.push(KvdagNode {
            is_template: true,
            expand_max: 0,
            ..node("shard", "One shard", Demand::Light, "Do a shard.")
        });
        let kvdag = Kvdag::try_new(KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:v1"),
            workflow_id: WorkflowId::new("workflow:1"),
            version: 3,
            parent: None,
            contract: String::new(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: true,
                default: None,
                description: String::new(),
            }],
            nodes: spec_nodes,
            edges: vec![
                KvdagEdge {
                    port: Some("findings".to_string()),
                    kind: EdgeKind::Data,
                    ..edge("research", "build")
                },
                edge("build", "review"),
            ],
        })
        .expect("graph with a template node is valid");
        let args = args();
        let history = HistoryIndex::new();
        let run = RunId::new("workflow_run:abc123");
        let text = render_lead_prompt(&LeadPromptInput {
            run_id: &run,
            workflow_name: "parser-work",
            kvdag: &kvdag,
            tier: Tier::High,
            args: &args,
            history: &history,
            summary_path: "/runs/abc123/summary.md",
        });
        assert!(!text.contains("shard: One shard"));
        assert!(text.contains("This plan has 3 tasks"));
    }

    #[test]
    fn subject_round_trips_through_the_projection_matcher() {
        let kvdag = fixture();
        let keys: Vec<NodeKey> = kvdag.nodes.iter().map(|node| node.key.clone()).collect();
        for node in &kvdag.nodes {
            let subject = task_subject(node);
            assert_eq!(subject_node_key(&subject, &keys).as_ref(), Some(&node.key));
        }
    }

    #[test]
    fn the_matcher_prefers_the_longest_key_and_rejects_unrelated_subjects() {
        let keys = vec![NodeKey::new("review"), NodeKey::new("review-notes")];
        assert_eq!(
            subject_node_key("review-notes: tidy up", &keys),
            Some(NodeKey::new("review-notes")),
        );
        assert_eq!(
            subject_node_key("review: the change", &keys),
            Some(NodeKey::new("review")),
        );
        // An emergent task: no key prefix, so no node.
        assert_eq!(subject_node_key("chase down a flaky test", &keys), None);
        // A key that only appears as a word, not as a prefix, is not a match.
        assert_eq!(subject_node_key("please review: the change", &keys), None);
    }

    #[test]
    fn the_matcher_accepts_a_bare_key_and_a_reworded_tail() {
        let keys = vec![NodeKey::new("build")];
        assert_eq!(
            subject_node_key("build", &keys),
            Some(NodeKey::new("build"))
        );
        assert_eq!(
            subject_node_key("build: something entirely different", &keys),
            Some(NodeKey::new("build")),
        );
    }
}
