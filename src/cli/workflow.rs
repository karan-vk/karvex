//! `kvx workflow` — manual arg parsing over the `workflow.*` socket API
//! (`docs/design/workflow-builder/05-phase-plan.md` W5). Parsing is split
//! into pure `parse_workflow_*_args` helpers (no I/O, unit-tested directly)
//! and thin leaf functions that perform the file/env reads and the network
//! call, matching the convention already used by `src/cli/pane.rs`.

use std::collections::{HashMap, HashSet};

use crate::api::schema::{
    Method, Request, WorkflowCreateParams, WorkflowDefinitionDocument, WorkflowDefinitionFormat,
    WorkflowInterrogationMode, WorkflowNodeExpandParams, WorkflowNodeInterrogateParams,
    WorkflowNodeReportParams, WorkflowNodeSteerParams, WorkflowNodeTarget, WorkflowRestoreRequest,
    WorkflowReviewAnswerParams, WorkflowReviewApplyParams, WorkflowReviewReportParams,
    WorkflowRunFinishParams, WorkflowRunListParams, WorkflowRunMessageParams, WorkflowRunParams,
    WorkflowRunReportSessionParams, WorkflowRunTarget, WorkflowSummaryListParams, WorkflowTarget,
    WorkflowTier, WorkflowVersionCreateParams, WorkflowVersionTarget,
};

/// Env karvex injects into a node's pane
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §4.2), read by
/// `kvx workflow node complete` so the self-report contract needs no
/// positional arguments.
const NODE_ENV_RUN_ID: &str = "KARVEX_WORKFLOW_RUN_ID";
const NODE_ENV_NODE_PATH: &str = "KARVEX_WORKFLOW_NODE_PATH";
const NODE_ENV_NODE_DIR: &str = "KARVEX_WORKFLOW_NODE_DIR";
const NODE_ENV_NODE_TOKEN: &str = "KARVEX_WORKFLOW_NODE_TOKEN";

/// Env karvex exports into a review interview pane
/// (`crate::app::workflow_review::REVIEW_RUN_ID_ENV_VAR`, duplicated here as
/// a literal the same way [`NODE_ENV_RUN_ID`] duplicates
/// `binding::spawn::RUN_ID_ENV_VAR` above: that module is feature-gated
/// behind `workflow` and this one has to build with
/// `--no-default-features` too). **Not** [`NODE_ENV_RUN_ID`] — deliberately:
/// an interview pane never carries `KARVEX_WORKFLOW_RUN_ID`, so it cannot
/// close the very run it is reviewing
/// (`.local/prd/phase4-retarget-plan.md`, amendment log). `pub(crate)` so
/// `skill_parity.rs` can pin `SKILL.md`'s prose against the same literal.
pub(crate) const REVIEW_RUN_ID_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_RUN_ID";
/// The team-roster name an interview pane answers for
/// (`crate::app::workflow_review::REVIEW_MEMBER_ENV_VAR`). Absent from a
/// synthesis pane, which is what tells `kvx workflow review answer` from
/// `report` apart at the env level, not just by which verb was typed.
pub(crate) const REVIEW_MEMBER_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_MEMBER";
// `KARVEX_WORKFLOW_REVIEW_CYCLE` (`crate::app::workflow_review::
// REVIEW_CYCLE_ENV_VAR`) is exported into both interview and synthesis
// panes too, but neither `WorkflowReviewAnswerParams` nor
// `WorkflowReviewReportParams` carries a cycle field on the wire — P3 froze
// `run_id` (`+ member`, for `answer`) as the whole self-report target, and a
// run has at most one live review cycle at a time, so there is nothing here
// to default with it. Not read, on purpose: inventing a required flag this
// packet's own wire contract has no room for would be the same class of
// dishonesty the packet exists to remove.

/// Canonical `kvx workflow` verb paths. Hand-maintained alongside the match
/// arms below and checked against the clap tree in `src/cli/spec.rs` by a
/// parity test there — see `05-phase-plan.md` W5 ("this trio is
/// hand-maintained and silently drifts otherwise"). `cfg(test)`-only: its
/// sole purpose is the parity test, and `just windows-lint` builds without
/// `--all-targets`, so a non-test-gated unused const would fail `-D warnings`
/// there.
#[cfg(test)]
pub(super) const VERB_PATHS: &[&[&str]] = &[
    &["list"],
    &["show"],
    &["create"],
    &["update"],
    &["run", "start"],
    &["run", "list"],
    &["run", "show"],
    &["run", "cancel"],
    &["run", "finish"],
    &["run", "message"],
    &["run", "report-session"],
    &["node", "show"],
    &["node", "steer"],
    &["node", "interrupt"],
    &["node", "restart"],
    &["node", "complete"],
    &["node", "expand"],
    &["node", "interrogate"],
    &["summary", "show"],
    &["summary", "list"],
    &["review", "start"],
    &["review", "show"],
    &["review", "apply"],
    &["review", "answer"],
    &["review", "report"],
];

pub(super) fn run_workflow_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_help();
        return Ok(2);
    };

    match subcommand {
        "list" => workflow_list(&args[1..]),
        "show" => workflow_show(&args[1..]),
        "create" => workflow_create(&args[1..]),
        "update" => workflow_update(&args[1..]),
        "run" => run_workflow_run_command(&args[1..]),
        "node" => run_workflow_node_command(&args[1..]),
        "summary" => run_workflow_summary_command(&args[1..]),
        "review" => run_workflow_review_command(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_help();
            Ok(0)
        }
        _ => {
            print_workflow_help();
            Ok(2)
        }
    }
}

fn run_workflow_run_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_run_help();
        return Ok(2);
    };

    match subcommand {
        "start" => workflow_run_start(&args[1..]),
        "list" => workflow_run_list(&args[1..]),
        "show" => workflow_run_show(&args[1..]),
        "cancel" => workflow_run_cancel(&args[1..]),
        "finish" => workflow_run_finish(&args[1..]),
        "report-session" => workflow_run_report_session(&args[1..]),
        "message" => workflow_run_message(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_run_help();
            Ok(0)
        }
        _ => {
            print_workflow_run_help();
            Ok(2)
        }
    }
}

fn run_workflow_node_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_node_help();
        return Ok(2);
    };

    match subcommand {
        "show" => workflow_node_show(&args[1..]),
        "steer" => workflow_node_steer(&args[1..]),
        "interrupt" => workflow_node_interrupt(&args[1..]),
        "restart" => workflow_node_restart(&args[1..]),
        "complete" => workflow_node_complete(&args[1..]),
        "expand" => workflow_node_expand(&args[1..]),
        "interrogate" => workflow_node_interrogate(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_node_help();
            Ok(0)
        }
        _ => {
            print_workflow_node_help();
            Ok(2)
        }
    }
}

fn run_workflow_summary_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_summary_help();
        return Ok(2);
    };

    match subcommand {
        "show" => workflow_summary_show(&args[1..]),
        "list" => workflow_summary_list(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_summary_help();
            Ok(0)
        }
        _ => {
            print_workflow_summary_help();
            Ok(2)
        }
    }
}

/// `kvx workflow review` (`.local/prd/phase4-retarget-plan.md` §5 packet
/// **P12**). `start`/`show`/`apply` are the human's verbs; `answer`/`report`
/// are an interview/synthesis pane's own self-report, matching the frozen
/// `Bash(kvx workflow review answer:*)` / `Bash(kvx workflow review
/// report:*)` `--allowedTools` prefixes exactly — see `spec.rs`'s
/// `workflow_review_command` doc for why the verb/flag names here cannot
/// drift from those.
fn run_workflow_review_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_workflow_review_help();
        return Ok(2);
    };

    match subcommand {
        "start" => workflow_review_start(&args[1..]),
        "show" => workflow_review_show(&args[1..]),
        "apply" => workflow_review_apply(&args[1..]),
        "answer" => workflow_review_answer(&args[1..]),
        "report" => workflow_review_report(&args[1..]),
        "help" | "--help" | "-h" => {
            print_workflow_review_help();
            Ok(0)
        }
        _ => {
            print_workflow_review_help();
            Ok(2)
        }
    }
}

// ── workflow list / show ────────────────────────────────────────────────

/// §4 D17: `list` gains `--json` in the sweep alongside `run cancel`/`run
/// list`/`node steer`/`node interrupt`/`node restart`. The response was
/// already the raw envelope unconditionally (no human renderer exists for
/// it), so the flag is accepted for CLI-wide consistency rather than
/// changing what gets printed.
fn workflow_list(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_list_args(args) {
        Ok(_json) => super::runtime::workflow_list(),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_list_args(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => Err("usage: kvx workflow list [--json]".into()),
    }
}

/// `workflow.get` alone only carries the workflow's summary and a version
/// history (`KvdagVersionSummary`, no nodes/edges/args) — §2.18: "`kvx
/// workflow show` never shows the workflow ... a user cannot discover which
/// node paths exist ... or which `--arg` names to pass". The full graph
/// (`KvdagNodeInfo`/`KvdagEdgeInfo`/`WorkflowArgSpec`) only exists on
/// `workflow.version.get`, so the human view chains a second request for the
/// head version. `--json` skips the second call and prints the raw
/// `workflow.get` result exactly as before, for scripts that already parse it.
fn workflow_show(args: &[String]) -> std::io::Result<i32> {
    let (target, json) = match parse_workflow_show_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    if json {
        return super::runtime::workflow_get(target.workflow_id);
    }

    let response = super::send_request(&Request {
        id: "cli:workflow:show".into(),
        method: Method::WorkflowGet(target),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }

    let head_version_id = response["result"]["workflow"]["head_version_id"]
        .as_str()
        .map(str::to_string);
    let version = match head_version_id {
        Some(version_id) => {
            let version_response = super::send_request(&Request {
                id: "cli:workflow:show:version".into(),
                method: Method::WorkflowVersionGet(WorkflowVersionTarget { version_id }),
            })?;
            (version_response.get("error").is_none()).then_some(version_response)
        }
        None => None,
    };

    print_workflow_show(&response, version.as_ref());
    Ok(0)
}

fn parse_workflow_show_args(args: &[String]) -> Result<(WorkflowTarget, bool), String> {
    let usage = "usage: kvx workflow show <name|id> [--json]";
    match args {
        [target] => Ok((
            WorkflowTarget {
                workflow_id: target.clone(),
            },
            false,
        )),
        [target, flag] if flag == "--json" => Ok((
            WorkflowTarget {
                workflow_id: target.clone(),
            },
            true,
        )),
        _ => Err(usage.into()),
    }
}

// ── workflow create / update ────────────────────────────────────────────

/// §2.16.3: `create`/`update` had no human/`--json` split at all, so the
/// carefully formatted TOML caret diagnostic from a parse error arrived as
/// `\n`-escaped literal text inside a JSON envelope. Default output is now
/// human-readable (the local definition error prints with real newlines via
/// [`print_workflow_local_error`]; the server response renders through
/// [`print_workflow_create_response`]); `--json` preserves the exact
/// previous machine-readable envelope for scripts.
fn workflow_create(args: &[String]) -> std::io::Result<i32> {
    let (file, name, json) = match parse_workflow_create_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let definition = match load_definition_document(&file, name) {
        Ok(definition) => definition,
        Err(message) => {
            return Ok(print_workflow_local_error(
                "invalid_definition",
                &message,
                json,
            ));
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:create".into(),
        method: Method::WorkflowCreate(WorkflowCreateParams { definition }),
    })?;
    print_workflow_create_response(&response, json)
}

fn parse_workflow_create_args(args: &[String]) -> Result<(String, Option<String>, bool), String> {
    let usage = "usage: kvx workflow create --file <definition.toml|json> [--name <name>] [--json]";
    let mut file = None;
    let mut name = None;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --file".into());
                };
                file = Some(value.clone());
                index += 2;
            }
            "--name" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --name".into());
                };
                name = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let Some(file) = file else {
        return Err(usage.into());
    };
    Ok((file, name, json))
}

/// §2.19's "`update` drops fields and misreports no-ops": resubmitting a
/// definition whose spec digest already matches the workflow's head version
/// deduplicates server-side, but the response still carries type
/// `workflow_version_created` with no wire-level way to tell the two cases
/// apart (see the notes returned alongside this task for the server-side
/// fix). The human view closes that gap client-side by reading the
/// workflow's head version *before* the update and comparing it to the
/// version the update actually returns: a match means nothing new was
/// created, so this prints "unchanged" and says so plainly instead of
/// letting the response's own claim stand uncorrected. `--json` is left
/// exactly as the server returned it.
fn workflow_update(args: &[String]) -> std::io::Result<i32> {
    let (workflow_id, file, change_summary, json) = match parse_workflow_update_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let definition = match load_definition_document(&file, None) {
        Ok(definition) => definition,
        Err(message) => {
            return Ok(print_workflow_local_error(
                "invalid_definition",
                &message,
                json,
            ));
        }
    };

    let previous_head_version = if json {
        None
    } else {
        fetch_head_version(&workflow_id)?
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:update".into(),
        method: Method::WorkflowVersionCreate(WorkflowVersionCreateParams {
            workflow_id,
            definition,
            change_summary: change_summary.clone().unwrap_or_default(),
        }),
    })?;
    print_workflow_update_response(&response, json, previous_head_version)
}

fn parse_workflow_update_args(
    args: &[String],
) -> Result<(String, String, Option<String>, bool), String> {
    let usage = "usage: kvx workflow update <name|id> --file <definition.toml|json> [--change-summary <text>] [--json]";
    let Some(workflow_id) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = workflow_id.clone();

    let mut file = None;
    let mut change_summary = None;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --file".into());
                };
                file = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--change-summary" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --change-summary".into());
                };
                change_summary = Some(value.clone());
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let Some(file) = file else {
        return Err(usage.into());
    };
    Ok((workflow_id, file, change_summary, json))
}

/// Reads the workflow's current head version number (before an update), so
/// the update response can be compared against it to detect a deduplicated
/// no-op. `None` on any lookup failure (unresolvable target, no version yet,
/// transport hiccup): `workflow.version.create` itself is the authority on
/// whether the update succeeds, so a failed precheck falls back to trusting
/// the update response's own claim rather than blocking the update.
fn fetch_head_version(workflow_id: &str) -> std::io::Result<Option<u32>> {
    let response = super::send_request(&Request {
        id: "cli:workflow:update:precheck".into(),
        method: Method::WorkflowGet(WorkflowTarget {
            workflow_id: workflow_id.to_string(),
        }),
    })?;
    Ok(response["result"]["workflow"]["head_version"]
        .as_u64()
        .map(|version| version as u32))
}

/// `WorkflowCreateParams`/`WorkflowVersionCreateParams` carry the definition
/// as opaque text (`05-phase-plan.md` §4: "the same shape as the kvdag
/// types"), so there is no separate wire field to override the document's
/// name. `--name` is therefore applied client-side by rewriting the
/// document's top-level `name` before it is sent.
fn load_definition_document(
    path: &str,
    name_override: Option<String>,
) -> Result<WorkflowDefinitionDocument, String> {
    let format = definition_format_from_path(path)?;
    let text =
        std::fs::read_to_string(path).map_err(|err| format!("failed to read {path}: {err}"))?;
    let text = match name_override {
        Some(name) => override_definition_name(format, &text, &name)?,
        None => text,
    };
    Ok(WorkflowDefinitionDocument { format, text })
}

fn definition_format_from_path(path: &str) -> Result<WorkflowDefinitionFormat, String> {
    match std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        Some("toml") => Ok(WorkflowDefinitionFormat::Toml),
        Some("json") => Ok(WorkflowDefinitionFormat::Json),
        _ => Err(format!(
            "definition file {path} must have a .toml or .json extension"
        )),
    }
}

fn override_definition_name(
    format: WorkflowDefinitionFormat,
    text: &str,
    name: &str,
) -> Result<String, String> {
    match format {
        WorkflowDefinitionFormat::Toml => {
            let mut value: toml::Value = text
                .parse()
                .map_err(|err| format!("invalid TOML definition: {err}"))?;
            let table = value
                .as_table_mut()
                .ok_or_else(|| "definition must be a TOML table".to_string())?;
            table.insert("name".to_string(), toml::Value::String(name.to_string()));
            toml::to_string_pretty(&value)
                .map_err(|err| format!("failed to re-serialize definition: {err}"))
        }
        WorkflowDefinitionFormat::Json => {
            let mut value: serde_json::Value = serde_json::from_str(text)
                .map_err(|err| format!("invalid JSON definition: {err}"))?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| "definition must be a JSON object".to_string())?;
            object.insert(
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
            serde_json::to_string_pretty(&value)
                .map_err(|err| format!("failed to re-serialize definition: {err}"))
        }
    }
}

// ── workflow run start / list / show / cancel ───────────────────────────

/// §2.19's "undeclared `--arg` ignored": `run start demo --arg goal=x --arg
/// bogus=y` used to succeed with `bogus` silently dropped — a typo'd argument
/// name is a silent no-op with no signal that anything went wrong. Before
/// sending the run, this fetches the target's declared `[[arg]]` names (from
/// its head version, the same version `workflow.run` defaults to since the
/// CLI never sends an explicit `version`) and rejects any `--arg` key that
/// was never declared, naming what *was* declared so the fix is obvious.
fn workflow_run_start(args: &[String]) -> std::io::Result<i32> {
    let (params, json) = match parse_workflow_run_start_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    if let Some(message) = validate_declared_run_args(&params.workflow_id, &params.args)? {
        eprintln!("{message}");
        return Ok(1);
    }

    let response = super::send_request(&Request {
        id: "cli:workflow:run:start".into(),
        method: Method::WorkflowRun(params),
    })?;
    print_workflow_run_response(&response, json)
}

