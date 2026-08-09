use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{
    AgentStatus, ClientWindowTitleSetParams, EmptyParams, Method, PaneAgentState, ReadFormat,
    ReadSource, Request, SplitDirection,
};

mod agent;
mod api;
mod completion;
mod integration;
mod notification;
mod pane;
mod plugin;
mod protocol_guard;
mod runtime;
mod server;
mod server_not_running;
mod spec;
mod status;
mod tab;
mod workflow;
mod workspace;
mod worktree;

const TERMINAL_SESSION_OBSERVE_USAGE: &str =
    "usage: kvx terminal session observe <target> [--cols N] [--rows N]";
const TERMINAL_SESSION_CONTROL_USAGE: &str =
    "usage: kvx terminal session control <target> [--takeover] [--cols N] [--rows N]";

pub(crate) fn parse_token_assignment(raw: &str) -> Result<(String, Option<String>), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err("token must use NAME=VALUE".into());
    };
    if key.is_empty() {
        return Err("token name must not be empty".into());
    }
    Ok((key.to_string(), Some(value.to_string())))
}

pub(crate) fn parse_env_assignment(raw: &str) -> Result<(String, String), String> {
    let Some((key, value)) = raw.split_once('=') else {
        return Err("env must use KEY=VALUE".into());
    };
    if key.is_empty() {
        return Err("env key must not be empty".into());
    }
    if key.contains('\0') || value.contains('\0') {
        return Err("env must not contain NUL bytes".into());
    }
    Ok((key.to_string(), value.to_string()))
}

pub enum CommandOutcome {
    Handled(i32),
    NotCli,
}

pub(super) fn print_read_response(response: &serde_json::Value) -> std::io::Result<i32> {
    if response.get("error").is_some() {
        eprintln!("{response}");
        return Ok(1);
    }
    if let Some(text) = response["result"]["read"]["text"].as_str() {
        print!("{text}");
    }
    Ok(0)
}

/// Whether this invocation is a print-and-exit CLI that should die quietly on
/// `SIGPIPE` — `kvx workflow run show <id> | head` — instead of panicking with
/// Rust's broken-pipe backtrace.
///
/// Deliberately an **allowlist**, not "everything except `kvx server`". Two
/// families must keep the Rust runtime's `SIG_IGN`, so that a write to a gone
/// reader comes back as an `io::Error` the caller can handle:
///
/// - Anything `maybe_run` does not recognise falls through to `main`, which
///   runs the TUI, the client runloop (`kvx client`), the remote bridge, or the
///   updater. Those own a terminal and a PTY/socket; dying mid-write would
///   leave the user's terminal in raw mode. `kvx server` is the daemon.
/// - The interactive attaches, which live inside otherwise print-and-exit
///   namespaces (`kvx agent attach`, `kvx terminal attach`,
///   `kvx terminal session …`) and take the terminal over through
///   `crate::client`.
fn restores_default_sigpipe(command: &str, subcommand: Option<&str>) -> bool {
    const PRINT_AND_EXIT: &[&str] = &[
        "api",
        "status",
        "completion",
        "completions",
        "config",
        "channel",
        "workspace",
        "worktree",
        "tab",
        "notification",
        "agent",
        "terminal",
        "pane",
        "plugin",
        "integration",
        "session",
        "workflow",
    ];
    // The top-level print-and-exit flags are handled by `main` after
    // `maybe_run` declines, and only when they are the *whole* invocation:
    // `kvx client --help` runs the client, so a flag anywhere in the arguments
    // proves nothing.
    const PRINT_AND_EXIT_FLAGS: &[&str] = &[
        "--help",
        "-h",
        "--version",
        "-V",
        "--default-config",
        "--skill",
    ];
    if subcommand.is_none() && PRINT_AND_EXIT_FLAGS.contains(&command) {
        return true;
    }
    if !PRINT_AND_EXIT.contains(&command) {
        return false;
    }
    !matches!(
        (command, subcommand),
        ("agent", Some("attach")) | ("terminal", Some("attach" | "session"))
    )
}

