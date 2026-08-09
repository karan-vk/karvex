# Claude Code Agent Teams → Karvex native panes: build plan

Status: proposed (not started) — **adversarially reviewed 2026-08-09, see §10**
Author: planning pass, 2026-08-09
Reviewer pass: 2026-08-09 (every path / line reference / recipe / CLI verb
re-verified against both trees; 5 blocking and 10 medium defects fixed in place)
Target: `master`, unreleased (latest release `v0.10.2`)

## Provenance

The design ported here originates in [`bakr`](https://github.com/nbaker47/bakr), a
sibling fork of the same `herdr` upstream that Karvex forks. bakr is Apache-2.0,
as is Karvex, so the code may be adapted directly; the commit arc is
`f9b223e → ffa81b3 → cb0ed15 → 205c063 → 2ebb8f5 → f4d05aa → 7d69200` (all
2026-07-28, author nbaker47). Attribution belongs in the Karvex spec document
produced by W8 and in the changelog entry.

What we take: the *shape* — export a tmux identity into panes, put a `tmux`-named
symlink to our own binary on PATH, dispatch on argv[0] stem, translate a narrow
tmux surface onto the JSON API.

What we do not take: the `BAKR_*`/`HERDR_*` dual-alias env scheme, the harness
kit, the SessionStart-only hook regression, and the donor's near-total absence of
tests.

---

## 1. Scope

### In scope

- **A. Core port.** tmux-compatible pane identity, the `tmux` shim binary
  surface, the full translation table.
- **B. Auto-install** of Karvex's existing Claude hook integration on server
  start, with an opt-out.
- **C. Beat the donor** where cheap: DCS passthrough correctness (new — see §4
  R1), teammate-variant detection, teammate accent colour, `no space` failure
  strings, and real tests.
- **D. Docs**: `docs/next` website content (en + ja + zh-cn), `SKILL.md`,
  `docs/next/CHANGELOG.md`, and a Karvex spec doc for the Claude tmux surface.

### Explicitly out of scope

- **bakr's harness kit** (`src/cli/harness.rs`, claude-harness skill/agent
  symlinking). Unrelated subsystem; no Karvex equivalent is planned here.
- **Windows tmux-compat.** Windows must *compile cleanly* and must *not* export
  `TMUX` (see D10) — a documented non-goal, not a silent gap. No `cfg(windows)`
  implementation beyond the no-op arm.
- **`claude --tmux` external-session helper** (`new-session -A`,
  `switch-client`, `attach-session`, `-L` named-socket mode). The shim refuses
  `-L` and passes it through to a real tmux. `show-options -g prefix` is stubbed
  only because it is one line.
- **tmux control mode** (`DCS 1000 p`) and any attempt to be a general tmux
  replacement.
- **Rendering a leader-30% / teammates-stacked layout.** `select-layout` and
  `resize-pane` stay accept-and-drop; Karvex's own split geometry stands in. (A
  real layout mapping is a follow-up, not a launch requirement.)

---

## 2. Verified ground truth

Everything below was read in both repos during planning. Recon claims that
survived verification are marked ✅; claims that changed are marked ⚠.

### Donor (`/home/karan/code/bakr`)

| Fact | Location | Status |
|---|---|---|
| `apply_tmux_compat_env(cmd, pane_id)` sets `TMUX=<socket>,<pid>,0` + `TMUX_PANE=<pane_id>`, opt-out `BAKR_NO_TMUX_COMPAT=1`, then prepends the shim dir to PATH | `src/pane.rs:149-167` | ✅ |
| Called only for `PaneLaunchIdentity::Managed` | `src/pane.rs:133` | ✅ |
| `ensure_tmux_shim()` unix-only, `<data_dir>/shims/tmux` symlink to `current_exe()`, stem guard `bakr`/`herdr`, macOS `~/.local/bin` mirror | `src/pane.rs:171-211` | ✅ |
| `install_shim_symlink()` never clobbers a real file or a foreign symlink | `src/pane.rs:216-256` | ✅ |
| argv[0] stem dispatch before all arg parsing | `src/main.rs:473-495` | ✅ |
| Translation table, `SERVICED` list, `should_service()` gates | `src/cli/tmux_compat.rs:28-45, 57-89, 178-197` | ✅ |
| `-l N%` inverted to the *existing* pane's ratio, clamped `0.1..0.9` | `src/cli/tmux_compat.rs:373-376` | ✅ |
| Trailing `-- <cmd>` on `split-window` deliberately not run | `src/cli/tmux_compat.rs:379-382` | ✅ |
| `respawn-pane` = `ctrl+u` then `<cmd>` + `Enter` via `pane.send_input` | `src/cli/tmux_compat.rs:409-441` | ✅ |
| `wait_for_shell(pane_id)` — a "bounded 10 × 150ms poll for the shell to be ready" | `src/cli/tmux_compat.rs:443-454` | ⚠ **It is a no-op.** Both success arms return immediately (`Ok(info) if info.agent.is_none() => return, Ok(_) => return`); it only sleeps when `pane_info` *errors*, i.e. when the pane does not exist yet. It never waits for a shell prompt. Do **not** port it as-is — see W2/R4. |
| Shim transport is `super::send_request` — the *checked* helper, with the protocol round trip | `src/cli/tmux_compat.rs:534` | ✅ (Karvex deviates, D3) |
| `passthrough` execs the first `tmux` on PATH whose canonicalised path differs from `current_exe()`; falls back to printing the version banner for `-V`, else `no server running` on stderr, exit 1 | `src/cli/tmux_compat.rs:579-612` | ✅ |
| Auto-install on server start, opt-out `BAKR_NO_AUTO_INTEGRATION=1`, no test | `src/server/headless.rs:4359-4380`, called at `:4399` | ✅ |
| 5 pure-logic unit tests; **zero** coverage of env/shim/dispatch/API | `src/cli/tmux_compat.rs:614-680` | ✅ |
| Claude's colours: `red blue green yellow magenta cyan colour208 colour205`, swallowed by the `set-option` no-op | `docs/claude-tmux-spec.md:60-61` | ✅ |
| Split failure stderr should contain `no space`/`too small`; bakr only does so incidentally, in one branch | `docs/claude-tmux-spec.md:90-92`; `tmux_compat.rs:397-401` | ⚠ bakr *does* include "no space" in the unexpected-response branch only, not on a genuine API error. Karvex must cover both. |

### Target (`/home/karan/code/karvex`)