/// `None` when every supplied `--arg` key is declared (or none were
/// supplied); `Some(message)` naming the unknown keys and the full declared
/// list otherwise. Fails open (returns `None`, letting `workflow.run` itself
/// report whatever is wrong) when the target or its args cannot be resolved,
/// so a lookup hiccup never blocks a run the server would have accepted.
fn validate_declared_run_args(
    workflow_id: &str,
    supplied: &HashMap<String, String>,
) -> std::io::Result<Option<String>> {
    if supplied.is_empty() {
        return Ok(None);
    }
    let Some(declared) = fetch_declared_arg_names(workflow_id)? else {
        return Ok(None);
    };
    Ok(unknown_arg_message(workflow_id, supplied, &declared))
}

/// Pure message-building half of [`validate_declared_run_args`], split out so
/// it is unit-testable without a live server: `None` when every supplied key
/// is declared, `Some(message)` naming the unknown keys and the full declared
/// list (sorted, for a deterministic message) otherwise.
fn unknown_arg_message(
    workflow_id: &str,
    supplied: &HashMap<String, String>,
    declared: &HashSet<String>,
) -> Option<String> {
    let mut unknown: Vec<&str> = supplied
        .keys()
        .filter(|key| !declared.contains(key.as_str()))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return None;
    }
    unknown.sort_unstable();

    let mut declared_names: Vec<&str> = declared.iter().map(String::as_str).collect();
    declared_names.sort_unstable();
    let declared_list = if declared_names.is_empty() {
        "(none declared)".to_string()
    } else {
        declared_names.join(", ")
    };

    Some(format!(
        "unknown --arg key(s): {} (declared args for {workflow_id}: {declared_list})",
        unknown.join(", ")
    ))
}

/// Resolves `workflow_id` to its head version and reads that version's
/// declared `[[arg]]` names. `None` on any unresolvable step (workflow
/// lookup fails, no head version yet, version lookup fails) rather than an
/// error, since the caller treats that as "cannot validate, don't block".
fn fetch_declared_arg_names(workflow_id: &str) -> std::io::Result<Option<HashSet<String>>> {
    let get_response = super::send_request(&Request {
        id: "cli:workflow:run:start:precheck".into(),
        method: Method::WorkflowGet(WorkflowTarget {
            workflow_id: workflow_id.to_string(),
        }),
    })?;
    if get_response.get("error").is_some() {
        return Ok(None);
    }
    let Some(version_id) = get_response["result"]["workflow"]["head_version_id"]
        .as_str()
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let version_response = super::send_request(&Request {
        id: "cli:workflow:run:start:precheck:version".into(),
        method: Method::WorkflowVersionGet(WorkflowVersionTarget { version_id }),
    })?;
    if version_response.get("error").is_some() {
        return Ok(None);
    }

    let names = version_response["result"]["version"]["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg["name"].as_str().map(str::to_string))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    Ok(Some(names))
}

fn parse_workflow_run_start_args(args: &[String]) -> Result<(WorkflowRunParams, bool), String> {
    let usage = "usage: kvx workflow run start <name|id> [--tier <tier>] [--arg KEY=VALUE]... \
        [--restore-from <run-id>] [--restore <selector>]... [--restore-allow-changed] \
        [--no-prior-summaries] [--json]";
    let Some(target) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = target.clone();

    let mut tier = None;
    let mut run_args = HashMap::new();
    let mut json = false;
    let mut restore_from = None;
    let mut restore_nodes = Vec::new();
    let mut restore_allow_changed = false;
    let mut no_prior_summaries = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--tier" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --tier".into());
                };
                tier = Some(parse_workflow_tier(value)?);
                index += 2;
            }
            "--arg" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --arg".into());
                };
                let (key, value) = parse_arg_assignment(value)?;
                run_args.insert(key, value);
                index += 2;
            }
            "--restore-from" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --restore-from".into());
                };
                restore_from = Some(value.clone());
                index += 2;
            }
            "--restore" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --restore".into());
                };
                restore_nodes.push(value.clone());
                index += 2;
            }
            "--restore-allow-changed" => {
                restore_allow_changed = true;
                index += 1;
            }
            "--no-prior-summaries" => {
                no_prior_summaries = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let restore_from = match restore_from {
        Some(run_id) => Some(WorkflowRestoreRequest {
            run_id,
            nodes: restore_nodes,
            allow_changed: restore_allow_changed,
        }),
        None if !restore_nodes.is_empty() => {
            return Err("--restore requires --restore-from".into());
        }
        None if restore_allow_changed => {
            return Err("--restore-allow-changed requires --restore-from".into());
        }
        None => None,
    };

    Ok((
        WorkflowRunParams {
            workflow_id,
            version: None,
            tier,
            args: run_args,
            restore_from,
            include_prior_summaries: no_prior_summaries.then_some(false),
        },
        json,
    ))
}

/// §4 D17: `--json` accepted for the same reason as [`workflow_list`] — the
/// response is already the raw envelope unconditionally.
fn workflow_run_list(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_list_args(args) {
        Ok((params, _json)) => super::runtime::workflow_run_list(params),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_run_list_args(args: &[String]) -> Result<(WorkflowRunListParams, bool), String> {
    let usage = "usage: kvx workflow run list <name|id> [--limit N] [--json]";
    let Some(target) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = target.clone();

    let mut limit = None;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --limit".into());
                };
                limit = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid value for --limit: {value}"))?,
                );
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    Ok((
        WorkflowRunListParams {
            workflow_id: Some(workflow_id),
            limit,
        },
        json,
    ))
}

fn workflow_run_show(args: &[String]) -> std::io::Result<i32> {
    let (target, json) = match parse_workflow_run_show_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:run:show".into(),
        method: Method::WorkflowRunGet(target),
    })?;
    print_workflow_run_response(&response, json)
}

fn parse_workflow_run_show_args(args: &[String]) -> Result<(WorkflowRunTarget, bool), String> {
    let usage = "usage: kvx workflow run show <run_id> [--json]";
    let Some(run_id) = args.first() else {
        return Err(usage.into());
    };
    let json = match args.get(1..) {
        Some([]) => false,
        Some([flag]) if flag == "--json" => true,
        _ => return Err(usage.into()),
    };
    Ok((
        WorkflowRunTarget {
            run_id: run_id.clone(),
        },
        json,
    ))
}

/// The mutating run/node verbs (`run cancel`, `node steer`, `node interrupt`,
/// `node restart`) print their success envelope exactly as before, but render a
/// refusal the way every other workflow error the user reads is rendered.
///
/// Without this the carefully worded refusals — `workflow_run_closed` naming
/// the run's status and the remedy, `workflow_node_delivery_failed` naming the
/// pane — arrive as a raw JSON envelope while `run show` and `node show` next
/// to them print prose. Only the terminal rendering changes; the wire envelope
/// these commands emit on success, and the `code` on failure, are untouched.
/// §4 D17: `json` now controls whether a refusal renders humanized
/// (`print_workflow_error`, the pre-existing default) or as the raw envelope
/// like every other `--json` path does — success was, and stays, the raw
/// envelope either way, since these mutation verbs have no human success
/// renderer.
fn send_workflow_mutation(id: &'static str, method: Method, json: bool) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: id.into(),
        method,
    })?;
    if !json {
        if let Some(code) = print_workflow_error(&response) {
            return Ok(code);
        }
    }
    super::print_response(&response)
}

fn workflow_run_cancel(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_cancel_args(args) {
        Ok((target, json)) => send_workflow_mutation(
            "cli:workflow:run:cancel",
            Method::WorkflowRunCancel(WorkflowRunTarget {
                run_id: target.run_id,
            }),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

/// `kvx workflow run finish` — the team lead's own end-of-run report
/// (`09-agent-teams-rework.md` §3.3).
///
/// The run id defaults to `KARVEX_WORKFLOW_RUN_ID`, which karvex exports into
/// the lead's pane, so the lead runs this with nothing but a summary path.
fn workflow_run_finish(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_finish_args(args, std::env::var(NODE_ENV_RUN_ID).ok()) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:run:finish",
            Method::WorkflowRunFinish(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_run_finish_args(
    args: &[String],
    env_run_id: Option<String>,
) -> Result<(WorkflowRunFinishParams, bool), String> {
    let usage = "usage: kvx workflow run finish [--run <run_id>] \
                 (--summary-file <path> | --summary <text>) [--outcome <word>] [--json]";
    let mut run_id: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut summary_file: Option<String> = None;
    let mut outcome: Option<String> = None;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |slot: &mut Option<String>| -> Result<(), String> {
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{arg} needs a value\n{usage}"))?;
            *slot = Some(value.clone());
            Ok(())
        };
        match arg {
            "--run" | "--run-id" => {
                take_value(&mut run_id)?;
                index += 2;
            }
            "--summary" => {
                take_value(&mut summary)?;
                index += 2;
            }
            "--summary-file" => {
                take_value(&mut summary_file)?;
                index += 2;
            }
            "--outcome" => {
                take_value(&mut outcome)?;
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            _ => return Err(usage.into()),
        }
    }

    let run_id = run_id
        .or(env_run_id)
        .filter(|run_id| !run_id.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no run to finish: pass --run <run_id>, or run this from the run's lead pane \
                 where {NODE_ENV_RUN_ID} is exported\n{usage}"
            )
        })?;
    if summary.is_none() && summary_file.is_none() {
        return Err(format!(
            "a run summary is required: pass --summary-file <path> or --summary <text>\n{usage}"
        ));
    }
    if summary.is_some() && summary_file.is_some() {
        return Err(format!(
            "pass either --summary or --summary-file, not both\n{usage}"
        ));
    }

    Ok((
        WorkflowRunFinishParams {
            run_id,
            summary,
            summary_file,
            outcome,
        },
        json,
    ))
}

/// `kvx workflow run report-session` — a run session identifying itself
/// (`09-agent-teams-rework.md` §3.1a).
///
/// The `SessionStart` hook karvex writes into the run's `--settings` runs this
/// with nothing but `--run`. Everything else comes from the hook's own inputs:
/// the payload Claude Code writes to stdin, and the two variables Claude Code
/// exports to every hook before any of them runs.
///
/// A shipped `kvx` verb rather than a shell asset like
/// `assets/claude/karvex-agent-state.sh`: that asset pattern exists because the
/// agent-state hook is *installed* into user settings and has to keep working
/// across karvex upgrades, which is what the `KARVEX_INTEGRATION_VERSION`
/// migration rule governs. This hook is written fresh into the run directory on
/// every launch, so there is no installed copy to migrate — and putting the
/// payload parsing here means it is unit tested and behaves identically on
/// Windows with no second PowerShell implementation to keep in step.
///
/// Always exits 0. A hook that fails a session's startup because karvex was not
/// listening would be a far worse failure than a run that falls back to the
/// documented `createdAt`/cwd inference.
fn workflow_run_report_session(args: &[String]) -> std::io::Result<i32> {
    use std::io::Read;

    let mut run_id: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--run" | "--run-id" => {
                run_id = args.get(index + 1).cloned();
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            _ => {
                eprintln!(
                    "usage: kvx workflow run report-session [--run <run_id>] [--json]\n\
                     (reads Claude Code's SessionStart hook payload on stdin)"
                );
                return Ok(2);
            }
        }
    }
    let run_id = run_id
        .or_else(|| std::env::var(NODE_ENV_RUN_ID).ok())
        .filter(|value| !value.trim().is_empty());
    let Some(run_id) = run_id else {
        // Not an error: the same hook runs in every session that inherits the
        // run's settings, and one that is not a run's session has nothing to
        // report.
        return Ok(0);
    };

    let mut payload = String::new();
    let _ = std::io::stdin().read_to_string(&mut payload);
    let Some(hook) = crate::workflow::binding::identity::parse_hook_input(&payload) else {
        return Ok(0);
    };

    let params = WorkflowRunReportSessionParams {
        run_id,
        session_id: hook.session_id,
        transcript_path: hook.transcript_path,
        pane_id: env_value(crate::integration::KARVEX_PANE_ID_ENV_VAR),
        cwd: hook.cwd,
        source: hook.source,
        messaging_socket: env_value(MESSAGING_SOCKET_ENV_VAR),
        messaging_token: env_value(MESSAGING_TOKEN_ENV_VAR),
        agent_id: hook.agent_id,
    };
    match super::send_request(&Request {
        id: "cli:workflow:run:report-session".into(),
        method: Method::WorkflowRunReportSession(params),
    }) {
        Ok(response) => {
            if json {
                return super::print_response(&response);
            }
            Ok(0)
        }
        // A hook must never fail a session's startup because karvex was not
        // reachable. The run falls back to inference and says so in its log.
        Err(_) => Ok(0),
    }
}

/// Claude Code exports these to every hook and Bash command, before any hook
/// runs, and each session exports its own rather than an inherited one.
const MESSAGING_SOCKET_ENV_VAR: &str = "CLAUDE_CODE_MESSAGING_SOCKET";
const MESSAGING_TOKEN_ENV_VAR: &str = "CLAUDE_CODE_MESSAGING_TOKEN";

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `kvx workflow run message` — send text into one of a live run's Claude Code
/// sessions (`09-agent-teams-rework.md` §3.5a).
fn workflow_run_message(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_message_args(args, std::env::var(NODE_ENV_RUN_ID).ok()) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:run:message",
            Method::WorkflowRunMessage(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_run_message_args(
    args: &[String],
    env_run_id: Option<String>,
) -> Result<(WorkflowRunMessageParams, bool), String> {
    let usage = "usage: kvx workflow run message [--run <run_id>] --to <name> \
                 (--text <text> | --text-file <path>) [--priority now|next|later] [--json]";
    let mut run_id: Option<String> = None;
    let mut target: Option<String> = None;
    let mut text: Option<String> = None;
    let mut text_file: Option<String> = None;
    let mut priority: Option<String> = None;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |slot: &mut Option<String>| -> Result<(), String> {
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{arg} needs a value\n{usage}"))?;
            *slot = Some(value.clone());
            Ok(())
        };
        match arg {
            "--run" | "--run-id" => {
                take_value(&mut run_id)?;
                index += 2;
            }
            "--to" | "--target" => {
                take_value(&mut target)?;
                index += 2;
            }
            "--text" => {
                take_value(&mut text)?;
                index += 2;
            }
            "--text-file" => {
                take_value(&mut text_file)?;
                index += 2;
            }
            "--priority" => {
                take_value(&mut priority)?;
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            _ => return Err(usage.into()),
        }
    }

    let run_id = run_id
        .or(env_run_id)
        .filter(|run_id| !run_id.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no run to message: pass --run <run_id>, or run this from the run's lead pane \
                 where {NODE_ENV_RUN_ID} is exported\n{usage}"
            )
        })?;
    let target = target.ok_or_else(|| {
        format!("name the session to reach with --to (the run's lead is `team-lead`)\n{usage}")
    })?;
    if text.is_some() && text_file.is_some() {
        return Err(format!(
            "pass either --text or --text-file, not both\n{usage}"
        ));
    }
    let text = match (text, text_file) {
        (Some(text), _) => text,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|error| format!("{path} could not be read: {error}\n{usage}"))?,
        (None, None) => return Err(format!("a message needs --text or --text-file\n{usage}")),
    };

    Ok((
        WorkflowRunMessageParams {
            run_id,
            target,
            text,
            priority,
        },
        json,
    ))
}

fn parse_workflow_run_cancel_args(args: &[String]) -> Result<(WorkflowRunTarget, bool), String> {
    let usage = "usage: kvx workflow run cancel <run_id> [--json]";
    match args {
        [run_id] => Ok((
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
            false,
        )),
        [run_id, flag] if flag == "--json" => Ok((
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
            true,
        )),
        _ => Err(usage.into()),
    }
}

// ── workflow node show / steer / interrupt / restart / complete ─────────

fn workflow_node_show(args: &[String]) -> std::io::Result<i32> {
    let (target, json) = match parse_workflow_node_show_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:node:show".into(),
        method: Method::WorkflowNodeGet(target),
    })?;
    print_workflow_node_response(&response, json)
}

fn parse_workflow_node_show_args(args: &[String]) -> Result<(WorkflowNodeTarget, bool), String> {
    let usage = "usage: kvx workflow node show <run_id> <path> [--json]";
    let (Some(run_id), Some(path)) = (args.first(), args.get(1)) else {
        return Err(usage.into());
    };
    let json = match args.get(2..) {
        Some([]) => false,
        Some([flag]) if flag == "--json" => true,
        _ => return Err(usage.into()),
    };
    Ok((
        WorkflowNodeTarget {
            run_id: run_id.clone(),
            path: path.clone(),
        },
        json,
    ))
}

