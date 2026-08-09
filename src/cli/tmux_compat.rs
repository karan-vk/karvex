//! tmux compatibility shim: lets Claude Code's Agent Teams tmux backend drive
//! Karvex panes natively.
//!
//! Karvex exports `TMUX=<socket>,<server_pid>,0` and `TMUX_PANE=<pane_id>`
//! inside every managed pane and puts a `tmux`-named symlink to the `kvx`
//! binary on PATH. Claude Code's backend detection sees `TMUX` set, selects its
//! tmux backend, and invokes `tmux -S <socket> <subcommand> ...`. Those
//! invocations land here and are translated onto the Karvex daemon's JSON API,
//! so teammates appear as real Karvex panes — tracked by the sidebar and agent
//! detection like any other pane.
//!
//! Anything outside the serviced surface is passed through to a real `tmux`
//! found later on PATH, so interactive tmux use keeps working.
//!
//! Structure: every subcommand splits into a **pure** `plan_x(args) -> XPlan`
//! builder and a thin executor that issues the API request. All translation
//! decisions live in the pure half so they are unit-testable without a server.
//!
//! Transport (see the port plan, D3/D5): the shim resolves its own socket and
//! talks to it through an explicitly targeted [`ApiClient`] with a short
//! timeout. It deliberately does **not** use `cli::send_request` /
//! `cli::send_request_unchecked`: those build `ApiClient::local()`, which
//! resolves through `session::active_api_socket_path()` and can fall back to
//! the *default session's* socket, and `send_request` additionally pays a
//! protocol round trip and can print a JSON envelope. Claude parses stdout
//! exactly and times its probes out quickly, so neither is acceptable here.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::client::{ApiClient, ApiClientError, ConnectionTarget};
use crate::api::schema::{
    Method, PaneCurrentParams, PaneInfo, PaneListParams, PaneReadParams, PaneRenameParams,
    PaneReportMetadataParams, PaneSendInputParams, PaneSplitParams, PaneTarget, ReadFormat,
    ReadIntent, ReadSource, Request, SplitDirection,
};

/// Banner printed for `tmux -V`. Claude's `isAvailable` only checks the exit
/// code; the suffix makes the shim identifiable in user-visible output.
const COMPAT_VERSION: &str = "tmux 3.5a (karvex-compat)";

/// Every serviced request is bounded. Claude treats a hang as a backend
/// failure and its probes have short timeouts, so failing fast beats stalling.
const SHIM_REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);

/// Claude keys a friendlier "not enough space" message off these substrings in
/// a split failure's stderr, so every `pane.split` failure carries them.
const SPLIT_FAILURE_HINT: &str = "no space for new pane (too small)";

const ERROR_PREFIX: &str = "karvex tmux-compat: ";

const SHELL_READY_POLLS: usize = 10;
const SHELL_READY_INTERVAL: Duration = Duration::from_millis(150);

const TMUX_BINARY_NAME: &str = "tmux";

// ---------------------------------------------------------------------------
// Teammate accent colour (D8)
// ---------------------------------------------------------------------------
//
// Claude carries the teammate accent colour through `set-option -p -t <pane>
// <style-option> fg=<colour>`. The shim reports it onward as a
// `pane.report_metadata` token so the sidebar can use it as a style hint (see
// `src/ui/sidebar.rs`, `src/ui/sidebar/tokens.rs`).

/// Metadata token key the accent colour is reported under. Referenced from
/// `src/ui/sidebar/tokens.rs` too, so it is `pub(crate)` rather than private.
///
/// ⚠ Must contain only `[A-Za-z0-9_-]`: `normalize_metadata_tokens`
/// (`src/app/api_helpers.rs`) rejects dots outright, which is why this is
/// `agent_accent` and not `agent.accent`. See `set_option_accent_token_key_is_api_valid`.
pub(crate) const AGENT_ACCENT_TOKEN_KEY: &str = "agent_accent";

/// `pane.report_metadata` source tag for every accent write this shim makes.
const ACCENT_METADATA_SOURCE: &str = "karvex:tmux-compat";

/// The exact three tmux style options Claude uses to carry the accent colour.
/// Matched by name, never by scanning argv for `fg=`: `pane-border-format`
/// carries a literal `fg=` inside a format string and must not be mistaken
/// for a style option (see `set_option_pane_border_format_does_not_yield_an_accent`).
const ACCENT_STYLE_OPTIONS: &[&str] = &[
    "window-style",
    "pane-border-style",
    "pane-active-border-style",
];

/// Claude's eight teammate colours, normalised per the port plan's mapping
/// table (`colour208` -> orange-ish, `colour205` -> pink-ish; everything else
/// passes through unchanged).
const KNOWN_CLAUDE_COLOURS: &[(&str, &str)] = &[
    ("red", "red"),
    ("blue", "blue"),
    ("green", "green"),
    ("yellow", "yellow"),
    ("magenta", "magenta"),
    ("cyan", "cyan"),
    ("colour208", "orange"),
    ("colour205", "pink"),
];

/// A per-process monotonic counter for the `seq` field on accent metadata
/// reports (plan Q4): several teammates can invoke `set-option` concurrently,
/// so a wall-clock timestamp is not guaranteed to be strictly increasing
/// across processes, while a counter within one shim invocation trivially is.
static ACCENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_accent_seq() -> u64 {
    ACCENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Subcommands serviced by the shim. Everything else falls through to a real
/// tmux when one exists.
const SERVICED: &[&str] = &[
    "display-message",
    "list-panes",
    "split-window",
    "splitw",
    "respawn-pane",
    "set-option",
    "set",
    "select-pane",
    "selectp",
    "kill-pane",
    "killp",
    "select-layout",
    "resize-pane",
    "resizep",
    "show-options",
    "send-keys",
];

/// A shim failure. Rendered as one concise plain-text line on stderr — never a
/// JSON envelope, on either stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShimError(String);

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// True when argv[0]'s file stem is exactly `tmux`, i.e. we were invoked
/// through the shim symlink rather than as `kvx`.
pub(crate) fn invoked_as_tmux(program: &str) -> bool {
    Path::new(program)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem == TMUX_BINARY_NAME)
}