| Fact | Location |
|---|---|
| Injection point `apply_pane_launch_env`, `Managed` arm sets the three id vars | `src/pane.rs:112-138` ✅ |
| `apply_pane_base_env` sets `KARVEX_SOCKET_PATH` | `src/integration/env.rs` ✅ |
| Platform wrapper pattern: `pub(crate) fn f(...)` + `#[cfg(not(windows))] fn f_platform(...) {}` | `src/platform/mod.rs:38-43` ✅ (per-OS modules wired at `:211-228`; a `#[cfg(unix)]` block directly in `mod.rs` is also existing style, `:94-173`) |
| `session::data_dir()` | `src/session.rs:157` ✅ |
| `active_api_socket_path()` prefers `$KARVEX_SOCKET_PATH` when no explicit session was requested — **but falls back to `api_socket_path_for(active_name())`, it does not return `None`** | `src/session.rs:173-181` ⚠ see D5 |
| `main()` runs `args_as_utf8` (`:475`) → `configure_from_args` → `extract_remote_args` → `cli::maybe_run`; `fn main` at `:488` | `src/main.rs:488-528` ✅ |
| `session::configure_from_args` interprets `session attach <name>`, `--session`/`--session=`, and stops at `--`; `apply_explicit_name` mutates the process-global `EXPLICIT_SESSION_REQUESTED` | `src/session.rs:29-90` ✅ (D4's hazard is real) |
| `cli::send_request` performs an **extra `status()` round trip** for the protocol guard and can print a JSON error envelope; `send_request_unchecked` does not | `src/cli.rs:814-848` ✅ |
| **Both** `send_request` and `send_request_unchecked` build `ApiClient::local()` → `ConnectionTarget::LocalSession(None)` → `crate::api::socket_path()` → `session::active_api_socket_path()`. Neither can be pointed at an explicit socket. `ApiClient::for_target(ConnectionTarget::SocketPath(p))` + `request_value_with_timeout` can, and both are `pub`. | `src/api/client.rs:14-75`, `src/api/mod.rs:99-101` ⚠ **new** — see D5 |
| `crate::platform::restore_default_sigpipe()` is `pub(crate)` with a unix impl and a non-unix no-op | `src/platform/mod.rs:136-148`; call precedent `src/cli.rs:142-144` ✅ |
| Bin name is `kvx`; package name is `karvex` | `Cargo.toml:2, 22-24` ✅ |
| `PaneInfo` has `pane_id, terminal_id, workspace_id, tab_id, focused, cwd, foreground_cwd, label, agent, title, terminal_title, terminal_title_stripped, display_agent, agent_status, state_labels, tokens, agent_session, scroll, revision` — **no colour/accent field** | `src/api/schema/panes.rs:398-431` ✅ |
| `PaneSplitParams { workspace_id, target_pane_id, direction, ratio: Option<f32>, cwd, focus, env }` (note `workspace_id`; `focus` defaults to `false`) | `src/api/schema/panes.rs:11-27` ✅ |
| Split ratio is the **first/existing** pane's share (`splits[].ratio`, `first`/`second`) | `src/app/api/panes.rs:3595-3630` (`api_pane_resize_changes_target_ratio_without_changing_focus` asserts `splits[0].ratio`); `layout.export`/`layout.apply` tree docs at `docs/next/website/src/content/docs/socket-api.mdx:205-235` ⚠ citation corrected — the mdx lines document the *layout* tree, not `pane.split` |
| Split failure surfaces as `pane_split_failed` / `pane_not_found`; **there is no minimum-pane-size guard** | `src/app/api/panes.rs:99-103` ✅ |
| `pane.report_metadata` is the sanctioned external-presentation channel (`source`, `state_labels`, `tokens` patch, `seq`, `ttl_ms`) | `src/api/schema/panes.rs:349-377` (`PaneReportMetadataParams`) ✅ |
| **Metadata `source` allows `[A-Za-z0-9]`, `:`, `.`, `_`, `-`, ≤80 chars — so `karvex:tmux-compat` is valid. Metadata token *keys* allow only `[A-Za-z0-9_-]` — a dot is REJECTED with `invalid_metadata_token`.** Max 32 token keys per pane. | `src/app/api_helpers.rs:209, 213-230, 247-280` ⚠ **new** — kills `agent.accent`, see D8 |
| Sidebar renders a metadata token as text **only when the user's sidebar config names it** (`AgentSidebarToken::Custom(key)` → `ResolvedTokenKind::Custom`); it is not rendered by default | `src/config/sidebar.rs:104-118`, `src/ui/sidebar/tokens.rs:76`, style application in `src/ui/sidebar.rs:990-1140` ⚠ recon overstated this |
| Claude hook reports **both** `session` → `pane.report_agent_session` and `stop` → `pane.report_agent{state:idle}`; ignores events carrying `agent_id` (`is_subagent`, line 52-54), and `SubagentStop` (line 55) | `src/integration/assets/claude/karvex-agent-state.sh` ✅ |
| `CLAUDE_INTEGRATION_VERSION = 8`, and **8 is what shipped in v0.10.2** (`git show v0.10.2:src/integration/mod.rs`) | `src/integration/mod.rs:40` ✅ |
| No auto-install-on-server-start anywhere; `run_server` at `headless.rs:4711`; `--handoff-import` early-returns at `:4732-4741`; `config::Config::load()` at `:4743` | `src/server/headless.rs:4711-4743` ✅ (W4's "call site is after the handoff early return" holds) |
| `installed_integration_statuses() -> Vec<IntegrationStatus{target, path, state, installed_version, expected_version}>`, `IntegrationStatusKind::{NotInstalled, Current, Outdated}`, `install_target(IntegrationTarget) -> io::Result<Vec<String>>` | `src/integration/registry.rs:220`, `src/integration/types.rs:121-134`, `src/integration/actions.rs:16` ✅ |
| Manifest edits must mirror the bundled manifest into `website/agent-detection/<id>.toml`, enforced by `scripts/agent_detection_manifest_check.py:288-350`. **`index.toml` entries are exactly `{id, path}` — there is no version and no sha256 field there, and `claude` is already listed.** The real rules: website `version` ≥ bundled `version`; if the versions are equal the two files must be **byte-identical**; every bundled manifest must appear in the catalog and vice versa. | `website/agent-detection/index.toml:12-13` ⚠ plan's step was wrong, see W6 |
| Docs parity enforces (a) the **same set of `*.mdx` filenames** in `ja/` and `zh-cn/` as in the en root, and (b) identical **heading-level outlines** (levels only, not text; fenced blocks skipped) | `scripts/docs_translation_parity.py:11-60` ✅ |
| Karvex's own code wraps notifications in `\ePtmux;…` **when `$TMUX` is set** | `src/terminal_notify.rs:46-97` ✅ |
| Karvex's pane byte stream **drops** `\ePtmux;…\e\\` passthrough entirely | Authoritative: libghostty-vt `vendor/libghostty-vt/src/terminal/dcs.zig:52-78` — `ESC P` + `t` parses as a DCS with `final='t'`, `tryHook` returns `null`, the handler sets `state = .ignore` and discards every subsequent byte until unhook. `DCS 1000 p` control mode is the only `intermediates.len()==0` hook, and it is additionally gated on `build_options.tmux_control_mode`. Corroborating (tracker-layer only): `src/pane/osc.rs:1321-1330`. ✅ |
| Karvex **does** support OSC 52 clipboard writes end to end: `process_pty_bytes` → `ProcessBytesResult.clipboard_writes` → `src/pane.rs:1891, 2053` | `src/ghostty/mod.rs:537-605, 965`; existing green test `process_pty_bytes_surfaces_clipboard_writes_without_other_results` at `src/pane/terminal.rs:3447-3463` ✅ |
| Pane inbound byte path entry point is `GhosttyPaneTerminal::process_pty_bytes` (`src/pane/terminal.rs:1108`), called from `src/pane.rs:1873, 2033`. It already contains a `Cow<[u8]>` filter precedent (`maybe_filter_primary_screen_scrollback_clear`, `:1157-1165`). | ✅ — W3's seam |
| `pane.send_input` key names: `parse_api_key` → `config::parse_key_combo`, which accepts `ctrl+u` and `Enter` | `src/app/api_helpers.rs:11-23`, `src/config/keybinds.rs:1218-1250` ✅ |
| `tests/cli/harness.rs` already provides the scratch-server harness: `unique_test_dir`, `spawn_karvex`, `spawn_karvex_with_path`, `spawn_named_server`, `wait_for_socket`, `run_cli`, `run_cli_json`, `run_named_cli_with_env`, `wait_until` | `tests/cli/harness.rs:18-445` ✅ — W7 does **not** need to build one |
| **`tests/cli.rs` is `#![cfg(not(target_os = "macos"))]`** — the whole `tests/cli/` tree is skipped on macOS | `tests/cli.rs:1` ⚠ **new** — see W7 |
| `just` recipes that exist: `test`, `test-one`, `lint`, `ci`, `check-slim <filter>`, `windows-lint`, `check` (= `ci` + `windows-lint`, and `ci` already runs `check-slim`), `integration-assets-test`, `release-docs-check` | `justfile` ✅ |
| `kvx agent read --source detection`, `kvx agent explain --json`, `kvx server reload-agent-manifests`, `kvx pane get <pane_id>` all exist. **`kvx pane get` takes exactly one argument and already prints JSON — `--json` exits 2.** | `src/cli/agent.rs:732, 798`, `src/cli/server.rs:15`, `src/cli/pane.rs:80-96` ⚠ see §7 step 8 |
| No file this plan touches is behind `#[cfg(feature = "workflow")]` | `grep 'cfg(feature' src/pane.rs src/cli.rs src/main.rs src/ui/sidebar*.rs src/pane/terminal.rs src/platform/mod.rs src/server/headless.rs` → empty ✅ (D9 holds) |

---

## 3. Key decisions

### D1 — Shim install gates the identity export (improvement over the donor)

bakr exports `TMUX`/`TMUX_PANE` first and *then* tries to install the shim. On
any platform or filesystem where the shim cannot be installed, Claude still
selects its tmux backend and then fails against whatever `tmux` it finds.

Karvex inverts the order: `apply_tmux_compat_env` calls
`platform::ensure_tmux_shim_dir()` **first** and exports `TMUX`, `TMUX_PANE` and
the PATH prepend **only on success**. On Windows and on any install failure the
pane env is unchanged and Claude falls back to its own backends. This makes the
Windows non-goal safe rather than merely undocumented.

### D2 — Unix-only code lives in `src/platform/tmux_shim.rs`

New module `src/platform/tmux_shim.rs`, `#[cfg(unix)]`, holding the symlink
install, the shim directory resolution, and the macOS `~/.local/bin` mirror
(`#[cfg(target_os = "macos")]` *inside* the platform module, which is where
target gates belong). `src/platform/mod.rs` exposes:

```rust
// src/platform/mod.rs
#[cfg(unix)]
mod tmux_shim;
#[cfg(unix)]
use tmux_shim::ensure_tmux_shim_dir_platform;

pub(crate) fn ensure_tmux_shim_dir() -> Option<std::path::PathBuf> { ensure_tmux_shim_dir_platform() }
#[cfg(not(unix))]
fn ensure_tmux_shim_dir_platform() -> Option<std::path::PathBuf> { None }
```

mirroring the existing `apply_pane_runtime_marker` pattern at
`src/platform/mod.rs:38-43` and the per-OS module wiring at `:211-228`.
`src/pane.rs` stays free of any `cfg`.

### D3 — Shim transport: an explicitly targeted `ApiClient`, not `cli::send_request*`

Two independent reasons to bypass the `cli::` helpers:

1. **Protocol round trip.** `cli::send_request` (`src/cli.rs:814`) runs
   `ensure_server_protocol_compatible`, which is a **second socket round trip**
   and can print a JSON envelope. Claude's probes have short timeouts (<2s per
   the spec) and it parses stdout exactly.
2. ⚠ **Socket targeting.** *Both* `send_request` and `send_request_unchecked`
   construct `ApiClient::local()`, which resolves through
   `session::active_api_socket_path()`. That helper falls back to the **default
   session's** socket path when `$KARVEX_SOCKET_PATH` is unset — see D5. Using
   either helper would make the shim's careful socket resolution decorative.

So the shim owns its transport:

```rust
let client = crate::api::client::ApiClient::for_target(
    crate::api::client::ConnectionTarget::SocketPath(resolved_socket),
);
let value = client.request_value_with_timeout(&request, SHIM_REQUEST_TIMEOUT)?;
```

`ApiClient`, `ConnectionTarget::SocketPath` and `request_value_with_timeout` are
all `pub` (`src/api/client.rs:16`, `:42`, `:63`).

Consequences of dropping the cli helpers, all of which the shim must now own:

- **`SHIM_REQUEST_TIMEOUT` is chosen explicitly: 1500 ms.** Claude treats a hang
  as a backend failure and its probes have short timeouts, so failing fast is
  strictly better than stalling its detection. The `send_request*` path had no
  timeout at all; this is a capability gained, not a compromise.
- **The shim maps `ApiClientError` itself** — one concise `tmux`-shaped line on
  stderr and a non-zero exit. Losing `map_server_not_running_or_io` is the
  point: no JSON envelope may appear on *either* stream. Nothing but the `-F`
  format's answer ever reaches stdout.
- **A connect failure inside the serviced path is `no server running` on stderr
  with exit 1 — not passthrough.** `should_service` only requires a *resolvable*
  socket, not a live one, so a dead server is reachable there; passing through
  would exec a real tmux carrying `-S <karvex socket>` and produce confusing
  output. Imitating real tmux's own failure is the correct behaviour.
- ⚠ **`send_request` / `send_request_unchecked` must not appear in
  `src/cli/tmux_compat.rs` at all.** No `super::` call into the cli helpers.
  They are the obvious-looking existing helper and a build agent will
  reintroduce them unless told not to; the only reason they are mentioned in
  this plan is to say they are wrong here.

A protocol mismatch between a stale shim symlink and a newer server surfaces as
a normal request error on stderr — acceptable, and cheaper than the guard.

**Server-down behaviour** (Claude probing a Karvex pane whose server died):
`-V` short-circuits before any socket work and still exits 0, so Claude's
`isAvailable` check keeps succeeding. Every serviced verb fails fast as above.
Tests: `shim_version_succeeds_with_no_server`,
`shim_serviced_verb_fails_fast_when_socket_is_dead`,
`shim_never_prints_a_json_envelope_on_error`.

The shim also calls `crate::platform::restore_default_sigpipe()` on entry, so
`tmux display-message -p '#{pane_id}' | head -1` exits `141` rather than
panicking (the same reasoning as `src/cli.rs:142-144`; the function is
`pub(crate)` with a non-unix no-op at `src/platform/mod.rs:147-148`, so the
call compiles on every target).

### D4 — Dispatch point in `main()`

The argv[0]-stem check goes **immediately after `args_as_utf8`** and before
`session::configure_from_args` — that function interprets `--session`, `--`, and
`session attach`, all of which appear in tmux argv with different meanings, and
it mutates process-global session state. On a non-UTF8 argv the existing error
path is kept (Claude never passes non-UTF8 args).

### D5 — Socket resolution

⚠ **Do not reuse `session::active_api_socket_path()`.** It prefers
`$KARVEX_SOCKET_PATH` only when `explicit_session_requested()` is false (which
it always is in the shim, since the shim dispatches before
`configure_from_args`) — but when the variable is *unset* it falls back to
`api_socket_path_for(active_name())`, i.e. the default session's socket
(`src/session.rs:173-181`). That is the exact failure D5 exists to prevent, and
it is reachable: `$TMUX` can carry a socket path while `$KARVEX_SOCKET_PATH` is
absent (a pane env inherited through `su`, `env -i`, a wrapper script, or a
teammate process spawned by something that scrubbed Karvex vars).

The shim resolves its own socket, with no fallback:

1. `$KARVEX_SOCKET_PATH` (non-empty after trim)
2. first comma-field of `$TMUX` (non-empty after trim)
3. otherwise: **not serviced** → passthrough.

The resolved path is then passed explicitly to
`ApiClient::for_target(ConnectionTarget::SocketPath(..))` per D3. The shim must
never construct `ApiClient::local()`.

`should_service` additionally compares a `-S <path>` argument against the
resolved path (canonicalised where both sides canonicalise, else a trimmed
string compare) and passes through on mismatch.

Tests: `resolves_socket_from_karvex_socket_path_env`,
`resolves_socket_from_tmux_env_when_socket_path_unset`,
`shim_targets_resolved_socket_not_default_session` (set `$TMUX` to a scratch
socket, leave `$KARVEX_SOCKET_PATH` unset, assert the request lands on the
scratch socket and the default-session socket is never opened).

### D6 — Ownership guard for the symlink

`current_exe()` file stem must equal `kvx` **exactly**. Cargo's test binaries are
`kvx-<hash>` and `karvex-<hash>` under `target/*/deps`, so exact-match excludes
them and nextest can never hijack a user's `tmux`. A pre-existing link is
replaceable only when its target stem is `kvx`, starts with `kvx-`/`karvex-`, or
lives under `session::data_dir()`; anything else logs a `tracing::warn!` and is
left alone. A real (non-symlink) file at the target path is never touched.

The stem test must be a **pure function** (`fn binary_owns_shim(stem: &str) ->
bool`) so it is unit-testable without spawning binaries.

**Dangling links.** Inspect the link with `symlink_metadata`, not `metadata`:
after a Homebrew/Nix/mise upgrade the recorded target (a versioned store path)
can vanish, so `Path::is_file()` on the link returns `false` while the link
itself still exists and still shadows the real `tmux`. A dangling link whose
recorded target stem passes `binary_owns_shim` is ours and must be re-pointed
at the current `current_exe()`; a dangling link to anything else is left alone
and logged. (`kvx update` replaces the binary with an in-place
`fs::rename` — `src/update.rs:619-622` — so the common upgrade path does not
break the link; package managers are the case that does.)

**Removal.** There is no `kvx uninstall` command, and `KARVEX_NO_TMUX_COMPAT=1`
deliberately does not delete anything (it only stops exporting). The
`<data_dir>/shims/tmux` link dies with the config dir, but the macOS
`~/.local/bin/tmux` mirror **outlives an uninstall of Karvex and keeps shadowing
the user's real tmux**. W1 must therefore: (a) never create the mirror unless
`~/.local/bin` already exists (do not create the directory), and (b) W8 must
document the exact removal command for both paths. Test:
`macos_mirror_is_not_created_when_local_bin_is_absent`.

### D7 — Hook reconciliation: keep Karvex's hook, do not adopt bakr's

Karvex's `karvex-agent-state.sh` handles `session` **and** `stop`, and its
`SubagentStop` guard carries a documented rationale about not reviving idle
panes. bakr's evolved down to SessionStart-only. Adopting that would regress
`stop → idle` reporting, which the workflow engine consumes
(`EngineInput::TurnEnded`). **Decision: no hook change, no
`CLAUDE_INTEGRATION_VERSION` bump.**

Conditional exception: the hook exits early when the event JSON carries
`agent_id` (`is_subagent`). It is **unverified** whether Claude's Agent-Teams
teammates — separate `claude` processes launched with `--agent-id`/`--agent-name`
— emit hook events carrying `agent_id`. If the gate-phase probe (§7 step 8)
shows teammate panes never report a session because of that guard, W9 makes the
minimal fix and bumps `CLAUDE_INTEGRATION_VERSION` **8 → 9 exactly once** (8 is
what shipped in v0.10.2, so one bump is correct per CLAUDE.md's
migration-version rule). If the probe is clean, the constant does not move.

### D8 — Teammate colour: `pane.report_metadata` token + a TUI-side style hint

Claude sends colours as `set-option -p -t %N pane-border-style fg=<c>` /
`window-style bg=default,fg=<c>` / `pane-active-border-style fg=<c>`, with
values from `{red, blue, green, yellow, magenta, cyan, colour208, colour205}`.

Classification under the runtime/client guardrail: *which* colour an agent was
assigned is a **shared runtime fact** — it is chosen by an external agent
runtime, it identifies the teammate, and any client (a future web client, `kvx
pane get --json`) would want it. *How* that colour is painted — border, row
accent, dot — is **TUI presentation**.

So: do not invent a new API field, and do not accept-and-drop. Use the channel
that already exists for exactly this: the shim maps the parsed colour onto
`pane.report_metadata` with `source = "karvex:tmux-compat"`, a monotonic `seq`,
no TTL, and a neutral reserved token key:

```json
{"tokens": {"agent_accent": "cyan"}}
```

⚠ **The key must be `agent_accent`, not `agent.accent`.** Metadata token keys are
validated by `normalize_metadata_tokens` (`src/app/api_helpers.rs:262-269`) as
`[A-Za-z0-9_-]` only — a dot is rejected with `invalid_metadata_token`. (The
`source` field *does* allow dots and colons, `:213-230`, so
`karvex:tmux-compat` is fine.) A pane accepts at most 32 token keys
(`MAX_METADATA_TOKEN_KEYS_PER_RESOURCE`).

Values are unproblematic: `normalize_metadata_tokens` trims, strips control
characters and length-caps, and plain colour names pass through cleanly.

Names stay surface-neutral (`agent_accent`, not `sidebar_*`).

⚠ **What the TUI side actually costs.** A metadata token is *not* rendered by
default: `src/ui/sidebar/tokens.rs:76-81` only resolves a token when the user's
sidebar config names it (`AgentSidebarToken::Custom(key)` →
`ResolvedTokenKind::Custom`, `src/config/sidebar.rs:104-118`). So "the token is
not rendered as text" is already true with no code at all — that is not a
feature to build or a test to write. The *real* work is tinting, and it is a
**render-path change, not a token-table change**, so W5 owns `src/ui/sidebar.rs`
too (see §5.10). The original "~30 lines in the sidebar" estimate is struck.

Likely-cheapest shape (confirm against the code before committing to it):
`AgentPanelEntry` already carries `pub tokens: HashMap<String, String>`
(`src/ui/sidebar.rs:39`), so the accent is probably reachable without a new
field. `resolved_token_spans` (`:990-1140`) already takes its styles as explicit
parameters and has four call sites (`:1340`, `:1502`, and two in tests), so
deriving an accent `Style` from `entry.tokens.get("agent_accent")` at the call
site and passing it in avoids both a new struct field and a new
`ResolvedTokenKind` variant. If the entry turns out not to be in hand at the
right moment, a field on `AgentPanelEntry` is the cheap fix — but **never** a
new enum variant (see the enum audit in §5.10).

**W5 is the lowest-priority workstream** and stays last: if it exceeds a day,
keep the `pane.report_metadata` write (the shared runtime fact is then still
visible via `kvx pane get`) and drop only the tint. It must not gate the gate
phase. Note the write-only variant is *only* acceptable as a time-boxed
fallback — as a target it would be accept-and-drop with extra steps, which is
what D8 exists to avoid.

### D9 — Unconditional, not feature-gated

tmux-compat has nothing to do with the `workflow` feature. All new code must
compile and pass in both `--features workflow` and `--no-default-features`
(`just check-slim`).

### D10 — Windows

`ensure_tmux_shim_dir()` returns `None`; by D1 nothing is exported; documented as
a non-goal in the docs workstream. `just windows-lint` must stay green.

### D11 — DCS `tmux;` passthrough must be unwrapped before we export `TMUX`

See §4 R1. This is a mandatory prerequisite, not a nicety.

---

## 4. Risks

### R1 — HIGHEST RISK: exporting `TMUX` silently breaks clipboard and colour queries inside panes

**Mechanism.** The near-universal convention among terminal apps (neovim's
OSC-52 clipboard provider, fzf, yazi, lazygit, tmux-aware shell prompts, and
Karvex's own `src/terminal_notify.rs:46`) is: *if `$TMUX` is set, wrap
OSC/DCS sequences in `\ePtmux;<escaped>\e\\` passthrough.* The moment
`apply_tmux_compat_env` exports `TMUX`, every such app starts wrapping.

**Verified, not assumed.** Karvex's terminal drops that wrapper, and the drop
happens inside libghostty-vt, not in Karvex code:
`vendor/libghostty-vt/src/terminal/dcs.zig:52-78` — for `\ePtmux;…` the VT
parser produces a DCS with `intermediates.len() == 0` and `final == 't'`;
`tryHook`'s `switch (dcs.final)` has only a `'p'` arm (tmux **control mode**,
additionally gated on `build_options.tmux_control_mode`), so it returns `null`,
`hook()` sets `state = .ignore`, and every payload byte is discarded until
unhook. Corroborating at the Karvex layer: `src/pane/osc.rs:1321-1330` asserts
the default-colour tracker sees nothing in a wrapped OSC 11 query. Karvex *does*
honour bare OSC 52 end to end — `src/ghostty/mod.rs:537-605` →
`ProcessBytesResult.clipboard_writes` → `src/pane.rs:1891, 2053`, with a green
test at `src/pane/terminal.rs:3447-3463`.

Net effect: **copy-to-clipboard from inside pane apps stops working**, and
background/foreground colour queries stop resolving — a broad regression
affecting every pane, not just Claude teammate panes, triggered by a feature
most users are not using.

**Mitigation (W3, mandatory, must land before or with W1).** Unwrap the
passthrough in the pane's inbound byte stream before it reaches the terminal:
recognise `\eP` + `tmux;`, un-double `\e\e` → `\e`, terminate at `\e\\` (and
tolerate a bare `\e\\` / `ST`), forward the inner bytes, and forward unrelated
DCS untouched. Must be a streaming state machine — PTY reads split anywhere —
with a bounded buffer that drops (not buffers unboundedly) an unterminated
sequence.

⚠ **The unwrap must run at the very top of `process_pty_bytes`, before
`core.default_color_tracker.observe(bytes)` (`src/pane/terminal.rs:1131`).**
Unwrapping only just before `write_pty_bytes_with_ordered_responses` would fix
clipboard but *not* the second half of this risk: the OSC 11/OSC 4 responses
Karvex synthesises come from `core.default_color_event_tracker`
(`:1179-1183`), and XTGETTCAP replies from `core.xtgettcap_query_tracker` —
both of which would still be fed wrapped bytes and still see nothing. Every
observer in `process_pty_bytes` (`default_color_tracker`, `osc_debug_tracker`,
`agent_osc_state`, `kitty_keyboard`, `default_color_event_tracker`,
`xtgettcap_query_tracker`, `decscusr_tracker`) must receive the unwrapped
stream. Acceptance for R1 is therefore two-part: clipboard **and** a
tmux-wrapped OSC 11 query producing a response.

**Fallback if W3 proves hard:** do not ship the `TMUX` export enabled by
default; invert the opt-out into an opt-in (`KARVEX_TMUX_COMPAT=1`) and document
the clipboard caveat. Shipping the export without W3 is not acceptable.

### R2 — Hijacking a user's real tmux

Guarded by D6 (exact-stem ownership, never clobber a real file or foreign
symlink) plus shim passthrough for everything outside the serviced surface. The
macOS `~/.local/bin` mirror is the riskiest write: it is outside Karvex's data
dir. Keep the same ownership rules there and log every refusal.

### R3 — PATH demotion

On macOS, `path_helper`/`brew shellenv`/user rc files re-order PATH after our
prepend, so a Homebrew `tmux` can win. Mitigated by the `~/.local/bin` mirror
(donor's `2ebb8f5`). If a real tmux does win, it receives `-S <karvex socket>`
and reports "no server running" — Claude reports a backend failure rather than
corrupting anything. Detectable in the gate phase.

### R4 — `respawn-pane` submits a command into a shell, not `exec`

Claude's teammate command arrives as one shell string and is typed into the
pane's shell. If the shell is not at a prompt (slow rc files, a bracketed-paste
prompt, an already-running program) the command can be mangled. This is the most
likely source of flaky teammate startup.

⚠ **Do not port the donor's `wait_for_shell`** (`bakr src/cli/tmux_compat.rs:443-454`):
it does not wait. Both success arms return immediately (`Ok(info) if
info.agent.is_none() => return, Ok(_) => return`); the 150ms sleep only runs
when `pane_info` *errors*, i.e. while the pane does not yet exist. Copying it
ships dead code that reads like a mitigation.

W2 implements a real bounded readiness wait instead, built from existing API
surface — poll `pane.read { source: "recent", lines: 2 }` (or
`pane.wait_for_output`) until the tail is non-empty and stable across two polls,
capped at 10 × 150ms, then send. If live testing (Q2) shows this is still flaky,
the fallback is a real `pane.run`-style path rather than typing into a shell —
a larger change, deliberately not planned here. Whatever ships, the helper's
loop condition must be a **pure function** over the observed tail so it is
unit-testable: `fn shell_looks_ready(prev: &str, now: &str) -> bool`.
Test: `shell_readiness_requires_two_stable_non_empty_samples`.

### R5 — Nested-karvex and prompt side effects

`TMUX` set inside panes changes unrelated behaviour: `src/input/model.rs:247`
enables `modifyOtherKeys` when `$TMUX` is set (that check reads the *karvex
process* env, so it only bites when Karvex runs inside a Karvex pane), and shell
prompt frameworks will start drawing a tmux session indicator. Cosmetic;
document the `KARVEX_NO_TMUX_COMPAT=1` opt-out prominently.

### R6 — `no space` / `too small` will rarely fire

Karvex has **no minimum-pane-size guard** (`src/app/api/panes.rs:99-103`); a
split essentially always succeeds. Including the substrings is still correct and
trivial, but be honest in the docs: Claude's friendlier "not enough space"
message is wired up, not frequently reachable.

### R7 — ⚠ the test suite inherits `TMUX`/`TMUX_PANE` from the developer's own session

Once W1 lands, running `just check` **from inside a Karvex pane** means the test
process itself has `TMUX`, `TMUX_PANE` and a shim-prefixed `PATH` in its
environment. Every W1/W2 test that reads those variables — socket resolution,
`should_service`, "opt-out leaves env unchanged", "PATH prepended exactly once"
— then depends on where the developer happened to run it. This is the same
class of failure as the known `KARVEX_*` scrub problem, and it will produce
green-locally/red-in-CI or the reverse.

Two mitigations, both required:

- §7's scrub list gains `TMUX` and `TMUX_PANE` (see §7).
- Every test that reads or writes those variables holds
  `crate::integration::integration_env_lock()` (`src/integration/env.rs:191`,
  exported `#[cfg(test)]` from `src/integration/mod.rs:13-14`, precedent
  `src/pane.rs:3330`) and explicitly **sets or removes** `TMUX`, `TMUX_PANE`,
  `KARVEX_SOCKET_PATH` and `KARVEX_NO_TMUX_COMPAT` rather than assuming an
  ambient value.

### R8 — the shim dir is on PATH ahead of everything, for every managed pane

`<data_dir>/shims` is prepended to `PATH` in every managed pane, so anything
that ever lands in that directory shadows a system binary for every process the
user runs. Invariant: **the shims directory contains exactly one entry,
`tmux`.** W1 must not use it as a general-purpose bin dir, and W7 asserts the
directory's contents are exactly `["tmux"]` after a pane spawn.

---

## 5. Workstreams (strict disjoint file ownership)

Every file below is owned by exactly one workstream at any moment. Contested
files are resolved in §5.10.

### W1 — Pane-side tmux identity + shim install

**Owns:** `src/pane.rs`, `src/platform/mod.rs`, `src/platform/tmux_shim.rs` (new)

**Approach.** Add `apply_tmux_compat_env(cmd, pane_id)` called from the
`Managed` arm of `apply_pane_launch_env` (`src/pane.rs:120-133`). Per D1: call
`platform::ensure_tmux_shim_dir()` first; return early on `None` or when
`KARVEX_NO_TMUX_COMPAT=1`; otherwise set `TMUX=<socket>,<pid>,0`,
`TMUX_PANE=<pane_id>`, and a PATH with the shim dir prepended exactly once and
all other entries order-preserved. `src/platform/tmux_shim.rs` holds
`ensure_tmux_shim_dir`, `install_shim_symlink`, `binary_owns_shim(stem)` and the
macOS mirror.

**Edge cases.** Existing shim dir already first in PATH (do not duplicate);
`PATH` unset; `HOME` unset (skip mirror); `~/.local/bin` absent (skip mirror —
do **not** create it, D6); concurrent pane spawns racing on the symlink
(`AlreadyExists` → success); read-only data dir; `current_exe()` failing; a
*directory* at the link path; a **dangling** own-link left by a package-manager
upgrade (inspect via `symlink_metadata`, re-point — D6); the shims directory
containing anything other than `tmux` (R8).

**Tests** (in-file `#[cfg(test)] mod tests`; all fail-before):
- `tmux_compat_env_exports_socket_and_pane_id_for_managed_panes`
- `tmux_compat_env_absent_for_inherit_identity`
- `tmux_compat_env_absent_for_omit_pane_identity`
- `tmux_compat_env_opt_out_via_karvex_no_tmux_compat`
- `tmux_compat_env_not_exported_when_shim_unavailable` (D1)
- `tmux_compat_env_prepends_shim_dir_once_and_preserves_path_order`
- `binary_owns_shim_accepts_kvx_and_rejects_test_binaries` (`kvx` ✓;
  `kvx-9f2a1c`, `karvex-9f2a1c`, `tmux`, `bakr` ✗)
- `install_shim_symlink_refuses_real_file`
- `install_shim_symlink_refuses_foreign_symlink`
- `install_shim_symlink_replaces_own_link_and_is_idempotent`
- `install_shim_symlink_repoints_dangling_own_link` (D6)
- `install_shim_symlink_leaves_dangling_foreign_link_alone` (D6)
- `macos_mirror_is_not_created_when_local_bin_is_absent` (D6; `#[cfg(target_os
  = "macos")]`, and this is the *only* automated coverage of the mirror because
  `tests/cli/` is skipped on macOS — see W7)
- `shims_dir_contains_only_tmux` (R8)

Note: these use `crate::integration::integration_env_lock()` (already exported
`#[cfg(test)]`, `src/integration/mod.rs:13-14`) or an equivalent guard — env
mutation in tests must be serialised, and per R7 each test must explicitly set
*or remove* `TMUX`, `TMUX_PANE`, `KARVEX_SOCKET_PATH` and
`KARVEX_NO_TMUX_COMPAT` rather than inheriting the developer's session.

**Risks.** R2, R3, R5, R7, R8.

---

### W2 — tmux shim: dispatch + translation surface

**Owns:** `src/main.rs`, `src/cli.rs` (one `mod tmux_compat;` line),
`src/cli/tmux_compat.rs` (new)

**Approach.** argv[0]-stem dispatch per D4. Port the donor's translation table,
restructured so the arg→request mapping is **pure and unit-testable**: each
handler splits into `plan_x(args) -> XPlan` (pure) and a thin `execute` that
issues the request. The donor's monolithic handlers are the reason it has no
tests; this is the single most important structural deviation.

Surface (`SERVICED` gate + `should_service`, per the donor):

| tmux | → Karvex |
|---|---|
| `-V` | print `tmux 3.5a (karvex-compat)`, exit 0 |
| `display-message -p #{pane_id}` / `#{window_id}` / `#{client_control_mode}` / `#{client_termtype}` | `pane.current` / `pane.list`; `"0"`; `$TERM` |
| `list-panes -t @N -F #{pane_id}` | `pane.list` filtered by tab, ordered by numeric pane-id suffix so element 0 is the leader |
| `split-window`/`splitw` | `pane.split`; `-v`→Down else Right; `-l N%` → `ratio = clamp(1 - N/100, 0.1, 0.9)`; **`-d` → `focus: false`, absent `-d` → `focus: true`** (Claude always passes `-d`; `PaneSplitParams.focus` defaults to `false`, so the mapping must be explicit rather than relying on the default); `workspace_id: None`; cwd from target's `foreground_cwd`/`cwd`; trailing `-- <cmd>` deliberately **not** run; `-P` prints the new pane id |
| `respawn-pane` | bounded shell-readiness wait (a real one — see R4, **not** the donor's no-op), then `pane.send_input{keys:["ctrl+u"]}` then `{text: cmd, keys:["Enter"]}`; `-k` is accepted and ignored |
| `select-pane -T <title>` | `pane.rename`; plain focus-select → accept |
| `kill-pane`/`killp` | `pane.close` |
| `set-option`/`set`, `select-layout`, `resize-pane`/`resizep` | accept, exit 0 (see W5 for the colour case) |
| `show-options … prefix` | print `prefix C-b` |
| `send-keys` | `pane.send_input`; strip flags; trailing literal `Enter` → key |
| anything else, `-L`, socket mismatch, no resolvable socket | passthrough: exec a real `tmux` found later on PATH (skipping our own canonicalised path), else emulate `no server running` |

**Beyond the donor:** on any `pane.split` failure — API error *or* unexpected
response — stderr contains `no space for new pane (too small)` so Claude's
friendlier UI path is reachable (R6). Transport, socket targeting, timeout and
server-down behaviour per D3/D5.

**Pane-id shape.** Real tmux ids are `%N` (pane) and `@N` (window); Karvex emits
its own `w1:p3` / `w1:t2` forms, as the donor does. Claude treats these as
opaque strings and hands them back via `-t`, and the donor shipped this way, so
we do the same — but the spec (`docs/claude-tmux-spec.md`) states the `%N`/`@N`
shape explicitly, so **gate-phase probe:** confirm Claude never validates the
sigil. If it does, the fix is a bijective `w1:p3 ↔ %<ordinal>` mapping in the
shim, which is why every id must pass through a single pair of pure
`encode_pane_id`/`decode_pane_id` functions rather than being formatted inline.

**Edge cases.** `-S` path given but not equal to ours (mismatch → passthrough);
`-S` given with a symlinked/relative socket path (compare canonicalised, fall
back to string compare); `TMUX_PANE` set but stale after a pane closed; `-t`
naming a tab rather than a pane; `-F` formats we do not recognise (print an empty
line, exit 0 — never a Rust error string on stdout); a `--` with no command;
`display-message` with no format argument; the server socket present but dead
(D3); `-V` with no server at all (must still exit 0).

**Tests** (in-file, pure; all fail-before):
- `parses_socket_and_subcommand`, `parses_version_flag`
- `named_socket_is_not_serviced`, `socket_mismatch_is_not_serviced`,
  `unknown_subcommand_is_not_serviced`, `no_resolvable_socket_is_not_serviced`
- `split_flags_extracted`, `trailing_command_absent`
- `split_ratio_inverts_tmux_percentage` (`-l 70%` → `0.30 ± f32::EPSILON`)
- `split_ratio_clamped_at_bounds` (`-l 5%` → `0.9`, `-l 95%` → `0.1`)
- `split_direction_defaults_to_right_and_honours_v`
- `split_d_flag_maps_to_focus_false_and_absent_d_focuses`
- `pane_ordinal_orders_creation` (`w1:p2 < w1:p10`)
- `send_keys_detects_trailing_enter`, `send_keys_strips_flag_values`
- `display_message_selects_format`, `display_message_unknown_format_is_empty`
- `select_pane_title_maps_to_rename`, `select_pane_without_title_accepts`
- `split_failure_message_mentions_no_space`
- `resolves_socket_from_karvex_socket_path_env`,
  `resolves_socket_from_tmux_env_when_socket_path_unset` (D5)
- `shell_readiness_requires_two_stable_non_empty_samples` (R4)
- `encode_decode_pane_id_roundtrips`

**Risks.** R4, R7; passthrough exec-loop if the canonicalisation skip fails
(test: `passthrough_skips_own_binary`).

---

### W3 — DCS `tmux;` passthrough unwrap  ⚠ mandatory, highest risk

**Owns:** `src/pane/terminal/tmux_passthrough.rs` (new), `src/pane/terminal.rs`,
`src/pane/osc.rs`

⚠ **Module placement is load-bearing.** The new module must live under
`src/pane/terminal/` and be declared from `src/pane/terminal.rs` (precedent:
`mod windows_recent_fallback;` at `src/pane/terminal.rs:19`). A module at
`src/pane/tmux_passthrough.rs` would require a `mod tmux_passthrough;` line in
the `src/pane.rs` declaration block (`:27-34`), and **`src/pane.rs` is W1's** —
that would break the disjoint-ownership guarantee in the middle of Wave 0.

**Approach.** A streaming filter in the pane's inbound byte path that unwraps
`\eP tmux ; <escaped> \e\\`, un-doubling `\e\e` → `\e`, and forwards the inner
bytes onward. Unrelated DCS strings pass through byte-identical.

**Exact seam.** `GhosttyPaneTerminal::process_pty_bytes`
(`src/pane/terminal.rs:1108`). The filter runs **first**, at the very top of the
function body, before `core.default_color_tracker.observe(bytes)` (`:1131`), and
its output is what every observer and the terminal write see. See R1 for why:
unwrapping later fixes clipboard but leaves the OSC 11 / OSC 4 / XTGETTCAP
response paths (`core.default_color_event_tracker`,
`core.xtgettcap_query_tracker`, `:1179-1183`) still blind. Filter state lives on
`core` (the `Mutex`-guarded struct already holds every other tracker) so it
survives read boundaries. Return a `Cow<'_, [u8]>`, mirroring the existing
`maybe_filter_primary_screen_scrollback_clear` precedent at `:1157-1165`.

**Edge cases.** Sequence split across PTY reads at every offset; 7-bit `\eP` and
**8-bit `\x90`** DCS introducers; `ST` given as `\e\\` vs `\x9c`; unterminated
sequence (bounded buffer, drop past a cap — reuse the ~1KiB discipline
`src/pane/osc.rs:1333-1341` already uses); nested/adjacent passthroughs; `\eP`
followed by something that is not `tmux;` (including a partial `tmu` at a read
boundary); empty payload; binary payload (OSC 52 base64 is safe but do not
assume UTF-8). Precedent for a two-form DCS state machine with split-read
handling: `src/pane/xtgettcap.rs` (see its
`tracker_accepts_eight_bit_dcs_and_string_terminator` test).

**Tests** (fail-before, unless marked):
- characterization, **already exists and must stay green**:
  `process_pty_bytes_surfaces_clipboard_writes_without_other_results`
  (`src/pane/terminal.rs:3447-3463`) — proves the filter does not regress the
  plain OSC 52 path. Extend rather than duplicate it.
- `tmux_passthrough_unwraps_osc52_clipboard_write`
- `tmux_passthrough_unwraps_osc11_query_and_a_response_is_produced` (the second
  half of R1's acceptance — asserts `result.terminal_responses` is non-empty)
- `tmux_passthrough_unescapes_doubled_escape`
- `tmux_passthrough_handles_sequence_split_across_reads` (parameterised over
  every split offset)
- `tmux_passthrough_accepts_eight_bit_dcs_and_st`
- `tmux_passthrough_forwards_unrelated_dcs_unchanged`
- `tmux_passthrough_drops_unterminated_sequence_at_cap`

**Also update (not a behaviour change).** `src/pane/osc.rs:1321-1330`
(`default_color_event_tracker_ignores_other_osc_and_dcs_payloads`) stays green —
it drives the tracker directly and the filter sits upstream — but after W3 it no
longer describes pane-level policy. Add a one-line comment saying so, or a
future reader will cite it as evidence that Karvex still drops passthrough.

**Risks.** Touching the hot pane read path — keep the filter allocation-free on
the common (no `\eP`/`\x90` byte) path and add an explicit "no DCS introducer
present → `Cow::Borrowed`" test.

---

### W4 — Auto-install the Claude integration on server start

**Owns:** `src/server/headless.rs`

**Approach.** Port `auto_install_claude_integration()` next to `run_server`, call
it right after `config::Config::load()` (`headless.rs:4743`; `run_server` begins
at `:4711`; this is the analogue of bakr's `:4398-4399`). The `--handoff-import`
path early-returns at `:4732-4741`, i.e. **before** the load site — verified, so
a handoff-import server never auto-installs. Opt-out
`KARVEX_NO_AUTO_INTEGRATION=1`. Uses the existing
`integration::installed_integration_statuses() -> Vec<IntegrationStatus>` +
`integration::install_target(IntegrationTarget::Claude) -> io::Result<Vec<String>>`
(`src/integration/registry.rs:220`, `src/integration/actions.rs:16`). Failures
`tracing::warn!` and never abort startup. No new `IntegrationTarget` variant and
no new `Method` variant anywhere in this plan — **nothing here adds an enum
variant, so there are no new exhaustive-match arms to chase** across
`src/api/schema.rs`, `src/api/subscriptions.rs`, the CLI dispatch tables or the
JSON schema fixtures. Any workstream that finds itself wanting one must stop and
escalate, because that also drags in `src/protocol/wire.rs::PROTOCOL_VERSION`
review.

**Deviation from the donor (testability).** Split the decision from the effect:

```rust
fn should_auto_install_claude(statuses: &[IntegrationStatus], opt_out: bool) -> bool
```

so the gating logic is unit-testable without touching a real `~/.claude`.

**Edge cases.** `~/.claude` missing → `install_claude` errors → warn, continue
(a user without Claude Code installed must not see a scary startup log);
`CLAUDE_CONFIG_DIR` set; read-only home; a server started by
`autodetect::spawn_server_daemon` with stdio at `/dev/null` (log only, never
`eprintln!`); a handoff-import server (`--handoff-import` returns before this
point — confirm the call site is after that early return).

**Tests** (in-file):
- `should_auto_install_claude_skips_when_current`
- `should_auto_install_claude_installs_when_outdated_or_missing`
- `should_auto_install_claude_respects_opt_out`
- `should_auto_install_claude_ignores_non_claude_targets`

---

### W5 — Teammate accent colour (lowest priority; starts after W2 merges)

**Owns:** `src/cli/tmux_compat.rs` (ownership **transfers** from W2 on merge),
`src/ui/sidebar/tokens.rs`, `src/ui/sidebar.rs`

**Approach.** Per D8. In the shim, `set-option -p -t <pane> <style-option>
<value>` parses `fg=<colour>` out of **exactly three** options —
`window-style`, `pane-border-style`, `pane-active-border-style` — normalises
Claude's eight values (`colour208`→`orange`, `colour205`→`pink`, others pass
through), and issues `pane.report_metadata { source: "karvex:tmux-compat",
tokens: {"agent_accent": Some(colour)}, seq }`. Unrecognised options stay
accept-and-drop. In the sidebar, `agent_accent` tints the pane row's agent token
and is never rendered as literal text.

⚠ **Match on the option name, never scan the whole argv for `fg=`.** Claude also
sends `set-option -p -t %N pane-border-format "#[fg=<c>,bold] #{pane_title}
#[default]"` — a *format string* that contains a literal `fg=`. A naive scan
extracts `<c>,bold]` and writes garbage into the UI.

**Edge cases.** `bg=default,fg=red` (comma list); `fg=` empty → clear the token
(`None` patch); an unknown colour name → drop, do not propagate garbage into the
UI; `pane-border-format`'s embedded `fg=` (above); `seq` monotonicity across
rapid re-styling (Q4 — use a per-process monotonic counter, not a timestamp; the
server enforces strict increase per `source` and caps distinct sources at 32,
`src/metadata_tokens.rs:16-41`); a pane that later runs something else (the
token should be clearable — `pane.close` disposes it anyway).

**Tests:**
- `set_option_extracts_fg_colour_from_style_value`
- `set_option_normalises_claude_colour_numbers`
- `set_option_unknown_option_is_accepted_without_metadata`
- `set_option_pane_border_format_does_not_yield_an_accent` (the `fg=` trap)
- `set_option_accent_token_key_is_api_valid` (asserts the key matches
  `[A-Za-z0-9_-]+`, i.e. that D8's rename is not silently reverted)
- `sidebar_agent_accent_tints_agent_token`
- `sidebar_unknown_accent_value_falls_back_to_the_default_style`

(There is deliberately **no** "accent token is not rendered as text" test: that
is already true with zero code, since the sidebar only renders tokens the user's
config names. Asserting it would be vacuous.)

**Fallback:** if this exceeds a day, keep the `pane.report_metadata` write and
drop only the TUI tint (the accent stays observable via `kvx pane get`), and
record the UI gap in the spec doc. It must never block the gate phase.

---

### W6 — Teammate-variant agent detection (gate phase, evidence-driven)

**Owns:** `src/detect/manifests/claude.toml`,
`website/agent-detection/claude.toml`, `website/agent-detection/index.toml`

**Approach — no blind manifest edits.** Per CLAUDE.md's Agent Detection Updates
process, this workstream does **not** start until a live teammate pane exists
(gate phase step 3):

1. Check whether `~/.config/karvex/agent-detection/claude.toml` already exists.
   If it does, back it up byte-for-byte and restore it exactly at the end;
   never overwrite a pre-existing override without alignment.
2. Drive a real teammate into each interesting state in a throwaway session
   (`karvex-throwaway-repro` skill).
3. Capture evidence: `kvx agent read <pane> --source detection --format text`,
   and `--format ansi` if styling or alternate-screen behaviour matters.
4. `kvx agent explain <pane> --json` to see what currently matches (and why the
   `@name` teammate banner does not).
5. Iterate against the local override + `kvx server reload-agent-manifests`.
6. Land the rule in the bundled manifest as explicit AND/OR gates over invariant
   controls — never whole-pane incidental text, never the user-scrollable
   viewport.
7. Bump the bundled manifest's `version` field (date-based scheme —
   `src/detect/manifests/claude.toml:2` is currently `"2026.08.04.1"`, so the
   next value is today's date with an ordinal) and copy the file **byte for
   byte** to `website/agent-detection/claude.toml`.
   ⚠ **Do not touch `website/agent-detection/index.toml`.** Its entries are
   exactly `{id, path}` — there is no `version` and no `sha256` field there, and
   `claude` is already listed (`:12-13`); the validator *rejects* any extra key
   (`scripts/agent_detection_manifest_check.py:305-307`). What
   `validate_catalog` (`:288-350`) actually enforces is: website `version` ≥
   bundled `version`, and if the versions are equal the two files must be
   byte-identical. So the failure mode to avoid is editing the bundled manifest
   without re-copying it, or bumping one version and not the other.
8. Remove the temporary override / restore the pre-existing one exactly.

**Tests.** Per CLAUDE.md, **no** large agent-specific full-screen fixture suite.
Rust tests only cover manifest parsing/rule semantics, which already exist.
Validation is `python3 -m unittest scripts.test_agent_detection_manifest_check`
plus the live re-read.

---

### W7 — End-to-end tests through the real shim binary

**Owns:** `tests/cli/tmux_compat.rs` (new), `tests/cli/mod.rs` (one `mod` line)

**Approach.** The scratch-server harness **already exists and does not need to be
built** — `tests/cli/harness.rs` provides `unique_test_dir` (`:18`),
`spawn_karvex` (`:122`), `spawn_karvex_with_path` (`:265`), `spawn_named_server`
(`:166`), `wait_for_socket` (`:111`), `run_cli` / `run_cli_json` (`:325`,
`:361`), `run_named_cli_with_env` (`:214`) and `wait_until` (`:399`), all
`pub(super)` and reachable from a sibling `tests/cli/tmux_compat.rs`. Use unique
temp config/runtime dirs, spawn `kvx server`, wait for the socket
(`spawn_karvex_with_path` is the right entry point for the PATH assertions).
Then create the `tmux` symlink in a temp dir pointing at the **built `kvx`
binary** (`env!("CARGO_BIN_EXE_kvx")`) and invoke it with `KARVEX_SOCKET_PATH`,
`TMUX`, and `TMUX_PANE` set to the scratch session.

**Named tests:**
- `tmux_shim_fakes_a_claude_teammate_lifecycle` — the flagship: `-V` → 0;
  `display-message -p '#{pane_id}'` → leader id; `list-panes -t <tab> -F
  '#{pane_id}'` → 1 line; `split-window -d -t <leader> -h -l 70% -P -F
  '#{pane_id}'` → prints a new id and `pane.list` shows 2 panes **leader
  first**; `select-pane -t <new> -T teammate-a` → the pane's label changes;
  `send-keys -t <new> echo hi Enter`; `respawn-pane -k -t <new> -- <cmd>`;
  `kill-pane -t <new>` → back to 1 pane.
- `tmux_shim_reports_version_without_a_server`
- `tmux_shim_passes_through_named_socket_invocation` (`-L foo` must not touch
  the API)
- `tmux_shim_ignores_mismatched_socket_argument`
- `tmux_shim_split_failure_stderr_mentions_no_space`
- `nextest_binary_never_installs_a_tmux_shim` — spawn a server from the test
  binary with a scratch `XDG_DATA_HOME`, spawn a pane, assert no
  `<data_dir>/shims/tmux` and an untouched `~/.local/bin/tmux` (this is the
  regression the donor needed commit `7d69200` to learn).
- `tmux_shim_does_not_leak_path_into_the_server_environment`
- `tmux_shim_version_succeeds_with_the_server_socket_dead` and
  `tmux_shim_serviced_verb_fails_fast_when_the_server_socket_is_dead` (D3;
  assert exit code, a plain stderr line, no JSON envelope, and completion well
  inside 2s)
- `tmux_shim_uses_the_socket_from_tmux_env_not_the_default_session` (D5 — spawn
  *two* scratch servers, point `$TMUX` at the second, unset
  `$KARVEX_SOCKET_PATH`, assert the pane appears on the second)
- `shim_dir_contains_only_the_tmux_entry` (R8)

**Constraints.** Unix-only: gate the suite with `#[cfg(unix)]` on its `mod` line
in `tests/cli/mod.rs` (the one line W7 owns there). Must pass under both `just
test` and `just check-slim`. Must not write outside its temp dirs — in
particular never to the developer's real `~/.local/bin`.

⚠ **This suite never runs on macOS.** `tests/cli.rs:1` is
`#![cfg(not(target_os = "macos"))]`, so the whole `tests/cli/` tree is compiled
out there. That matters because macOS is exactly where the riskiest write lives
(the `~/.local/bin` mirror, R2) and where `nextest_binary_never_installs_a_tmux_shim`
would be most valuable. Do **not** try to lift the crate-level gate as part of
this work. Instead: W1's in-file unit tests carry the macOS-relevant guarantees
(`binary_owns_shim_*`, `install_shim_symlink_refuses_*`,
`macos_mirror_is_not_created_when_local_bin_is_absent`) because `src/` unit
tests do run on macOS, and gate step 2 covers the rest by hand if a macOS box is
available. Record the residual gap in the spec doc.

---

### W8 — Docs, spec, skill, changelog

**Owns:** `docs/next/website/src/content/docs/**` (en + `ja/` + `zh-cn/`),
`docs/next/CHANGELOG.md`, `skills/karvex/SKILL.md`,
`docs/design/claude-teammates/02-claude-tmux-spec.md` (new)

**Content:**
1. **Spec doc** — the Karvex equivalent of bakr's `docs/claude-tmux-spec.md`:
   backend detection order, the two invocation wrappers, the exact command
   surface, the colour list, behavioural notes (<2s ops, element-0-is-leader,
   exact stdout), Karvex's deviations (D1, D3 — including that the shim owns its
   socket target and never uses the cli request helpers — and D8, whose metadata
   token key is **`agent_accent`**, not `agent.accent`; the dotted form is
   rejected by the API and must not appear anywhere in the docs), the residual
   gaps (no macOS e2e coverage, `select-layout`/`resize-pane` accept-and-drop,
   W5's tint if the fallback was taken), the §7 step 8 hook-probe result, and
   explicit donor attribution with the Apache-2.0 / shared-`herdr`-lineage note.
2. **User docs** — a Claude Agent Teams section in `integrations.mdx` (or a new
   page if it outgrows a section): what happens automatically, the
   `claude --teammate-mode auto` requirement (**a user settings file that pins
   `teammateMode` defeats the tmux backend — Karvex cannot force it**), the two
   opt-outs (`KARVEX_NO_TMUX_COMPAT=1`, `KARVEX_NO_AUTO_INTEGRATION=1`), the
   coexistence story with a real tmux, the macOS `~/.local/bin` mirror **and how
   to remove it** (D6 — there is no `kvx uninstall`, so the removal command must
   be written down: delete `<data_dir>/shims/tmux` and, on macOS,
   `~/.local/bin/tmux` if it points at Karvex), and the Windows non-goal.
3. **Parity** — `scripts/docs_translation_parity.py` enforces two things:
   (a) the **set of `*.mdx` filenames** under `ja/` and `zh-cn/` must equal the
   en root's, so adding a new en page means adding two more files, not one; and
   (b) identical **heading-level outlines** (levels only, not text; fenced code
   blocks skipped). Real translations, not stubs, for any heading that exists in
   en.
4. **`skills/karvex/SKILL.md`** — how an agent driving Karvex should reason about
   teammate panes.
5. **`docs/next/CHANGELOG.md`** under `## Unreleased`, in the existing
   descriptive house style.

⚠ **Naming.** `docs/next` has **not** been renamed for this fork: `integrations.mdx`
alone contains 108 `herdr`/`Herdr` occurrences and zero `kvx`, and the same
holds for `cli-reference.mdx` (199) and `agents.mdx` (34). This is not "a few
stray lines" — it is the tree's current convention. New content must therefore
use `herdr`/`Herdr` **to match the file it is written into**, so the page does
not end up half-renamed. Do not rename anything as part of this work; flag the
fork-wide rename separately. (`skills/karvex/SKILL.md` is the opposite case — it
already uses `kvx` throughout; match *it* there.)

---

### W9 — Hook reconciliation (conditional — only if the gate probe demands it)

**Owns:** `src/integration/mod.rs`, `src/integration/assets/claude/*`,
`src/integration/tests.rs` (or wherever the claude install tests live)

Triggered only by gate step 8. Scope if triggered: relax the `is_subagent`
guard so Agent-Teams teammates (a full `claude` process owning its own pane) are
distinguished from in-process subagents, keep the `SubagentStop` guard and the
`stop → idle` report intact, bump `CLAUDE_INTEGRATION_VERSION` 8 → 9 **once**,
update the `# KARVEX_INTEGRATION_VERSION=` marker in both `.sh` and `.ps1`
assets, and extend the existing install/status tests
(`install_claude_writes_hook_and_updates_settings`, the `claude_v*_status_is_outdated`
family) plus `just integration-assets-test`.

---

### 5.10 Contested files — resolved

| File | Wanted by | Resolution |
|---|---|---|
| `src/pane.rs` | W1 (env export), W3 (byte path) | W1 owns `src/pane.rs`. ⚠ W3 must **not** need it: its new module goes at `src/pane/terminal/tmux_passthrough.rs`, declared from `src/pane/terminal.rs` (precedent `mod windows_recent_fallback;` at `src/pane/terminal.rs:19`). A module at `src/pane/tmux_passthrough.rs` would force a `mod` line into `src/pane.rs:27-34` and break the guarantee. If W3 still finds it must touch `src/pane.rs`, it hands the edit to W1 rather than editing. |
| `src/pane/osc.rs` | W3 (comment only) | W3. Nobody else touches it; the change is a clarifying comment on the tracker-layer test at `:1321-1330`. |
| `src/platform/mod.rs`, `src/platform/tmux_shim.rs` | W1 | W1 only. W3 needs no platform code. |
| `src/cli/tmux_compat.rs` | W2, W5 | Sequenced: W2 exclusively until merge, then ownership transfers to W5. Never concurrent. |
| `src/cli.rs` | W2 (one `mod tmux_compat;` line) | W2 only. No other workstream touches `src/cli.rs`: W8 edits no Rust, W9's scope is `src/integration/**`, and W7 lives in `tests/`. |
| `src/main.rs` | W2 (argv[0] dispatch) | W2 only. |
| `src/ui/sidebar.rs`, `src/ui/sidebar/tokens.rs` | W5 | W5 only, and only in Wave 1 after W2 merges. Nothing in Wave 0 touches `src/ui/`. |
| `src/server/headless.rs` | W4 | W4 only. W7's e2e spawns the binary; it does not edit server code. |
| `src/integration/**` | W4 (reads the API), W9 (edits assets) | W4 **reads** `integration::` public items and edits nothing under `src/integration/`. W9 owns that tree, and only if triggered. |
| `tests/**` | W7 | W7 owns the `tests/` tree. W1–W5 own only their own in-file `#[cfg(test)] mod tests`. |
| `src/detect/manifests/claude.toml`, `website/agent-detection/claude.toml` | W6 | W6 only, in the gate phase. `website/agent-detection/index.toml` is **not** edited by anyone (see W6 step 7). |
| `docs/**`, `skills/**` | W8 | W8 only. |
| `docs/next/CHANGELOG.md` | W8 | W8 only — every workstream sends its changelog line to W8 rather than editing. |

**Enum-variant audit.** No workstream in this plan adds a variant to any enum:
not `IntegrationTarget` (W4 explicitly), not `Method`/`ResponseResult`
(everything rides existing `pane.*` methods), not the events/subscriptions
enums, not `PaneReadSource`. Consequently there are no new exhaustive-match arms
and no `src/protocol/wire.rs::PROTOCOL_VERSION` question. The one place a
builder might be tempted is W5: tinting must reuse
`ResolvedTokenKind`/`SidebarTokenStyle` rather than adding a
`ResolvedTokenKind` variant (which has match sites in `src/ui/sidebar.rs`
around `resolved_token_spans`, `:990-1140`, and in `tokens.rs`'s own tests). If
any workstream concludes it needs a new variant, it stops and escalates instead
of adding one.

---

## 6. Sequencing

```
(P0 is gone — the zig 0.15.2 toolchain is resolved; see §8 Q0. All cargo/just
 invocations go through the `with-zig.sh` wrapper.)

Wave 0 (fully parallel, no shared files)
      W3  DCS passthrough unwrap        ← highest risk, start first
      W1  pane identity + shim install
      W2  shim dispatch + translation
      W4  auto-install on server start
      W8  docs + spec skeleton (spec doc is writable from the recon alone)

Wave 1 (after its inputs merge)
      W7  e2e through the shim binary      needs W1 + W2
      W5  teammate accent colour           needs W2 (ownership transfer)

GATE  live verification (§7)               needs W1 + W2 + W3 + W4
      W6  detection manifest tuning        happens inside the gate, evidence-first
      W9  hook reconciliation              only if gate step 8 says so

Wave 2
      W8  finalise docs + translations + changelog from what actually shipped
      just check  (already covers both feature legs + windows-lint)
      just release-docs-check
```

W3 must not merge *after* W1: shipping `TMUX` export without the unwrap is the
one ordering that produces a user-visible regression. If they land in the same
PR, fine; if separately, W3 first.

---

## 7. Gate phase — live verification

Run in a throwaway session per the `karvex-throwaway-repro` skill. Build and
test invocations need the env scrub:

```
env -u KARVEX_STARTUP_CWD -u KARVEX_ENV -u KARVEX_SOCKET_PATH \
    -u KARVEX_CLIENT_SOCKET_PATH -u KARVEX_PANE_ID -u KARVEX_WORKSPACE_ID \
    -u KARVEX_TAB_ID -u TMUX -u TMUX_PANE  <command>
```

⚠ `TMUX` and `TMUX_PANE` are new to this list and are **required** once W1
lands: after that change a Karvex pane exports them, so a `just check` run from
inside one feeds the W1/W2 env-reading tests the developer's own session. See
R7. (`PATH` is deliberately *not* scrubbed — the shims dir being present is
harmless for the build, and scrubbing `PATH` breaks the toolchain.)

Every cargo/just invocation is additionally wrapped in `with-zig.sh`, which
supplies the zig 0.15.2 toolchain the vendored libghostty-vt build needs (Q0 —
resolved). Talking to a debug build
from inside a live Karvex session uses `cargo run -- …` with the socket
overrides cleared, per CLAUDE.md.

1. **Build both legs.** `just check` — which already runs `just ci` (workflow-on
   nextest + clippy) → `just check-slim` (the `--no-default-features` leg) →
   `just windows-lint` for D10. Running `check-slim` separately is only needed
   when iterating on one leg.
2. **Identity.** Start a scratch server, open a pane, verify inside it: `echo
   $TMUX` (points at the scratch socket), `echo $TMUX_PANE`, `command -v tmux`
   resolves to `<data_dir>/shims/tmux`, and `tmux -V` prints the compat banner.
3. **Teammates as panes.** In a pane run `claude --teammate-mode auto`, ask for
   two teammates. Expect: two new Karvex panes appear, titled with the teammate
   names, leader keeps focus, the sidebar tracks them.
4. **Manifest tuning (W6).** For a teammate pane: `kvx agent read <pane>
   --source detection --format text`, `kvx agent explain <pane> --json`. Tune
   via the local override + `kvx server reload-agent-manifests` until the
   teammate `@name` banner is matched and the pane reports `claude` with correct
   idle/working transitions. Then land it in the bundled manifest, sync the
   website assets, and restore/remove the override.
5. **Round-trip.** Lead sends a message to a teammate; teammate replies. Verify
   the teammate's pane shows the exchange and its detection state moves
   working→idle.
6. **Reaping.** End the team / kill a teammate; the corresponding Karvex pane
   closes. Then quit Claude and verify no orphan panes or processes.
7. **Clipboard + colour queries (W3 acceptance, two parts).** Inside a pane with
   `TMUX` exported: (a) yank in neovim (or `printf` an OSC 52 through a
   tmux-wrapping tool) and confirm the system clipboard receives it; (b) send a
   tmux-wrapped OSC 11 background query
   (`printf '\ePtmux;\e\e]11;?\a\e\\'`) and confirm a response comes back.
   Both must pass — see R1; (b) is the half that a late-placed filter would
   silently fail.
8. **Hook probe (decides W9).** `kvx pane get <teammate-pane>` — does it carry an
   `agent_session`? (⚠ **no `--json` flag**: `kvx pane get` takes exactly one
   argument and already prints JSON; `kvx pane get <id> --json` exits 2,
   `src/cli/pane.rs:80-96`.) If the session is missing, capture the raw hook
   JSON (temporary `tee` in a *copy* of the hook, never in the installed asset)
   and check for an `agent_id` field — that is what
   `src/integration/assets/claude/karvex-agent-state.sh:52-54` gates on. Result
   decides whether W9 runs and whether `CLAUDE_INTEGRATION_VERSION` moves.
   Record the raw JSON in the spec doc either way; a clean probe is the evidence
   that D7's "no bump" is correct rather than merely convenient.
9. **Real-tmux coexistence.** With a real tmux installed: `tmux new -s probe`
   inside a Karvex pane must reach the real tmux via passthrough; `tmux -L foo
   ls` must not touch the Karvex API; a pre-existing real `~/.local/bin/tmux`
   must be left untouched (check the log for the refusal warning).
10. **Auto-install.** Delete the Claude hook, restart the server, confirm it is
    reinstalled and logged; then with `KARVEX_NO_AUTO_INTEGRATION=1` confirm it
    is not.
11. **Opt-out.** With `KARVEX_NO_TMUX_COMPAT=1`, panes have no `TMUX`,
    no `TMUX_PANE`, and an unmodified PATH. Note the opt-out does **not** delete
    an already-installed shim (D6) — confirm that is what the docs say.
12. **Dead server.** Kill the server while a pane is alive, then from that pane:
    `tmux -V` must still exit 0, and `tmux display-message -p '#{pane_id}'` must
    fail in well under 2s with a plain stderr line and no JSON envelope (D3).
13. **Pane-id sigil probe.** Confirm Claude accepts Karvex-shaped `w1:pN` /
    `w1:tN` ids where the spec documents `%N` / `@N` (see W2 "Pane-id shape").
    If it rejects them, the `encode_pane_id`/`decode_pane_id` seam is where the
    `%N` mapping goes.
14. **Removal.** Remove `<data_dir>/shims/tmux` and (macOS) `~/.local/bin/tmux`
    using exactly the commands W8 documented, and confirm the user's real tmux
    is back on PATH.

---

## 8. Open questions (need live probing, not more reading)

- **Q0 — RESOLVED (was the blocking prerequisite).** Zig 0.15.2 is installed at
  `~/.local/share/zig/zig-x86_64-linux-0.15.2/zig` (durable, survives `/tmp`
  wipes) and the wrapper
  `<scratchpad>/with-zig.sh` exports `ZIG` and prepends the toolchain to `PATH`
  before `exec`ing its argument. Re-verified during review:
  `with-zig.sh zig version` → `0.15.2`. **Every cargo/just invocation in this
  plan runs through that wrapper**, composed with the §7 env scrub, e.g.

  ```
  with-zig.sh env -u KARVEX_ENV -u KARVEX_SOCKET_PATH … -u TMUX -u TMUX_PANE \
      just check
  ```

  There is no P0 gate any more; Wave 0 starts immediately.
- **Q1.** Do Agent-Teams teammate hook events carry `agent_id`? Decides W9 and
  the integration-version bump. (§7 step 8.) The trigger is testable in the gate
  as written: the hook's guard is a single `bool(hook_input.get("agent_id"))`
  check at `src/integration/assets/claude/karvex-agent-state.sh:52-54`, so the
  probe is "does a teammate pane get an `agent_session` in `kvx pane get`", with
  a raw-JSON capture as the tiebreaker. If it *does* fire, D7's conditional
  applies and `CLAUDE_INTEGRATION_VERSION` goes 8 → 9 exactly once (8 shipped in
  v0.10.2 — verified against `git show v0.10.2:src/integration/mod.rs` — so one
  bump is correct under CLAUDE.md's migration-version rule, and a second change
  before the next release must **not** bump again).
- **Q1b (gate-phase probe).** Does Claude validate the `%N` / `@N` sigil on pane
  and window ids, or treat them as opaque? §7 step 13. The donor shipped native
  ids and reports success, but the extracted spec documents the sigil form, so
  this is asserted nowhere and must be observed.
- **Q1c (gate-phase probe).** Does anything parse the `-V` banner more strictly
  than "exit 0"? We print `tmux 3.5a (karvex-compat)`; real tmux prints
  `tmux 3.5a`. Claude's `isAvailable` only checks the exit code, but shells and
  plugins on the user's PATH may not. If it bites, drop the suffix.
- **Q2.** Does the `respawn-pane` → shell-submission path survive real teammate
  commands (long quoted argv, bracketed paste, slow rc files)? R4. If it is
  flaky, the alternative is a real `pane.run`-style API path rather than typing
  into a shell — a larger change, deliberately not planned here.
- **Q3.** How bad does the layout look without `select-layout`/`resize-pane`?
  Claude expects leader-left-at-30% with teammates stacked. If the result is
  unusable at 3+ teammates, a follow-up mapping `main-vertical` onto
  `layout.apply` is warranted — out of scope for this landing.
- **Q4.** Does Claude ever call the serviced surface *concurrently* from several
  teammates? If so, `pane.report_metadata` `seq` monotonicity in W5 needs a
  per-process counter rather than a timestamp.
- **Q5.** Does anything else in a Karvex pane misbehave with `TMUX` set beyond
  the DCS wrapping (R1) and prompt indicators (R5)? Step 7 is the canary;
  watch for colour-scheme detection (OSC 11) failures too.

---

## 9. Definition of done

- `just check` green (it covers both feature legs and `windows-lint`).
- Every test named in §5 exists and fails before its change.
- Gate steps 2–14 pass on this machine, with step 7 (**both** clipboard and the
  tmux-wrapped OSC 11 query) explicitly signed off. Step 7 is a hard gate: if
  either half fails, R1's fallback applies — invert the export to opt-in
  (`KARVEX_TMUX_COMPAT=1`) rather than shipping a clipboard regression.
- `python3 -m unittest scripts.test_agent_detection_manifest_check
  scripts.test_docs_translation_parity` green.
- `docs/next/CHANGELOG.md` has an `## Unreleased` entry; `docs/next` en/ja/zh-cn
  in **filename and heading-outline** parity; the spec doc credits bakr and
  records the residual gaps (macOS e2e coverage, `select-layout`/`resize-pane`
  no-ops, W5 fallback if taken).
- `CLAUDE_INTEGRATION_VERSION` moved **at most once** (only if Q1 says so), and
  the `# KARVEX_INTEGRATION_VERSION=` marker in both `.sh` and `.ps1` matches.
- No new enum variants anywhere; `src/protocol/wire.rs::PROTOCOL_VERSION`
  untouched.
- No `unwrap()` in new production code; `tracing` for all diagnostics; no
  `#[cfg(target_os)]` outside `src/platform/`.
- No workstream edited a file outside its §5.10 row.

---

## 10. Adversarial review log (2026-08-09)

Every path, line reference, function name, `just` recipe and CLI verb in this
document was re-checked against the two working trees.

Two findings were escalated to the plan's author and **both were confirmed by
the author after independent re-verification**: (1) D3 and D5 were in genuine
contradiction — the intent behind D3 was "don't pay for the status round trip",
not "reuse the cli helper", and the fallback arm of `active_api_socket_path()`
reachable through `ApiClient::local()` makes `send_request_unchecked` unsafe
here; (2) the `agent.accent` key is rejected outright, and the write-only W5
variant is "accept-and-drop with extra steps" and so is acceptable only as a
time-boxed fallback, never as the target. Both readings are now encoded, along
with the author's additions: the explicit timeout choice, shim-owned
`ApiClientError` mapping, dead-server-is-not-passthrough, the ban on `super::`
calls into the cli helpers, and the `AgentPanelEntry.tokens` shortcut for W5.

Changes made:

**Blocking defects fixed**

1. **D3/D5 transport was unimplementable as specified.** `send_request_unchecked`
   builds `ApiClient::local()`, which resolves through
   `session::active_api_socket_path()` and falls back to the *default session's*
   socket when `$KARVEX_SOCKET_PATH` is unset — so a shim that resolved its
   socket from `$TMUX` would have driven the wrong server, the exact hazard D5
   forbids. D3 rewritten around `ApiClient::for_target(ConnectionTarget::SocketPath(..))`
   + `request_value_with_timeout`; D5 rewritten with the fallback called out and
   a targeting test added.
2. **D8's token key `agent.accent` is rejected by the API.**
   `normalize_metadata_tokens` allows `[A-Za-z0-9_-]` only. Renamed to
   `agent_accent` throughout, with a guard test.
3. **W3's module placement broke the ownership guarantee.**
   `src/pane/tmux_passthrough.rs` needs a `mod` line in W1's `src/pane.rs`.
   Relocated to `src/pane/terminal/tmux_passthrough.rs`.
4. **W3's insertion point was underspecified in a way that would half-fix R1.**
   Pinned to the top of `process_pty_bytes`, before the observer chain, with the
   OSC 11 half added to the acceptance criteria.
5. **W6 step 7 described fields that do not exist.**
   `website/agent-detection/index.toml` has no `version` and no `sha256`; the
   validator rejects extra keys. Replaced with what the checker actually
   enforces.

**Medium**

6. W5's owned set was missing `src/ui/sidebar.rs`, and its "not rendered as
   text" test was vacuous (tokens only render when the user's config names
   them). Scope, ownership and tests corrected; the `pane-border-format`
   `fg=` trap added.
7. The donor's `wait_for_shell` is a no-op (both success arms return
   immediately). "Port it" removed; a real, unit-testable readiness check
   specified.
8. No removal path for the macOS `~/.local/bin/tmux` mirror — it outlives an
   uninstall. Added the "never create `~/.local/bin`" rule, dangling-link
   handling, a docs requirement and gate step 14.
9. `tests/cli.rs` is `#![cfg(not(target_os = "macos"))]`, so W7 never runs on
   macOS — the platform where the mirror risk lives. Gap documented and pushed
   onto W1 unit tests.
10. New R7: the test suite inherits `TMUX`/`TMUX_PANE` from the developer's own
    Karvex session once W1 lands. §7's scrub list extended.
11. Server-down shim behaviour was unspecified. Defined (with a request
    timeout, plain-stderr contract and tests).
12. `split-window -d` → `focus` mapping was missing from W2's table.
13. Gate step 8 used `kvx pane get <id> --json`, which exits 2.
14. Docs naming: `docs/next` is 100% `herdr`, not "a few stray lines"; new
    content must match, not mix.
15. New R8: the shims dir sits ahead of everything on PATH for every pane;
    pinned to a single-entry invariant.

**Claims verified sound (no change beyond tightening citations)**

- `src/pane.rs:112-138`, `src/platform/mod.rs:38-43`, `src/session.rs:157`,
  `src/session.rs:29-90` (D4's `configure_from_args` hazards are all real),
  `src/main.rs:488-528`, `src/cli.rs:142-144`, `src/app/api/panes.rs:99-103`,
  `src/integration/mod.rs:40` (= v0.10.2), `headless.rs:4711/4732/4743`
  (`--handoff-import` really does return first), `Cargo.toml` bin/package names.
- **R1 is real.** Confirmed at the source: `vendor/libghostty-vt/src/terminal/dcs.zig:52-78`
  drops `\ePtmux;…` wholesale, and OSC 52 is honoured end to end with an
  existing green test. The ordering constraint (W3 before or with W1) and the
  no-clipboard-regression gate are **retained and strengthened**, not weakened.
- `send_request_unchecked` exists and does skip the `status()` round trip
  (`src/cli.rs:822-827`) — the D3 *rationale* was right even though the
  mechanism needed replacing.
- `restore_default_sigpipe` is `pub(crate)` with a non-unix no-op, so the shim
  can opt in on every target.
- Nothing this plan touches is behind `#[cfg(feature = "workflow")]`; D9 holds.
  `just check-slim`, `just windows-lint`, `just integration-assets-test`,
  `just release-docs-check` all exist.
- The nextest-binary guard design works: cargo test binaries are `kvx-<hash>` /
  `karvex-<hash>` under `target/*/deps`, and an exact `kvx` stem match excludes
  them.
- `tests/cli/harness.rs` already provides the scratch-server harness — W7 does
  not have to build one (the plan previously implied it might).
- Karvex's API shapes match what the shim needs 1:1 (`PaneSplitParams`,
  `PaneSendInputParams`, `PaneRenameParams`, `PaneTarget`, `PaneListParams`,
  `PaneCurrentParams`), and `parse_key_combo` accepts `ctrl+u` and `Enter`.
- The translation table matches `bakr/docs/claude-tmux-spec.md`: the `-l N%`
  inversion, element-0-is-leader ordering, `-d`, `-P`, the eight colours,
  `remain-on-exit`/`pane-border-status` as accept-and-drop, the <2s budget and
  exact-stdout requirement.
- W6's evidence-first process matches CLAUDE.md's Agent Detection Updates rules.
- `kvx agent read --source detection`, `kvx agent explain --json`,
  `kvx server reload-agent-manifests` and the `karvex-throwaway-repro` skill all
  exist.
- Q0 **resolved** during the review window: zig 0.15.2 at
  `~/.local/share/zig/zig-x86_64-linux-0.15.2/zig` with a working `with-zig.sh`
  wrapper (`with-zig.sh zig version` → `0.15.2`, verified). The P0 row is struck
  from §6 and Wave 0 is unblocked; all cargo/just invocations now go through the
  wrapper.

**Outstanding for the build team, in priority order**

- W3 lands before or with W1. Non-negotiable — it is the only ordering that
  avoids a user-visible clipboard regression.
- Gate step 7 has **two** halves (clipboard *and* a tmux-wrapped OSC 11 query).
  Both must be signed off; if either fails, invert the export to opt-in.
- Two live-only probes are marked as probes, not assumptions: Q1b (does Claude
  validate the `%N`/`@N` sigil?) and Q1c (does anything parse the `-V` banner
  strictly?). Neither blocks Wave 0; both have a named fix if they bite.
- Q1 (teammate hook events carrying `agent_id`) still decides W9 and the single
  permitted `CLAUDE_INTEGRATION_VERSION` 8 → 9 bump.