pub fn maybe_run(args: &[String]) -> std::io::Result<CommandOutcome> {
    let Some(command) = args.get(1).map(|arg| arg.as_str()) else {
        return Ok(CommandOutcome::NotCli);
    };

    // Before any printing happens (including `--help`), so a piped
    // `kvx <command> --help | head` dies quietly too, like the command's own
    // output does. Only the print-and-exit verbs — see
    // `restores_default_sigpipe`.
    if restores_default_sigpipe(command, args.get(2).map(|arg| arg.as_str())) {
        crate::platform::restore_default_sigpipe();
    }

    if spec::print_requested_help(args)? {
        return Ok(CommandOutcome::Handled(0));
    }

    let exit_code = match command {
        "server" => {
            let Some(exit_code) = server::run_server_command(&args[2..])? else {
                return Ok(CommandOutcome::NotCli);
            };
            exit_code
        }
        "api" => api::run_api_command(&args[2..])?,
        "status" => status::run_status_command(&args[2..])?,
        "completion" | "completions" => completion::run_completion_command(&args[2..])?,
        "config" => run_config_command(&args[2..])?,
        "channel" => run_channel_command(&args[2..])?,
        "workspace" => workspace::run_workspace_command(&args[2..])?,
        "worktree" => worktree::run_worktree_command(&args[2..])?,
        "tab" => tab::run_tab_command(&args[2..])?,
        "notification" => notification::run_notification_command(&args[2..])?,
        "agent" => agent::run_agent_command(&args[2..])?,
        "terminal" => run_terminal_command(&args[2..])?,
        "pane" => pane::run_pane_command(&args[2..])?,
        "plugin" => plugin::run_plugin_command(&args[2..])?,
        "integration" => integration::run_integration_command(&args[2..])?,
        "session" => run_session_command(&args[2..])?,
        "workflow" => workflow::run_workflow_command(&args[2..])?,
        _ => return Ok(CommandOutcome::NotCli),
    };

    Ok(CommandOutcome::Handled(exit_code))
}

fn run_channel_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("set") => channel_set(&args[1..]),
        Some("show") if args.len() == 1 => {
            let config = crate::config::Config::load().config;
            println!("{}", config.update.channel.as_str());
            Ok(0)
        }
        Some("help" | "--help" | "-h") => {
            print_channel_help();
            Ok(0)
        }
        _ => {
            print_channel_help();
            Ok(2)
        }
    }
}

fn channel_set(args: &[String]) -> std::io::Result<i32> {
    let Some(channel) = parse_channel_set_arg(args) else {
        eprintln!("usage: kvx channel set <stable|preview>");
        return Ok(2);
    };

    if let Some(reason) = channel_set_rejection(
        channel,
        crate::update::preview_channel_rejection_for_current_install(),
    ) {
        eprintln!("{reason}.");
        return Ok(1);
    }

    let path = crate::config::config_path();
    let content = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    if let Err(err) = content.parse::<toml::Value>() {
        eprintln!(
            "config file at {} is invalid TOML: {err}. Fix it before changing the update channel.",
            path.display()
        );
        return Ok(1);
    }

    let updated = crate::config::upsert_section_value(
        &content,
        "update",
        "channel",
        &format!("\"{channel}\""),
    );
    if let Err(err) = updated.parse::<toml::Value>() {
        eprintln!(
            "changing the update channel would make {} invalid TOML: {err}; leaving config unchanged",
            path.display()
        );
        return Ok(1);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, updated)?;
    println!(
        "Karvex update channel set to {channel} in {}.",
        path.display()
    );

    match channel_set_install_action(
        crate::update::package_manager_channel_update_guidance_for_current_install(),
    ) {
        ChannelSetInstallAction::PrintGuidance(guidance) => {
            println!("{guidance}");
            return Ok(0);
        }
        ChannelSetInstallAction::RunSelfUpdate => {}
    }

    if let Err(err) = crate::update::self_update(crate::update::SelfUpdateOptions::default()) {
        eprintln!("update failed: {err}");
        eprintln!("Run `kvx update` to retry.");
        return Ok(1);
    }

    Ok(0)
}

fn parse_channel_set_arg(args: &[String]) -> Option<&str> {
    let channel = args.first().map(|arg| arg.as_str())?;
    if args.len() == 1 && matches!(channel, "stable" | "preview") {
        Some(channel)
    } else {
        None
    }
}

fn channel_set_rejection(
    channel: &str,
    install_rejection: Option<&'static str>,
) -> Option<&'static str> {
    if cfg!(windows) && channel == "stable" {
        return Some(
            "stable channel is not available on Windows yet; Windows builds are preview-only",
        );
    }

    if channel == "preview" {
        return install_rejection;
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelSetInstallAction {
    RunSelfUpdate,
    PrintGuidance(&'static str),
}

fn channel_set_install_action(
    package_manager_guidance: Option<&'static str>,
) -> ChannelSetInstallAction {
    match package_manager_guidance {
        Some(guidance) => ChannelSetInstallAction::PrintGuidance(guidance),
        None => ChannelSetInstallAction::RunSelfUpdate,
    }
}

fn print_channel_help() {
    eprintln!("kvx channel commands:");
    eprintln!("  kvx channel show                  print the configured update channel");
    eprintln!("  kvx channel set <stable|preview>  choose the update channel");
}

fn run_config_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_config_help();
        return Ok(2);
    };

    match subcommand {
        "check" => config_check(&args[1..]),
        "reset-keys" => config_reset_keys(&args[1..]),
        "help" | "--help" | "-h" => {
            print_config_help();
            Ok(0)
        }
        _ => {
            print_config_help();
            Ok(2)
        }
    }
}

fn config_check(args: &[String]) -> std::io::Result<i32> {
    match args {
        [] => {}
        [flag] if matches!(flag.as_str(), "help" | "--help" | "-h") => {
            eprintln!("usage: kvx config check");
            return Ok(0);
        }
        _ => {
            eprintln!("usage: kvx config check");
            return Ok(2);
        }
    }

    let diagnostics = crate::config::Config::load().diagnostics;
    if diagnostics.is_empty() {
        println!("config: ok");
    } else {
        println!("config: issues found");
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
    }

    Ok(i32::from(!diagnostics.is_empty()))
}