fn workflow_node_steer(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_steer_args(args) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:node:steer",
            Method::WorkflowNodeSteer(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

/// A trailing `--json` is only recognized as the flag when at least one text
/// word remains without it — `node steer run-1 plan --json` (no other words)
/// keeps `--json` as the steered text rather than silently swallowing it,
/// since `<text>` is free-form and `--json` is itself a legal thing to say to
/// a node.
fn parse_workflow_node_steer_args(
    args: &[String],
) -> Result<(WorkflowNodeSteerParams, bool), String> {
    let usage = "usage: kvx workflow node steer <run_id> <path> <text> [--json]";
    if args.len() < 3 {
        return Err(usage.into());
    }
    let (json, text_args) = match args.last() {
        Some(flag) if flag == "--json" && args.len() > 3 => (true, &args[..args.len() - 1]),
        _ => (false, args),
    };
    Ok((
        WorkflowNodeSteerParams {
            run_id: text_args[0].clone(),
            path: text_args[1].clone(),
            text: text_args[2..].join(" "),
        },
        json,
    ))
}

fn workflow_node_interrupt(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_pair_args(
        args,
        "usage: kvx workflow node interrupt <run_id> <path> [--json]",
    ) {
        Ok((target, json)) => send_workflow_mutation(
            "cli:workflow:node:interrupt",
            Method::WorkflowNodeInterrupt(target),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn workflow_node_restart(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_pair_args(
        args,
        "usage: kvx workflow node restart <run_id> <path> [--json]",
    ) {
        Ok((target, json)) => send_workflow_mutation(
            "cli:workflow:node:restart",
            Method::WorkflowNodeRestart(target),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_node_pair_args(
    args: &[String],
    usage: &str,
) -> Result<(WorkflowNodeTarget, bool), String> {
    match args {
        [run_id, path] => Ok((
            WorkflowNodeTarget {
                run_id: run_id.clone(),
                path: path.clone(),
            },
            false,
        )),
        [run_id, path, flag] if flag == "--json" => Ok((
            WorkflowNodeTarget {
                run_id: run_id.clone(),
                path: path.clone(),
            },
            true,
        )),
        _ => Err(usage.into()),
    }
}

/// `04-kvdag-and-execution.md` §3.4: "the node calls `kvx workflow node
/// expand` mid-run." Like `node complete`, this is normally run by the node's
/// own process, so the token is read from `KARVEX_WORKFLOW_NODE_TOKEN` rather
/// than taken as an argument — the same env `node complete` reads, minted for
/// this node alone. `run_id`/`path` are explicit positionals, matching
/// `steer`/`interrupt`/`restart`, so the command still works from outside the
/// node's own pane for anyone holding that token.
///
/// A rejected proposal is a **success** response (§3 frozen interface 7): only
/// a missing token, a bad run/path, or a closed run exits non-zero.
fn workflow_node_expand(args: &[String]) -> std::io::Result<i32> {
    let (mut params, json) = match parse_workflow_node_expand_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let token = match required_env(NODE_ENV_NODE_TOKEN) {
        Ok(token) => token,
        Err(message) => {
            return Ok(print_workflow_local_error(
                "missing_node_environment",
                &message,
                json,
            ));
        }
    };
    params.token = token;

    let response = super::send_request(&Request {
        id: "cli:workflow:node:expand".into(),
        method: Method::WorkflowNodeExpand(params),
    })?;
    print_workflow_node_expand_response(&response, json)
}

fn parse_workflow_node_expand_args(
    args: &[String],
) -> Result<(WorkflowNodeExpandParams, bool), String> {
    let usage = "usage: kvx workflow node expand <run_id> <path> --template <key> --label <text> [--input KEY=VALUE]... [--count N] [--json]";
    let (Some(run_id), Some(path)) = (args.first(), args.get(1)) else {
        return Err(usage.into());
    };
    let run_id = run_id.clone();
    let path = path.clone();

    let mut template = None;
    let mut label = None;
    let mut inputs = HashMap::new();
    let mut count = None;
    let mut json = false;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--template" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --template".into());
                };
                template = Some(value.clone());
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --label".into());
                };
                label = Some(value.clone());
                index += 2;
            }
            "--input" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --input".into());
                };
                let (key, value) = parse_arg_assignment(value)?;
                inputs.insert(key, value);
                index += 2;
            }
            "--count" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --count".into());
                };
                count = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid value for --count: {value}"))?,
                );
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let Some(template) = template else {
        return Err(usage.into());
    };
    let Some(label) = label else {
        return Err(usage.into());
    };

    Ok((
        WorkflowNodeExpandParams {
            run_id,
            path,
            token: String::new(),
            template,
            label,
            inputs,
            count,
        },
        json,
    ))
}

/// One accepted child or one rejection line, borrowed straight from the
/// response so [`summarize_expand_response`] allocates nothing beyond the two
/// `Vec`s.
struct ExpandRejectionLine<'a> {
    template: &'a str,
    reason: &'a str,
    message: &'a str,
}

struct ExpandSummary<'a> {
    accepted: Vec<&'a str>,
    rejected: Vec<ExpandRejectionLine<'a>>,
}

/// Pure extraction half of [`print_workflow_node_expand_response`], so the
/// partial-acceptance shape (some accepted, some rejected, both non-empty) is
/// unit-testable without a live server.
fn summarize_expand_response(response: &serde_json::Value) -> ExpandSummary<'_> {
    let accepted = response["result"]["accepted"]
        .as_array()
        .map(|accepted| accepted.iter().filter_map(|path| path.as_str()).collect())
        .unwrap_or_default();
    let rejected = response["result"]["rejected"]
        .as_array()
        .map(|rejected| {
            rejected
                .iter()
                .map(|rejection| ExpandRejectionLine {
                    template: rejection["template"].as_str().unwrap_or(""),
                    reason: rejection["reason"].as_str().unwrap_or(""),
                    message: rejection["message"].as_str().unwrap_or(""),
                })
                .collect()
        })
        .unwrap_or_default();
    ExpandSummary { accepted, rejected }
}

/// §3 frozen interface 7: a rejection is a **success** response, so this never
/// routes through [`print_workflow_error`] for a rejection — only a genuine
/// error envelope (bad run/path/token, closed run) does.
fn print_workflow_node_expand_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let summary = summarize_expand_response(response);
    println!("accepted: {}", summary.accepted.len());
    for path in &summary.accepted {
        println!("  {path}");
    }
    println!("rejected: {}", summary.rejected.len());
    for rejection in &summary.rejected {
        println!(
            "  template={} reason={}",
            rejection.template, rejection.reason
        );
        if !rejection.message.is_empty() {
            println!("    {}", rejection.message);
        }
    }
    Ok(0)
}

/// `07-phase3-plan.md` §WS-E / §4 D7: opens a forked Claude session on a past
/// node's transcript (or, with `--reconstructed`, a fresh session seeded from
/// the node's stored checkpoint) so a person can ask it questions without
/// touching the source transcript. Unlike `node expand`/`node complete`, this
/// is invoked by a human, not the node's own process, so there is no node
/// token to read from the environment. A `workflow_transcript_unavailable`
/// refusal (a missing transcript, or a command-runner node that never had
/// one) is an ordinary error response, rendered like any other.
fn workflow_node_interrogate(args: &[String]) -> std::io::Result<i32> {
    let (params, json) = match parse_workflow_node_interrogate_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:node:interrogate".into(),
        method: Method::WorkflowNodeInterrogate(params),
    })?;
    print_workflow_node_interrogate_response(&response, json)
}

fn parse_workflow_node_interrogate_args(
    args: &[String],
) -> Result<(WorkflowNodeInterrogateParams, bool), String> {
    let usage = "usage: kvx workflow node interrogate <run_id> <path> [--reconstructed] [--note <text>] [--json]";
    let (Some(run_id), Some(path)) = (args.first(), args.get(1)) else {
        return Err(usage.into());
    };
    let run_id = run_id.clone();
    let path = path.clone();

    let mut mode = WorkflowInterrogationMode::Resumed;
    let mut note = None;
    let mut json = false;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--reconstructed" => {
                mode = WorkflowInterrogationMode::Reconstructed;
                index += 1;
            }
            "--note" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --note".into());
                };
                note = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    Ok((
        WorkflowNodeInterrogateParams {
            run_id,
            path,
            mode,
            note,
        },
        json,
    ))
}

/// §WS-E: "`node interrogate` prints the pane id and mode."
fn print_workflow_node_interrogate_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let interrogation = &response["result"]["interrogation"];
    let mode = if interrogation["reconstructed"].as_bool().unwrap_or(false) {
        "reconstructed"
    } else {
        "resumed"
    };
    println!("id:      {}", interrogation["id"].as_str().unwrap_or(""));
    println!("mode:    {mode}");
    if let Some(pane_id) = interrogation["pane_id"].as_str() {
        println!("pane_id: {pane_id}");
    }
    Ok(0)
}

fn workflow_node_complete(args: &[String]) -> std::io::Result<i32> {
    let result_file = match parse_workflow_node_complete_args(args) {
        Ok(result_file) => result_file,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let env = match read_node_complete_env() {
        Ok(env) => env,
        Err(message) => {
            print_workflow_cli_error("missing_node_environment", &message);
            return Ok(1);
        }
    };

    let result_path = result_file.unwrap_or_else(|| default_node_result_path(&env.node_dir));

    // `04-kvdag-and-execution.md` §4.3 makes the server the single completion
    // authority: it owns schema validation, the one corrective re-prompt, and
    // the `NeedsAttention` fallback for a report that carries no result
    // artifact. A client-side parse failure is a fast warning, never a veto —
    // exiting here would strand the node `Running` forever with nothing on the
    // server side ever learning the node tried to finish.
    let params = build_node_report_params(&env, &result_path);
    if let Some(warning) = params.local_error.as_deref() {
        eprintln!("warning: {warning}");
    }

    let response = super::send_request(&Request {
        id: "cli:workflow:node:complete".into(),
        method: Method::WorkflowNodeReport(params.params),
    })?;
    // The server owns schema validation, so a result that does not validate
    // comes back as an error envelope. Printing it plainly and exiting non-zero
    // is the only correction channel a `runner = "command"` node has: its
    // script is what has to rewrite result.json and call this again.
    if let Some(code) = print_workflow_error(&response) {
        return Ok(code);
    }
    super::print_response(&response)
}

fn parse_workflow_node_complete_args(args: &[String]) -> Result<Option<String>, String> {
    let mut result_file = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--result-file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --result-file".into());
                };
                result_file = Some(value.clone());
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(result_file)
}

struct NodeCompleteEnv {
    run_id: String,
    node_path: String,
    node_dir: String,
    node_token: String,
}

fn read_node_complete_env() -> Result<NodeCompleteEnv, String> {
    Ok(NodeCompleteEnv {
        run_id: required_env(NODE_ENV_RUN_ID)?,
        node_path: required_env(NODE_ENV_NODE_PATH)?,
        node_dir: required_env(NODE_ENV_NODE_DIR)?,
        node_token: required_env(NODE_ENV_NODE_TOKEN)?,
    })
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing required environment variable {name}"))
}

fn default_node_result_path(node_dir: &str) -> String {
    std::path::Path::new(node_dir)
        .join("result.json")
        .display()
        .to_string()
}

/// A report that is always sent, plus whatever the client noticed about the
/// result file on the way. `local_error` is advisory: the server decides what a
/// missing or unparseable result means for the node.
struct NodeReport {
    params: WorkflowNodeReportParams,
    local_error: Option<String>,
}

/// Builds the `workflow.node.report` params. A result file that cannot be read
/// or parsed reports `null`, which is the wire's "I have no result artifact" —
/// the server turns that into `NeedsAttention` (§4.3) instead of the node
/// stalling `Running` behind a client-side exit.
fn build_node_report_params(env: &NodeCompleteEnv, result_path: &str) -> NodeReport {
    let (result, local_error) = match read_node_result(result_path) {
        Ok(result) => (result, None),
        Err(message) => (serde_json::Value::Null, Some(message)),
    };
    NodeReport {
        params: WorkflowNodeReportParams {
            run_id: env.run_id.clone(),
            path: env.node_path.clone(),
            token: env.node_token.clone(),
            result,
        },
        local_error,
    }
}

fn read_node_result(result_path: &str) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(result_path)
        .map_err(|err| format!("failed to read {result_path}: {err}"))?;
    serde_json::from_str(&text).map_err(|err| format!("invalid JSON in {result_path}: {err}"))
}

// ── workflow summary show / list ────────────────────────────────────────

fn workflow_summary_show(args: &[String]) -> std::io::Result<i32> {
    let (target, json) = match parse_workflow_summary_show_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:summary:show".into(),
        method: Method::WorkflowSummaryGet(target),
    })?;
    print_workflow_summary_show_response(&response, json)
}

fn parse_workflow_summary_show_args(args: &[String]) -> Result<(WorkflowRunTarget, bool), String> {
    let usage = "usage: kvx workflow summary show <run_id> [--json]";
    let Some(run_id) = args.first() else {
        return Err(usage.into());
    };
    let json = match args.get(1..) {
        Some([]) => false,
        Some([flag]) if flag == "--json" => true,
        _ => return Err(usage.into()),
    };
    Ok((
        WorkflowRunTarget {
            run_id: run_id.clone(),
        },
        json,
    ))
}

/// `None` means "no summary was written yet" (`07-phase3-plan.md` §4 D1,
/// D10) — a normal answer, not an error, so this prints a plain message and
/// exits `0` rather than routing through [`print_workflow_error`].
fn print_workflow_summary_show_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let summary = &response["result"]["summary"];
    if summary.is_null() {
        println!("no summary recorded for this run yet");
        return Ok(0);
    }
    print_run_summary(summary);
    Ok(0)
}

/// The shared render of a `WorkflowRunSummaryInfo`: outcome, text, highlights,
/// open gaps, per-node lines (`07-phase3-plan.md` §WS-E).
fn print_run_summary(summary: &serde_json::Value) {
    println!("run_id:   {}", summary["run_id"].as_str().unwrap_or(""));
    println!(
        "workflow: {}",
        summary["workflow_name"].as_str().unwrap_or("")
    );
    println!("outcome:  {}", summary["outcome"].as_str().unwrap_or(""));
    if summary["run_pruned"].as_bool().unwrap_or(false) {
        println!("pruned:   yes");
    }
    println!();
    println!("{}", summary["text"].as_str().unwrap_or(""));

    if let Some(highlights) = summary["highlights"].as_array() {
        if !highlights.is_empty() {
            println!();
            println!("highlights:");
            for highlight in highlights {
                println!("  - {}", highlight.as_str().unwrap_or(""));
            }
        }
    }
    if let Some(open_gaps) = summary["open_gaps"].as_array() {
        if !open_gaps.is_empty() {
            println!();
            println!("open gaps:");
            for gap in open_gaps {
                println!("  - {}", gap.as_str().unwrap_or(""));
            }
        }
    }
    if let Some(per_node) = summary["per_node"].as_array() {
        if !per_node.is_empty() {
            println!();
            println!("nodes:");
            for line in per_node {
                let node_key = line["node_key"].as_str().unwrap_or("");
                let verdict = line["verdict"].as_str().unwrap_or("");
                let one_liner = line["one_liner"].as_str().unwrap_or("");
                println!("  {node_key:<20} {verdict:<12} {one_liner}");
            }
        }
    }
}

fn workflow_summary_list(args: &[String]) -> std::io::Result<i32> {
    let (params, json) = match parse_workflow_summary_list_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:summary:list".into(),
        method: Method::WorkflowSummaryList(params),
    })?;
    print_workflow_summary_list_response(&response, json)
}

fn parse_workflow_summary_list_args(
    args: &[String],
) -> Result<(WorkflowSummaryListParams, bool), String> {
    let usage = "usage: kvx workflow summary list [<workflow>] [--limit N] [--json]";
    let mut workflow_id = None;
    let mut limit = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --limit".into());
                };
                limit = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid value for --limit: {value}"))?,
                );
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option: {other}"));
            }
            other if workflow_id.is_none() => {
                workflow_id = Some(other.to_string());
                index += 1;
            }
            _ => return Err(usage.into()),
        }
    }
    Ok((WorkflowSummaryListParams { workflow_id, limit }, json))
}

fn print_workflow_summary_list_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let Some(summaries) = response["result"]["summaries"].as_array() else {
        return Ok(0);
    };
    if summaries.is_empty() {
        println!("no summaries recorded");
        return Ok(0);
    }
    for summary in summaries {
        println!("{}", format_summary_list_row(summary));
    }
    Ok(0)
}

/// One row of `summary list`'s output — run id, workflow name, outcome, and a
/// `(pruned)` marker when the source run no longer exists
/// (`07-phase3-plan.md` §WS-E, §4 D9).
fn format_summary_list_row(summary: &serde_json::Value) -> String {
    let run_id = summary["run_id"].as_str().unwrap_or("");
    let workflow_name = summary["workflow_name"].as_str().unwrap_or("");
    let outcome = summary["outcome"].as_str().unwrap_or("");
    let created = summary["created_at_unix_ms"]
        .as_u64()
        .map(format_unix_ms)
        .unwrap_or_default();
    let pruned = if summary["run_pruned"].as_bool().unwrap_or(false) {
        " (pruned)"
    } else {
        ""
    };
    format!("{run_id}  {workflow_name:<20} {outcome:<12} {created}{pruned}")
}

// ── workflow review ────────────────────────────────────────────────────