/// Entry point when the binary is invoked as `tmux`. `args` excludes argv[0].
/// Returns the process exit code; all diagnostics are already printed.
pub(crate) fn run_shim(args: &[String]) -> i32 {
    // `tmux display-message -p '#{pane_id}' | head -1` must die quietly rather
    // than panic on the closed pipe, like the other print-and-exit verbs.
    crate::platform::restore_default_sigpipe();

    let parsed = parse_invocation(args);

    // `-V` short-circuits before any socket work: Claude's availability probe
    // must keep succeeding even when the Karvex server is gone.
    if parsed.version_only {
        println!("{COMPAT_VERSION}");
        return 0;
    }

    let resolved = resolve_socket_path();
    if !should_service(&parsed, resolved.as_deref()) {
        return passthrough(args, &parsed);
    }
    // `should_service` already established both of these.
    let (Some(socket), Some(subcommand)) = (resolved, parsed.subcommand.as_deref()) else {
        return passthrough(args, &parsed);
    };

    let shim = Shim::new(PathBuf::from(socket));
    let outcome = match subcommand {
        "display-message" => shim.display_message(&parsed.args),
        "list-panes" => shim.list_panes(&parsed.args),
        "split-window" | "splitw" => shim.split_window(&parsed.args),
        "respawn-pane" => shim.respawn_pane(&parsed.args),
        "set-option" | "set" => shim.set_option(&parsed.args),
        "select-pane" | "selectp" => shim.select_pane(&parsed.args),
        "kill-pane" | "killp" => shim.kill_pane(&parsed.args),
        "select-layout" | "resize-pane" | "resizep" => accept_ok(),
        "show-options" => show_options(&parsed.args),
        "send-keys" => shim.send_keys(&parsed.args),
        // `SERVICED` and this match are kept in step; anything else is a bug in
        // one of them, and passthrough is the safe answer.
        _ => return passthrough(args, &parsed),
    };

    match outcome {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Invocation parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedInvocation {
    socket_path: Option<String>,
    named_socket: Option<String>,
    version_only: bool,
    subcommand: Option<String>,
    args: Vec<String>,
}

fn parse_invocation(args: &[String]) -> ParsedInvocation {
    let mut socket_path = None;
    let mut named_socket = None;
    let mut version_only = false;
    let mut subcommand: Option<String> = None;
    let mut rest = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if subcommand.is_none() {
            match arg {
                "-S" => {
                    if let Some(value) = args.get(index + 1) {
                        socket_path = Some(value.clone());
                        index += 2;
                        continue;
                    }
                }
                "-L" => {
                    if let Some(value) = args.get(index + 1) {
                        named_socket = Some(value.clone());
                        index += 2;
                        continue;
                    }
                }
                "-V" => {
                    version_only = true;
                    index += 1;
                    continue;
                }
                // Global flags that take a value we do not interpret.
                "-f" | "-c" | "-T" => {
                    index += 2;
                    continue;
                }
                // Global boolean flags.
                "-2" | "-C" | "-CC" | "-D" | "-l" | "-N" | "-u" | "-v" => {
                    index += 1;
                    continue;
                }
                other if !other.starts_with('-') => {
                    subcommand = Some(other.to_string());
                    index += 1;
                    continue;
                }
                _ => {
                    index += 1;
                    continue;
                }
            }
        }
        rest.push(args[index].clone());
        index += 1;
    }

    ParsedInvocation {
        socket_path,
        named_socket,
        version_only,
        subcommand,
        args: rest,
    }
}

// ---------------------------------------------------------------------------
// Socket resolution and servicing gates
// ---------------------------------------------------------------------------

/// Resolve the socket this shim speaks to, with **no** fallback to the default
/// session (see D5): `$KARVEX_SOCKET_PATH`, else the first comma-field of
/// `$TMUX`, else "not serviced".
fn resolve_socket_path() -> Option<String> {
    let socket_env = std::env::var(crate::api::SOCKET_PATH_ENV_VAR).ok();
    let tmux_env = std::env::var("TMUX").ok();
    socket_from_env_values(socket_env.as_deref(), tmux_env.as_deref())
}

/// Pure half of [`resolve_socket_path`].
fn socket_from_env_values(socket_path: Option<&str>, tmux: Option<&str>) -> Option<String> {
    if let Some(value) = socket_path {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let first = tmux?.split(',').next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

fn should_service(parsed: &ParsedInvocation, resolved_socket: Option<&str>) -> bool {
    if parsed.named_socket.is_some() {
        // `-L <name>` is Claude's external-session mode (only used when TMUX is
        // unset, i.e. outside Karvex panes) or a user talking to a named tmux
        // server. Real tmux territory either way.
        return false;
    }
    let Some(ours) = resolved_socket else {
        return false;
    };
    if let Some(requested) = parsed.socket_path.as_deref() {
        if !socket_paths_match(requested, ours) {
            return false;
        }
    }
    matches!(parsed.subcommand.as_deref(), Some(sub) if SERVICED.contains(&sub))
}

/// Compare an `-S <path>` argument against our own socket: canonicalised when
/// both sides canonicalise, else a trimmed string compare.
fn socket_paths_match(requested: &str, ours: &str) -> bool {
    let requested = requested.trim();
    let ours = ours.trim();
    if requested == ours {
        return true;
    }
    let requested = Path::new(requested).canonicalize().ok();
    let ours = Path::new(ours).canonicalize().ok();
    match (requested, ours) {
        (Some(requested), Some(ours)) => requested == ours,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pane / window id encoding
// ---------------------------------------------------------------------------
//
// Real tmux ids are `%N` (pane) and `@N` (window); Karvex emits `w1:p3` /
// `w1:t2`. Claude appears to treat them as opaque strings and hands them back
// through `-t`, so the mapping is currently the identity — but **every** id
// crossing the shim boundary goes through these functions, so if the gate-phase
// probe shows Claude validates the sigil, the `%N` / `@N` mapping is a
// one-place change here (plan Q1b).

fn encode_pane_id(native: &str) -> String {
    native.to_string()
}

fn decode_pane_id(external: &str) -> String {
    external.to_string()
}

fn encode_window_id(native: &str) -> String {
    native.to_string()
}

fn decode_window_id(external: &str) -> String {
    external.to_string()
}

/// Creation order within a tab: a Karvex pane id carries a monotonically
/// increasing pane number, base-32 encoded (`w1:p9`, `w1:pA`, …). Claude relies
/// on element 0 of `list-panes` being the leader (oldest) pane.
fn pane_ordinal(pane_id: &str) -> u64 {
    if let Some((_, number)) = pane_id.rsplit_once(":p") {
        if let Some(decoded) = crate::workspace::decode_public_number(number) {
            return decoded as u64;
        }
    }
    // Fall back to a trailing decimal run for any other id shape.
    pane_id
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(u64::MAX)
}

/// A `%N`-shaped id can only have come from a real tmux server, never from
/// Karvex, so an inherited `TMUX_PANE` carrying one is stale or foreign and
/// must not be trusted. (If Q1b ever makes the shim emit `%N`, `decode_pane_id`
/// maps it back to a Karvex id first and this check stops firing.)
fn looks_like_foreign_pane_id(pane_id: &str) -> bool {
    pane_id.starts_with('%')
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag {
            return args.get(index + 1).map(String::as_str);
        }
        index += 1;
    }
    None
}

/// Arguments before a `--` separator. Used where the trailing command is a
/// shell string that must never be mistaken for a flag.
fn args_before_separator(args: &[String]) -> &[String] {
    match args.iter().position(|arg| arg == "--") {
        Some(position) => &args[..position],
        None => args,
    }
}

fn trailing_command(args: &[String]) -> Option<String> {
    let position = args.iter().position(|arg| arg == "--")?;
    let command = args[position + 1..].join(" ");
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

// ---------------------------------------------------------------------------
// Pure plans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayFormat {
    PaneId,
    WindowId,
    ClientControlMode,
    ClientTermtype,
    /// Anything we do not translate: print an empty line and exit 0, never a
    /// Rust error string on stdout.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayMessagePlan {
    target: Option<String>,
    format: DisplayFormat,
}

fn plan_display_message(args: &[String]) -> DisplayMessagePlan {
    let format = match args
        .iter()
        .rev()
        .find(|arg| arg.contains("#{"))
        .map(String::as_str)
    {
        Some("#{pane_id}") => DisplayFormat::PaneId,
        Some("#{window_id}") => DisplayFormat::WindowId,
        Some("#{client_control_mode}") => DisplayFormat::ClientControlMode,
        Some("#{client_termtype}") => DisplayFormat::ClientTermtype,
        _ => DisplayFormat::Unsupported,
    };
    DisplayMessagePlan {
        target: flag_value(args, "-t").map(str::to_string),
        format,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListPanesPlan {
    target: Option<String>,
}

fn plan_list_panes(args: &[String]) -> ListPanesPlan {
    ListPanesPlan {
        target: flag_value(args, "-t").map(str::to_string),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SplitPlan {
    target: Option<String>,
    direction: SplitDirection,
    ratio: Option<f32>,
    focus: bool,
    print_pane_id: bool,
}

fn plan_split_window(args: &[String]) -> SplitPlan {
    let flags = args_before_separator(args);
    let direction = if flags.iter().any(|arg| arg == "-v") {
        SplitDirection::Down
    } else {
        SplitDirection::Right
    };
    // tmux `-l N%` sizes the NEW pane; Karvex's split ratio is the share kept
    // by the EXISTING (first) pane, so invert.
    let ratio = flag_value(flags, "-l")
        .and_then(|value| value.strip_suffix('%'))
        .and_then(|value| value.parse::<f32>().ok())
        .map(|percent| (1.0 - percent / 100.0).clamp(0.1, 0.9));
    SplitPlan {
        target: flag_value(flags, "-t").map(str::to_string),
        direction,
        ratio,
        // Claude always passes `-d` (don't focus the new pane). `PaneSplitParams.focus`
        // defaults to false, so the mapping is made explicit rather than implied.
        focus: !flags.iter().any(|arg| arg == "-d"),
        print_pane_id: flags.iter().any(|arg| arg == "-P"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RespawnPlan {
    target: Option<String>,
    command: Option<String>,
}

fn plan_respawn_pane(args: &[String]) -> RespawnPlan {
    RespawnPlan {
        // `-k` is accepted and ignored: a Karvex pane keeps its own shell.
        target: flag_value(args_before_separator(args), "-t").map(str::to_string),
        command: trailing_command(args),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectPanePlan {
    target: Option<String>,
    title: Option<String>,
}

fn plan_select_pane(args: &[String]) -> SelectPanePlan {
    SelectPanePlan {
        target: flag_value(args, "-t").map(str::to_string),
        title: flag_value(args, "-T").map(str::to_string),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KillPanePlan {
    target: Option<String>,
}

fn plan_kill_pane(args: &[String]) -> KillPanePlan {
    KillPanePlan {
        target: flag_value(args, "-t").map(str::to_string),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SendKeysPlan {
    target: Option<String>,
    text: String,
    keys: Vec<String>,
}

fn plan_send_keys(args: &[String]) -> SendKeysPlan {
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-t" => index += 2,
            arg if arg.starts_with('-') => index += 1,
            arg => {
                positional.push(arg.to_string());
                index += 1;
            }
        }
    }
    let mut keys = Vec::new();
    if positional.last().map(String::as_str) == Some("Enter") {
        positional.pop();
        keys.push("Enter".to_string());
    }
    SendKeysPlan {
        target: flag_value(args, "-t").map(str::to_string),
        text: positional.join(" "),
        keys,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShowOptionsPlan {
    print_prefix: bool,
}

fn plan_show_options(args: &[String]) -> ShowOptionsPlan {
    ShowOptionsPlan {
        print_prefix: args.iter().any(|arg| arg == "prefix"),
    }
}

/// Two consecutive reads of a pane's tail that are non-empty and identical are
/// taken as "the shell has settled and is ready for input".
fn shell_looks_ready(previous: &str, current: &str) -> bool {
    !current.trim().is_empty() && previous == current
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetOptionPlan {
    target: Option<String>,
    option: Option<String>,
    value: Option<String>,
}

/// tmux `set-option` syntax is `set-option [flags] [-t target-pane] option
/// value`; boolean flags (`-p`, `-g`, `-a`, ...) carry no value of their own.
/// Only the option name and its value are positional.
fn plan_set_option(args: &[String]) -> SetOptionPlan {
    let target = flag_value(args, "-t").map(str::to_string);
    let positional = positional_args_excluding_target(args);
    SetOptionPlan {
        target,
        option: positional.first().map(|value| value.to_string()),
        value: positional.get(1).map(|value| value.to_string()),
    }
}

/// Positional (non-flag) arguments, skipping `-t <value>` and any other
/// leading `-`-prefixed flag. Order-preserving.
fn positional_args_excluding_target(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-t" => index += 2,
            arg if arg.starts_with('-') => index += 1,
            arg => {
                out.push(arg);
                index += 1;
            }
        }
    }
    out
}

/// What a `set-option` invocation means for the teammate accent colour.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AccentPlan {
    /// Not one of the three accent-carrying style options, or the value has
    /// no `fg=` component: leave metadata untouched (accept-and-drop).
    NotAccent,
    /// A recognised Claude colour: write/patch the token.
    Set(String),
    /// An explicit empty `fg=` value: clear the token.
    Clear,
    /// `fg=` present but the colour name is not one Claude sends: drop rather
    /// than propagate garbage into the UI.
    Unknown,
}

/// Pure parse of a `set-option <option> <value>` pair into an [`AccentPlan`].
/// Matches on the option **name** first — never scans the value for `fg=`
/// blindly — so `pane-border-format`'s format string (which legitimately
/// contains a literal `fg=`) is never mistaken for a style option.
fn plan_accent(option: &str, value: &str) -> AccentPlan {
    if !ACCENT_STYLE_OPTIONS.contains(&option) {
        return AccentPlan::NotAccent;
    }
    let Some(fg) = value.split(',').find_map(|part| part.strip_prefix("fg=")) else {
        return AccentPlan::NotAccent;
    };
    let fg = fg.trim();
    if fg.is_empty() {
        return AccentPlan::Clear;
    }
    match normalize_claude_colour(fg) {
        Some(normalized) => AccentPlan::Set(normalized.to_string()),
        None => AccentPlan::Unknown,
    }
}

fn normalize_claude_colour(raw: &str) -> Option<&'static str> {
    KNOWN_CLAUDE_COLOURS
        .iter()
        .find(|(claude_name, _)| *claude_name == raw)
        .map(|(_, normalized)| *normalized)
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

struct Shim {
    client: ApiClient,
}

impl Shim {
    fn new(socket: PathBuf) -> Self {
        Self {
            client: ApiClient::for_target(ConnectionTarget::SocketPath(socket)),
        }
    }

    fn request(&self, id: &str, method: Method) -> Result<serde_json::Value, ShimError> {
        let request = Request {
            id: id.to_string(),
            method,
        };
        let response = self
            .client
            .request_value_with_timeout(&request, SHIM_REQUEST_TIMEOUT)
            .map_err(|err| ShimError(describe_client_error(&err)))?;
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("request failed");
            return Err(ShimError(format!("{ERROR_PREFIX}{message}")));
        }
        Ok(response)
    }

    // -- target resolution --------------------------------------------------

    /// The pane a command applies to, in Karvex-native form.
    fn resolve_pane(&self, target: Option<&str>) -> Result<String, ShimError> {
        match target {
            Some(target) => Ok(decode_pane_id(target)),
            None => self.current_pane_id(),
        }
    }

    fn current_pane_id(&self) -> Result<String, ShimError> {
        if let Ok(value) = std::env::var("TMUX_PANE") {
            let decoded = decode_pane_id(value.trim());
            if !decoded.is_empty() && !looks_like_foreign_pane_id(&decoded) {
                return Ok(decoded);
            }
        }
        if let Ok(value) = std::env::var(crate::integration::KARVEX_PANE_ID_ENV_VAR) {
            if !value.trim().is_empty() {
                return Ok(value.trim().to_string());
            }
        }
        let response = self.request(
            "tmux-compat:pane:current",
            Method::PaneCurrent(PaneCurrentParams::default()),
        )?;
        result_str(&response, &["pane.pane_id"])
            .ok_or_else(|| ShimError(format!("{ERROR_PREFIX}could not resolve the current pane")))
    }

    fn list_all_panes(&self) -> Result<Vec<PaneInfo>, ShimError> {
        let response = self.request(
            "tmux-compat:pane:list",
            Method::PaneList(PaneListParams::default()),
        )?;
        let panes = response
            .get("result")
            .and_then(|result| result.get("panes"))
            .cloned()
            .ok_or_else(|| ShimError(format!("{ERROR_PREFIX}pane.list returned no panes")))?;
        serde_json::from_value(panes)
            .map_err(|err| ShimError(format!("{ERROR_PREFIX}could not read pane.list: {err}")))
    }

    fn pane_info(&self, pane_id: &str) -> Result<PaneInfo, ShimError> {
        self.list_all_panes()?
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)
            .ok_or_else(|| ShimError(format!("can't find pane: {}", encode_pane_id(pane_id))))
    }

    /// Panes of one tab in creation order.
    fn tab_panes(&self, tab_id: &str) -> Result<Vec<PaneInfo>, ShimError> {
        let mut panes: Vec<PaneInfo> = self
            .list_all_panes()?
            .into_iter()
            .filter(|pane| pane.tab_id == tab_id)
            .collect();
        panes.sort_by_key(|pane| pane_ordinal(&pane.pane_id));
        Ok(panes)
    }

    /// A `-t` target may be a pane id or a window (tab) id; resolve to a tab id.
    fn resolve_window_target(&self, target: &str) -> Result<String, ShimError> {
        let pane_target = decode_pane_id(target);
        let window_target = decode_window_id(target);
        let panes = self.list_all_panes()?;
        if let Some(pane) = panes.iter().find(|pane| pane.pane_id == pane_target) {
            return Ok(pane.tab_id.clone());
        }
        if panes.iter().any(|pane| pane.tab_id == window_target) {
            return Ok(window_target);
        }
        Err(ShimError(format!("can't find window: {target}")))
    }

    // -- subcommands --------------------------------------------------------

    fn display_message(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_display_message(args);
        let output = match plan.format {
            DisplayFormat::PaneId => encode_pane_id(&self.resolve_pane(plan.target.as_deref())?),
            DisplayFormat::WindowId => {
                let pane = self.resolve_pane(plan.target.as_deref())?;
                encode_window_id(&self.pane_info(&pane)?.tab_id)
            }
            DisplayFormat::ClientControlMode => "0".to_string(),
            DisplayFormat::ClientTermtype => std::env::var("TERM").unwrap_or_default(),
            DisplayFormat::Unsupported => String::new(),
        };
        println!("{output}");
        Ok(0)
    }

    fn list_panes(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_list_panes(args);
        let tab_id = match plan.target.as_deref() {
            Some(target) => self.resolve_window_target(target)?,
            None => {
                let pane = self.current_pane_id()?;
                self.pane_info(&pane)?.tab_id
            }
        };
        // The only format Claude uses is `#{pane_id}`.
        for pane in self.tab_panes(&tab_id)? {
            println!("{}", encode_pane_id(&pane.pane_id));
        }
        Ok(0)
    }

    fn split_window(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_split_window(args);
        let target_pane = self.split_target_pane(plan.target.as_deref())?;

        // NOTE: any trailing `-- <cmd>` is deliberately NOT run. Claude passes a
        // holder command whose only job is keeping a plain tmux pane alive until
        // `respawn-pane` replaces it; a Karvex pane keeps its shell alive on its
        // own, and `respawn-pane` submits the real command into that shell.
        let params = PaneSplitParams {
            workspace_id: None,
            target_pane_id: Some(target_pane.pane_id.clone()),
            direction: plan.direction,
            ratio: plan.ratio,
            cwd: target_pane
                .foreground_cwd
                .clone()
                .or_else(|| target_pane.cwd.clone()),
            focus: plan.focus,
            env: HashMap::new(),
        };
        let response = self
            .request("tmux-compat:pane:split", Method::PaneSplit(params))
            .map_err(|err| split_failure(&err.0))?;
        let new_pane = result_str(&response, &["pane.pane_id", "pane_id"])
            .ok_or_else(|| split_failure(&format!("unexpected split response: {response}")))?;

        if plan.print_pane_id {
            println!("{}", encode_pane_id(&new_pane));
        }
        Ok(0)
    }

    /// `-t` may name a window; split its most recently created pane then.
    fn split_target_pane(&self, target: Option<&str>) -> Result<PaneInfo, ShimError> {
        let pane_id = self.resolve_pane(target)?;
        match self.pane_info(&pane_id) {
            Ok(pane) => Ok(pane),
            Err(err) => {
                let Some(target) = target else {
                    return Err(err);
                };
                let tab_id = self.resolve_window_target(target)?;
                self.tab_panes(&tab_id)?
                    .pop()
                    .ok_or_else(|| ShimError(format!("can't find pane: {target}")))
            }
        }
    }

    fn respawn_pane(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_respawn_pane(args);
        let pane_id = self.resolve_pane(plan.target.as_deref())?;
        let Some(command) = plan.command else {
            // Bare respawn: nothing to run; the Karvex pane shell is already alive.
            return Ok(0);
        };

        self.wait_for_shell_ready(&pane_id);

        // Clear any pending input on the line, then submit the command to the
        // pane's shell. tmux runs single-argument commands through the shell,
        // so shell submission preserves semantics (the command arrives
        // pre-quoted).
        self.request(
            "tmux-compat:pane:send",
            Method::PaneSendInput(PaneSendInputParams {
                pane_id: pane_id.clone(),
                text: String::new(),
                keys: vec!["ctrl+u".to_string()],
            }),
        )?;
        self.request(
            "tmux-compat:pane:send",
            Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text: command,
                keys: vec!["Enter".to_string()],
            }),
        )?;
        Ok(0)
    }

    /// Bounded, best-effort wait for a freshly split pane's shell to settle: a
    /// pane answers `pane.list` before its shell has drawn a prompt, and typing
    /// into a shell that is not at a prompt mangles the command.
    fn wait_for_shell_ready(&self, pane_id: &str) {
        let mut previous: Option<String> = None;
        for attempt in 0..SHELL_READY_POLLS {
            if let Ok(current) = self.read_pane_screen(pane_id) {
                if let Some(previous) = previous.as_deref() {
                    if shell_looks_ready(previous, &current) {
                        return;
                    }
                }
                previous = Some(current);
            }
            if attempt + 1 < SHELL_READY_POLLS {
                std::thread::sleep(SHELL_READY_INTERVAL);
            }
        }
    }

    /// The pane's current screen. `Visible` rather than `Recent`: `Recent` is
    /// anchored on the cursor row and reads empty for a pane that has not
    /// written a scrollback row yet (including alternate-screen programs),
    /// which is exactly the state this poll has to observe.
    fn read_pane_screen(&self, pane_id: &str) -> Result<String, ShimError> {
        let response = self.request(
            "tmux-compat:pane:read",
            Method::PaneRead(PaneReadParams {
                pane_id: pane_id.to_string(),
                source: ReadSource::Visible,
                lines: None,
                format: ReadFormat::Text,
                strip_ansi: true,
                intent: ReadIntent::Passive,
            }),
        )?;
        Ok(result_str(&response, &["read.text"]).unwrap_or_default())
    }

    fn select_pane(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_select_pane(args);
        let (Some(target), Some(title)) = (plan.target, plan.title) else {
            // Plain focus select: accepted but not acted on (Claude splits with
            // `-d` and never moves focus itself; leaving focus with the leader
            // matches the intent).
            return accept_ok();
        };
        self.request(
            "tmux-compat:pane:rename",
            Method::PaneRename(PaneRenameParams {
                pane_id: decode_pane_id(&target),
                label: Some(title),
            }),
        )?;
        Ok(0)
    }

    fn kill_pane(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_kill_pane(args);
        let pane_id = self.resolve_pane(plan.target.as_deref())?;
        self.request(
            "tmux-compat:pane:close",
            Method::PaneClose(PaneTarget { pane_id }),
        )?;
        Ok(0)
    }

    fn send_keys(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_send_keys(args);
        let pane_id = self.resolve_pane(plan.target.as_deref())?;
        self.request(
            "tmux-compat:pane:send",
            Method::PaneSendInput(PaneSendInputParams {
                pane_id,
                text: plan.text,
                keys: plan.keys,
            }),
        )?;
        Ok(0)
    }

    /// `set-option` / `set` are accept-and-drop, except for the three style
    /// options that carry the teammate accent colour (D8), which are turned
    /// into a `pane.report_metadata` write so the sidebar can use it as a
    /// style hint.
    fn set_option(&self, args: &[String]) -> Result<i32, ShimError> {
        let plan = plan_set_option(args);
        let (Some(option), Some(value)) = (plan.option.as_deref(), plan.value.as_deref()) else {
            return accept_ok();
        };
        match plan_accent(option, value) {
            AccentPlan::NotAccent | AccentPlan::Unknown => accept_ok(),
            AccentPlan::Clear => self.report_accent(plan.target.as_deref(), None),
            AccentPlan::Set(colour) => self.report_accent(plan.target.as_deref(), Some(colour)),
        }
    }

    /// Patch the `agent_accent` metadata token: `Some(colour)` sets it,
    /// `None` clears it. `seq` is a per-process monotonic counter (plan Q4),
    /// not a timestamp, so concurrent shim invocations from several
    /// teammates cannot race each other into an out-of-order write.
    fn report_accent(
        &self,
        target: Option<&str>,
        colour: Option<String>,
    ) -> Result<i32, ShimError> {
        let pane_id = self.resolve_pane(target)?;
        let mut tokens = HashMap::new();
        tokens.insert(AGENT_ACCENT_TOKEN_KEY.to_string(), colour);
        self.request(
            "tmux-compat:pane:report-metadata",
            Method::PaneReportMetadata(PaneReportMetadataParams {
                pane_id,
                source: ACCENT_METADATA_SOURCE.to_string(),
                agent: None,
                applies_to_source: None,
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
                tokens,
                clear_title: false,
                clear_display_agent: false,
                clear_state_labels: false,
                seq: Some(next_accent_seq()),
                ttl_ms: None,
            }),
        )?;
        accept_ok()
    }
}

fn show_options(args: &[String]) -> Result<i32, ShimError> {
    // Only used by Claude's `--tmux` entry helper to read the prefix key.
    if plan_show_options(args).print_prefix {
        println!("prefix C-b");
    }
    Ok(0)
}

fn accept_ok() -> Result<i32, ShimError> {
    Ok(0)
}

fn split_failure(detail: &str) -> ShimError {
    ShimError(format!(
        "create pane failed: {SPLIT_FAILURE_HINT}: {detail}"
    ))
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Map a transport failure onto one concise tmux-shaped stderr line. The shim
/// owns this mapping precisely so no JSON envelope can reach either stream.
fn describe_client_error(err: &ApiClientError) -> String {
    match err {
        ApiClientError::Io(io_err) if is_connect_failure(io_err.kind()) => {
            "no server running".to_string()
        }
        ApiClientError::Io(io_err) if is_timeout(io_err.kind()) => format!(
            "{ERROR_PREFIX}server did not respond within {}ms",
            SHIM_REQUEST_TIMEOUT.as_millis()
        ),
        ApiClientError::Io(io_err) => format!("{ERROR_PREFIX}{io_err}"),
        ApiClientError::Json(_) => format!("{ERROR_PREFIX}malformed response from server"),
        ApiClientError::EmptyResponse => format!("{ERROR_PREFIX}empty response from server"),
        ApiClientError::ErrorResponse(response) => {
            format!("{ERROR_PREFIX}{}", response.error.message)
        }
        ApiClientError::UnexpectedResult(_) => {
            format!("{ERROR_PREFIX}unexpected response from server")
        }
    }
}

fn is_connect_failure(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

fn is_timeout(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Pull a string out of the response result, trying dotted paths in order.
fn result_str(response: &serde_json::Value, paths: &[&str]) -> Option<String> {
    let result = response.get("result")?;
    for path in paths {
        let mut node = result;
        let mut found = true;
        for part in path.split('.') {
            match node.get(part) {
                Some(next) => node = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(value) = node.as_str() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Exec a real tmux found on PATH, skipping our own shim. When no real tmux
/// exists, emulate tmux's own "no server" failure.
fn passthrough(args: &[String], parsed: &ParsedInvocation) -> i32 {
    let own = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    if let Some(candidate) = find_real_tmux(&path_var, own.as_deref()) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // `exec` only returns on failure.
            let err = std::process::Command::new(&candidate).args(args).exec();
            eprintln!("{ERROR_PREFIX}{err}");
            return 1;
        }
        #[cfg(not(unix))]
        {
            return match std::process::Command::new(&candidate).args(args).status() {
                Ok(status) => status.code().unwrap_or(1),
                Err(err) => {
                    eprintln!("{ERROR_PREFIX}{err}");
                    1
                }
            };
        }
    }

    if parsed.version_only {
        println!("{COMPAT_VERSION}");
        return 0;
    }
    eprintln!(
        "no server running ({ERROR_PREFIX}real tmux not installed, and this invocation is outside the serviced Agent Teams surface)"
    );
    1
}

/// First `tmux` on PATH that is not our own binary. `own` is the canonicalised
/// path of the running executable.
fn find_real_tmux(path_var: &OsStr, own: Option<&Path>) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(TMUX_BINARY_NAME);
        if !candidate.is_file() {
            continue;
        }
        if let (Some(canonical), Some(own)) = (candidate.canonicalize().ok().as_deref(), own) {
            if canonical == own {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn serviced(socket: &str, args: &[&str]) -> bool {
        should_service(&parse_invocation(&strings(args)), Some(socket))
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "karvex-tmux-compat-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // -- dispatch ----------------------------------------------------------

    #[test]
    fn invoked_as_tmux_matches_only_the_tmux_stem() {
        assert!(invoked_as_tmux("tmux"));
        assert!(invoked_as_tmux("/usr/local/bin/tmux"));
        assert!(invoked_as_tmux("./shims/tmux"));
        assert!(!invoked_as_tmux("kvx"));
        assert!(!invoked_as_tmux("/usr/bin/kvx"));
        assert!(!invoked_as_tmux("/tmp/tmux-wrapper"));
    }

    // -- invocation parsing ------------------------------------------------

    #[test]
    fn parses_socket_and_subcommand() {
        let parsed = parse_invocation(&strings(&[
            "-S",
            "/tmp/karvex.sock",
            "display-message",
            "-p",
            "#{pane_id}",
        ]));
        assert_eq!(parsed.socket_path.as_deref(), Some("/tmp/karvex.sock"));
        assert_eq!(parsed.subcommand.as_deref(), Some("display-message"));
        assert_eq!(parsed.args, strings(&["-p", "#{pane_id}"]));
        assert!(!parsed.version_only);
    }

    #[test]
    fn parses_version_flag() {
        let parsed = parse_invocation(&strings(&["-V"]));
        assert!(parsed.version_only);
        assert!(parsed.subcommand.is_none());
    }

    // -- servicing gates ---------------------------------------------------

    #[test]
    fn named_socket_is_not_serviced() {
        let parsed = parse_invocation(&strings(&["-L", "claude", "has-session", "-t", "x"]));
        assert_eq!(parsed.named_socket.as_deref(), Some("claude"));
        assert!(!should_service(&parsed, Some("/tmp/karvex.sock")));
        // Even a serviced subcommand stays unserviced under `-L`.
        assert!(!serviced(
            "/tmp/karvex.sock",
            &["-L", "claude", "kill-pane", "-t", "w1:p1"]
        ));
    }

    #[test]
    fn socket_mismatch_is_not_serviced() {
        assert!(!serviced(
            "/tmp/karvex-a.sock",
            &["-S", "/tmp/karvex-b.sock", "kill-pane", "-t", "w1:p1"]
        ));
        assert!(serviced(
            "/tmp/karvex-a.sock",
            &["-S", "/tmp/karvex-a.sock", "kill-pane", "-t", "w1:p1"]
        ));
    }

    #[test]
    fn unknown_subcommand_is_not_serviced() {
        assert!(!serviced(
            "/tmp/karvex.sock",
            &["new-session", "-s", "work"]
        ));
        assert!(!serviced("/tmp/karvex.sock", &[]));
    }

    #[test]
    fn no_resolvable_socket_is_not_serviced() {
        let parsed = parse_invocation(&strings(&["kill-pane", "-t", "w1:p1"]));
        assert!(!should_service(&parsed, None));
    }

    #[test]
    fn socket_paths_match_compares_canonicalised_paths() {
        let dir = scratch_dir("socket-compare");
        let real = dir.join("karvex.sock");
        std::fs::write(&real, b"").expect("write socket stand-in");
        let link = dir.join("alias.sock");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(not(unix))]
        std::fs::copy(&real, &link).expect("copy");

        assert!(socket_paths_match(
            &real.to_string_lossy(),
            &real.to_string_lossy()
        ));
        #[cfg(unix)]
        assert!(socket_paths_match(
            &link.to_string_lossy(),
            &real.to_string_lossy()
        ));
        assert!(!socket_paths_match(
            &dir.join("other.sock").to_string_lossy(),
            &real.to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- socket resolution (D5) --------------------------------------------

    #[test]
    fn socket_resolution_prefers_socket_path_then_tmux() {
        assert_eq!(
            socket_from_env_values(Some("/tmp/a.sock"), Some("/tmp/b.sock,42,0")),
            Some("/tmp/a.sock".to_string())
        );
        assert_eq!(
            socket_from_env_values(None, Some("/tmp/b.sock,42,0")),
            Some("/tmp/b.sock".to_string())
        );
        assert_eq!(
            socket_from_env_values(Some("  "), Some("/tmp/b.sock,42,0")),
            Some("/tmp/b.sock".to_string())
        );
        assert_eq!(socket_from_env_values(None, Some(",42,0")), None);
        assert_eq!(socket_from_env_values(None, None), None);
    }

    #[test]
    fn resolves_socket_from_karvex_socket_path_env() {
        let _guard = crate::integration::integration_env_lock();
        let previous_socket = std::env::var(crate::api::SOCKET_PATH_ENV_VAR).ok();
        let previous_tmux = std::env::var("TMUX").ok();

        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/karvex-explicit.sock");
        std::env::set_var("TMUX", "/tmp/karvex-from-tmux.sock,42,0");
        assert_eq!(
            resolve_socket_path(),
            Some("/tmp/karvex-explicit.sock".to_string())
        );

        restore_env(crate::api::SOCKET_PATH_ENV_VAR, previous_socket);
        restore_env("TMUX", previous_tmux);
    }

    #[test]
    fn resolves_socket_from_tmux_env_when_socket_path_unset() {
        let _guard = crate::integration::integration_env_lock();
        let previous_socket = std::env::var(crate::api::SOCKET_PATH_ENV_VAR).ok();
        let previous_tmux = std::env::var("TMUX").ok();

        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::set_var("TMUX", "/tmp/karvex-from-tmux.sock,42,0");
        assert_eq!(
            resolve_socket_path(),
            Some("/tmp/karvex-from-tmux.sock".to_string())
        );

        std::env::remove_var("TMUX");
        assert_eq!(resolve_socket_path(), None);

        restore_env(crate::api::SOCKET_PATH_ENV_VAR, previous_socket);
        restore_env("TMUX", previous_tmux);
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    /// D5: the shim must drive the socket it resolved, never
    /// `ApiClient::local()`'s default-session fallback.
    #[test]
    fn shim_targets_resolved_socket_not_default_session() {
        let _guard = crate::integration::integration_env_lock();
        let previous_socket = std::env::var(crate::api::SOCKET_PATH_ENV_VAR).ok();
        let previous_tmux = std::env::var("TMUX").ok();

        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::set_var("TMUX", "/tmp/karvex-scratch-target.sock,42,0");
        let resolved = resolve_socket_path().expect("socket resolves from TMUX");
        let shim = Shim::new(PathBuf::from(&resolved));
        assert_eq!(
            shim.client.socket_path(),
            PathBuf::from("/tmp/karvex-scratch-target.sock")
        );
        assert_ne!(shim.client.socket_path(), ApiClient::local().socket_path());

        restore_env(crate::api::SOCKET_PATH_ENV_VAR, previous_socket);
        restore_env("TMUX", previous_tmux);
    }

    /// Two servers, one request: the request must land on the socket the shim
    /// resolved and the other socket must never be opened.
    #[cfg(unix)]
    #[test]
    fn shim_request_reaches_only_the_resolved_socket() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let base = PathBuf::from("/tmp");
        let unique = format!("kvx-tc-{}", std::process::id());
        let wanted = base.join(format!("{unique}-a.sock"));
        let other = base.join(format!("{unique}-b.sock"));
        for path in [&wanted, &other] {
            let _ = std::fs::remove_file(path);
        }

        let wanted_listener = UnixListener::bind(&wanted).expect("bind wanted socket");
        let other_listener = UnixListener::bind(&other).expect("bind other socket");
        other_listener
            .set_nonblocking(true)
            .expect("non-blocking other listener");

        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = wanted_listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let _ = tx.send(line);
            let mut stream = reader.into_inner();
            stream
                .write_all(
                    b"{\"id\":\"tmux-compat:pane:list\",\"result\":{\"type\":\"pane_list\",\"panes\":[]}}\n",
                )
                .expect("write response");
            stream.flush().expect("flush");
        });

        let shim = Shim::new(wanted.clone());
        let panes = shim.list_all_panes().expect("request succeeds");
        assert!(panes.is_empty());
        server.join().expect("server thread");

        let request = rx.recv().expect("request observed");
        assert!(
            request.contains("pane.list"),
            "unexpected request {request}"
        );
        assert!(
            other_listener.accept().is_err(),
            "the other server must never be contacted"
        );

        for path in [&wanted, &other] {
            let _ = std::fs::remove_file(path);
        }
    }

    // -- pane ids ----------------------------------------------------------

    #[test]
    fn encode_decode_pane_id_roundtrips() {
        for id in ["w1:p1", "w2:p10", "w1:p3"] {
            assert_eq!(decode_pane_id(&encode_pane_id(id)), id);
            assert_eq!(encode_pane_id(&decode_pane_id(id)), id);
        }
        for id in ["w1:t1", "w2:t7"] {
            assert_eq!(decode_window_id(&encode_window_id(id)), id);
        }
    }

    #[test]
    fn pane_ordinal_orders_creation() {
        assert!(pane_ordinal("w1:p2") < pane_ordinal("w1:p10"));
        assert!(pane_ordinal("w1:p1") < pane_ordinal("w1:p2"));
        // Pane numbers are base-32 encoded, so pane 10 is `pA`, not `p10`.
        assert_eq!(
            pane_ordinal(&crate::workspace::public_pane_id_for_number("w1", 10)),
            10
        );
        assert!(pane_ordinal("w1:p9") < pane_ordinal("w1:pA"));
        let ordered: Vec<String> = (1..=12)
            .map(|number| crate::workspace::public_pane_id_for_number("w1", number))
            .collect();
        let mut shuffled = ordered.clone();
        shuffled.reverse();
        shuffled.sort_by_key(|id| pane_ordinal(id));
        assert_eq!(shuffled, ordered);
    }

    #[test]
    fn foreign_tmux_pane_ids_are_not_trusted() {
        assert!(looks_like_foreign_pane_id("%12"));
        assert!(!looks_like_foreign_pane_id("w1:p2"));
        assert!(!looks_like_foreign_pane_id(""));
    }

    // -- split -------------------------------------------------------------

    #[test]
    fn split_flags_extracted() {
        let args = strings(&[
            "-d",
            "-t",
            "w1:p1",
            "-h",
            "-l",
            "70%",
            "-P",
            "-F",
            "#{pane_id}",
            "--",
            "sleep 100000",
        ]);
        let plan = plan_split_window(&args);
        assert_eq!(plan.target.as_deref(), Some("w1:p1"));
        assert!(plan.print_pane_id);
        assert_eq!(trailing_command(&args).as_deref(), Some("sleep 100000"));
    }

    #[test]
    fn trailing_command_absent() {
        assert_eq!(trailing_command(&strings(&["-t", "w1:p1"])), None);
        assert_eq!(trailing_command(&strings(&["-t", "w1:p1", "--"])), None);
    }

    #[test]
    fn split_ratio_inverts_tmux_percentage() {
        let plan = plan_split_window(&strings(&["-t", "w1:p1", "-h", "-l", "70%"]));
        let ratio = plan.ratio.expect("ratio parsed");
        assert!(
            (ratio - 0.30).abs() <= f32::EPSILON * 4.0,
            "unexpected ratio {ratio}"
        );
    }

    #[test]
    fn split_ratio_clamped_at_bounds() {
        let low = plan_split_window(&strings(&["-l", "5%"]))
            .ratio
            .expect("ratio parsed");
        assert!((low - 0.9).abs() <= f32::EPSILON * 4.0, "unexpected {low}");
        let high = plan_split_window(&strings(&["-l", "95%"]))
            .ratio
            .expect("ratio parsed");
        assert!(
            (high - 0.1).abs() <= f32::EPSILON * 4.0,
            "unexpected {high}"
        );
        assert_eq!(plan_split_window(&strings(&["-l", "20"])).ratio, None);
        assert_eq!(plan_split_window(&strings(&[])).ratio, None);
    }

    #[test]
    fn split_direction_defaults_to_right_and_honours_v() {
        assert_eq!(
            plan_split_window(&strings(&["-t", "w1:p1", "-h"])).direction,
            SplitDirection::Right
        );
        assert_eq!(
            plan_split_window(&strings(&["-t", "w1:p1"])).direction,
            SplitDirection::Right
        );
        assert_eq!(
            plan_split_window(&strings(&["-t", "w1:p1", "-v"])).direction,
            SplitDirection::Down
        );
        // A `-v` inside the holder command must not flip the direction.
        assert_eq!(
            plan_split_window(&strings(&["-t", "w1:p1", "--", "grep", "-v", "x"])).direction,
            SplitDirection::Right
        );
    }

    #[test]
    fn split_d_flag_maps_to_focus_false_and_absent_d_focuses() {
        assert!(!plan_split_window(&strings(&["-d", "-t", "w1:p1", "-h"])).focus);
        assert!(plan_split_window(&strings(&["-t", "w1:p1", "-h"])).focus);
    }

    #[test]
    fn split_failure_message_mentions_no_space() {
        let message = split_failure("pane_split_failed").to_string();
        assert!(message.contains("no space"), "unexpected {message}");
        assert!(message.contains("too small"), "unexpected {message}");
    }

    // -- respawn -----------------------------------------------------------

    #[test]
    fn respawn_plan_extracts_target_and_command() {
        let plan = plan_respawn_pane(&strings(&[
            "-k",
            "-t",
            "w1:p2",
            "--",
            "claude --agent-name teammate-a",
        ]));
        assert_eq!(plan.target.as_deref(), Some("w1:p2"));
        assert_eq!(
            plan.command.as_deref(),
            Some("claude --agent-name teammate-a")
        );
        assert_eq!(plan_respawn_pane(&strings(&["-t", "w1:p2"])).command, None);
    }

    #[test]
    fn shell_readiness_requires_two_stable_non_empty_samples() {
        assert!(!shell_looks_ready("", ""));
        assert!(!shell_looks_ready("   ", "   "));
        assert!(!shell_looks_ready("", "user@host:~$"));
        assert!(!shell_looks_ready("user@host:~", "user@host:~$"));
        assert!(shell_looks_ready("user@host:~$", "user@host:~$"));
    }

    // -- send-keys ---------------------------------------------------------

    #[test]
    fn send_keys_detects_trailing_enter() {
        let plan = plan_send_keys(&strings(&["-t", "w1:p2", "echo", "hi", "Enter"]));
        assert_eq!(plan.target.as_deref(), Some("w1:p2"));
        assert_eq!(plan.text, "echo hi");
        assert_eq!(plan.keys, strings(&["Enter"]));

        let plan = plan_send_keys(&strings(&["-t", "w1:p2", "echo", "hi"]));
        assert_eq!(plan.text, "echo hi");
        assert!(plan.keys.is_empty());
    }

    #[test]
    fn send_keys_strips_flag_values() {
        let plan = plan_send_keys(&strings(&["-l", "-t", "w1:p2", "-R", "payload", "Enter"]));
        assert_eq!(plan.target.as_deref(), Some("w1:p2"));
        assert_eq!(plan.text, "payload");
        assert_eq!(plan.keys, strings(&["Enter"]));
    }

    // -- display-message ---------------------------------------------------

    #[test]
    fn display_message_selects_format() {
        assert_eq!(
            plan_display_message(&strings(&["-p", "#{pane_id}"])),
            DisplayMessagePlan {
                target: None,
                format: DisplayFormat::PaneId,
            }
        );
        assert_eq!(
            plan_display_message(&strings(&["-t", "w1:p2", "-p", "#{window_id}"])),
            DisplayMessagePlan {
                target: Some("w1:p2".to_string()),
                format: DisplayFormat::WindowId,
            }
        );
        assert_eq!(
            plan_display_message(&strings(&["-p", "#{client_control_mode}"])).format,
            DisplayFormat::ClientControlMode
        );
        assert_eq!(
            plan_display_message(&strings(&["-p", "#{client_termtype}"])).format,
            DisplayFormat::ClientTermtype
        );
    }

    #[test]
    fn display_message_unknown_format_is_empty() {
        assert_eq!(
            plan_display_message(&strings(&["-p", "#{session_name}"])).format,
            DisplayFormat::Unsupported
        );
        assert_eq!(
            plan_display_message(&strings(&["-p"])).format,
            DisplayFormat::Unsupported
        );
        assert_eq!(
            plan_display_message(&strings(&[])).format,
            DisplayFormat::Unsupported
        );
    }

    // -- select-pane / kill-pane / show-options ----------------------------

    #[test]
    fn select_pane_title_maps_to_rename() {
        let plan = plan_select_pane(&strings(&["-t", "w1:p2", "-T", "teammate-a"]));
        assert_eq!(plan.target.as_deref(), Some("w1:p2"));
        assert_eq!(plan.title.as_deref(), Some("teammate-a"));
    }

    #[test]
    fn select_pane_without_title_accepts() {
        let plan = plan_select_pane(&strings(&["-t", "w1:p2"]));
        assert_eq!(plan.title, None);
        assert_eq!(accept_ok(), Ok(0));
    }

    #[test]
    fn kill_pane_plan_reads_target() {
        assert_eq!(
            plan_kill_pane(&strings(&["-t", "w1:p2"])).target.as_deref(),
            Some("w1:p2")
        );
        assert_eq!(plan_kill_pane(&strings(&[])).target, None);
    }

    #[test]
    fn show_options_prints_prefix_only_when_asked() {
        assert!(plan_show_options(&strings(&["-g", "prefix"])).print_prefix);
        assert!(!plan_show_options(&strings(&["-g", "status"])).print_prefix);
    }

    // -- set-option / teammate accent colour (D8) ---------------------------

    #[test]
    fn set_option_plan_extracts_target_option_and_value() {
        let plan = plan_set_option(&strings(&[
            "-p",
            "-t",
            "w1:p2",
            "pane-border-style",
            "fg=cyan",
        ]));
        assert_eq!(plan.target.as_deref(), Some("w1:p2"));
        assert_eq!(plan.option.as_deref(), Some("pane-border-style"));
        assert_eq!(plan.value.as_deref(), Some("fg=cyan"));
    }

    #[test]
    fn set_option_extracts_fg_colour_from_style_value() {
        assert_eq!(
            plan_accent("pane-border-style", "fg=cyan"),
            AccentPlan::Set("cyan".to_string())
        );
        assert_eq!(
            plan_accent("pane-active-border-style", "fg=magenta"),
            AccentPlan::Set("magenta".to_string())
        );
        // `bg=default,fg=<c>` is a comma list; the fg component is still
        // extracted correctly.
        assert_eq!(
            plan_accent("window-style", "bg=default,fg=blue"),
            AccentPlan::Set("blue".to_string())
        );
    }

    #[test]
    fn set_option_normalises_claude_colour_numbers() {
        assert_eq!(
            plan_accent("pane-border-style", "fg=colour208"),
            AccentPlan::Set("orange".to_string())
        );
        assert_eq!(
            plan_accent("pane-border-style", "fg=colour205"),
            AccentPlan::Set("pink".to_string())
        );
    }

    #[test]
    fn set_option_unknown_option_is_accepted_without_metadata() {
        assert_eq!(plan_accent("remain-on-exit", "on"), AccentPlan::NotAccent);
        assert_eq!(
            plan_accent("pane-border-status", "top"),
            AccentPlan::NotAccent
        );
    }

    #[test]
    fn set_option_pane_border_format_does_not_yield_an_accent() {
        // The `fg=` trap: `pane-border-format`'s format string legitimately
        // contains a literal `fg=`, but the option name is not one of the
        // three accent-carrying options, so it must never be scanned.
        assert_eq!(
            plan_accent(
                "pane-border-format",
                "#[fg=colour208,bold] #{pane_title} #[default]"
            ),
            AccentPlan::NotAccent
        );
    }

    #[test]
    fn set_option_unknown_colour_is_dropped_not_propagated() {
        assert_eq!(
            plan_accent("pane-border-style", "fg=colour99"),
            AccentPlan::Unknown
        );
    }

    #[test]
    fn set_option_empty_fg_clears_the_token() {
        assert_eq!(plan_accent("pane-border-style", "fg="), AccentPlan::Clear);
        assert_eq!(
            plan_accent("window-style", "bg=default,fg="),
            AccentPlan::Clear
        );
    }

    /// D8's rename from `agent.accent` to `agent_accent`: guards against the
    /// dotted key silently coming back, since `normalize_metadata_tokens`
    /// (`src/app/api_helpers.rs`) rejects `[^A-Za-z0-9_-]` outright.
    #[test]
    fn set_option_accent_token_key_is_api_valid() {
        assert!(!AGENT_ACCENT_TOKEN_KEY.is_empty());
        assert!(AGENT_ACCENT_TOKEN_KEY
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')));
        assert!(!AGENT_ACCENT_TOKEN_KEY.contains('.'));
    }

    #[test]
    fn set_option_accent_seq_is_monotonic_per_process() {
        let first = next_accent_seq();
        let second = next_accent_seq();
        let third = next_accent_seq();
        assert!(second > first);
        assert!(third > second);
    }

    // -- transport error mapping -------------------------------------------

    #[test]
    fn dead_socket_maps_to_plain_no_server_running() {
        let err = ApiClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert_eq!(describe_client_error(&err), "no server running");
    }

    #[test]
    fn shim_never_prints_a_json_envelope_on_error() {
        let errors = [
            ApiClientError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            )),
            ApiClientError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
            ApiClientError::Io(std::io::Error::other("boom")),
            ApiClientError::EmptyResponse,
            ApiClientError::UnexpectedResult("Pong".to_string()),
        ];
        for err in &errors {
            let message = describe_client_error(err);
            assert!(!message.contains('{'), "json-ish stderr: {message}");
            assert!(!message.contains('}'), "json-ish stderr: {message}");
            assert!(!message.contains('\n'), "multi-line stderr: {message}");
        }
    }

    // -- passthrough -------------------------------------------------------

    #[test]
    fn passthrough_skips_own_binary() {
        let dir = scratch_dir("passthrough");
        let shim = dir.join(TMUX_BINARY_NAME);
        std::fs::write(&shim, b"#!/bin/sh\n").expect("write fake tmux");
        let own = shim.canonicalize().expect("canonicalize fake tmux");
        let path_var = std::env::join_paths([dir.as_path()]).expect("join paths");

        assert_eq!(find_real_tmux(&path_var, Some(&own)), None);
        assert_eq!(
            find_real_tmux(&path_var, Some(Path::new("/nonexistent/kvx"))),
            Some(shim)
        );
        assert_eq!(
            find_real_tmux(
                &std::env::join_paths([dir.join("empty")]).expect("join paths"),
                Some(&own)
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