fn config_reset_keys(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: kvx config reset-keys");
        return Ok(2);
    }

    let path = crate::config::config_path();
    if !path.exists() {
        println!(
            "No config file found at {}. Built-in v2 keybindings already apply.",
            path.display()
        );
        return Ok(0);
    }

    let content = std::fs::read_to_string(&path)?;
    let parsed = match content.parse::<toml::Value>() {
        Ok(value) => value,
        Err(err) => {
            eprintln!(
                "config file at {} is invalid TOML: {err}. Fix it manually or move it aside to use defaults.",
                path.display()
            );
            return Ok(1);
        }
    };
    let Some(table) = parsed.as_table() else {
        eprintln!(
            "config file at {} is invalid TOML: top-level config must be a table.",
            path.display()
        );
        return Ok(1);
    };

    if !table.contains_key("keys") {
        println!(
            "No [keys] config found in {}. Built-in v2 keybindings already apply.",
            path.display()
        );
        return Ok(0);
    }

    let (updated, removed) = crate::config::remove_keybinding_config_sections(&content);
    if !removed {
        eprintln!(
            "could not safely remove keybinding config from {} without rewriting comments; edit the file manually or remove the top-level keys setting.",
            path.display()
        );
        return Ok(1);
    }
    if let Err(err) = updated.parse::<toml::Value>() {
        eprintln!(
            "removing keybinding config would make {} invalid TOML: {err}; leaving config unchanged",
            path.display()
        );
        return Ok(1);
    }

    let backup_path = key_config_backup_path(&path);
    std::fs::copy(&path, &backup_path)?;
    std::fs::write(&path, updated)?;

    println!("Created backup: {}", backup_path.display());
    println!(
        "Removed [keys], [keys.indexed], and [[keys.command]] from {}.",
        path.display()
    );
    println!("Built-in v2 keybindings will apply after Karvex restarts or reloads config.");
    println!("If a Karvex server is running, run `kvx server reload-config` to apply this now.");
    println!(
        "To restore: cp {} {}",
        backup_path.display(),
        path.display()
    );
    Ok(0)
}

fn key_config_backup_path(path: &std::path::Path) -> std::path::PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!("{file_name}.bak-keybind-v2-{timestamp}"))
}

fn run_terminal_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_terminal_help();
        return Ok(2);
    };

    match subcommand {
        "attach" => terminal_attach(&args[1..]),
        "session" => terminal_session(&args[1..]),
        "title" => terminal_title(&args[1..]),
        "help" | "--help" | "-h" => {
            print_terminal_help();
            Ok(0)
        }
        _ => {
            print_terminal_help();
            Ok(2)
        }
    }
}

fn run_session_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_session_help();
        return Ok(2);
    };

    match subcommand {
        "list" => session_list(&args[1..]),
        "attach" => session_attach_help(&args[1..]),
        "stop" => session_stop(&args[1..]),
        "delete" => session_delete(&args[1..]),
        "help" | "--help" | "-h" => {
            print_session_help();
            Ok(0)
        }
        _ => {
            print_session_help();
            Ok(2)
        }
    }
}

fn session_attach_help(args: &[String]) -> std::io::Result<i32> {
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        eprintln!("usage: kvx session attach <name>");
        return Ok(0);
    }
    eprintln!("usage: kvx session attach <name>");
    Ok(2)
}

fn session_list(args: &[String]) -> std::io::Result<i32> {
    let json = match parse_session_json_only(args, "usage: kvx session list [--json]") {
        Ok(json) => json,
        Err(code) => return Ok(code),
    };

    let sessions = crate::session::list_sessions()?;
    if json {
        _print_json(&serde_json::json!({
            "sessions": sessions,
        }));
    } else {
        print_session_table(&sessions);
    }
    Ok(0)
}

fn session_stop(args: &[String]) -> std::io::Result<i32> {
    let (name, json) =
        match parse_session_name_and_json(args, "usage: kvx session stop <name> [--json]") {
            Ok(parsed) => parsed,
            Err(code) => return Ok(code),
        };

    let target = match crate::session::parse_target_name(&name) {
        Ok(target) => target,
        Err(message) => {
            print_session_error("invalid_session_name", &message);
            return Ok(1);
        }
    };
    match crate::session::stop_session(target.as_deref()) {
        Ok(session) => {
            if json {
                _print_json(&serde_json::json!({
                    "stopped": true,
                    "session": session,
                }));
            } else {
                println!("stopped session {}", session.name);
            }
            Ok(0)
        }
        Err(message) => {
            print_session_error("session_stop_failed", &message);
            Ok(1)
        }
    }
}

