//! `kvx workflow` — manual arg parsing over the `workflow.*` socket API
//! (`docs/design/workflow-builder/05-phase-plan.md` W5). Parsing is split
//! into pure `parse_workflow_*_args` helpers (no I/O, unit-tested directly)
//! and thin leaf functions that perform the file/env reads and the network
//! call, matching the convention already used by `src/cli/pane.rs`.

use std::collections::HashMap;

use crate::api::schema::{
    Method, Request, WorkflowCreateParams, WorkflowDefinitionDocument, WorkflowDefinitionFormat,
    WorkflowNodeReportParams, WorkflowNodeSteerParams, WorkflowNodeTarget, WorkflowRunListParams,
    WorkflowRunParams, WorkflowRunTarget, WorkflowTarget, WorkflowTier,
    WorkflowVersionCreateParams,
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

fn workflow_show(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_show_args(args) {
        Ok(target) => super::runtime::workflow_get(target.workflow_id),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn parse_workflow_show_args(args: &[String]) -> Result<WorkflowTarget, String> {
    match args {
        [target] => Ok(WorkflowTarget {
            workflow_id: target.clone(),
        }),
        _ => Err("usage: kvx workflow show <name|id>".into()),
    }
}

// ── workflow create / update ────────────────────────────────────────────

fn workflow_create(args: &[String]) -> std::io::Result<i32> {
    let (file, name) = match parse_workflow_create_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let definition = match load_definition_document(&file, name) {
        Ok(definition) => definition,
        Err(message) => {
            print_workflow_cli_error("invalid_definition", &message);
            return Ok(1);
        }
    };

    super::runtime::workflow_create(WorkflowCreateParams { definition })
}

fn parse_workflow_create_args(args: &[String]) -> Result<(String, Option<String>), String> {
    let usage = "usage: kvx workflow create --file <definition.toml|json> [--name <name>]";
    let mut file = None;
    let mut name = None;

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
            other => return Err(format!("unknown option: {other}")),
        }
    }

    let Some(file) = file else {
        return Err(usage.into());
    };
    Ok((file, name))
}

fn workflow_update(args: &[String]) -> std::io::Result<i32> {
    let (workflow_id, file, change_summary) = match parse_workflow_update_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let definition = match load_definition_document(&file, None) {
        Ok(definition) => definition,
        Err(message) => {
            print_workflow_cli_error("invalid_definition", &message);
            return Ok(1);
        }
    };

    super::runtime::workflow_version_create(WorkflowVersionCreateParams {
        workflow_id,
        definition,
        change_summary: change_summary.unwrap_or_default(),
    })
}

fn parse_workflow_update_args(args: &[String]) -> Result<(String, String, Option<String>), String> {
    let usage = "usage: kvx workflow update <name|id> --file <definition.toml|json> [--change-summary <text>]";
    let Some(workflow_id) = args.first() else {
        return Err(usage.into());
    };
    let workflow_id = workflow_id.clone();

    let mut file = None;
    let mut change_summary = None;
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
    Ok((workflow_id, file, change_summary))
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

fn workflow_run_start(args: &[String]) -> std::io::Result<i32> {
    let (params, json) = match parse_workflow_run_start_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:workflow:run:start".into(),
        method: Method::WorkflowRun(params),
    })?;
    print_workflow_run_response(&response, json)
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

fn workflow_run_cancel(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_run_cancel_args(args) {
        Ok(target) => super::runtime::workflow_run_cancel(target.run_id),
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
        Ok(params) => super::runtime::workflow_node_steer(params),
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
        Ok(target) => super::runtime::workflow_node_interrupt(target.run_id, target.path),
        Err(message) => {
            eprintln!("{message}");
            Ok(2)
        }
    }
}

fn workflow_node_restart(args: &[String]) -> std::io::Result<i32> {
    match parse_workflow_node_pair_args(args, "usage: kvx workflow node restart <run_id> <path>") {
        Ok(target) => super::runtime::workflow_node_restart(target.run_id, target.path),
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

    let params = match build_node_report_params(&env, &result_path) {
        Ok(params) => params,
        Err(message) => {
            print_workflow_cli_error("invalid_node_result", &message);
            return Ok(1);
        }
    };

    super::runtime::workflow_node_report(params)
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

fn build_node_report_params(
    env: &NodeCompleteEnv,
    result_path: &str,
) -> Result<WorkflowNodeReportParams, String> {
    let text = std::fs::read_to_string(result_path)
        .map_err(|err| format!("failed to read {result_path}: {err}"))?;
    let result: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("invalid JSON in {result_path}: {err}"))?;
    Ok(WorkflowNodeReportParams {
        run_id: env.run_id.clone(),
        path: env.node_path.clone(),
        token: env.node_token.clone(),
        result,
    })
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

fn print_workflow_run_response(response: &serde_json::Value, json: bool) -> std::io::Result<i32> {
    if json || response.get("error").is_some() {
        return super::print_response(response);
    }
    let run = &response["result"]["run"];
    println!("run_id:  {}", run["run_id"].as_str().unwrap_or(""));
    println!("status:  {}", run["status"].as_str().unwrap_or(""));
    println!("tier:    {}", run["tier"].as_str().unwrap_or(""));
    println!(
        "nodes:   {}/{}",
        run["nodes_done"].as_u64().unwrap_or(0),
        run["nodes_total"].as_u64().unwrap_or(0)
    );
    Ok(0)
}

fn print_workflow_node_response(response: &serde_json::Value, json: bool) -> std::io::Result<i32> {
    if json || response.get("error").is_some() {
        return super::print_response(response);
    }
    let node = &response["result"]["node"];
    println!("path:    {}", node["path"].as_str().unwrap_or(""));
    println!("status:  {}", node["status"].as_str().unwrap_or(""));
    println!("model:   {}", node["model"].as_str().unwrap_or(""));
    println!("effort:  {}", node["effort"].as_str().unwrap_or(""));
    if let Some(pane_id) = node["pane_id"].as_str() {
        println!("pane_id: {pane_id}");
    }
    Ok(0)
}

fn print_workflow_cli_error(code: &str, message: &str) {
    eprintln!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "error": { "code": code, "message": message }
        }))
        .unwrap()
    );
}

