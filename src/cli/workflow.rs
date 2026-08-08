//! `kvx workflow` — manual arg parsing over the `workflow.*` socket API
//! (`docs/design/workflow-builder/05-phase-plan.md` W5). Parsing is split
//! into pure `parse_workflow_*_args` helpers (no I/O, unit-tested directly)
//! and thin leaf functions that perform the file/env reads and the network
//! call, matching the convention already used by `src/cli/pane.rs`.

use std::collections::{HashMap, HashSet};

use crate::api::schema::{
    Method, Request, WorkflowCreateParams, WorkflowDefinitionDocument, WorkflowDefinitionFormat,
    WorkflowNodeExpandParams, WorkflowNodeReportParams, WorkflowNodeSteerParams,
    WorkflowNodeTarget, WorkflowRunListParams, WorkflowRunParams, WorkflowRunTarget,
    WorkflowTarget, WorkflowTier, WorkflowVersionCreateParams, WorkflowVersionTarget,
};

/// Env karvex injects into a node's pane
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §4.2), read by
/// `kvx workflow node complete` so the self-report contract needs no
/// positional arguments.
const NODE_ENV_RUN_ID: &str = "KARVEX_WORKFLOW_RUN_ID";
const NODE_ENV_NODE_PATH: &str = "KARVEX_WORKFLOW_NODE_PATH";
const NODE_ENV_NODE_DIR: &str = "KARVEX_WORKFLOW_NODE_DIR";
const NODE_ENV_NODE_TOKEN: &str = "KARVEX_WORKFLOW_NODE_TOKEN";

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
    &["node", "show"],
    &["node", "steer"],
    &["node", "interrupt"],
    &["node", "restart"],
    &["node", "complete"],
    &["node", "expand"],
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

// ── workflow list / show ────────────────────────────────────────────────

fn workflow_list(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_list_args(args) {
        Ok(()) => super::runtime::workflow_list(),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_list_args(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: kvx workflow list".into());
    }
    Ok(())
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
    let usage =
        "usage: kvx workflow run start <name|id> [--tier <tier>] [--arg KEY=VALUE]... [--json]";
    let Some(target) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = target.clone();

    let mut tier = None;
    let mut run_args = HashMap::new();
    let mut json = false;
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
            "--json" => {
                json = true;
                index += 1;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }

    Ok((
        WorkflowRunParams {
            workflow_id,
            version: None,
            tier,
            args: run_args,
        },
        json,
    ))
}

fn workflow_run_list(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_list_args(args) {
        Ok(params) => super::runtime::workflow_run_list(params),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_run_list_args(args: &[String]) -> Result<WorkflowRunListParams, String> {
    let usage = "usage: kvx workflow run list <name|id> [--limit N]";
    let Some(target) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = target.clone();

    let mut limit = None;
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
            other => return Err(format!("unknown option: {other}")),
        }
    }

    Ok(WorkflowRunListParams { workflow_id, limit })
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
fn send_workflow_mutation(id: &'static str, method: Method) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: id.into(),
        method,
    })?;
    if let Some(code) = print_workflow_error(&response) {
        return Ok(code);
    }
    super::print_response(&response)
}