fn session_delete(args: &[String]) -> std::io::Result<i32> {
    let (name, json) =
        match parse_session_name_and_json(args, "usage: kvx session delete <name> [--json]") {
            Ok(parsed) => parsed,
            Err(code) => return Ok(code),
        };

    match crate::session::delete_session(&name) {
        Ok(session) => {
            if json {
                _print_json(&serde_json::json!({
                    "deleted": true,
                    "session": session,
                }));
            } else {
                println!("deleted session {}", session.name);
            }
            Ok(0)
        }
        Err(message) => {
            print_session_error("session_delete_failed", &message);
            Ok(1)
        }
    }
}

fn terminal_attach(args: &[String]) -> std::io::Result<i32> {
    let (terminal_id, takeover) = match parse_attach_target(
        args,
        "usage: kvx terminal attach <terminal_id> [--takeover]",
    ) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };
    crate::client::run_terminal_attach(terminal_id, takeover)?;
    Ok(0)
}

fn terminal_session(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("control") => terminal_session_control(&args[1..]),
        Some("observe") => terminal_session_observe(&args[1..]),
        Some("help" | "--help" | "-h") => {
            eprintln!("{TERMINAL_SESSION_CONTROL_USAGE}");
            eprintln!("{TERMINAL_SESSION_OBSERVE_USAGE}");
            Ok(0)
        }
        _ => {
            eprintln!("{TERMINAL_SESSION_CONTROL_USAGE}");
            eprintln!("{TERMINAL_SESSION_OBSERVE_USAGE}");
            Ok(2)
        }
    }
}

fn terminal_session_control(args: &[String]) -> std::io::Result<i32> {
    let options = match parse_terminal_session_options(
        args,
        TERMINAL_SESSION_CONTROL_USAGE,
        "control",
        true,
    )? {
        Ok(options) => options,
        Err(code) => return Ok(code),
    };

    crate::client::run_terminal_session_control(
        options.target,
        options.takeover,
        options.cols,
        options.rows,
    )?;
    Ok(0)
}

fn terminal_session_observe(args: &[String]) -> std::io::Result<i32> {
    let options = match parse_terminal_session_options(
        args,
        TERMINAL_SESSION_OBSERVE_USAGE,
        "observe",
        false,
    )? {
        Ok(options) => options,
        Err(code) => return Ok(code),
    };

    crate::client::run_terminal_session_observe(options.target, options.cols, options.rows)?;
    Ok(0)
}

struct TerminalSessionOptions {
    target: String,
    cols: u16,
    rows: u16,
    takeover: bool,
}

fn parse_terminal_session_options(
    args: &[String],
    usage: &str,
    command: &str,
    allow_takeover: bool,
) -> std::io::Result<Result<TerminalSessionOptions, i32>> {
    if matches!(
        args.first().map(|arg| arg.as_str()),
        Some("help" | "--help" | "-h")
    ) {
        eprintln!("{usage}");
        return Ok(Err(0));
    }
    let Some(target) = args.first() else {
        eprintln!("{usage}");
        return Ok(Err(2));
    };

    let mut cols = 120;
    let mut rows = 40;
    let mut takeover = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--takeover" if allow_takeover => {
                takeover = true;
                i += 1;
            }
            "--cols" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("{usage}");
                    return Ok(Err(2));
                };
                cols = parse_terminal_dimension(value, "--cols")?;
                i += 2;
            }
            "--rows" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("{usage}");
                    return Ok(Err(2));
                };
                rows = parse_terminal_dimension(value, "--rows")?;
                i += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("{usage}");
                return Ok(Err(0));
            }
            other => {
                eprintln!("unknown terminal session {command} option: {other}");
                eprintln!("{usage}");
                return Ok(Err(2));
            }
        }
    }

    Ok(Ok(TerminalSessionOptions {
        target: target.clone(),
        cols,
        rows,
        takeover,
    }))
}

fn parse_terminal_dimension(raw: &str, flag: &str) -> std::io::Result<u16> {
    let parsed = raw.parse::<u16>().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{flag} must be an integer between 1 and {}", u16::MAX),
        )
    })?;
    if parsed == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{flag} must be greater than 0"),
        ));
    }
    Ok(parsed)
}

fn terminal_title(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("set") => {
            if args.len() != 2 {
                eprintln!("usage: kvx terminal title set <title>");
                return Ok(2);
            }
            print_response(&send_request(&Request {
                id: "cli:terminal:title:set".into(),
                method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams {
                    title: args[1].clone(),
                }),
            })?)
        }
        Some("clear") => {
            if args.len() != 1 {
                eprintln!("usage: kvx terminal title clear");
                return Ok(2);
            }
            print_response(&send_request(&Request {
                id: "cli:terminal:title:clear".into(),
                method: Method::ClientWindowTitleClear(EmptyParams::default()),
            })?)
        }
        Some("help" | "--help" | "-h") => {
            eprintln!("usage: kvx terminal title set <title>");
            eprintln!("       kvx terminal title clear");
            Ok(0)
        }
        _ => {
            eprintln!("usage: kvx terminal title set <title>");
            eprintln!("       kvx terminal title clear");
            Ok(2)
        }
    }
}

