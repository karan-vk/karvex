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

use crate::workflow::model::{Demand, Kvdag, KvdagNode, NodeKey, RestoredSeed, RunId};
use crate::workflow::tier::{self, Assignment, HistoryIndex, Tier};
use crate::workflow::watchdog::WATCHDOG_FRAME;

/// Bumped whenever the rendered text changes in a way a lead would behave
/// differently for. Recorded in the prompt itself so a stored run says which
/// contract produced it, and asserted by the render tests so a change to the
/// template is never silent.
///
/// v3 (`phase4-retarget-plan.md` P14): renders a node's authored
/// `output_schema` and `timeout_ms` (D-6, previously silent no-ops), the
/// server's `[workflow] max_parallel_nodes` as a concurrency hint (also
/// D-6), what `--restore-from` resolved (WI-R1 — previously computed and
/// discarded), and a heads-up about the watchdog so a rung-3 escalation does
/// not arrive as an unexplained interruption.
pub const LEAD_PROMPT_VERSION: u32 = 3;

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
    /// `context/prior-runs.md`, when this run was given the summaries of past
    /// runs of the same workflow (`workflow.run`'s `include_prior_summaries`,
    /// which defaults on). `None` when the caller opted out or there is no
    /// history yet — karvex writes the file either way, so the lead has to be
    /// *told* it exists or the parameter does nothing at all.
    pub prior_runs_path: Option<&'a str>,
    /// `[workflow] max_parallel_nodes` (config default 4). D-6: nothing
    /// downstream of the launch path reads this any more — there is no
    /// engine left to cap concurrency — so rendering it here as a hint is
    /// the only way an authored/configured value does anything at all.
    pub max_parallel_nodes: usize,
    /// What `--restore-from` resolved, when this run asked for one (WI-R1).
    /// `None` when the run did not ask. `Some` with empty seeds when it
    /// asked and nothing was restorable — that is still told to the lead,
    /// not silently collapsed into `None`, so "asked and got nothing" never
    /// looks identical to "never asked".
    pub restore: Option<RestoreContext<'a>>,
}

/// What a `--restore-from` request resolved to, for [`render_restore`].
///
/// Deliberately thin: the full skip taxonomy
/// (`crate::api::schema::WorkflowRestoreSkipReason`) already reaches the
/// caller in the `workflow.run` response's restore report, so the lead only
/// needs the fact that some selectors were skipped, not why — and this type
/// stays pure by not depending on the wire schema to say even that much.
#[derive(Debug, Clone, Copy)]
pub struct RestoreContext<'a> {
    /// The run this run asked to restore from.
    pub source_run: &'a RunId,
    /// Nodes that resolved to a usable checkpoint, in resolution order.
    pub seeds: &'a [RestoredSeed],
    /// How many requested selectors did not resolve to a seed.
    pub skipped: usize,
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
    render_restore(&mut out, input);
    render_prior_runs(&mut out, input);
    render_contract(&mut out, kvdag);
    render_plan(&mut out, input);
    render_teammates(&mut out, input);
    render_watchdog(&mut out);
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

/// WI-R1: `--restore-from` used to resolve a source run into seeds and then
/// discard them (`app/api/workflows.rs`'s `let _ = (&assignments, &seeds, /// &context_runs, &restore_from_run);`) — a "restore" that silently started
/// an ordinary fresh run. This renders what was actually resolved, so the
/// lead is told what to carry forward instead of the selection vanishing
/// between the resolver and the plan.
fn render_restore(out: &mut String, input: &LeadPromptInput<'_>) {
    let Some(restore) = &input.restore else {
        return;
    };
    out.push_str("## Restored from a previous run\n\n");
    if restore.seeds.is_empty() {
        let _ = writeln!(
            out,
            "This run was started with `--restore-from {}`, but none of the requested selectors could be restored (see this run's restore report for why). Every task below is planned fresh, the same as any other run.",
            restore.source_run.as_str(),
        );
        out.push('\n');
        return;
    }
    let _ = writeln!(
        out,
        "This run was started with `--restore-from {}`. The tasks below already have a result from that run — carry it forward instead of redoing the work, unless you have a specific reason to believe it is stale or wrong. Any planned task not listed here has nothing restored and should be planned normally.",
        restore.source_run.as_str(),
    );
    out.push('\n');
    for seed in restore.seeds {
        let _ = writeln!(
            out,
            "### `{}` — restored from `{}` (checkpoint {})",
            seed.node_key.as_str(),
            seed.source.run.as_str(),
            seed.source.checkpoint_seq,
        );
        out.push('\n');
        if !seed.summary.trim().is_empty() {
            let _ = writeln!(out, "Summary: {}", seed.summary.trim());
            out.push('\n');
        }
        out.push_str("Result:\n\n");
        push_json_block(out, &seed.payload);
        if !seed.artifact_paths.is_empty() {
            let _ = writeln!(out, "Artifacts: {}", seed.artifact_paths.join(", "));
        }
        out.push('\n');
    }
    if restore.skipped > 0 {
        let _ = writeln!(
            out,
            "{} requested selector{} could not be restored and will be planned fresh like any other task (see this run's restore report for why).",
            restore.skipped,
            plural(restore.skipped),
        );
        out.push('\n');
    }
}