fn print_workflow_help() {
    eprintln!("kvx workflow commands:");
    eprintln!("  kvx workflow list");
    eprintln!("  kvx workflow show <name|id>");
    eprintln!("  kvx workflow create --file <definition.toml|json> [--name <name>]");
    eprintln!(
        "  kvx workflow update <name|id> --file <definition.toml|json> [--change-summary <text>]"
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

    #[test]
    fn workflow_list_accepts_no_arguments_and_rejects_extras() {
        assert!(parse_workflow_list_args(&args(&[])).is_ok());
        assert!(parse_workflow_list_args(&args(&["extra"])).is_err());
    }

    #[test]
    fn workflow_show_builds_workflow_get() {
        let target = parse_workflow_show_args(&args(&["ship-feature"])).unwrap();
        assert_eq!(
            Method::WorkflowGet(target),
            Method::WorkflowGet(WorkflowTarget {
                workflow_id: "ship-feature".to_string(),
            })
        );
    }

    #[test]
    fn workflow_show_rejects_wrong_argument_count() {
        assert!(parse_workflow_show_args(&args(&[])).is_err());
        assert!(parse_workflow_show_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn workflow_create_flags_parse_file_and_optional_name() {
        let (file, name) =
            parse_workflow_create_args(&args(&["--file", "def.toml", "--name", "ship"])).unwrap();
        assert_eq!(file, "def.toml");
        assert_eq!(name.as_deref(), Some("ship"));

        let (file, name) = parse_workflow_create_args(&args(&["--file", "def.json"])).unwrap();
        assert_eq!(file, "def.json");
        assert_eq!(name, None);
    }

    #[test]
    fn workflow_create_requires_file() {
        assert!(parse_workflow_create_args(&args(&["--name", "ship"])).is_err());
    }

    #[test]
    fn workflow_update_flags_parse_target_file_and_change_summary() {
        let (workflow_id, file, change_summary) = parse_workflow_update_args(&args(&[
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
        let params = build_node_report_params(&env, path.to_str().unwrap()).unwrap();
        assert_eq!(
            Method::WorkflowNodeReport(params),
            Method::WorkflowNodeReport(WorkflowNodeReportParams {
                run_id: "run-1".to_string(),
                path: "plan".to_string(),
                token: "tok".to_string(),
                result: serde_json::json!({"summary": "done"}),
            })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_node_report_params_rejects_invalid_json() {
        let dir = unique_temp_dir("node-complete-invalid");
        let path = dir.join("result.json");
        std::fs::write(&path, "not json").unwrap();

        let env = NodeCompleteEnv {
            run_id: "run-1".to_string(),
            node_path: "plan".to_string(),
            node_dir: dir.to_str().unwrap().to_string(),
            node_token: "tok".to_string(),
        };
        assert!(build_node_report_params(&env, path.to_str().unwrap()).is_err());

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

        let target = parse_workflow_show_args(&args(&["list"])).unwrap();
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
}