pub(super) fn parse_attach_target(args: &[String], usage: &str) -> Result<(String, bool), i32> {
    let Some(target) = args.first() else {
        eprintln!("{usage}");
        return Err(2);
    };
    let mut takeover = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--takeover" => takeover = true,
            "help" | "--help" | "-h" => {
                eprintln!("{usage}");
                return Err(0);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Err(2);
            }
        }
    }
    Ok((target.clone(), takeover))
}

pub(super) fn print_response(response: &serde_json::Value) -> std::io::Result<i32> {
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(response).unwrap());
        return Ok(1);
    }

    println!("{}", serde_json::to_string(response).unwrap());
    Ok(0)
}

pub(super) fn send_ok_request(method: Method) -> std::io::Result<i32> {
    let response = send_request(&Request {
        id: "cli:request".into(),
        method,
    })?;

    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }

    Ok(0)
}

pub(super) fn send_request(request: &Request) -> std::io::Result<serde_json::Value> {
    let client = ApiClient::local();
    ensure_server_protocol_compatible(&client, &request.id)?;
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(err, &request.id, &client))
}

pub(super) fn send_request_unchecked(request: &Request) -> std::io::Result<serde_json::Value> {
    let client = ApiClient::local();
    client
        .request_value(request)
        .map_err(|err| map_server_not_running_or_io(err, &request.id, &client))
}

fn ensure_server_protocol_compatible(client: &ApiClient, request_id: &str) -> std::io::Result<()> {
    let status = client
        .status()
        .map_err(|err| map_server_not_running_or_io(err, request_id, client))?;
    let server_protocol = status
        .protocol
        .ok_or_else(|| std::io::Error::other("server ping did not include a protocol version"))?;
    let Some(response) = protocol_guard::mismatch_response(
        request_id,
        server_protocol,
        &crate::session::active_restart_after_update_guidance(),
    ) else {
        return Ok(());
    };

    eprintln!(
        "{}",
        serde_json::to_string(&response).map_err(std::io::Error::other)?
    );
    Err(protocol_guard::reported_error())
}

pub(crate) fn protocol_mismatch_was_reported(err: &std::io::Error) -> bool {
    protocol_guard::was_reported(err)
}

pub(crate) fn server_not_running_was_reported(err: &std::io::Error) -> bool {
    server_not_running::was_reported(err)
}

/// Returns the `ErrorResponse` carried by a `server_not_running` marker, if any,
/// so the edge that surfaces the error can print it exactly once (deferred
/// printing: recovering callers like plugin offline fallback print nothing).
pub(crate) fn server_not_running_reported_response(
    err: &std::io::Error,
) -> Option<&crate::api::schema::ErrorResponse> {
    server_not_running::reported_response(err)
}

/// True when an io::Error indicates nothing is listening on the API socket.
/// Classify by `ErrorKind` only: Windows named pipes surface different raw
/// errno values than Unix domain sockets but the same error kinds.
pub(super) fn server_not_running_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// True when a socket connect failed because the resolved path is longer than
/// this OS's unix domain socket address can hold. `interprocess` reports this
/// as `io::ErrorKind::InvalidInput` with a fixed message naming the kernel
/// struct field (`src/os/unix/ud_addr.rs::name_too_long` in the `interprocess`
/// crate) rather than the path or a remedy — see `socket_path_too_long_reported_error`
/// for the actionable message this classifies into.
fn socket_path_too_long_error(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::InvalidInput && err.to_string().contains("sun_path")
}

/// Maps an `ApiClientError` from a socket command into the io::Error that
/// bubbles up to `main`. A dead-server connect failure is reported as a
/// friendly `server_not_running` JSON error plus a recognizable marker; a
/// socket path that is too long for this OS's unix domain socket address gets
/// the same treatment instead of the raw `io::Error` Debug dump `main`'s
/// default `Result` termination would otherwise print; all other errors fall
/// through unchanged so existing handling is preserved.
fn map_server_not_running_or_io(
    err: ApiClientError,
    request_id: &str,
    client: &ApiClient,
) -> std::io::Error {
    match err {
        ApiClientError::Io(io_err) if server_not_running_error(&io_err) => {
            server_not_running::reported_error(server_not_running::response(
                request_id,
                &client.socket_path(),
            ))
        }
        ApiClientError::Io(io_err) if socket_path_too_long_error(&io_err) => {
            server_not_running::reported_error(socket_path_too_long_response(
                request_id,
                &client.socket_path(),
            ))
        }
        err => api_client_error_to_io(err),
    }
}