/// `kvx workflow review start <run_id>` (§3.5): plan, spawn one interview
/// pane per interviewable member, and answer with the cycle that is now
/// running. Never automatic — this call, or the TUI's `V` ask, is the only
/// trigger there is.
fn workflow_review_start(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_review_run_target_args(
        args,
        "usage: kvx workflow review start <run_id> [--json]",
    ) {
        Ok((target, json)) => send_workflow_mutation(
            "cli:workflow:review:start",
            Method::WorkflowReviewStart(target),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

/// `kvx workflow review show <run_id>`: the cycle and the findings it
/// produced, or an honest "no review cycle for this run yet" — a run that
/// has never been reviewed is a normal answer, not an error
/// (`WorkflowReviewGet`'s own doc, matching `workflow.summary.get`'s
/// precedent).
fn workflow_review_show(args: &[String]) -> std::io::Result<i32> {
    let (target, json) = match parse_workflow_review_run_target_args(
        args,
        "usage: kvx workflow review show <run_id> [--json]",
    ) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:review:show".into(),
        method: Method::WorkflowReviewGet(target),
    })?;
    print_workflow_review_show_response(&response, json)
}

fn parse_workflow_review_run_target_args(
    args: &[String],
    usage: &str,
) -> Result<(WorkflowRunTarget, bool), String> {
    let Some(run_id) = args.first() else {
        return Err(usage.into());
    };
    let json = match args.get(1..) {
        Some([]) => false,
        Some([flag]) if flag == "--json" => true,
        _ => return Err(usage.into()),
    };
    Ok((
        WorkflowRunTarget {
            run_id: run_id.clone(),
        },
        json,
    ))
}

/// `kvx workflow review apply <run_id> (--accept <node_key>... | --decline-all)`
/// (§3.5, §5 packet P12): the human's per-finding accept, minting a new
/// version. `accept` is repeatable and everything not named is declined; an
/// explicit `--decline-all` sends the same empty `accept` the wire already
/// treats as "decline the whole cycle" — but a bare `apply` with **neither**
/// flag is refused locally, before any request is sent, because minting an
/// immutable version is irreversible and an irreversible action must never
/// default to anything.
fn workflow_review_apply(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_review_apply_args(args) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:review:apply",
            Method::WorkflowReviewApply(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_review_apply_args(
    args: &[String],
) -> Result<(WorkflowReviewApplyParams, bool), String> {
    let usage = "usage: kvx workflow review apply <run_id> \
                 (--accept <node_key>... | --decline-all) [--json]";
    let Some(run_id) = args.first() else {
        return Err(usage.into());
    };
    let run_id = run_id.clone();

    let mut accept: Vec<String> = Vec::new();
    let mut decline_all = false;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--accept" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for --accept\n{usage}"));
                };
                accept.push(value.clone());
                index += 2;
            }
            "--decline-all" => {
                decline_all = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}\n{usage}")),
        }
    }

    if decline_all && !accept.is_empty() {
        return Err(format!(
            "pass either --accept or --decline-all, not both\n{usage}"
        ));
    }
    if !decline_all && accept.is_empty() {
        return Err(format!(
            "an apply must say what to do: pass --accept <node_key> (repeatable) or \
             --decline-all — minting a version is irreversible and never defaults\n{usage}"
        ));
    }

    Ok((WorkflowReviewApplyParams { run_id, accept }, json))
}

/// `kvx workflow review answer --file <path>`: an interview pane's own
/// self-report (§3.5). Run from inside that pane — the rendered interview
/// prompt tells the agent to run exactly this — so `run_id`/`member` are
/// never CLI flags, only [`REVIEW_RUN_ID_ENV_VAR`]/[`REVIEW_MEMBER_ENV_VAR`],
/// which karvex itself exported into the pane. A parse refusal from the
/// server is printed back verbatim and the interview stays open — that
/// refusal *is* the corrective re-prompt.
fn workflow_review_answer(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_review_answer_args(
        args,
        std::env::var(REVIEW_RUN_ID_ENV_VAR).ok(),
        std::env::var(REVIEW_MEMBER_ENV_VAR).ok(),
    ) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:review:answer",
            Method::WorkflowReviewAnswer(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_review_answer_args(
    args: &[String],
    env_run_id: Option<String>,
    env_member: Option<String>,
) -> Result<(WorkflowReviewAnswerParams, bool), String> {
    let usage = "usage: kvx workflow review answer --file <path> [--json]";
    let mut file: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for --file\n{usage}"));
                };
                file = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}\n{usage}")),
        }
    }
    let Some(file) = file else {
        return Err(format!(
            "an answer file is required: pass --file <path>\n{usage}"
        ));
    };
    let run_id = env_run_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no run to answer for: run this from a review interview pane, where \
                 {REVIEW_RUN_ID_ENV_VAR} is exported\n{usage}"
            )
        })?;
    let member = env_member
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no member to answer as: run this from a review interview pane, where \
                 {REVIEW_MEMBER_ENV_VAR} is exported\n{usage}"
            )
        })?;

    Ok((
        WorkflowReviewAnswerParams {
            run_id,
            member,
            answer: None,
            answer_file: Some(file),
        },
        json,
    ))
}

/// `kvx workflow review report --file <path>`: the synthesis pane's own
/// self-report (§3.5), the [`workflow_review_answer`] precedent minus
/// `member` — a findings document speaks for the whole cycle, not for one
/// interview.
fn workflow_review_report(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_review_report_args(args, std::env::var(REVIEW_RUN_ID_ENV_VAR).ok()) {
        Ok((params, json)) => send_workflow_mutation(
            "cli:workflow:review:report",
            Method::WorkflowReviewReport(params),
            json,
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_review_report_args(
    args: &[String],
    env_run_id: Option<String>,
) -> Result<(WorkflowReviewReportParams, bool), String> {
    let usage = "usage: kvx workflow review report --file <path> [--json]";
    let mut file: Option<String> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for --file\n{usage}"));
                };
                file = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}\n{usage}")),
        }
    }
    let Some(file) = file else {
        return Err(format!(
            "a findings file is required: pass --file <path>\n{usage}"
        ));
    };
    let run_id = env_run_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no run to report for: run this from a review synthesis pane, where \
                 {REVIEW_RUN_ID_ENV_VAR} is exported\n{usage}"
            )
        })?;

    Ok((
        WorkflowReviewReportParams {
            run_id,
            findings: None,
            findings_file: Some(file),
        },
        json,
    ))
}

/// `review show`'s human rendering: the cycle summary, then its findings —
/// `run show`/`node show`'s prose precedent rather than the raw-envelope
/// convention the other four review verbs use, because unlike a mutation this
/// is a read with real content to lay out.
fn print_workflow_review_show_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let review = &response["result"]["review"];
    if review.is_null() {
        println!("no review cycle for this run yet");
        return Ok(0);
    }
    print_workflow_review_summary(review);

    if let Some(findings) = response["result"]["findings"].as_array() {
        if !findings.is_empty() {
            println!();
            println!("findings:");
            for finding in findings {
                println!("{}", format_review_finding_line(finding));
            }
        }
    }
    Ok(0)
}

fn print_workflow_review_summary(review: &serde_json::Value) {
    println!("id:      {}", review["id"].as_str().unwrap_or(""));
    println!("run_id:  {}", review["run_id"].as_str().unwrap_or(""));
    println!("status:  {}", review["status"].as_str().unwrap_or(""));
    if let Some(at) = review["started_at_unix_ms"].as_u64() {
        println!("started: {}", format_unix_ms(at));
    }
    if let Some(at) = review["ended_at_unix_ms"].as_u64() {
        println!("ended:   {}", format_unix_ms(at));
    }
    if let Some(version) = review["resulting_version_id"].as_str() {
        println!("version: {version}");
    }
    let evidence_only = review["evidence_only_count"].as_u64().unwrap_or(0);
    if evidence_only > 0 {
        println!("evidence_only: {evidence_only} interview(s) could not be attributed");
    }
}

/// One finding's line under `review show`'s `findings:` heading.
fn format_review_finding_line(finding: &serde_json::Value) -> String {
    let node_key = finding["node_key"].as_str().unwrap_or("");
    let level = finding["level"].as_str().unwrap_or("");
    let verdict = finding["verdict"].as_str().unwrap_or("");
    let mode = finding["interview_mode"].as_str().unwrap_or("");
    let accepted = if finding["accepted"].as_bool().unwrap_or(false) {
        "accepted"
    } else {
        "pending"
    };
    format!("  {node_key:<24} {level:<11} {verdict:<8} ({mode}, {accepted})")
}

// ── shared parsing / printing helpers ───────────────────────────────────

fn parse_workflow_tier(value: &str) -> Result<WorkflowTier, String> {
    match value {
        "auto" => Ok(WorkflowTier::Auto),
        "max" => Ok(WorkflowTier::Max),
        "high" => Ok(WorkflowTier::High),
        "medium" => Ok(WorkflowTier::Medium),
        "low" => Ok(WorkflowTier::Low),
        _ => Err(format!(
            "invalid tier: {value} (expected auto, max, high, medium, or low)"
        )),
    }
}

fn parse_arg_assignment(raw: &str) -> Result<(String, String), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err("--arg must use KEY=VALUE".to_string());
    };
    if key.is_empty() {
        return Err("--arg key must not be empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

/// §2.10: with a node blocked and the run paused, the entire human output
/// used to be `run_id`/`status`/`tier`/`nodes` — never naming the node the
/// docs promise `run show` "names the exact node responsible" for. `graph`
/// only exists on `workflow.run.get` (not on `run start`'s
/// `WorkflowRunStarted` or `run cancel`'s `WorkflowRunCancelled`, which carry
/// no graph), so this reads it defensively and simply prints nothing extra
/// when it is absent instead of assuming every caller has one.
fn print_workflow_run_response(response: &serde_json::Value, json: bool) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let run = &response["result"]["run"];
    let nodes = response["result"]["graph"]["nodes"].as_array();
    let blocking = nodes
        .map(|nodes| blocking_run_nodes(nodes))
        .unwrap_or_default();

    println!("run_id:  {}", run["run_id"].as_str().unwrap_or(""));
    print!("status:  {}", run["status"].as_str().unwrap_or(""));
    if let Some(first) = blocking.first() {
        println!(" (node \"{}\": {})", first.path, first.status);
    } else {
        println!();
    }
    println!("tier:    {}", run["tier"].as_str().unwrap_or(""));
    if let Some(line) = format_run_limits_line(run) {
        println!("{line}");
    }
    println!(
        "nodes:   {}/{}",
        run["nodes_done"].as_u64().unwrap_or(0),
        run["nodes_total"].as_u64().unwrap_or(0)
    );
    if let Some(line) = format_run_growth_line(run) {
        println!("{line}");
    }
    if let Some(line) = format_restore_report_line(&response["result"]["restore"]) {
        println!("{line}");
    }

    for node in &blocking {
        println!("blocked: {} ({})", node.path, node.status);
        if let Some(reason) = node.reason {
            println!("reason:  {reason}");
        }
        if let Some(resume_when) = node.resume_when {
            println!("resume:  {resume_when}");
        }
    }

    if let Some(team) = run["team_name"].as_str().filter(|name| !name.is_empty()) {
        println!("team:    {team}");
    }

    if let Some(nodes) = nodes {
        if !nodes.is_empty() {
            println!();
            println!("nodes:");
            for node in nodes {
                println!("{}", format_run_node_line(node));
            }
        }
    }

    // §3.4: who actually worked on the run, and in which pane. Snapshotted
    // rather than live, so this still answers for a finished run whose team
    // config Claude Code has already deleted.
    if let Some(members) = response["result"]["graph"]["members"].as_array() {
        if !members.is_empty() {
            println!();
            println!("members:");
            for member in members {
                println!("{}", format_run_member_line(member));
            }
        }
    }

    Ok(0)
}

/// One team member's line under `run show`'s `members:` heading.
fn format_run_member_line(member: &serde_json::Value) -> String {
    let name = member["name"].as_str().unwrap_or("");
    let pane = member["pane_id"].as_str().unwrap_or("");
    let model = member["model"].as_str().unwrap_or("");
    let backend = member["backend_type"].as_str().unwrap_or("");
    let active = if member["is_active"].as_bool().unwrap_or(false) {
        "active"
    } else {
        "idle"
    };
    let mut line = format!("  {name} ({active}");
    if !model.is_empty() {
        line.push_str(&format!(", {model}"));
    }
    line.push(')');
    if pane.is_empty() {
        // The lead has no pane of its own in the team config; it is the
        // session, not a teammate.
        line.push_str(&format!(" — {backend}"));
    } else {
        line.push_str(&format!(" — pane {pane}"));
    }
    line
}

/// One node's line under `run show`'s `nodes:` heading.
///
/// The label trails the status because the *path* is the identity every other
/// command takes as an argument, and a fan-out's children differ only in their
/// label: `fanout/worker/1..3` all read `worker` on every key-derived surface,
/// which is what made a generation indistinguishable here
/// (`04-kvdag-and-execution.md` §3.4, 2026-08-08 amendment). Omitted when it
/// would only repeat the path, so a static node's line is unchanged.
fn format_run_node_line(node: &serde_json::Value) -> String {
    let path = node["path"].as_str().unwrap_or("");
    let status = node["status"].as_str().unwrap_or("");
    let label = node["label"].as_str().unwrap_or("").trim();
    if label.is_empty() || label == path {
        format!("  {path:<28} {status}")
    } else {
        format!("  {path:<28} {status:<12} {label}")
    }
}

/// One graph node in `needs_attention`/`blocked`/`failed` — the statuses
/// that can be "the node responsible" for a paused or stuck run — plus
/// whatever `blocker` detail the wire carried for it. Every `needs_attention`
/// trigger now records a `Succession::Blocked` and so carries both fields
/// (`src/workflow/engine/mod.rs::needs_attention`), but `reason`/`resume_when`
/// stay `Option`: a node can also be `blocked`/`failed` for reasons that record
/// a different succession, and a run read back from an older journal predates
/// the blocker being recorded at all.
struct BlockingRunNode<'a> {
    path: &'a str,
    status: &'a str,
    reason: Option<&'a str>,
    resume_when: Option<&'a str>,
}

fn blocking_run_nodes(nodes: &[serde_json::Value]) -> Vec<BlockingRunNode<'_>> {
    nodes
        .iter()
        .filter(|node| {
            matches!(
                node["status"].as_str(),
                Some("needs_attention" | "blocked" | "failed")
            )
        })
        .map(|node| BlockingRunNode {
            path: node["path"].as_str().unwrap_or(""),
            status: node["status"].as_str().unwrap_or(""),
            reason: node["blocker"]["reason"].as_str(),
            resume_when: node["blocker"]["resume_when"].as_str(),
        })
        .collect()
}

/// `run show`'s enforced ceilings — `max_nodes` and `max_depth` used to be
/// `--json`-only, contradicting `workflows.mdx`'s promise that "what the run
/// enforces is what `kvx workflow run show` and the JSON API report". `None`
/// only when both keys are entirely absent from the JSON value (a defensive
/// fallback for a hand-built fixture missing the run's own growth ceilings,
/// not something `WorkflowRunInfo` itself ever omits), so `run start` /
/// `run cancel` print nothing extra when handed a response that never carried
/// them.
fn format_run_limits_line(run: &serde_json::Value) -> Option<String> {
    let max_nodes = run.get("max_nodes")?;
    let max_depth = run.get("max_depth")?;
    Some(format!(
        "limits:  max_nodes={} · max_depth={}",
        max_nodes.as_u64().unwrap_or(0),
        max_depth.as_u64().unwrap_or(0)
    ))
}

/// §3.4 / §4 D11's headline guarantee, CLI half: a run that ever hit a growth
/// guardrail says so on `run show`, human and `--json` alike (the wire's
/// `growth_limited` field on `WorkflowRunInfo` already carries it either way —
/// this only builds the human line). `None` when the run has never been
/// limited, matching WS-I's "prints a `growth:` line ... whenever the run has
/// a growth limit recorded" — an unlimited run gets no line at all rather than
/// a reassuring "not limited" one nobody asked for.
///
/// `nodes_live`/`max_nodes` come from the run's own effective ceilings (WS-A/
/// WS-E's one authority — §4 D9), not from the specific rejection's own
/// requested/accepted counts, so the line always describes the run's current
/// state rather than a snapshot of whichever proposal happened to trip the
/// guardrail.
fn format_run_growth_line(run: &serde_json::Value) -> Option<String> {
    let limit = run.get("growth_limited")?;
    if limit.is_null() {
        return None;
    }
    let nodes_live = run["nodes_live"].as_u64().unwrap_or(0);
    let max_nodes = run["max_nodes"].as_u64().unwrap_or(0);
    let kind = limit["kind"].as_str().unwrap_or("limit");
    let at = limit["at_unix_ms"].as_u64().unwrap_or(0);
    Some(format!(
        "growth:  {nodes_live} of {max_nodes} nodes · limited: {kind} reached at {}",
        format_unix_ms_clock(at)
    ))
}

/// `run start`'s restore report (`07-phase3-plan.md` §WS-E: "prints the
/// restore report (`restored: plan, implement · skipped: review (definition
/// changed)`)"). `None` when the response carries no `restore` field at all —
/// a plain (non-restoring) run start, where nothing about restore should
/// print. A present-but-empty report (a `--restore-from` run that restored
/// nothing) still prints, since the run *was* a restore attempt and silence
/// would look like the flag was ignored.
fn format_restore_report_line(restore: &serde_json::Value) -> Option<String> {
    if restore.is_null() {
        return None;
    }
    let restored: Vec<&str> = restore["restored"]
        .as_array()
        .map(|nodes| nodes.iter().filter_map(|node| node.as_str()).collect())
        .unwrap_or_default();
    let skipped = restore["skipped"].as_array().cloned().unwrap_or_default();

    let mut parts = Vec::new();
    if !restored.is_empty() {
        parts.push(format!("restored: {}", restored.join(", ")));
    }
    if !skipped.is_empty() {
        let skip_strs: Vec<String> = skipped
            .iter()
            .map(|skip| {
                let selector = skip["selector"].as_str().unwrap_or("");
                let reason = skip["reason"].as_str().unwrap_or("").replace('_', " ");
                format!("{selector} ({reason})")
            })
            .collect();
        parts.push(format!("skipped: {}", skip_strs.join(", ")));
    }
    if parts.is_empty() {
        return Some("restore: none restored, none skipped".to_string());
    }
    Some(parts.join(" · "))
}

/// The node's own last rejection as a *proposer* (`WorkflowRunNodeInfo.
/// growth_limited`) — distinct from [`format_run_growth_line`], which reports
/// the run's most recent limit regardless of which node hit it. `None` when
/// this node has never proposed into a guardrail.
fn format_node_growth_limited_line(node: &serde_json::Value) -> Option<String> {
    let limit = node.get("growth_limited")?;
    if limit.is_null() {
        return None;
    }
    let kind = limit["kind"].as_str().unwrap_or("limit");
    let requested = limit["requested"].as_u64().unwrap_or(0);
    let accepted = limit["accepted"].as_u64().unwrap_or(0);
    let at = limit["at_unix_ms"].as_u64().unwrap_or(0);
    Some(format!(
        "growth_limited: {kind} reached at {} ({accepted} of {requested} accepted)",
        format_unix_ms_clock(at)
    ))
}