fn render_prior_runs(out: &mut String, input: &LeadPromptInput<'_>) {
    let Some(path) = input.prior_runs_path else {
        return;
    };
    out.push_str("## What past runs of this workflow found\n\n");
    let _ = writeln!(
        out,
        "`{path}` holds the summaries of previous runs of this same workflow. Read \
         it before you plan: it is the cheapest way to avoid repeating work that \
         has already been done, or a mistake that has already been made."
    );
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
        render_output_shape(out, node);
        render_time_budget(out, node);

        out.push_str("- Description to give the teammate:\n\n");
        let body = interpolate(&node.prompt_template, input.args);
        for line in body.trim_end().lines() {
            let _ = writeln!(out, " > {line}");
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

/// D-6: an authored `output_schema` used to be a silent no-op — nothing
/// read it once the engine that validated `result.json` against it was
/// deleted (`phase4-retarget-plan.md` §1.2). Rendered only when the author
/// wrote something beyond the unauthored default (`definition.rs`'s
/// `apply_authoring_defaults` backfills the empty, accept-anything `{}`), so
/// a node nobody put a schema on stays silent rather than showing an empty
/// fence.
fn render_output_shape(out: &mut String, node: &KvdagNode) {
    let schema = node.output_schema.as_json();
    if schema == &serde_json::json!({}) {
        return;
    }
    out.push_str(
        "- Result shape: this task's result should match this JSON Schema — karvex does not validate it any more (there is no engine left to enforce it), but downstream tasks and your own run summary depend on the shape being right:\n\n",
    );
    push_json_block(out, schema);
}

/// D-6: `timeout_ms` "becomes real in this phase (as a surfaced budget)" —
/// the watchdog (P4, `workflow::watchdog::ObservedNode::budget_exceeded`)
/// already treats an exceeded budget as a fact about the node, jumping
/// straight to the silent rung-4 "surface" mark and skipping the usual
/// nudge/re-prompt. This is the other half: telling the lead the number
/// exists and what happens if it is blown, so the mark does not appear out
/// of nowhere.
fn render_time_budget(out: &mut String, node: &KvdagNode) {
    let Some(timeout_ms) = node.timeout_ms else {
        return;
    };
    let _ = writeln!(
        out,
        "- Time budget: about {}. Karvex does not enforce this — nothing can stop the clock — but if this task badly overruns it, the watchdog skips its usual nudge-then-reprompt and silently marks the task as needing attention instead.",
        human_duration(timeout_ms),
    );
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
    // D-6: `max_parallel_nodes` used to be unreachable — nothing downstream of
    // the launch path reads it once a run starts (`WI-R2`). Rendered as a hint
    // rather than dropped, per D-6's recommendation.
    let _ = writeln!(
        out,
        "This server's configured concurrency guideline (`[workflow] max_parallel_nodes`) is **{}**: how many teammates to keep in flight at once as a starting point, not a hard cap karvex enforces — there is no engine watching your team's size.",
        input.max_parallel_nodes,
    );
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

/// P14 (b): the lead was never told karvex might message it, so a rung-3
/// escalation about a stuck teammate — or a direct nudge/re-prompt about the
/// lead's own pane — used to arrive as an unexplained interruption. Sourced
/// from `workflow::watchdog`'s actual behaviour (P4, binding on this
/// packet), not re-described from memory: the ladder order, what each rung
/// can and cannot do, the lead's own shortened ladder (nudge, re-prompt,
/// silent surface — no rung 3, escalating a lead to itself is a message to
/// nobody), and the frame constant itself are all read from there so this
/// paragraph cannot drift out of sync with the sentences karvex actually
/// sends.
fn render_watchdog(out: &mut String) {
    out.push_str("## If karvex messages you\n\n");
    out.push_str(
        "Karvex watches this run passively: your team's shared task list, and what each teammate's pane is actually doing, on its own poll cadence. It never claims, retries, or restarts anything itself — but if a task stays `in_progress` while its owner's pane looks idle for a while, karvex escalates in steps: first it nudges that teammate directly, then it re-prompts them naming the exact disagreement between the task status and the pane, and only if neither lands does it tell **you** instead — with what it measured and the two things only you can do about it: reassign the task, or respawn the teammate. Karvex itself cannot restart, reassign, or respawn anyone; that is why it asks you.\n\n",
    );
    out.push_str(
        "Karvex may nudge or re-prompt *you*, the lead, the same way if your own pane looks stuck — for example, if you have gone quiet without writing the summary and calling `kvx workflow run finish`. There is nobody above you to escalate to, so if you stay unresponsive after that karvex stops sending messages and simply marks the run as needing attention instead.\n\n",
    );
    let _ = writeln!(
        out,
        "A task or run that badly overruns its authored time budget can also be marked that way directly, skipping the nudges — see \"Time budget\" on the tasks that have one, above.\n",
    );
    let _ = writeln!(
        out,
        "Every message karvex sends this way opens with the line `{WATCHDOG_FRAME}`. It comes from the karvex runtime around this session, not from your human operator, and it is not a request you reply to — read it and act in your own session, the same way you would act on your own judgment.\n",
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

/// A fenced ```json``` block, used for both a node's `output_schema` and a
/// restored seed's `payload`.
fn push_json_block(out: &mut String, value: &serde_json::Value) {
    out.push_str("```json\n");
    out.push_str(&serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
    out.push_str("\n```\n\n");
}

/// `900_000` → `"15m"`. Coarse on purpose: the lead needs an order of
/// magnitude, not a stopwatch.
fn human_duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rest_minutes = minutes % 60;
    if rest_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{rest_minutes}m")
    }
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
        KvdagVersionId, NodeKind, OutputSchema, RestoredRef, Runner, WorkflowId,
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
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: None,
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
        assert_eq!(
            LEAD_PROMPT_VERSION, 3,
            "bump the contract when the render changes"
        );
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

    /// The `include_prior_summaries` parameter writes `context/prior-runs.md`
    /// whether or not anything reads it, so the render is the only thing that
    /// makes it mean something.
    #[test]
    fn prior_run_context_is_named_when_the_run_was_given_any() {
        let kvdag = fixture();
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
            prior_runs_path: Some("/runs/abc123/context/prior-runs.md"),
            max_parallel_nodes: 4,
            restore: None,
        });
        assert!(text.contains("## What past runs of this workflow found"));
        assert!(text.contains("`/runs/abc123/context/prior-runs.md`"));
        assert!(text.contains("Read it before you plan"));
    }

    #[test]
    fn prior_run_context_is_absent_when_the_run_was_given_none() {
        let text = render(Tier::High);
        assert!(!text.contains("What past runs of this workflow found"));
        assert!(!text.contains("prior-runs.md"));
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
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: None,
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
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: None,
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

    #[test]
    fn a_node_with_no_authored_output_schema_renders_no_result_shape_paragraph() {
        // `definition.rs`'s `apply_authoring_defaults` backfills an omitted
        // `output_schema` to the empty, accept-anything `{}` — the same shape
        // as v2's silent no-op. P14 must add nothing for it.
        let mut kvdag = fixture();
        for node in &mut kvdag.nodes {
            node.output_schema = OutputSchema::parse(serde_json::json!({})).unwrap();
        }
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
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: None,
        });
        assert!(!text.contains("Result shape:"));
    }

    #[test]
    fn a_node_with_an_authored_output_schema_renders_it_as_a_result_shape() {
        // The default `fixture()` nodes already carry `schema()` — a
        // non-trivial `{"type": "object", "required": ["report"]}` — so this
        // is exactly what an authored, unread `output_schema` looks like
        // before this packet (D-6's third silent no-op).
        let text = render(Tier::High);
        assert!(text.contains("- Result shape: this task's result should match"));
        assert!(text.contains("karvex does not validate it any more"));
        assert!(text.contains("```json"));
        assert!(text.contains("\"required\""));
        assert!(text.contains("\"report\""));
    }

    #[test]
    fn a_node_with_a_time_budget_renders_it_and_the_watchdog_consequence() {
        let mut kvdag = fixture();
        kvdag.nodes[0].timeout_ms = Some(15 * 60_000);
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
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: None,
        });
        assert!(text.contains("- Time budget: about 15m."));
        assert!(text.contains("skips its usual nudge-then-reprompt"));
    }

    #[test]
    fn a_node_with_no_time_budget_renders_no_time_budget_line() {
        let text = render(Tier::High);
        assert!(!text.contains("Time budget:"));
    }

    #[test]
    fn max_parallel_nodes_is_rendered_as_a_concurrency_hint() {
        let kvdag = fixture();
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
            prior_runs_path: None,
            max_parallel_nodes: 9,
            restore: None,
        });
        assert!(text.contains("max_parallel_nodes"));
        assert!(text.contains("**9**"));
        assert!(text.contains("not a hard cap karvex enforces"));
    }

    #[test]
    fn the_watchdog_paragraph_explains_the_ladder_and_uses_the_shared_frame() {
        let text = render(Tier::High);
        assert!(text.contains("## If karvex messages you"));
        // The frame is the shared constant `watchdog::WATCHDOG_FRAME`, not a
        // re-typed literal, so this and the sentences karvex actually sends
        // cannot drift apart (`phase4-retarget-plan.md` P14's requirement
        // that this prompt "must not contradict" P4's watchdog wording).
        assert!(text.contains(&format!("`{WATCHDOG_FRAME}`")));
        assert_eq!(WATCHDOG_FRAME, "[karvex \u{b7} watchdog]");
        assert!(text.contains("cannot restart, reassign, or respawn anyone"));
        assert!(text.contains("nobody above you to escalate to"));
        assert!(text.contains("not from your human operator"));
    }

    #[test]
    fn restore_is_absent_when_the_run_did_not_ask_for_one() {
        let text = render(Tier::High);
        assert!(!text.contains("Restored from a previous run"));
        assert!(!text.contains("--restore-from"));
    }

    #[test]
    fn restore_renders_the_carried_forward_seeds() {
        let kvdag = fixture();
        let args = args();
        let history = HistoryIndex::new();
        let run = RunId::new("workflow_run:abc123");
        let source = RunId::new("workflow_run:source1");
        let seeds = vec![RestoredSeed {
            node_key: NodeKey::new("research"),
            payload: serde_json::json!({"findings": "already surveyed"}),
            summary: "Surveyed three approaches; picked the second.".to_string(),
            artifact_paths: vec!["/runs/source1/research/notes.md".to_string()],
            digest: "sha256:deadbeef".to_string(),
            source: RestoredRef {
                run: source.clone(),
                node_key: NodeKey::new("research"),
                checkpoint_seq: 2,
            },
        }];
        let restore = RestoreContext {
            source_run: &source,
            seeds: &seeds,
            skipped: 1,
        };
        let text = render_lead_prompt(&LeadPromptInput {
            run_id: &run,
            workflow_name: "parser-work",
            kvdag: &kvdag,
            tier: Tier::High,
            args: &args,
            history: &history,
            summary_path: "/runs/abc123/summary.md",
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: Some(restore),
        });
        assert!(text.contains("## Restored from a previous run"));
        assert!(text.contains("--restore-from workflow_run:source1"));
        assert!(
            text.contains("### `research` — restored from `workflow_run:source1` (checkpoint 2)")
        );
        assert!(text.contains("Surveyed three approaches; picked the second."));
        assert!(text.contains("\"already surveyed\""));
        assert!(text.contains("/runs/source1/research/notes.md"));
        assert!(text.contains("1 requested selector could not be restored"));
        assert!(text.contains("carry it forward instead of redoing the work"));
    }

    #[test]
    fn restore_says_so_honestly_when_nothing_was_restorable() {
        let kvdag = fixture();
        let args = args();
        let history = HistoryIndex::new();
        let run = RunId::new("workflow_run:abc123");
        let source = RunId::new("workflow_run:source1");
        let seeds: Vec<RestoredSeed> = Vec::new();
        let restore = RestoreContext {
            source_run: &source,
            seeds: &seeds,
            skipped: 3,
        };
        let text = render_lead_prompt(&LeadPromptInput {
            run_id: &run,
            workflow_name: "parser-work",
            kvdag: &kvdag,
            tier: Tier::High,
            args: &args,
            history: &history,
            summary_path: "/runs/abc123/summary.md",
            prior_runs_path: None,
            max_parallel_nodes: 4,
            restore: Some(restore),
        });
        assert!(text.contains("## Restored from a previous run"));
        assert!(text.contains("none of the requested selectors could be restored"));
        assert!(text.contains("planned fresh, the same as any other run"));
        // Honest, not misleading: no per-node restore blocks were rendered.
        assert!(!text.contains("### `research` — restored from"));
    }
}