/// Builds the actionable `socket_path_too_long` error shown when the resolved
/// API socket path cannot fit in a unix domain socket address. Reuses the
/// `server_not_running` marker/print plumbing (`main.rs` already special-cases
/// it to print the carried `ErrorResponse` once and exit non-zero instead of
/// falling through to the default `Result` termination's `Debug` dump) since
/// the payload shape and edge behavior are identical — only the code and
/// message differ.
fn socket_path_too_long_response(
    request_id: &str,
    socket_path: &std::path::Path,
) -> crate::api::schema::ErrorResponse {
    let path = socket_path.display().to_string();
    let path_len = socket_path.as_os_str().len();
    crate::api::schema::ErrorResponse {
        id: request_id.to_string(),
        error: crate::api::schema::ErrorBody {
            code: "socket_path_too_long".into(),
            message: format!(
                "cannot connect: the karvex socket path is {path_len} bytes, longer than this OS's unix domain socket address can hold (the kernel's sun_path field typically caps out around 100-108 bytes): {path}. Set {}=<a shorter path> to override where karvex looks for the socket, or use a shorter --session name.",
                crate::api::SOCKET_PATH_ENV_VAR
            ),
        },
    }
}

fn api_client_error_to_io(err: ApiClientError) -> std::io::Error {
    match err {
        ApiClientError::Io(err) => err,
        err => std::io::Error::other(err),
    }
}

pub(super) fn normalize_workspace_id(value: &str) -> String {
    value.to_string()
}

pub(super) fn normalize_tab_id(value: &str) -> String {
    value.to_string()
}

pub(super) fn normalize_pane_id(value: &str) -> String {
    value.to_string()
}

pub(super) fn parse_split_direction(value: &str) -> std::io::Result<SplitDirection> {
    match value {
        "right" => Ok(SplitDirection::Right),
        "down" => Ok(SplitDirection::Down),
        _ => Err(std::io::Error::other(format!(
            "invalid split direction: {value}"
        ))),
    }
}

pub(super) fn parse_read_source(value: &str) -> std::io::Result<ReadSource> {
    match value {
        "visible" => Ok(ReadSource::Visible),
        "recent" => Ok(ReadSource::Recent),
        "recent-unwrapped" | "recent_unwrapped" => Ok(ReadSource::RecentUnwrapped),
        "detection" => Ok(ReadSource::Detection),
        _ => Err(std::io::Error::other(format!(
            "invalid read source: {value}"
        ))),
    }
}

pub(super) fn parse_read_format(value: &str) -> std::io::Result<ReadFormat> {
    match value {
        "text" => Ok(ReadFormat::Text),
        "ansi" => Ok(ReadFormat::Ansi),
        _ => Err(std::io::Error::other(format!(
            "invalid read format: {value}"
        ))),
    }
}

fn parse_agent_status(value: &str) -> std::io::Result<AgentStatus> {
    match value {
        "idle" => Ok(AgentStatus::Idle),
        "working" => Ok(AgentStatus::Working),
        "blocked" => Ok(AgentStatus::Blocked),
        "done" => Ok(AgentStatus::Done),
        "unknown" => Ok(AgentStatus::Unknown),
        _ => Err(std::io::Error::other(format!(
            "invalid agent status: {value} (expected idle, working, blocked, done, or unknown)"
        ))),
    }
}

pub(super) fn parse_pane_agent_state(value: &str) -> std::io::Result<PaneAgentState> {
    match value {
        "idle" => Ok(PaneAgentState::Idle),
        "working" => Ok(PaneAgentState::Working),
        "blocked" => Ok(PaneAgentState::Blocked),
        "unknown" => Ok(PaneAgentState::Unknown),
        _ => Err(std::io::Error::other(format!(
            "invalid pane agent state: {value} (expected idle, working, blocked, or unknown)"
        ))),
    }
}

pub(super) fn parse_u32_flag(flag: &str, value: &str) -> std::io::Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| std::io::Error::other(format!("invalid value for {flag}: {value}")))
}

pub(super) fn parse_u64_flag(flag: &str, value: &str) -> std::io::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| std::io::Error::other(format!("invalid value for {flag}: {value}")))
}

fn parse_session_json_only(args: &[String], usage: &str) -> Result<bool, i32> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => {
            eprintln!("{usage}");
            Err(2)
        }
    }
}

fn parse_session_name_and_json(args: &[String], usage: &str) -> Result<(String, bool), i32> {
    let mut name = None;
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            json = true;
        } else if name.is_none() {
            name = Some(arg.clone());
        } else {
            eprintln!("{usage}");
            return Err(2);
        }
    }

    let Some(name) = name else {
        eprintln!("{usage}");
        return Err(2);
    };
    Ok((name, json))
}

fn print_session_table(sessions: &[crate::session::SessionInfo]) {
    println!("{:<20} {:<8} {:<48} socket", "name", "status", "directory");
    for session in sessions {
        println!(
            "{:<20} {:<8} {:<48} {}",
            session.name,
            if session.running {
                "running"
            } else {
                "stopped"
            },
            session.session_dir,
            session.socket_path
        );
    }
}

fn print_session_error(code: &str, message: &str) {
    eprintln!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .unwrap()
    );
}

fn print_config_help() {
    eprintln!("kvx config commands:");
    eprintln!("  kvx config check  validate config.toml and print diagnostics");
    eprintln!("  kvx config reset-keys  back up config.toml and remove custom keybindings");
}