/// The last pane delivery the runtime refused for this node (`04-kvdag-and-
/// execution.md` §5) — a runtime fact carried on `WorkflowRunNodeInfo`, not a
/// TUI-only one, so `node show` names it exactly as the DAG overlay does.
fn format_node_delivery_failure_line(node: &serde_json::Value) -> Option<String> {
    let text = node["delivery_failure"].as_str()?;
    Some(format!("delivery_failure: {text}"))
}

/// `HH:MM UTC` for a growth-limit timestamp — the plan's own example
/// ("... reached at 14:22 UTC") is clock time, not a full date, so this is
/// deliberately narrower than [`format_unix_ms`]. Names `UTC` explicitly like
/// its sibling `format_unix_ms` does, rather than leaving a bare clock time
/// that reads as local time to whoever's terminal it lands in.
fn format_unix_ms_clock(ms: u64) -> String {
    let secs_of_day = (ms / 1000) % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    format!("{hour:02}:{minute:02} UTC")
}

/// §2.10: `node show`'s human output printed `path`/`status`/`model`/
/// `effort`/`pane_id` but silently dropped `blocker` — the one field that
/// says *why* a `needs_attention` or `failed` node is stuck and what would
/// unblock it, even though the docs quote exactly this wording as the
/// product's best-designed string.
fn print_workflow_node_response(response: &serde_json::Value, json: bool) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let node = &response["result"]["node"];
    println!("path:    {}", node["path"].as_str().unwrap_or(""));
    // Between path and status because it is what a human calls this node; a
    // static node whose label is its key adds nothing, so it is skipped.
    let label = node["label"].as_str().unwrap_or("").trim();
    if !label.is_empty() && label != node["path"].as_str().unwrap_or("") {
        println!("label:   {label}");
    }
    println!("status:  {}", node["status"].as_str().unwrap_or(""));
    // The watchdog's own verdict, surfaced right under status because it is a
    // stronger claim than Claude Code's projected status: `None` means the
    // watchdog has nothing to report, which includes both "healthy" and "the
    // watchdog has not looked yet" (`.local/prd/phase4-retarget-plan.md` §6
    // D-10), so it prints nothing rather than a fabricated "ok".
    if let Some(attention) = node["attention"].as_str() {
        println!("attention: {attention}");
    }
    println!("model:   {}", node["model"].as_str().unwrap_or(""));
    println!("effort:  {}", node["effort"].as_str().unwrap_or(""));
    if let Some(pane_id) = node["pane_id"].as_str() {
        println!("pane_id: {pane_id}");
    }
    // `run_node.watchdog_interventions` (`app/workflow_watchdog.rs`'s own doc):
    // counts SURFACED watchdog opinions, not every ladder rung it walked —
    // WI-R5 (`.local/prd/phase4-retarget-plan.md` amendment log) names this a
    // known undercount, still open. Printed verbatim rather than reworded into
    // "interventions", which the column does not yet mean. Zero is the common
    // case and is skipped, matching `growth_limited`/`delivery_failure` below.
    if let Some(interventions) = node["watchdog_interventions"].as_u64() {
        if interventions > 0 {
            println!("interventions: {interventions}");
        }
    }
    if let Some(reason) = node["blocker"]["reason"].as_str() {
        println!("blocker: {reason}");
    }
    if let Some(resume_when) = node["blocker"]["resume_when"].as_str() {
        println!("resume:  {resume_when}");
    }
    if let Some(line) = format_node_growth_limited_line(node) {
        println!("{line}");
    }
    if let Some(line) = format_node_delivery_failure_line(node) {
        println!("{line}");
    }
    println!("{}", format_node_transcript_line(node));
    if let Some(line) = format_node_restored_from_line(node) {
        println!("{line}");
    }
    Ok(0)
}

/// §WS-E: "`node show` gains `transcript:` (present/absent) ... lines" —
/// present/absent rather than the raw path, since the path is filesystem
/// detail `--json` already carries verbatim on `transcript_path`.
fn format_node_transcript_line(node: &serde_json::Value) -> String {
    if node["transcript_path"].as_str().is_some() {
        "transcript: present".to_string()
    } else {
        "transcript: absent".to_string()
    }
}

/// §WS-E's `restored from:` line and §4 D4's provenance triple (source run,
/// node key, checkpoint seq). `None` for a node that was executed rather than
/// restored.
fn format_node_restored_from_line(node: &serde_json::Value) -> Option<String> {
    let restored_from = node.get("restored_from")?;
    if restored_from.is_null() {
        return None;
    }
    let run_id = restored_from["run_id"].as_str().unwrap_or("");
    let node_key = restored_from["node_key"].as_str().unwrap_or("");
    let checkpoint_seq = restored_from["checkpoint_seq"].as_u64().unwrap_or(0);
    Some(format!(
        "restored from: run {run_id} · node {node_key} · checkpoint #{checkpoint_seq}"
    ))
}

/// Renders a `workflow.*` error envelope the way a person reads it rather than
/// as one long JSON line. The server's best messages are multi-line — the kvdag
/// validators, the TOML caret diagram, the schema-violation list — and a raw
/// envelope turns every one of them into `\n` escapes at the terminal.
/// Returns `None` when the response is not an error, so callers fall through to
/// their normal rendering.
fn format_workflow_error(response: &serde_json::Value) -> Option<String> {
    let error = response.get("error")?;
    let code = error.get("code").and_then(serde_json::Value::as_str)?;
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let message = humanize_workflow_error_message(code, message);
    let mut rendered = format!("error: {code}");
    for line in message.lines() {
        rendered.push_str("\n  ");
        rendered.push_str(line);
    }
    Some(rendered)
}

/// §2.16.2: a duplicate workflow name leaked store internals straight to the
/// terminal — `` workflow store query failed: Database index `workflow_name`
/// already contains 'demo', with record `workflow:6v4g4nctlshyixd756r8` `` —
/// under the same `workflow_store_error` code a genuine graph-validation
/// failure also uses. A real fix needs the store to return a distinct code
/// (cross-need — this only rewrites the message actually shown at the
/// terminal; the wire-level `code` is untouched, so `--json` and scripted
/// callers still see the raw envelope unchanged). Falls through to the raw
/// message when the narrow duplicate-name shape isn't recognized, so an
/// unrelated `workflow_store_error` is never silently reworded into
/// something it isn't.
fn humanize_workflow_error_message<'a>(code: &str, message: &'a str) -> std::borrow::Cow<'a, str> {
    if code == "workflow_store_error" {
        if let Some(name) = duplicate_workflow_name(message) {
            return std::borrow::Cow::Owned(format!(
                "a workflow named \"{name}\" already exists; choose a different name, or run `kvx workflow update {name} --file <definition>` to add a new version to the existing one"
            ));
        }
    }
    std::borrow::Cow::Borrowed(message)
}

/// Extracts the offending name from the store's `workflow_name` unique-index
/// violation message. Narrowly matched on both the index name and the
/// `already contains '...'` marker so this never fires on an unrelated store
/// error that merely happens to share the `workflow_store_error` code.
fn duplicate_workflow_name(message: &str) -> Option<&str> {
    if !message.contains("workflow_name") {
        return None;
    }
    let marker = "already contains '";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let end = rest.find('\'')?;
    Some(&rest[..end])
}

/// Prints [`format_workflow_error`] to stderr. `Some(1)` is the exit code the
/// caller should return; `None` means the response carried no error.
fn print_workflow_error(response: &serde_json::Value) -> Option<i32> {
    let rendered = format_workflow_error(response)?;
    eprintln!("{rendered}");
    Some(1)
}

/// The `--json` form of a local (pre-network) definition/environment error —
/// unchanged from before this task, and still what `kvx workflow node
/// complete` uses for its `missing_node_environment` error, since that
/// command is normally invoked by a node's own script rather than a human at
/// a terminal.
fn print_workflow_cli_error(code: &str, message: &str) {
    let envelope = serde_json::json!({
        "error": { "code": code, "message": message }
    });
    // Serialising an object of string leaves cannot fail, but an error path
    // that panics instead of reporting the error is the worst place to find
    // out otherwise.
    match serde_json::to_string(&envelope) {
        Ok(json) => eprintln!("{json}"),
        Err(_) => eprintln!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"{code}\"}}}}"),
    }
}

/// §2.16.3's local-error half: `create`/`update` can fail before any network
/// call (a bad `--file` path, invalid TOML/JSON, an unresolvable
/// `--name` rewrite). `--json` keeps the exact previous envelope
/// ([`print_workflow_cli_error`]); the human default prints the message with
/// its real newlines intact instead of JSON-escaping the TOML parser's caret
/// diagram into one unreadable line. Returns the exit code so call sites stay
/// a one-line `return Ok(print_workflow_local_error(...))`.
fn print_workflow_local_error(code: &str, message: &str, json: bool) -> i32 {
    if json {
        print_workflow_cli_error(code, message);
    } else {
        eprintln!("error: {code}");
        for line in message.lines() {
            eprintln!("  {line}");
        }
    }
    1
}

/// §2.16.3's success half, and §2.19's "`update` ... misreports no-ops" for
/// `create` specifically: the wire only has one response shape, so this adds
/// the human summary formatting that shape never had.
fn print_workflow_create_response(
    response: &serde_json::Value,
    json: bool,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let workflow = &response["result"]["workflow"];
    let version = &response["result"]["version"];
    println!(
        "workflow_id: {}",
        workflow["workflow_id"].as_str().unwrap_or("")
    );
    println!("name:        {}", workflow["name"].as_str().unwrap_or(""));
    println!(
        "version:     {}",
        version["version"].as_u64().unwrap_or_default()
    );
    let change_summary = version["change_summary"].as_str().unwrap_or("");
    if !change_summary.is_empty() {
        println!("change:      {change_summary}");
    }
    Ok(0)
}

/// See [`workflow_update`] for why `previous_head_version` exists: it is the
/// workflow's head version number read *before* this update, compared here
/// against the version the update actually returns to tell "a new version
/// was created" from "this content already matched the head version, so
/// nothing new was created" — a distinction the wire's single
/// `workflow_version_created` response type cannot make on its own.
///
/// `previous`/`new` disagree in width (`u32`/`u64`) because that's what the
/// two call sites actually hold — a decoded `head_version` field and a raw
/// `as_u64()` JSON read — so the comparison widens rather than making a
/// caller narrow first.
fn update_was_deduplicated(previous: Option<u32>, new: Option<u64>) -> bool {
    matches!(
        (previous, new),
        (Some(previous), Some(new)) if u64::from(previous) == new
    )
}

fn print_workflow_update_response(
    response: &serde_json::Value,
    json: bool,
    previous_head_version: Option<u32>,
) -> std::io::Result<i32> {
    if json {
        return super::print_response(response);
    }
    if let Some(code) = print_workflow_error(response) {
        return Ok(code);
    }
    let workflow = &response["result"]["workflow"];
    let version = &response["result"]["version"];
    let new_version = version["version"].as_u64();
    let unchanged = update_was_deduplicated(previous_head_version, new_version);

    println!(
        "workflow_id: {}",
        workflow["workflow_id"].as_str().unwrap_or("")
    );
    println!("name:        {}", workflow["name"].as_str().unwrap_or(""));
    if unchanged {
        println!(
            "version:     {} (unchanged — this definition matches the current version; no new version was created)",
            new_version.unwrap_or(0)
        );
    } else {
        println!("version:     {}", new_version.unwrap_or(0));
        let change_summary = version["change_summary"].as_str().unwrap_or("");
        if !change_summary.is_empty() {
            println!("change:      {change_summary}");
        }
    }
    Ok(0)
}

/// §2.18: `workflow show` used to print `workflow.get`'s raw JSON — metadata
/// plus a version history and nothing else, no nodes, no edges, no args, for
/// any version, plus unreadable epoch-millisecond timestamps. This renders
/// the workflow summary, a version history with formatted timestamps, and —
/// when the head version's detail was fetched successfully — the head
/// version's nodes (key/label/runner/demand), edges (from/to/kind/port), and
/// declared args, so a user can discover node paths (for `node show`/
/// `steer`/`restart`) and `--arg` names without going back to the original
/// definition file.
fn print_workflow_show(
    get_response: &serde_json::Value,
    version_response: Option<&serde_json::Value>,
) {
    let workflow = &get_response["result"]["workflow"];
    println!(
        "workflow_id: {}",
        workflow["workflow_id"].as_str().unwrap_or("")
    );
    println!("name:        {}", workflow["name"].as_str().unwrap_or(""));
    let description = workflow["description"].as_str().unwrap_or("");
    if !description.is_empty() {
        println!("description: {description}");
    }
    println!(
        "default_tier: {}",
        workflow["default_tier"].as_str().unwrap_or("")
    );
    if let Some(head_version) = workflow["head_version"].as_u64() {
        println!("head_version: {head_version}");
    }

    if let Some(versions) = get_response["result"]["versions"].as_array() {
        println!();
        println!("versions:");
        for version in versions {
            let number = version["version"].as_u64().unwrap_or(0);
            let created = version["created_at_unix_ms"]
                .as_u64()
                .map(format_unix_ms)
                .unwrap_or_default();
            let change_summary = version["change_summary"].as_str().unwrap_or("");
            print!("  v{number}  {created}");
            if !change_summary.is_empty() {
                print!("  {change_summary}");
            }
            println!();
        }
    }

    let Some(version_response) = version_response else {
        println!();
        println!("(head version detail unavailable; nodes/edges/args not shown)");
        return;
    };
    let version = &version_response["result"]["version"];

    if let Some(args) = version["args"].as_array() {
        if !args.is_empty() {
            println!();
            println!("args:");
            for arg in args {
                let name = arg["name"].as_str().unwrap_or("");
                let required = arg["required"].as_bool().unwrap_or(false);
                let default = arg["default"].as_str();
                let description = arg["description"].as_str().unwrap_or("");
                print!(
                    "  {name:<20} {}",
                    if required { "required" } else { "optional" }
                );
                if let Some(default) = default {
                    print!("  default={default}");
                }
                if !description.is_empty() {
                    print!("  {description}");
                }
                println!();
            }
        }
    }

    if let Some(nodes) = version["nodes"].as_array() {
        println!();
        println!("nodes:");
        for node in nodes {
            let key = node["node_key"].as_str().unwrap_or("");
            let label = node["label"].as_str().unwrap_or("");
            let runner = node["runner"].as_str().unwrap_or("");
            let demand = node["demand"].as_str().unwrap_or("");
            println!("  {key:<20} {label:<24} runner={runner:<8} demand={demand}");
        }
    }

    if let Some(edges) = version["edges"].as_array() {
        if !edges.is_empty() {
            println!();
            println!("edges:");
            for edge in edges {
                let from = edge["from"].as_str().unwrap_or("");
                let to = edge["to"].as_str().unwrap_or("");
                let kind = edge["kind"].as_str().unwrap_or("");
                let port = edge["port"].as_str();
                print!("  {from} -> {to}  kind={kind}");
                if let Some(port) = port {
                    print!("  port={port}");
                }
                println!();
            }
        }
    }
}

/// Formats an epoch-millisecond timestamp as `YYYY-MM-DD HH:MM:SS UTC` —
/// §2.18: "version history ... prints raw JSON, with epoch-millisecond
/// timestamps ... effectively unreadable". Uses the standard
/// days-since-epoch civil calendar conversion (Howard Hinnant's
/// `civil_from_days`, public domain) rather than adding a date/time
/// dependency for one CLI formatting call.
fn format_unix_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let days = (total_secs / 86400) as i64;
    let secs_of_day = total_secs % 86400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days-since-1970-01-01 to (year, month, day), proleptic Gregorian
/// calendar. Public-domain algorithm by Howard Hinnant
/// (`https://howardhinnant.github.io/date_algorithms.html#civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

fn print_workflow_help() {
    eprintln!("kvx workflow commands:");
    eprintln!("  kvx workflow list [--json]");
    eprintln!("  kvx workflow show <name|id> [--json]");
    eprintln!("  kvx workflow create --file <definition.toml|json> [--name <name>] [--json]");
    eprintln!(
        "  kvx workflow update <name|id> --file <definition.toml|json> [--change-summary <text>] [--json]"
    );
    eprintln!("  kvx workflow run <subcommand> ...");
    eprintln!("  kvx workflow node <subcommand> ...");
    eprintln!("  kvx workflow summary <subcommand> ...");
    eprintln!("  kvx workflow review <subcommand> ...");
}

fn print_workflow_run_help() {
    eprintln!("kvx workflow run commands:");
    eprintln!(
        "  kvx workflow run start <name|id> [--tier <tier>] [--arg KEY=VALUE]... [--restore-from <run-id>] [--restore <selector>]... [--restore-allow-changed] [--no-prior-summaries] [--json]"
    );
    eprintln!("  kvx workflow run list <name|id> [--limit N] [--json]");
    eprintln!("  kvx workflow run show <run_id> [--json]");
    eprintln!("  kvx workflow run cancel <run_id> [--json]");
    eprintln!(
        "  kvx workflow run finish [--run <run_id>] (--summary-file <path> | --summary <text>) [--outcome <word>] [--json]"
    );
    eprintln!(
        "  kvx workflow run message [--run <run_id>] --to <name> (--text <text> | --text-file <path>) [--priority now|next|later] [--json]"
    );
    eprintln!("  kvx workflow run report-session [--run <run_id>] [--json]");
}