fn workflow_run_cancel(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_cancel_args(args) {
        Ok(target) => send_workflow_mutation(
            "cli:workflow:run:cancel",
            Method::WorkflowRunCancel(WorkflowRunTarget {
                run_id: target.run_id,
            }),
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_run_cancel_args(args: &[String]) -> Result<WorkflowRunTarget, String> {
    match args {
        [run_id] => Ok(WorkflowRunTarget {
            run_id: run_id.clone(),
        }),
        _ => Err("usage: kvx workflow run cancel <run_id>".into()),
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
        Ok(params) => {
            send_workflow_mutation("cli:workflow:node:steer", Method::WorkflowNodeSteer(params))
        }
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_node_steer_args(args: &[String]) -> Result<WorkflowNodeSteerParams, String> {
    let usage = "usage: kvx workflow node steer <run_id> <path> <text>";
    if args.len() < 3 {
        return Err(usage.into());
    }
    Ok(WorkflowNodeSteerParams {
        run_id: args[0].clone(),
        path: args[1].clone(),
        text: args[2..].join(" "),
    })
}

fn workflow_node_interrupt(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_pair_args(args, "usage: kvx workflow node interrupt <run_id> <path>")
    {
        Ok(target) => send_workflow_mutation(
            "cli:workflow:node:interrupt",
            Method::WorkflowNodeInterrupt(target),
        ),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn workflow_node_restart(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_pair_args(args, "usage: kvx workflow node restart <run_id> <path>") {
        Ok(target) => send_workflow_mutation(
            "cli:workflow:node:restart",
            Method::WorkflowNodeRestart(target),
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
) -> Result<WorkflowNodeTarget, String> {
    match args {
        [run_id, path] => Ok(WorkflowNodeTarget {
            run_id: run_id.clone(),
            path: path.clone(),
        }),
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
    println!(
        "nodes:   {}/{}",
        run["nodes_done"].as_u64().unwrap_or(0),
        run["nodes_total"].as_u64().unwrap_or(0)
    );
    if let Some(line) = format_run_growth_line(run) {
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

    if let Some(nodes) = nodes {
        if !nodes.is_empty() {
            println!();
            println!("nodes:");
            for node in nodes {
                println!("{}", format_run_node_line(node));
            }
        }
    }

    Ok(0)
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
/// whatever `blocker` detail the wire carried for it. `blocker` is not
/// populated for every failure mode yet (a spawn failure currently leaves it
/// `null` — see the notes returned alongside this task), so `reason`/
/// `resume_when` are best-effort, not guaranteed.
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

/// `HH:MM` for a growth-limit timestamp — the plan's own example
/// ("... reached at 14:22") is clock time, not a full date, so this is
/// deliberately narrower than [`format_unix_ms`].
fn format_unix_ms_clock(ms: u64) -> String {
    let secs_of_day = (ms / 1000) % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    format!("{hour:02}:{minute:02}")
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
    println!("model:   {}", node["model"].as_str().unwrap_or(""));
    println!("effort:  {}", node["effort"].as_str().unwrap_or(""));
    if let Some(pane_id) = node["pane_id"].as_str() {
        println!("pane_id: {pane_id}");
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
    Ok(0)
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
    eprintln!("  kvx workflow list");
    eprintln!("  kvx workflow show <name|id> [--json]");
    eprintln!("  kvx workflow create --file <definition.toml|json> [--name <name>] [--json]");
    eprintln!(
        "  kvx workflow update <name|id> --file <definition.toml|json> [--change-summary <text>] [--json]"
    );
    eprintln!("  kvx workflow run <subcommand> ...");
    eprintln!("  kvx workflow node <subcommand> ...");
}

fn print_workflow_run_help() {
    eprintln!("kvx workflow run commands:");
    eprintln!("  kvx workflow run start <name|id> [--tier <tier>] [--arg KEY=VALUE]... [--json]");
    eprintln!("  kvx workflow run list <name|id> [--limit N]");
    eprintln!("  kvx workflow run show <run_id> [--json]");
    eprintln!("  kvx workflow run cancel <run_id>");
}

fn print_workflow_node_help() {
    eprintln!("kvx workflow node commands:");
    eprintln!("  kvx workflow node show <run_id> <path> [--json]");
    eprintln!("  kvx workflow node steer <run_id> <path> <text>");
    eprintln!("  kvx workflow node interrupt <run_id> <path>");
    eprintln!("  kvx workflow node restart <run_id> <path>");
    eprintln!(
        "  kvx workflow node complete [--result-file <path>]   # run by the node itself; reads KARVEX_WORKFLOW_RUN_ID/NODE_PATH/NODE_DIR/NODE_TOKEN"
    );
    eprintln!(
        "  kvx workflow node expand <run_id> <path> --template <key> --label <text> [--input KEY=VALUE]... [--count N] [--json]   # run by the node itself; reads KARVEX_WORKFLOW_NODE_TOKEN"
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

    #[test]
    fn workflow_run_list_builds_workflow_run_list_with_limit() {
        let params =
            parse_workflow_run_list_args(&args(&["ship-feature", "--limit", "10"])).unwrap();
        assert_eq!(
            Method::WorkflowRunList(params),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: "ship-feature".to_string(),
                limit: Some(10),
            })
        );
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
        let target = parse_workflow_run_cancel_args(&args(&["run-1"])).unwrap();
        assert_eq!(
            Method::WorkflowRunCancel(target),
            Method::WorkflowRunCancel(WorkflowRunTarget {
                run_id: "run-1".to_string(),
            })
        );
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
        let params =
            parse_workflow_node_steer_args(&args(&["run-1", "plan", "please", "hurry"])).unwrap();
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

    #[test]
    fn workflow_node_interrupt_builds_workflow_node_interrupt() {
        let target = parse_workflow_node_pair_args(&args(&["run-1", "plan"]), "usage").unwrap();
        assert_eq!(
            Method::WorkflowNodeInterrupt(target),
            Method::WorkflowNodeInterrupt(WorkflowNodeTarget {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
            })
        );
    }

    #[test]
    fn workflow_node_restart_builds_workflow_node_restart() {
        let target = parse_workflow_node_pair_args(&args(&["run-1", "plan"]), "usage").unwrap();
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
    fn format_unix_ms_clock_renders_hour_and_minute() {
        // 1970-01-01T00:00:00Z
        assert_eq!(format_unix_ms_clock(0), "00:00");
        // 2024-01-01T14:22:00Z
        assert_eq!(format_unix_ms_clock(1_704_118_920_000), "14:22");
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

        let params = parse_workflow_run_list_args(&args(&["list"])).unwrap();
        assert_eq!(params.workflow_id, "list");

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
                    matches!(*group, "run" | "node"),
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
}