fn print_terminal_help() {
    eprintln!("kvx terminal commands:");
    eprintln!("  kvx terminal attach <terminal_id> [--takeover]");
    eprintln!("  kvx terminal session control <target> [--takeover] [--cols N] [--rows N]");
    eprintln!("  kvx terminal session observe <target> [--cols N] [--rows N]");
    eprintln!("  kvx terminal title set <title>");
    eprintln!("  kvx terminal title clear");
    eprintln!("  detach from direct attach with ctrl+b q; send literal ctrl+b with ctrl+b ctrl+b");
}

fn print_session_help() {
    eprintln!("kvx session commands:");
    eprintln!("  kvx session list [--json]");
    eprintln!("  kvx session attach <name>");
    eprintln!("  kvx session stop <name> [--json]");
    eprintln!("  kvx session delete <name> [--json]");
    eprintln!("  use 'default' as <name> to target the default session for stop");
}

fn _print_json<T: Serialize>(value: &T) {
    println!("{}", serde_json::to_string(value).unwrap());
}

#[cfg(test)]
mod tests {
    /// `SIG_DFL` for `SIGPIPE` turns a write to a closed reader into process
    /// death, which is right for a print-and-exit CLI and wrong for anything
    /// that owns a terminal or a long-lived socket: those need `EPIPE` back as
    /// an `io::Error` so they can restore the terminal and report. So the
    /// policy has to be an allowlist. An exclusion list of one (`"server"`)
    /// silently covered every argument `maybe_run` does *not* recognise —
    /// `kvx client`, `kvx remote-client-bridge`, `kvx --session <name>`, and a
    /// bare `kvx --help` all fall through `maybe_run` to the TUI and the client
    /// runloop in `main`.
    #[test]
    fn restores_default_sigpipe_only_for_print_and_exit_commands() {
        for (command, subcommand) in [
            ("workflow", Some("run")),
            ("pane", Some("list")),
            ("agent", Some("read")),
            ("status", None),
            ("api", Some("methods")),
            ("tab", Some("list")),
            ("completion", Some("bash")),
            ("workspace", Some("list")),
            ("terminal", Some("title")),
            // Handled by `main` after `maybe_run` declines, but still just a
            // print and an exit.
            ("--help", None),
            ("--version", None),
            ("--skill", None),
            ("--default-config", None),
        ] {
            assert!(
                super::restores_default_sigpipe(command, subcommand),
                "{command} {subcommand:?} prints and exits"
            );
        }

        // `kvx client --help` runs the client runloop; the flag being present
        // somewhere is not proof the process prints and exits.
        assert!(!super::restores_default_sigpipe("client", Some("--help")));
        assert!(!super::restores_default_sigpipe(
            "--help",
            Some("--session")
        ));

        // The daemon, and every path that falls through `maybe_run` into the
        // TUI or the client runloop in `main`.
        for command in [
            "server",
            "client",
            "remote-client-bridge",
            "update",
            "--session",
            "--remote",
            "my-session-name",
        ] {
            assert!(
                !super::restores_default_sigpipe(command, None),
                "{command} does not print and exit"
            );
        }

        // Interactive attaches live inside otherwise print-and-exit
        // namespaces: both take over the terminal through `crate::client`.
        for (command, subcommand) in [
            ("agent", Some("attach")),
            ("terminal", Some("attach")),
            ("terminal", Some("session")),
        ] {
            assert!(
                !super::restores_default_sigpipe(command, subcommand),
                "{command} {subcommand:?} takes over the terminal"
            );
        }
    }

    #[test]
    fn parses_channel_set_argument() {
        assert_eq!(
            super::parse_channel_set_arg(&["preview".to_string()]),
            Some("preview")
        );
        assert_eq!(
            super::parse_channel_set_arg(&["stable".to_string()]),
            Some("stable")
        );
        assert_eq!(super::parse_channel_set_arg(&["nightly".to_string()]), None);
        assert_eq!(
            super::parse_channel_set_arg(&["preview".to_string(), "stable".to_string()]),
            None
        );
    }

    #[test]
    fn channel_set_rejects_package_managed_preview_before_config_write() {
        assert_eq!(
            super::channel_set_rejection("preview", Some("no preview")),
            Some("no preview")
        );
        assert_eq!(
            super::channel_set_rejection("stable", Some("no preview")),
            if cfg!(windows) {
                Some(
                    "stable channel is not available on Windows yet; Windows builds are preview-only",
                )
            } else {
                None
            }
        );
        assert_eq!(super::channel_set_rejection("preview", None), None);
    }

    #[test]
    fn channel_set_rejects_stable_only_on_windows() {
        assert_eq!(
            super::channel_set_rejection("stable", None),
            if cfg!(windows) {
                Some(
                    "stable channel is not available on Windows yet; Windows builds are preview-only",
                )
            } else {
                None
            }
        );
    }

    #[test]
    fn channel_set_skips_self_update_for_package_manager_guidance() {
        assert_eq!(
            super::channel_set_install_action(Some("use package manager")),
            super::ChannelSetInstallAction::PrintGuidance("use package manager")
        );
        assert_eq!(
            super::channel_set_install_action(None),
            super::ChannelSetInstallAction::RunSelfUpdate
        );
    }