fn print_workflow_node_help() {
    eprintln!("kvx workflow node commands:");
    eprintln!("  kvx workflow node show <run_id> <path> [--json]");
    eprintln!("  kvx workflow node steer <run_id> <path> <text> [--json]");
    eprintln!("  kvx workflow node interrupt <run_id> <path> [--json]");
    eprintln!("  kvx workflow node restart <run_id> <path> [--json]");
    eprintln!(
        "  kvx workflow node complete [--result-file <path>]   # run by the node itself; reads KARVEX_WORKFLOW_RUN_ID/NODE_PATH/NODE_DIR/NODE_TOKEN"
    );
    eprintln!(
        "  kvx workflow node expand <run_id> <path> --template <key> --label <text> [--input KEY=VALUE]... [--count N] [--json]   # run by the node itself; reads KARVEX_WORKFLOW_NODE_TOKEN"
    );
    eprintln!(
        "  kvx workflow node interrogate <run_id> <path> [--reconstructed] [--note <text>] [--json]"
    );
}

fn print_workflow_summary_help() {
    eprintln!("kvx workflow summary commands:");
    eprintln!("  kvx workflow summary show <run_id> [--json]");
    eprintln!("  kvx workflow summary list [<workflow>] [--limit N] [--json]");
}

fn print_workflow_review_help() {
    eprintln!("kvx workflow review commands:");
    eprintln!("  kvx workflow review start <run_id> [--json]");
    eprintln!("  kvx workflow review show <run_id> [--json]");
    eprintln!(
        "  kvx workflow review apply <run_id> (--accept <node_key>... | --decline-all) [--json]"
    );
    eprintln!(
        "  kvx workflow review answer --file <path> [--json]   # run from an interview pane; reads KARVEX_WORKFLOW_REVIEW_RUN_ID/MEMBER"
    );
    eprintln!(
        "  kvx workflow review report --file <path> [--json]   # run from the synthesis pane; reads KARVEX_WORKFLOW_REVIEW_RUN_ID"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("karvex-cli-workflow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 2.16: the server's best messages are multi-line — the schema-violation
    /// list a rejected `node complete` gets back, the kvdag validators, the TOML
    /// caret diagram — and the raw envelope turned every newline into a literal
    /// `\n` on one unreadable line.
    #[test]
    fn a_workflow_error_envelope_renders_as_readable_lines() {
        let response = serde_json::json!({
            "error": {
                "code": "workflow_node_result_invalid",
                "message": "result.json does not validate against the node's output_schema:\n  - missing required field \"summary\"\nfix result.json and run `kvx workflow node complete` again",
            }
        });
        let rendered = format_workflow_error(&response).expect("an error envelope renders");
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines,
            vec![
                "error: workflow_node_result_invalid",
                "  result.json does not validate against the node's output_schema:",
                "    - missing required field \"summary\"",
                "  fix result.json and run `kvx workflow node complete` again",
            ],
            "every message line is indented under the code, never escaped: {rendered}"
        );
        assert!(
            !rendered.contains("\\n"),
            "no escaped newlines survive: {rendered}"
        );

        assert_eq!(
            format_workflow_error(&serde_json::json!({ "result": { "type": "ok" } })),
            None,
            "a success response is left to the caller's own rendering"
        );
    }

    #[test]
    fn workflow_list_accepts_no_arguments_and_rejects_extras() {
        assert!(parse_workflow_list_args(&args(&[])).is_ok());
        assert!(parse_workflow_list_args(&args(&["extra"])).is_err());
    }

    #[test]
    fn workflow_show_builds_workflow_get() {
        let (target, json) = parse_workflow_show_args(&args(&["ship-feature"])).unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowGet(target),
            Method::WorkflowGet(WorkflowTarget {
                workflow_id: "ship-feature".to_string(),
            })
        );
    }

    #[test]
    fn workflow_show_json_flag_parses() {
        let (target, json) = parse_workflow_show_args(&args(&["ship-feature", "--json"])).unwrap();
        assert!(json);
        assert_eq!(target.workflow_id, "ship-feature");
    }

    #[test]
    fn workflow_show_rejects_wrong_argument_count() {
        assert!(parse_workflow_show_args(&args(&[])).is_err());
        assert!(parse_workflow_show_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn workflow_create_flags_parse_file_and_optional_name() {
        let (file, name, json) =
            parse_workflow_create_args(&args(&["--file", "def.toml", "--name", "ship"])).unwrap();
        assert_eq!(file, "def.toml");
        assert_eq!(name.as_deref(), Some("ship"));
        assert!(!json);

        let (file, name, json) =
            parse_workflow_create_args(&args(&["--file", "def.json"])).unwrap();
        assert_eq!(file, "def.json");
        assert_eq!(name, None);
        assert!(!json);
    }

    #[test]
    fn workflow_create_json_flag_parses() {
        let (file, _name, json) =
            parse_workflow_create_args(&args(&["--file", "def.toml", "--json"])).unwrap();
        assert_eq!(file, "def.toml");
        assert!(json);
    }

    #[test]
    fn workflow_create_requires_file() {
        assert!(parse_workflow_create_args(&args(&["--name", "ship"])).is_err());
    }

    /// A9: `--help` advertises `<name|id>` for `show`/`update`/`run start`, so
    /// the CLI's job is to forward whatever the caller typed — a workflow
    /// *name*, not just a `workflow:<key>` id — unchanged as `workflow_id`.
    /// Resolving the name is the server's job (`src/app/api/workflows.rs`'s
    /// `resolve_workflow_selector`); the CLI must never reject or reshape it
    /// first.
    #[test]
    fn name_and_id_selectors_reach_the_wire_unchanged_for_show_update_and_run_start() {
        for selector in ["ship-feature", "workflow:abc123"] {
            let (target, _json) = parse_workflow_show_args(&args(&[selector])).unwrap();
            assert_eq!(target.workflow_id, selector);

            let (workflow_id, _file, _change_summary, _json) =
                parse_workflow_update_args(&args(&[selector, "--file", "def.toml"])).unwrap();
            assert_eq!(workflow_id, selector);

            let (params, _json) = parse_workflow_run_start_args(&args(&[selector])).unwrap();
            assert_eq!(params.workflow_id, selector);
        }
    }

    #[test]
    fn workflow_update_flags_parse_target_file_and_change_summary() {
        let (workflow_id, file, change_summary, json) = parse_workflow_update_args(&args(&[
            "ship-feature",
            "--file",
            "def.toml",
            "--change-summary",
            "widen retries",
        ]))
        .unwrap();
        assert_eq!(workflow_id, "ship-feature");
        assert_eq!(file, "def.toml");
        assert_eq!(change_summary.as_deref(), Some("widen retries"));
        assert!(!json);
    }

    #[test]
    fn workflow_update_json_flag_parses() {
        let (_workflow_id, _file, _change_summary, json) =
            parse_workflow_update_args(&args(&["ship-feature", "--file", "def.toml", "--json"]))
                .unwrap();
        assert!(json);
    }

    #[test]
    fn workflow_update_requires_target_and_file() {
        assert!(parse_workflow_update_args(&args(&[])).is_err());
        assert!(parse_workflow_update_args(&args(&["ship-feature"])).is_err());
    }

    #[test]
    fn definition_format_detected_from_extension() {
        assert_eq!(
            definition_format_from_path("def.toml").unwrap(),
            WorkflowDefinitionFormat::Toml
        );
        assert_eq!(
            definition_format_from_path("def.json").unwrap(),
            WorkflowDefinitionFormat::Json
        );
        assert!(definition_format_from_path("def.yaml").is_err());
    }

    #[test]
    fn override_definition_name_rewrites_toml_top_level_name() {
        let rewritten = override_definition_name(
            WorkflowDefinitionFormat::Toml,
            "name = \"old\"\nmax_depth = 3\n",
            "new-name",
        )
        .unwrap();
        let value: toml::Value = rewritten.parse().unwrap();
        assert_eq!(value["name"].as_str(), Some("new-name"));
        assert_eq!(value["max_depth"].as_integer(), Some(3));
    }

    #[test]
    fn override_definition_name_rewrites_json_top_level_name() {
        let rewritten = override_definition_name(
            WorkflowDefinitionFormat::Json,
            r#"{"name":"old","max_depth":3}"#,
            "new-name",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(value["name"], "new-name");
        assert_eq!(value["max_depth"], 3);
    }

    #[test]
    fn load_definition_document_reads_file_and_applies_name_override() {
        let dir = unique_temp_dir("load-definition");
        let path = dir.join("def.toml");
        std::fs::write(&path, "name = \"old\"\n").unwrap();

        let definition =
            load_definition_document(path.to_str().unwrap(), Some("new-name".to_string())).unwrap();
        assert_eq!(definition.format, WorkflowDefinitionFormat::Toml);
        assert!(definition.text.contains("new-name"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workflow_run_start_builds_workflow_run_with_tier_and_args() {
        let (params, json) = parse_workflow_run_start_args(&args(&[
            "ship-feature",
            "--tier",
            "high",
            "--arg",
            "goal=add dark mode",
            "--json",
        ]))
        .unwrap();
        assert!(json);
        assert_eq!(
            Method::WorkflowRun(params),
            Method::WorkflowRun(WorkflowRunParams {
                workflow_id: "ship-feature".to_string(),
                version: None,
                tier: Some(WorkflowTier::High),
                args: HashMap::from([("goal".to_string(), "add dark mode".to_string())]),
                restore_from: None,
                include_prior_summaries: None,
            })
        );
    }

    #[test]
    fn workflow_run_start_defaults_tier_and_json_when_omitted() {
        let (params, json) = parse_workflow_run_start_args(&args(&["ship-feature"])).unwrap();
        assert!(!json);
        assert_eq!(params.tier, None);
        assert!(params.args.is_empty());
    }

    #[test]
    fn workflow_run_start_rejects_malformed_arg_assignment() {
        assert!(
            parse_workflow_run_start_args(&args(&["ship-feature", "--arg", "no-equals-sign"]))
                .is_err()
        );
    }

    #[test]
    fn workflow_run_start_rejects_invalid_tier() {
        assert!(
            parse_workflow_run_start_args(&args(&["ship-feature", "--tier", "ultra"])).is_err()
        );
    }

    // ── §4 D18/D11 restore flags ─────────────────────────────────────────

    /// §4 D18: bare `--restore-from <run>` means "everything restorable" —
    /// wire-encoded as an empty `nodes` selector list.
    #[test]
    fn workflow_run_start_bare_restore_from_means_everything_restorable() {
        let (params, _json) =
            parse_workflow_run_start_args(&args(&["ship-feature", "--restore-from", "run-1"]))
                .unwrap();
        assert_eq!(
            params.restore_from,
            Some(WorkflowRestoreRequest {
                run_id: "run-1".to_string(),
                nodes: vec![],
                allow_changed: false,
            })
        );
    }

    /// `--restore` is repeatable, narrows the bare form, and its selector
    /// value is taken verbatim — including one containing `=`, which must not
    /// be mistaken for a `KEY=VALUE` assignment the way `--arg`/`--input` do.
    #[test]
    fn workflow_run_start_restore_selectors_are_repeatable_and_verbatim() {
        let (params, _json) = parse_workflow_run_start_args(&args(&[
            "ship-feature",
            "--restore-from",
            "run-1",
            "--restore",
            "plan",
            "--restore",
            "build=x",
            "--restore-allow-changed",
        ]))
        .unwrap();
        assert_eq!(
            params.restore_from,
            Some(WorkflowRestoreRequest {
                run_id: "run-1".to_string(),
                nodes: vec!["plan".to_string(), "build=x".to_string()],
                allow_changed: true,
            })
        );
    }

    #[test]
    fn workflow_run_start_restore_requires_restore_from() {
        assert!(
            parse_workflow_run_start_args(&args(&["ship-feature", "--restore", "plan"])).is_err()
        );
    }

    #[test]
    fn workflow_run_start_restore_allow_changed_requires_restore_from() {
        assert!(
            parse_workflow_run_start_args(&args(&["ship-feature", "--restore-allow-changed"]))
                .is_err()
        );
    }

    /// §4 D21: absent `--no-prior-summaries` means `include_prior_summaries:
    /// None`, which the docs and the handler both read as "default true" —
    /// the flag only ever narrows to `Some(false)`, never sets `Some(true)`.
    #[test]
    fn workflow_run_start_no_prior_summaries_sets_include_prior_summaries_false() {
        let (params, _json) =
            parse_workflow_run_start_args(&args(&["ship-feature", "--no-prior-summaries"]))
                .unwrap();
        assert_eq!(params.include_prior_summaries, Some(false));
    }

    #[test]
    fn workflow_run_start_omits_include_prior_summaries_by_default() {
        let (params, _json) = parse_workflow_run_start_args(&args(&["ship-feature"])).unwrap();
        assert_eq!(params.include_prior_summaries, None);
    }

    #[test]
    fn workflow_run_list_builds_workflow_run_list_with_limit() {
        let (params, json) =
            parse_workflow_run_list_args(&args(&["ship-feature", "--limit", "10"])).unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowRunList(params),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: Some("ship-feature".to_string()),
                limit: Some(10),
            })
        );
    }

    #[test]
    fn workflow_run_list_accepts_json_flag() {
        let (_, json) = parse_workflow_run_list_args(&args(&["ship-feature", "--json"])).unwrap();
        assert!(json);
    }

    #[test]
    fn workflow_run_list_requires_target() {
        assert!(parse_workflow_run_list_args(&args(&[])).is_err());
    }

    #[test]
    fn workflow_run_show_builds_workflow_run_get() {
        let (target, json) = parse_workflow_run_show_args(&args(&["run-1", "--json"])).unwrap();
        assert!(json);
        assert_eq!(
            Method::WorkflowRunGet(target),
            Method::WorkflowRunGet(WorkflowRunTarget {
                run_id: "run-1".to_string(),
            })
        );
    }

    #[test]
    fn workflow_run_show_defaults_json_to_false() {
        let (_, json) = parse_workflow_run_show_args(&args(&["run-1"])).unwrap();
        assert!(!json);
    }

    #[test]
    fn workflow_run_cancel_builds_workflow_run_cancel() {
        let (target, json) = parse_workflow_run_cancel_args(&args(&["run-1"])).unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowRunCancel(target),
            Method::WorkflowRunCancel(WorkflowRunTarget {
                run_id: "run-1".to_string(),
            })
        );
    }

    /// §4 D17's regression pin: `run cancel --json` used to be a parse error
    /// (`unknown option: --json`) — the exact gap the sweep closes.
    #[test]
    fn workflow_run_cancel_accepts_json_flag() {
        let (_, json) = parse_workflow_run_cancel_args(&args(&["run-1", "--json"])).unwrap();
        assert!(json);
    }

    #[test]
    fn workflow_node_show_builds_workflow_node_get() {
        let (target, json) =
            parse_workflow_node_show_args(&args(&["run-1", "plan", "--json"])).unwrap();
        assert!(json);
        assert_eq!(
            Method::WorkflowNodeGet(target),
            Method::WorkflowNodeGet(WorkflowNodeTarget {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
            })
        );
    }

    /// `run show` used to print path + status only, so the three children of a
    /// fan-out — which differ *only* by label — read as three anonymous rows.
    /// The label is the one thing that says what each of them was sent to do.
    #[test]
    fn run_show_names_each_node_by_its_own_label() {
        let lines: Vec<String> = [
            serde_json::json!({"path": "fanout/worker/1", "status": "succeeded", "label": "Shard 1"}),
            serde_json::json!({"path": "fanout/worker/2", "status": "succeeded", "label": "Shard 2"}),
            serde_json::json!({"path": "fanout/worker/3", "status": "running", "label": "Shard 3"}),
        ]
        .iter()
        .map(format_run_node_line)
        .collect();
        assert_eq!(
            lines,
            vec![
                "  fanout/worker/1              succeeded    Shard 1".to_string(),
                "  fanout/worker/2              succeeded    Shard 2".to_string(),
                "  fanout/worker/3              running      Shard 3".to_string(),
            ],
            "a generation must not read as three identical rows"
        );
    }

    /// A node whose label says nothing the path did not already say keeps the
    /// original two-column line — the label column is for disambiguation, not
    /// decoration.
    #[test]
    fn run_show_omits_a_label_that_only_repeats_the_path() {
        assert_eq!(
            format_run_node_line(
                &serde_json::json!({"path": "plan", "status": "succeeded", "label": "plan"})
            ),
            "  plan                         succeeded"
        );
        assert_eq!(
            format_run_node_line(&serde_json::json!({"path": "plan", "status": "succeeded"})),
            "  plan                         succeeded",
            "a wire payload from an older server carries no label at all"
        );
    }

    #[test]
    fn workflow_node_steer_joins_trailing_words_as_text() {
        let (params, json) =
            parse_workflow_node_steer_args(&args(&["run-1", "plan", "please", "hurry"])).unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowNodeSteer(params),
            Method::WorkflowNodeSteer(WorkflowNodeSteerParams {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
                text: "please hurry".to_string(),
            })
        );
    }

    #[test]
    fn workflow_node_steer_requires_text() {
        assert!(parse_workflow_node_steer_args(&args(&["run-1", "plan"])).is_err());
    }

    /// A trailing `--json` after at least one text word is the flag, not text.
    #[test]
    fn workflow_node_steer_recognizes_trailing_json_flag() {
        let (params, json) =
            parse_workflow_node_steer_args(&args(&["run-1", "plan", "hurry", "--json"])).unwrap();
        assert!(json);
        assert_eq!(params.text, "hurry");
    }

    /// With no text word at all, a lone trailing `--json` is kept as the
    /// (odd but legal) steered text rather than silently eaten as a flag.
    #[test]
    fn workflow_node_steer_treats_bare_json_as_text_without_other_words() {
        let (params, json) =
            parse_workflow_node_steer_args(&args(&["run-1", "plan", "--json"])).unwrap();
        assert!(!json);
        assert_eq!(params.text, "--json");
    }

    #[test]
    fn workflow_node_interrupt_builds_workflow_node_interrupt() {
        let (target, json) =
            parse_workflow_node_pair_args(&args(&["run-1", "plan"]), "usage").unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowNodeInterrupt(target),
            Method::WorkflowNodeInterrupt(WorkflowNodeTarget {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
            })
        );
    }

    #[test]
    fn workflow_node_interrupt_accepts_json_flag() {
        let (_, json) =
            parse_workflow_node_pair_args(&args(&["run-1", "plan", "--json"]), "usage").unwrap();
        assert!(json);
    }

    #[test]
    fn workflow_node_restart_builds_workflow_node_restart() {
        let (target, json) =
            parse_workflow_node_pair_args(&args(&["run-1", "plan"]), "usage").unwrap();
        assert!(!json);
        assert_eq!(
            Method::WorkflowNodeRestart(target),
            Method::WorkflowNodeRestart(WorkflowNodeTarget {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
            })
        );
    }

    // ── node expand ──────────────────────────────────────────────────────

    #[test]
    fn workflow_node_expand_builds_workflow_node_expand() {
        let (params, json) = parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
            "--count",
            "4",
            "--json",
        ]))
        .unwrap();
        assert!(json);
        assert_eq!(params.run_id, "run-1");
        assert_eq!(params.path, "plan");
        assert_eq!(params.template, "worker");
        assert_eq!(params.label, "Worker");
        assert_eq!(params.count, Some(4));
        // The token is never taken from argv: it is read from
        // `KARVEX_WORKFLOW_NODE_TOKEN` by the leaf function, exactly like
        // `node complete`'s env-sourced credential.
        assert_eq!(params.token, "");
    }

    #[test]
    fn workflow_node_expand_json_flag_defaults_to_false() {
        let (_params, json) = parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
        ]))
        .unwrap();
        assert!(!json);
    }

    #[test]
    fn workflow_node_expand_requires_template_and_label() {
        assert!(parse_workflow_node_expand_args(&args(&["run-1", "plan"])).is_err());
        assert!(
            parse_workflow_node_expand_args(&args(&["run-1", "plan", "--template", "worker"]))
                .is_err()
        );
        assert!(
            parse_workflow_node_expand_args(&args(&["run-1", "plan", "--label", "Worker"]))
                .is_err()
        );
    }

    /// The `--input k=v` case §WS-I's "Tested" section names explicitly: a
    /// value that itself contains `=` still parses whole, because
    /// `parse_arg_assignment` splits on the *first* `=` only.
    #[test]
    fn workflow_node_expand_input_with_equals_in_the_value_parses_whole() {
        let (params, _json) = parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
            "--input",
            "goal=a=b=c",
        ]))
        .unwrap();
        assert_eq!(params.inputs.get("goal").map(String::as_str), Some("a=b=c"));
    }

    #[test]
    fn workflow_node_expand_accepts_repeated_inputs() {
        let (params, _json) = parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
            "--input",
            "a=1",
            "--input",
            "b=2",
        ]))
        .unwrap();
        assert_eq!(params.inputs.get("a").map(String::as_str), Some("1"));
        assert_eq!(params.inputs.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn workflow_node_expand_rejects_unparseable_count() {
        assert!(parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
            "--count",
            "many",
        ]))
        .is_err());
    }

    #[test]
    fn workflow_node_expand_omits_count_when_not_given() {
        let (params, _json) = parse_workflow_node_expand_args(&args(&[
            "run-1",
            "plan",
            "--template",
            "worker",
            "--label",
            "Worker",
        ]))
        .unwrap();
        assert_eq!(params.count, None);
    }

    /// §3 frozen interface 7: a rejection is a success response, never routed
    /// through the error renderer, and partial acceptance (both non-empty) is
    /// the interesting case — never accept-all, never reject-all reported as
    /// though it were an error.
    #[test]
    fn summarize_expand_response_reports_partial_acceptance() {
        let response = serde_json::json!({
            "result": {
                "type": "workflow_node_expanded",
                "accepted": ["plan/worker/1", "plan/worker/2"],
                "rejected": [
                    {
                        "template": "worker",
                        "reason": "truncated",
                        "requested": 4,
                        "accepted": 2,
                        "message": "expand_max 2 reached; 2 of 4 requested nodes created",
                    }
                ],
            }
        });
        let summary = summarize_expand_response(&response);
        assert_eq!(summary.accepted, vec!["plan/worker/1", "plan/worker/2"]);
        assert_eq!(summary.rejected.len(), 1);
        assert_eq!(summary.rejected[0].template, "worker");
        assert_eq!(summary.rejected[0].reason, "truncated");
        assert!(summary.rejected[0].message.contains("2 of 4"));
    }

    #[test]
    fn summarize_expand_response_is_empty_for_a_wholly_accepted_proposal() {
        let response = serde_json::json!({
            "result": { "accepted": ["plan/worker/1"], "rejected": [] }
        });
        let summary = summarize_expand_response(&response);
        assert_eq!(summary.accepted, vec!["plan/worker/1"]);
        assert!(summary.rejected.is_empty());
    }

    // ── node interrogate ─────────────────────────────────────────────────

    #[test]
    fn workflow_node_interrogate_defaults_to_resumed_mode() {
        let (params, json) =
            parse_workflow_node_interrogate_args(&args(&["run-1", "plan"])).unwrap();
        assert!(!json);
        assert_eq!(params.run_id, "run-1");
        assert_eq!(params.path, "plan");
        assert_eq!(params.mode, WorkflowInterrogationMode::Resumed);
        assert_eq!(params.note, None);
    }

    #[test]
    fn workflow_node_interrogate_reconstructed_flag_sets_mode() {
        let (params, _json) =
            parse_workflow_node_interrogate_args(&args(&["run-1", "plan", "--reconstructed"]))
                .unwrap();
        assert_eq!(params.mode, WorkflowInterrogationMode::Reconstructed);
    }

    #[test]
    fn workflow_node_interrogate_parses_note_and_json() {
        let (params, json) = parse_workflow_node_interrogate_args(&args(&[
            "run-1",
            "plan",
            "--note",
            "why did this fail",
            "--json",
        ]))
        .unwrap();
        assert!(json);
        assert_eq!(params.note.as_deref(), Some("why did this fail"));
    }

    #[test]
    fn workflow_node_interrogate_requires_run_id_and_path() {
        assert!(parse_workflow_node_interrogate_args(&args(&["run-1"])).is_err());
    }

    // ── workflow summary show / list ─────────────────────────────────────

    #[test]
    fn workflow_summary_show_builds_workflow_summary_get() {
        let (target, json) = parse_workflow_summary_show_args(&args(&["run-1", "--json"])).unwrap();
        assert!(json);
        assert_eq!(
            Method::WorkflowSummaryGet(target),
            Method::WorkflowSummaryGet(WorkflowRunTarget {
                run_id: "run-1".to_string(),
            })
        );
    }

    #[test]
    fn workflow_summary_show_requires_run_id() {
        assert!(parse_workflow_summary_show_args(&args(&[])).is_err());
    }

    #[test]
    fn workflow_summary_list_workflow_is_optional() {
        let (params, json) = parse_workflow_summary_list_args(&args(&[])).unwrap();
        assert!(!json);
        assert_eq!(params.workflow_id, None);
        assert_eq!(params.limit, None);
    }

    #[test]
    fn workflow_summary_list_parses_workflow_limit_and_json() {
        let (params, json) =
            parse_workflow_summary_list_args(&args(&["ship-feature", "--limit", "5", "--json"]))
                .unwrap();
        assert!(json);
        assert_eq!(params.workflow_id, Some("ship-feature".to_string()));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn workflow_summary_list_rejects_a_second_positional() {
        assert!(parse_workflow_summary_list_args(&args(&["one", "two"])).is_err());
    }

    // ── restore report / transcript / restored-from rendering ───────────

    #[test]
    fn format_restore_report_line_is_none_for_a_non_restoring_run() {
        assert_eq!(format_restore_report_line(&serde_json::Value::Null), None);
    }

    /// The plan's own example string
    /// (`07-phase3-plan.md` §WS-E): `restored: plan, implement · skipped:
    /// review (definition changed)`.
    #[test]
    fn format_restore_report_line_matches_the_plan_example() {
        let restore = serde_json::json!({
            "restored": ["plan", "implement"],
            "skipped": [
                {"selector": "review", "reason": "definition_changed", "message": "prompt changed"},
            ],
        });
        assert_eq!(
            format_restore_report_line(&restore).unwrap(),
            "restored: plan, implement · skipped: review (definition changed)"
        );
    }

    #[test]
    fn format_restore_report_line_reports_an_all_skipped_restore() {
        let restore = serde_json::json!({
            "restored": [],
            "skipped": [
                {"selector": "plan", "reason": "no_checkpoint", "message": "no checkpoint"},
            ],
        });
        let line = format_restore_report_line(&restore).unwrap();
        assert!(line.starts_with("skipped:"), "{line}");
        assert!(!line.contains("restored:"), "{line}");
    }

    #[test]
    fn format_node_transcript_line_reports_present_or_absent() {
        assert_eq!(
            format_node_transcript_line(&serde_json::json!({"transcript_path": "/tmp/x.jsonl"})),
            "transcript: present"
        );
        assert_eq!(
            format_node_transcript_line(&serde_json::json!({})),
            "transcript: absent"
        );
    }

    #[test]
    fn format_node_restored_from_line_names_run_node_and_checkpoint() {
        let node = serde_json::json!({
            "restored_from": {
                "run_id": "run-1",
                "node_key": "plan",
                "checkpoint_seq": 3,
            }
        });
        assert_eq!(
            format_node_restored_from_line(&node).unwrap(),
            "restored from: run run-1 · node plan · checkpoint #3"
        );
        assert_eq!(format_node_restored_from_line(&serde_json::json!({})), None);
    }

    // ── growth/rejection lines (run show / node show) ───────────────────

    #[test]
    fn format_run_growth_line_is_none_when_the_run_has_never_been_limited() {
        let run = serde_json::json!({
            "nodes_live": 3, "max_nodes": 12, "growth_limited": null
        });
        assert_eq!(format_run_growth_line(&run), None);

        let run_without_field = serde_json::json!({ "nodes_live": 3, "max_nodes": 12 });
        assert_eq!(format_run_growth_line(&run_without_field), None);
    }

    #[test]
    fn format_run_growth_line_names_the_kind_and_clock_time() {
        let run = serde_json::json!({
            "nodes_live": 3,
            "max_nodes": 12,
            "growth_limited": {
                "kind": "max_nodes",
                "at_unix_ms": 1_700_000_000_000u64,
            }
        });
        let line = format_run_growth_line(&run).expect("a limited run has a growth line");
        assert!(line.starts_with("growth:"), "{line}");
        assert!(line.contains("3 of 12 nodes"), "{line}");
        assert!(line.contains("max_nodes reached at"), "{line}");
        assert!(line.ends_with(" UTC"), "{line}");
    }

    #[test]
    fn format_node_growth_limited_line_is_none_without_a_rejection() {
        let node = serde_json::json!({ "path": "plan" });
        assert_eq!(format_node_growth_limited_line(&node), None);
    }

    #[test]
    fn format_node_growth_limited_line_names_accepted_of_requested() {
        let node = serde_json::json!({
            "growth_limited": {
                "kind": "expand_max",
                "requested": 4,
                "accepted": 2,
                "at_unix_ms": 1_700_000_000_000u64,
            }
        });
        let line =
            format_node_growth_limited_line(&node).expect("a limited node has a growth line");
        assert!(
            line.starts_with("growth_limited: expand_max reached at"),
            "{line}"
        );
        assert!(line.contains("(2 of 4 accepted)"), "{line}");
        assert!(line.contains(" UTC "), "{line}");
    }

    /// §H6's sibling surface: `delivery_failure` is a shared runtime fact
    /// (`WorkflowRunNodeInfo.delivery_failure`), so `node show` names it in
    /// prose exactly as it already appears on the wire.
    #[test]
    fn format_node_delivery_failure_line_names_the_refused_delivery() {
        let node = serde_json::json!({
            "delivery_failure": "steer never reached the process: pane closed"
        });
        let line = format_node_delivery_failure_line(&node)
            .expect("a node with a delivery failure has a line");
        assert_eq!(
            line,
            "delivery_failure: steer never reached the process: pane closed"
        );

        let clean_node = serde_json::json!({ "path": "plan" });
        assert_eq!(format_node_delivery_failure_line(&clean_node), None);
    }

    #[test]
    fn format_run_limits_line_reports_the_enforced_ceilings() {
        let run = serde_json::json!({ "max_nodes": 12, "max_depth": 3 });
        assert_eq!(
            format_run_limits_line(&run),
            Some("limits:  max_nodes=12 · max_depth=3".to_string())
        );
    }

    #[test]
    fn format_run_limits_line_is_none_without_ceilings() {
        // `run start` / `run cancel` responses that never carried the keys.
        let run = serde_json::json!({ "run_id": "run-1", "status": "running" });
        assert_eq!(format_run_limits_line(&run), None);
    }

    #[test]
    fn format_unix_ms_clock_names_utc_like_its_sibling() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_unix_ms_clock(0), "00:00 UTC");
        // 2024-01-01T14:22:00Z
        assert_eq!(format_unix_ms_clock(1_704_118_920_000), "14:22 UTC");
    }

    #[test]
    fn workflow_node_pair_args_rejects_wrong_argument_count() {
        assert!(parse_workflow_node_pair_args(&args(&["run-1"]), "usage").is_err());
        assert!(
            parse_workflow_node_pair_args(&args(&["run-1", "plan", "extra"]), "usage").is_err()
        );
    }

    #[test]
    fn workflow_node_complete_accepts_optional_result_file_flag() {
        assert_eq!(parse_workflow_node_complete_args(&args(&[])).unwrap(), None);
        assert_eq!(
            parse_workflow_node_complete_args(&args(&["--result-file", "out.json"])).unwrap(),
            Some("out.json".to_string())
        );
    }

    #[test]
    fn workflow_node_complete_rejects_unknown_flag() {
        assert!(parse_workflow_node_complete_args(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn default_node_result_path_joins_node_dir_and_result_json() {
        let node_dir = std::env::temp_dir().join("karvex-cli-workflow-node-dir");
        let path = default_node_result_path(node_dir.to_str().unwrap());
        assert_eq!(std::path::PathBuf::from(path), node_dir.join("result.json"));
    }

    #[test]
    fn build_node_report_params_reads_and_parses_result_file() {
        let dir = unique_temp_dir("node-complete");
        let path = dir.join("result.json");
        std::fs::write(&path, r#"{"summary":"done"}"#).unwrap();

        let env = NodeCompleteEnv {
            run_id: "run-1".to_string(),
            node_path: "plan".to_string(),
            node_dir: dir.to_str().unwrap().to_string(),
            node_token: "tok".to_string(),
        };
        let report = build_node_report_params(&env, path.to_str().unwrap());
        assert_eq!(report.local_error, None);
        assert_eq!(
            Method::WorkflowNodeReport(report.params),
            Method::WorkflowNodeReport(WorkflowNodeReportParams {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
                token: "tok".to_string(),
                result: serde_json::json!({"summary": "done"}),
            })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `04-kvdag-and-execution.md` §4.3: the server owns completion. A result
    /// file the client cannot read or parse must still be reported — as a
    /// `null` result, which is the wire's "no result artifact" — so the
    /// server's `NeedsAttention` path is reachable for a `runner = "command"`
    /// node, whose only completion signal is this self-report.
    #[test]
    fn an_unreadable_result_is_still_reported_to_the_server() {
        let dir = unique_temp_dir("node-complete-invalid");
        let invalid = dir.join("result.json");
        std::fs::write(&invalid, "not json").unwrap();

        let env = NodeCompleteEnv {
            run_id: "run-1".to_string(),
            node_path: "plan".to_string(),
            node_dir: dir.to_str().unwrap().to_string(),
            node_token: "tok".to_string(),
        };

        for (result_path, expected_warning) in [
            (invalid.clone(), "invalid JSON"),
            (dir.join("absent.json"), "failed to read"),
        ] {
            let report = build_node_report_params(&env, result_path.to_str().unwrap());
            assert_eq!(
                report.params.result,
                serde_json::Value::Null,
                "an unusable result file reports null rather than blocking the report"
            );
            assert_eq!(report.params.run_id, "run-1");
            assert_eq!(report.params.path, "plan");
            assert_eq!(report.params.token, "tok");
            let warning = report
                .local_error
                .expect("the client still says what it could not do");
            assert!(
                warning.contains(expected_warning),
                "unexpected warning: {warning}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_arg_assignment_splits_on_first_equals() {
        assert_eq!(
            parse_arg_assignment("goal=add dark mode").unwrap(),
            ("goal".to_string(), "add dark mode".to_string())
        );
        assert!(parse_arg_assignment("no-equals-sign").is_err());
        assert!(parse_arg_assignment("=missing-key").is_err());
    }

    #[test]
    fn parse_workflow_tier_matches_wire_variants() {
        assert_eq!(parse_workflow_tier("auto").unwrap(), WorkflowTier::Auto);
        assert_eq!(parse_workflow_tier("max").unwrap(), WorkflowTier::Max);
        assert_eq!(parse_workflow_tier("high").unwrap(), WorkflowTier::High);
        assert_eq!(parse_workflow_tier("medium").unwrap(), WorkflowTier::Medium);
        assert_eq!(parse_workflow_tier("low").unwrap(), WorkflowTier::Low);
        assert!(parse_workflow_tier("ultra").is_err());
    }

    /// The grammar note in `05-phase-plan.md` W5 exists precisely so a
    /// workflow literally named `list` (or `show`/`start`/etc.) stays
    /// reachable: `run` is a pure namespace and `start`/`list` are verbs, so
    /// the workflow name always lands in the argument position, never in the
    /// subcommand position.
    #[test]
    fn workflow_named_list_is_reachable_through_run_start_and_run_list() {
        let (params, _json) = parse_workflow_run_start_args(&args(&["list"])).unwrap();
        assert_eq!(params.workflow_id, "list");

        let (params, _json) = parse_workflow_run_list_args(&args(&["list"])).unwrap();
        assert_eq!(params.workflow_id, Some("list".to_string()));

        let (target, _json) = parse_workflow_show_args(&args(&["list"])).unwrap();
        assert_eq!(target.workflow_id, "list");
    }

    #[test]
    fn every_verb_path_is_dispatched_without_reaching_the_network() {
        // Each top-level/namespace dispatcher recognizes every verb listed in
        // `VERB_PATHS` as a known subcommand rather than falling through to
        // "unknown subcommand" (exit 2 with no args always reaches that path
        // without a network call, since positional/flag validation happens
        // before `super::send_request`).
        for path in VERB_PATHS {
            match path {
                [verb] => assert!(
                    matches!(*verb, "list" | "show" | "create" | "update"),
                    "unexpected top-level verb {verb}"
                ),
                [group, verb] => assert!(
                    matches!(*group, "run" | "node" | "summary" | "review"),
                    "unexpected verb group {group} for verb {verb}"
                ),
                other => panic!("unexpected verb path shape: {other:?}"),
            }
        }
    }

    // ── §2.19 undeclared --arg ───────────────────────────────────────────

    /// §2.19: `run start demo --arg goal=x --arg bogus=y` used to succeed
    /// with `bogus` silently dropped. Fails before the fix (there was no
    /// message-building path at all — every `--arg` reached the wire
    /// unchecked); now the unknown key is named alongside every key that
    /// *was* declared.
    #[test]
    fn unknown_arg_message_names_the_unknown_key_and_the_declared_list() {
        let supplied = HashMap::from([
            ("goal".to_string(), "x".to_string()),
            ("bogus".to_string(), "y".to_string()),
        ]);
        let declared = HashSet::from(["goal".to_string(), "tier_hint".to_string()]);
        let message = unknown_arg_message("ship-feature", &supplied, &declared)
            .expect("an undeclared key should produce a message");
        assert!(message.contains("bogus"), "message: {message}");
        assert!(
            !message.contains("unknown --arg key(s): goal"),
            "goal is declared and must not be listed as unknown: {message}"
        );
        assert!(
            message.contains("goal"),
            "declared list should still name goal: {message}"
        );
        assert!(
            message.contains("tier_hint"),
            "declared list should name every declared arg: {message}"
        );
        assert!(message.contains("ship-feature"), "message: {message}");
    }

    #[test]
    fn unknown_arg_message_is_none_when_every_supplied_key_is_declared() {
        let supplied = HashMap::from([("goal".to_string(), "x".to_string())]);
        let declared = HashSet::from(["goal".to_string()]);
        assert_eq!(
            unknown_arg_message("ship-feature", &supplied, &declared),
            None
        );
    }

    #[test]
    fn unknown_arg_message_names_no_declared_args_when_there_are_none() {
        let supplied = HashMap::from([("goal".to_string(), "x".to_string())]);
        let declared = HashSet::new();
        let message = unknown_arg_message("ship-feature", &supplied, &declared).unwrap();
        assert!(message.contains("(none declared)"), "message: {message}");
    }

    // ── §2.10 run/node show naming the responsible node ─────────────────

    /// Fails before the fix: `blocking_run_nodes` did not exist, and
    /// `print_workflow_run_response` never looked past `run_id`/`status`/
    /// `tier`/`nodes_done`/`nodes_total`.
    #[test]
    fn blocking_run_nodes_finds_needs_attention_blocked_and_failed_only() {
        let nodes = serde_json::json!([
            { "path": "plan", "status": "succeeded" },
            { "path": "lint", "status": "needs_attention", "blocker": { "reason": "no workspace to host the node's pane", "resume_when": "a workspace is available" } },
            { "path": "gate", "status": "pending" },
            { "path": "deploy", "status": "failed" },
        ]);
        let nodes = nodes.as_array().unwrap();
        let blocking = blocking_run_nodes(nodes);
        let paths: Vec<&str> = blocking.iter().map(|node| node.path).collect();
        assert_eq!(paths, vec!["lint", "deploy"]);

        let lint = &blocking[0];
        assert_eq!(lint.status, "needs_attention");
        assert_eq!(lint.reason, Some("no workspace to host the node's pane"));
        assert_eq!(lint.resume_when, Some("a workspace is available"));

        // §2.11: blocker is not populated for every failure mode — the
        // `failed` node here carries none, and that must not panic or
        // fabricate a reason.
        let deploy = &blocking[1];
        assert_eq!(deploy.reason, None);
        assert_eq!(deploy.resume_when, None);
    }

    #[test]
    fn blocking_run_nodes_is_empty_when_nothing_is_stuck() {
        let nodes = serde_json::json!([
            { "path": "plan", "status": "succeeded" },
            { "path": "implement", "status": "running" },
        ]);
        assert!(blocking_run_nodes(nodes.as_array().unwrap()).is_empty());
    }

    // ── §2.19 update unchanged detection ─────────────────────────────────

    /// Fails before the fix: `update_was_deduplicated` did not exist, and
    /// `workflow_update`'s human output always described the response as a
    /// new version regardless of whether the store deduplicated it.
    #[test]
    fn update_was_deduplicated_matches_when_the_new_version_equals_the_previous_head() {
        assert!(update_was_deduplicated(Some(3), Some(3)));
        assert!(!update_was_deduplicated(Some(3), Some(4)));
        assert!(!update_was_deduplicated(None, Some(3)));
        assert!(!update_was_deduplicated(Some(3), None));
    }

    // ── §2.16 error-message quality ──────────────────────────────────────

    /// §2.16.2: fails before the fix — `humanize_workflow_error_message` did
    /// not exist, so the DB internals in the store's uniqueness-violation
    /// message reached the terminal verbatim.
    #[test]
    fn duplicate_workflow_name_error_is_reworded_without_db_internals() {
        let message = "workflow store query failed: Database index `workflow_name` already contains 'demo', with record `workflow:6v4g4nctlshyixd756r8`";
        let humanized = humanize_workflow_error_message("workflow_store_error", message);
        assert!(humanized.contains("\"demo\""), "humanized: {humanized}");
        assert!(
            humanized.contains("already exists"),
            "humanized: {humanized}"
        );
        assert!(
            !humanized.contains("Database index"),
            "DB internals must not survive: {humanized}"
        );
        assert!(
            !humanized.contains("workflow:6v4g4nctlshyixd756r8"),
            "the raw record id must not survive: {humanized}"
        );
    }

    /// An unrelated `workflow_store_error` (or any other code) must pass
    /// through unchanged rather than being guessed at.
    #[test]
    fn unrelated_store_errors_are_left_alone() {
        let message = "workflow store query failed: connection reset";
        assert_eq!(
            humanize_workflow_error_message("workflow_store_error", message),
            message
        );
        let kvdag_message = "invalid kvdag: graph is cyclic through: a, b";
        assert_eq!(
            humanize_workflow_error_message("invalid_kvdag", kvdag_message),
            kvdag_message
        );
    }

    #[test]
    fn duplicate_workflow_name_extracts_the_offending_name() {
        let message = "Database index `workflow_name` already contains 'ship-feature', with record `workflow:abc`";
        assert_eq!(duplicate_workflow_name(message), Some("ship-feature"));
        assert_eq!(duplicate_workflow_name("totally unrelated message"), None);
    }

    // ── §2.16 sun_path/local-error rendering ─────────────────────────────

    /// §2.16.3: fails before the fix — there was no human/`--json` split at
    /// all, so a local `invalid_definition` error (a TOML parse failure with
    /// a caret diagram) always went through the JSON-only
    /// `print_workflow_cli_error`, escaping every newline.
    #[test]
    fn print_workflow_local_error_returns_a_nonzero_exit_code() {
        assert_eq!(
            print_workflow_local_error("invalid_definition", "line one\nline two", false),
            1
        );
        assert_eq!(
            print_workflow_local_error("invalid_definition", "line one\nline two", true),
            1
        );
    }

    /// The mutating verbs answer a refusal the same way the read verbs do.
    ///
    /// Fails before the fix: `run cancel`, `node steer`, `node interrupt`, and
    /// `node restart` called `runtime::print_method_response`, so the refusals
    /// a human is most likely to hit — `workflow_run_closed` naming the run's
    /// status and the remedy, `workflow_node_delivery_failed` naming the pane —
    /// arrived as a raw JSON envelope while `run show` and `node show` next to
    /// them printed prose. Scanned from source because the difference is which
    /// helper the leaf calls, and the leaf itself needs a live server.
    #[test]
    fn every_mutating_workflow_verb_renders_its_refusal_for_a_human() {
        let source = include_str!("workflow.rs");
        /// From the `fn` header to the closing brace of that item, so a match
        /// can never leak in from the next function.
        fn body_of<'a>(source: &'a str, header: &str) -> &'a str {
            let start = source
                .find(header)
                .unwrap_or_else(|| panic!("{header} still exists"));
            let rest = &source[start..];
            &rest[..rest.find("\n}\n").unwrap_or(rest.len())]
        }

        for leaf in [
            "fn workflow_run_cancel(",
            "fn workflow_node_steer(",
            "fn workflow_node_interrupt(",
            "fn workflow_node_restart(",
        ] {
            let body = body_of(source, leaf);
            assert!(
                body.contains("send_workflow_mutation"),
                "{leaf} must render its refusal for a human: {body}"
            );
            assert!(
                !body.contains("runtime::"),
                "{leaf} must not go back to the JSON-only path: {body}"
            );
        }
        // `send_workflow_mutation` is the thing that makes that true.
        assert!(
            body_of(source, "fn send_workflow_mutation(").contains("print_workflow_error"),
            "the helper is what routes a refusal through the human renderer"
        );
    }

    // ── §2.18 timestamp formatting ────────────────────────────────────────

    /// Fails before the fix: `format_unix_ms` did not exist, and
    /// `workflow show`'s version history printed the raw
    /// `created_at_unix_ms` integer.
    #[test]
    fn format_unix_ms_renders_known_epoch_instants() {
        assert_eq!(format_unix_ms(0), "1970-01-01 00:00:00 UTC");
        // 2024-01-01T00:00:00Z
        assert_eq!(format_unix_ms(1_704_067_200_000), "2024-01-01 00:00:00 UTC");
        // 2000-02-29T12:34:56Z (leap day, exercises the leap-year path)
        assert_eq!(format_unix_ms(951_827_696_000), "2000-02-29 12:34:56 UTC");
    }

    #[test]
    fn civil_from_days_matches_known_calendar_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 1_704_067_200 / 86_400 = 19_723, and format_unix_ms_renders_known_epoch_instants
        // independently confirms 1_704_067_200_000ms is 2024-01-01.
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    // ── §5 packet P12: `kvx workflow review` ────────────────────────────

    #[test]
    fn workflow_review_start_and_show_parse_run_id_and_json() {
        let (target, json) = parse_workflow_review_run_target_args(
            &args(&["run-1"]),
            "usage: kvx workflow review start <run_id> [--json]",
        )
        .unwrap();
        assert_eq!(target.run_id, "run-1");
        assert!(!json);

        let (target, json) = parse_workflow_review_run_target_args(
            &args(&["run-1", "--json"]),
            "usage: kvx workflow review start <run_id> [--json]",
        )
        .unwrap();
        assert_eq!(target.run_id, "run-1");
        assert!(json);

        assert!(parse_workflow_review_run_target_args(
            &args(&[]),
            "usage: kvx workflow review start <run_id> [--json]",
        )
        .is_err());
    }

    /// `--accept` is repeatable — the plan's own contract
    /// (`.local/prd/phase4-retarget-plan.md` §5 packet P12: "`apply --accept
    /// <node_key>...` repeatable").
    #[test]
    fn workflow_review_apply_accept_is_repeatable_and_deduplication_is_the_server_side() {
        let (params, json) = parse_workflow_review_apply_args(&args(&[
            "run-1", "--accept", "plan", "--accept", "lint",
        ]))
        .unwrap();
        assert_eq!(params.run_id, "run-1");
        assert_eq!(params.accept, vec!["plan".to_string(), "lint".to_string()]);
        assert!(!json);
    }

    #[test]
    fn workflow_review_apply_decline_all_sends_an_empty_accept() {
        let (params, _json) =
            parse_workflow_review_apply_args(&args(&["run-1", "--decline-all", "--json"])).unwrap();
        assert_eq!(params.run_id, "run-1");
        assert!(params.accept.is_empty());
    }

    /// The plan's own contract: "bare `apply` is an error because an
    /// irreversible version mint never defaults."
    #[test]
    fn workflow_review_apply_bare_is_refused() {
        let error = parse_workflow_review_apply_args(&args(&["run-1"])).unwrap_err();
        assert!(
            error.contains("--accept") && error.contains("--decline-all"),
            "error should name both ways to decide: {error}"
        );
    }

    /// "mutually exclusive" — the plan's own wording.
    #[test]
    fn workflow_review_apply_accept_and_decline_all_together_is_refused() {
        let error = parse_workflow_review_apply_args(&args(&[
            "run-1",
            "--accept",
            "plan",
            "--decline-all",
        ]))
        .unwrap_err();
        assert!(error.contains("not both"), "error: {error}");
    }

    #[test]
    fn workflow_review_answer_defaults_run_id_and_member_from_env() {
        let (params, json) = parse_workflow_review_answer_args(
            &args(&["--file", "/abs/answers/scout.json"]),
            Some("run-1".to_string()),
            Some("scout".to_string()),
        )
        .unwrap();
        assert_eq!(params.run_id, "run-1");
        assert_eq!(params.member, "scout");
        assert_eq!(
            params.answer_file.as_deref(),
            Some("/abs/answers/scout.json")
        );
        assert_eq!(params.answer, None);
        assert!(!json);
    }

    /// §5 packet P12 / amendment log: `answer` takes no positional (or flag)
    /// run id or member — only the env karvex itself exported names them.
    #[test]
    fn workflow_review_answer_has_no_run_or_member_override_flag() {
        let error = parse_workflow_review_answer_args(
            &args(&["--file", "x.json", "--run", "run-1"]),
            Some("run-1".to_string()),
            Some("scout".to_string()),
        )
        .unwrap_err();
        assert!(error.contains("unknown option"), "error: {error}");
    }

    #[test]
    fn workflow_review_answer_missing_run_id_env_is_refused() {
        let error = parse_workflow_review_answer_args(
            &args(&["--file", "x.json"]),
            None,
            Some("scout".into()),
        )
        .unwrap_err();
        assert!(error.contains(REVIEW_RUN_ID_ENV_VAR), "error: {error}");
    }

    #[test]
    fn workflow_review_answer_missing_member_env_is_refused() {
        let error = parse_workflow_review_answer_args(
            &args(&["--file", "x.json"]),
            Some("run-1".into()),
            None,
        )
        .unwrap_err();
        assert!(error.contains(REVIEW_MEMBER_ENV_VAR), "error: {error}");
    }

    #[test]
    fn workflow_review_answer_requires_a_file() {
        let error = parse_workflow_review_answer_args(
            &args(&[]),
            Some("run-1".into()),
            Some("scout".into()),
        )
        .unwrap_err();
        assert!(error.contains("--file"), "error: {error}");
    }

    #[test]
    fn workflow_review_report_defaults_run_id_from_env_and_carries_no_member() {
        let (params, _json) = parse_workflow_review_report_args(
            &args(&["--file", "/abs/findings.json"]),
            Some("run-1".to_string()),
        )
        .unwrap();
        assert_eq!(params.run_id, "run-1");
        assert_eq!(params.findings_file.as_deref(), Some("/abs/findings.json"));
        assert_eq!(params.findings, None);
    }

    #[test]
    fn workflow_review_report_missing_run_id_env_is_refused() {
        let error =
            parse_workflow_review_report_args(&args(&["--file", "x.json"]), None).unwrap_err();
        assert!(error.contains(REVIEW_RUN_ID_ENV_VAR), "error: {error}");
    }

    /// P11's own doc: its own code rather than `workflow_invalid_definition`,
    /// because nothing was authored and the client's next move is "accept a
    /// smaller set" — the CLI must never rename or reword that code away.
    #[test]
    fn workflow_review_compile_failed_code_surfaces_verbatim() {
        let response = serde_json::json!({
            "id": "cli:workflow:review:apply",
            "error": {
                "code": "workflow_review_compile_failed",
                "message": "finding \"plan\" has no replacement, required for verdict replace"
            }
        });
        let rendered =
            format_workflow_error(&response).expect("an error envelope renders a message");
        assert!(
            rendered.contains("workflow_review_compile_failed"),
            "rendered: {rendered}"
        );
        assert!(
            !rendered.contains("invalid_definition"),
            "must not be relabelled as the authoring-error code: {rendered}"
        );
    }

    /// The five review verbs are real, dispatched, verb paths — same shape as
    /// `VERB_PATHS`'s own coverage test for `run`/`node`/`summary`.
    #[test]
    fn review_verb_paths_are_all_present() {
        for verb in ["start", "show", "apply", "answer", "report"] {
            assert!(
                VERB_PATHS.contains(&["review", verb].as_slice()),
                "review {verb} is missing from VERB_PATHS"
            );
        }
    }
}