    #[test]
    fn parse_env_assignment_accepts_empty_values() {
        assert_eq!(
            super::parse_env_assignment("KARVEX_ROLE=").unwrap(),
            ("KARVEX_ROLE".to_string(), String::new())
        );
    }

    #[test]
    fn parse_env_assignment_requires_key_value_separator() {
        assert_eq!(
            super::parse_env_assignment("KARVEX_ROLE").unwrap_err(),
            "env must use KEY=VALUE"
        );
    }

    #[test]
    fn maps_dead_server_connect_failure_to_friendly_error() {
        use crate::api::client::{ApiClient, ApiClientError};

        let client = ApiClient::local();
        let socket = client.socket_path().display().to_string();

        // The helper does NOT print; it returns a recognizable marker carrying
        // the ErrorResponse so the surfacing edge can print it exactly once.
        let mapped = super::map_server_not_running_or_io(
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "cli:workspace:create",
            &client,
        );

        let response = super::server_not_running::reported_response(&mapped)
            .expect("dead-server connect failure should carry a server_not_running response");
        assert_eq!(response.id, "cli:workspace:create");
        assert_eq!(response.error.code, "server_not_running");
        assert!(response.error.message.contains(&socket));

        // The mapping is recognizable without string matching.
        assert!(super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn classifier_ignores_unrelated_io_kinds() {
        use crate::api::client::{ApiClient, ApiClientError};

        let client = ApiClient::local();
        let mapped = super::map_server_not_running_or_io(
            ApiClientError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            "cli:workspace:create",
            &client,
        );
        assert!(!super::server_not_running::was_reported(&mapped));
    }

    /// §2.16.1 of the UX report: connecting with a socket path too long for
    /// this OS's unix domain socket address previously bubbled the raw
    /// `interprocess`-crate `io::Error` all the way to `main`'s default
    /// `Result` termination, which prints its `Debug` form —
    /// `Error: Custom { kind: InvalidInput, error: "local socket name length
    /// exceeds capacity of sun_path of sockaddr_un" }` — naming no path,
    /// length, limit, or fix. This asserts the mapped error instead carries
    /// an actionable `ErrorResponse` through the same "reported" plumbing
    /// `main` already special-cases for `server_not_running`.
    #[test]
    fn maps_sun_path_too_long_connect_failure_to_actionable_error() {
        use crate::api::client::{ApiClient, ApiClientError};

        let client = ApiClient::local();
        let socket = client.socket_path().display().to_string();
        let socket_len = client.socket_path().as_os_str().len();

        let sun_path_error = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local socket name length exceeds capacity of sun_path of sockaddr_un",
        );
        let mapped = super::map_server_not_running_or_io(
            ApiClientError::Io(sun_path_error),
            "cli:session:list",
            &client,
        );

        let response = super::server_not_running::reported_response(&mapped)
            .expect("a sun_path-too-long connect failure should carry a reported response");
        assert_eq!(response.id, "cli:session:list");
        assert_eq!(response.error.code, "socket_path_too_long");
        assert!(
            response.error.message.contains(&socket),
            "message should name the offending path: {}",
            response.error.message
        );
        assert!(
            response.error.message.contains(&socket_len.to_string()),
            "message should name the actual path length: {}",
            response.error.message
        );
        assert!(
            response
                .error
                .message
                .contains(crate::api::SOCKET_PATH_ENV_VAR),
            "message should name the env var to override: {}",
            response.error.message
        );
        assert!(
            !response.error.message.contains("Custom {"),
            "message should never leak raw io::Error Debug formatting: {}",
            response.error.message
        );

        // The mapping is recognizable without string matching, exactly like
        // the dead-server case.
        assert!(super::server_not_running::was_reported(&mapped));
    }

    #[test]
    fn sun_path_classifier_only_matches_the_specific_os_error() {
        assert!(super::socket_path_too_long_error(&std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local socket name length exceeds capacity of sun_path of sockaddr_un",
        )));
        assert!(!super::socket_path_too_long_error(&std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "some other invalid input",
        )));
        assert!(!super::socket_path_too_long_error(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    /// Part of the W5 "manual parser / spec.rs / cli.rs dispatch" parity
    /// concern (`docs/design/workflow-builder/05-phase-plan.md`): confirms
    /// the top-level `"workflow"` arm in `maybe_run` actually routes into
    /// `cli::workflow::run_workflow_command` rather than falling through to
    /// `CommandOutcome::NotCli`. An unrecognized `kvx workflow` subcommand
    /// short-circuits to a usage message before any network call, so this
    /// stays a pure dispatch-routing test.
    #[test]
    fn workflow_command_is_dispatched_without_reaching_the_network() {
        let outcome = super::maybe_run(&[
            "kvx".to_string(),
            "workflow".to_string(),
            "not-a-real-verb".to_string(),
        ])
        .unwrap();
        assert!(matches!(outcome, super::CommandOutcome::Handled(2)));
    }
}
