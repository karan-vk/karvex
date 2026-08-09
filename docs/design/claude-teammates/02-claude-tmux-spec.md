# Claude Code teammate tmux backend — Karvex spec

Status: describes the code as built and merged into the working tree
(W1–W5, W7, W8 complete; adversarially audited 2026-08-09). **The sections
still marked TBD are gate-phase outputs** — they can only be filled in by
`01-port-plan.md` §7's live run against a real Claude Code Agent Teams
session, which has not happened yet. Everything not marked TBD reflects
shipped behavior and is backed by tests.

## Provenance

This document is the Karvex adaptation of
[`bakr`](https://github.com/nbaker47/bakr)'s
[`docs/claude-tmux-spec.md`](https://github.com/nbaker47/bakr/blob/main/docs/claude-tmux-spec.md).
bakr is a sibling fork of the same `herdr` upstream Karvex forks; both are
Apache-2.0, so the reverse-engineered protocol description and the shape of
the shim it documents were ported directly. The commit arc that produced
bakr's tmux-compat feature is `f9b223e → ffa81b3 → cb0ed15 → 205c063 →
2ebb8f5 → f4d05aa → 7d69200` (all 2026-07-28, author nbaker47).

What Karvex kept from bakr: the *shape* of the integration — export a
tmux identity into managed panes, put a `tmux`-named symlink to Karvex's own
binary on `PATH`, dispatch on `argv[0]` stem, and translate a narrow tmux
surface onto Karvex's own control API.

What Karvex did not port, and why: bakr's `BAKR_*`/`HERDR_*` dual-alias env
scheme (Karvex has one prefix, `KARVEX_*`); bakr's harness kit
(`src/cli/harness.rs`, claude-harness skill/agent symlinking — an unrelated
subsystem with no Karvex equivalent); the `SessionStart`-only hook regression
bakr's hook evolved into (Karvex's existing Claude hook already handles both
`session` and `stop`, and changing that would regress `stop → idle`
reporting — see [Deviations, D7](#d7-hook-reconciliation)); and the donor's
near-total absence of tests (bakr ships 5 pure-logic unit tests and zero
coverage of env/shim/dispatch/API; Karvex's build plan requires fail-before
tests for every new behavior, see `01-port-plan.md` §5).

Full design rationale, file ownership, risk analysis, and the adversarial
review pass live in [`01-port-plan.md`](./01-port-plan.md); this document is
the protocol-level reference distilled from it, aimed at anyone implementing
against or debugging the shim rather than building it.

## Backend detection (`teammateMode = "auto"`)

Reverse-engineered from the embedded JS in Claude Code (`TmuxBackend` class
and backend registry). Priority order in `detectAndGetBackend`:

1. `teammateMode == "iterm2"` (explicit) → iTerm2/it2, error if not available.
2. **`insideTmux` → tmux backend. Checked before iTerm2.** `insideTmux` is a
   synchronous check of `process.env.TMUX`.
3. Inside iTerm2 (`ITERM_SESSION_ID`) → it2 if available, unless the user
   preference `preferTmuxOverIterm2` is set.
4. Neither → tmux "external session mode" (a separate `-L` server), only if
   `tmux -V` succeeds.

**Consequence for Karvex:** exporting `TMUX` + `TMUX_PANE` inside every
Karvex-managed pane, plus a `tmux` executable on `PATH` that speaks to the
Karvex server, makes Claude Code take branch 2 and treat the pane as
tmux. `ITERM_SESSION_ID` leaking through (e.g. inside an iTerm2-hosted
terminal) is harmless — branch 2 already won.

`isAvailable` is `tmux -V` exiting 0. The shim must always handle `-V`, even
with no Karvex server reachable (see [D3](#d3-shim-transport)).

⚠ **This entire mechanism requires `teammateMode` to actually resolve to
`auto`.** A user's own Claude settings file that pins `teammateMode` to
something else defeats the tmux backend outright, and Karvex has no way to
override that from outside Claude's own config. This is documented for users
in `integrations.mdx`, not just here.

## Invocation forms

Two wrappers exist in Claude's own code:

- `Tfe(args)`: if a socket path can be parsed from the `TMUX` env var (its
  first comma-field), invoke `tmux -S <socket> <args>`; else plain
  `tmux <args>`. Used for all in-session (leader) operations — this is the
  path the Karvex shim serves.
- `jse(args)`: `tmux -L <name> <args>` — only for "external session mode"
  (i.e. not inside tmux). Never taken while `TMUX` is set, so the Karvex shim
  never needs to implement it; it is explicitly out of scope
  (`01-port-plan.md` §1).

`TMUX` env format (real tmux): `<socket_path>,<server_pid>,<session_index>`.
Karvex sets `TMUX=<karvex_socket_path>,<karvex_server_pid>,0` and
`TMUX_PANE=<karvex_pane_id>` (Karvex's own `w1:p3`-shaped id — see
[Pane-id shape](#pane-id-shape) below) via `apply_tmux_compat_env`, called
from the `Managed` arm of pane launch (`src/pane.rs`).

⚠ **Found during W2 live probing:** the shim does not trust an inherited
`TMUX_PANE` value at face value the way the donor does. If the environment
the shim runs in carries a real-tmux-shaped `TMUX_PANE=%N` — inherited
through a wrapper, an outer real tmux session, or simply stale — the shim
recognizes the real-tmux `%N` shape (`looks_like_foreign_pane_id`, applied to
the decoded value) — the one shape a Karvex id can never take — and ignores
it, falling back to `$KARVEX_PANE_ID`, then to a `pane.current` request only
if that is also absent. Live probing showed the donor's take-it-at-face-value
approach answers a query like `display-message -p '#{pane_id}'` with the
stale/foreign `%N` — confidently and wrongly — without ever contacting the
server. ⚠ **This is a narrow, deliberate guard, not a full grammar check:**
it rejects only the `%N` shape; an arbitrary junk `TMUX_PANE` value (e.g.
`foo`) is still trusted and used as a target, surfacing as an ordinary
`can't find pane: foo` from the server rather than being caught here. Karvex
pane ids use a 32-character alphabet across several accepted shapes
(`w1:pA`, `p_<n>`, aliases), so a shape check strict enough to be useful
would risk rejecting legitimate ids — rejecting only the one shape Karvex
can never emit is the safe subset. This check runs on the decoded id,
downstream of the same `encode_pane_id`/`decode_pane_id` pair
[Pane-id shape](#pane-id-shape) describes, so it needs no update even if the
Q1b probe later changes what shape the shim itself emits.

## Command surface

The shim only services commands reachable through `Tfe`. Everything else —
`-L`, a `-S` argument that does not match the resolved socket, or an
unrecognized subcommand — passes through to a real `tmux` found later on
`PATH` (skipping the shim's own canonicalized path), or emulates
`no server running` if none exists. See [D5](#d5-socket-resolution) for
exactly how the shim resolves which socket is "the resolved socket".

| tmux invocation | Karvex mapping |
|---|---|
| `tmux -V` | print `tmux 3.5a (karvex-compat)`, exit 0 — even with no server reachable |
| `display-message -p '#{pane_id}'` | `$TMUX_PANE` when it is not `%N`-shaped, else `$KARVEX_PANE_ID`, else `pane.current` |
| `display-message [-t %N] -p '#{window_id}'` | `pane.current`/`pane.list` — the pane's tab, Karvex's `w1:t2`-shaped id |
| `display-message -p '#{client_control_mode}'` | `"0"` (harmless startup probe) |
| `display-message -p '#{client_termtype}'` | `$TERM` |
| `list-panes -t @N -F '#{pane_id}'` | `pane.list` filtered by tab, one id per line, ordered so element 0 is the leader (creation order) |
| `split-window`/`splitw -d -t <target> [-h\|-v] -l N% -P -F '#{pane_id}' [-- <cmd>]` | `pane.split`; `-v` → `Down`, absent → `Right`; `-l N%` inverted to the **existing** pane's ratio: `ratio = clamp(1 - N/100, 0.1, 0.9)`; `-d` present → `focus: false`, absent → `focus: true` (`PaneSplitParams.focus` defaults to `false`, so this must be mapped explicitly — Claude always passes `-d`); `workspace_id: None`; `cwd` from the target pane's `foreground_cwd`/`cwd`; a trailing `-- <cmd>` is **not** run; `-P` prints the new pane id |
| `respawn-pane -k -t <pane> -- <cmd>` | a real bounded shell-readiness wait — poll `pane.read{source:"visible"}` (the whole viewport, **not** `"recent"`) until two consecutive samples are non-empty and identical, capped at 10 × 150ms — then `pane.send_input{keys:["ctrl+u"]}` followed by `pane.send_input{text:<cmd>, keys:["Enter"]}`; `-k` is accepted and ignored |
| `select-pane -t <pane> -T <title>` | `pane.rename` |
| `select-pane -t <pane>` (no `-T`) | accept, exit 0 |
| `kill-pane`/`killp -t <pane>` | `pane.close` |
| `set-option -p -t <pane> {window-style, pane-border-style, pane-active-border-style} fg=<c>` | parse `fg=<colour>` and issue `pane.report_metadata{source:"karvex:tmux-compat", tokens:{"agent_accent": <colour>}}`, which the sidebar uses to tint the teammate's name in the agent panel — see [D8](#d8-teammate-accent-colour). Shipped by W5; every other `set-option`/`set` target stays accept-and-drop |
| `select-layout`, `resize-pane`/`resizep` | accept, exit 0 — Karvex's own split geometry stands in; no leader-30%/teammates-stacked layout mapping (see [Residual gaps](#residual-gaps)) |
| `show-options -g prefix` | print `prefix C-b` (stub — one line) |
| `send-keys -t <pane> <text> [Enter]` | `pane.send_input`; flags stripped; a trailing literal `Enter` token becomes a key, not text |
| anything else, `-L`, socket mismatch, no resolvable socket | passthrough to a real `tmux` on `PATH` (skipping the shim's own path), else emulate `no server running` on stderr, exit 1 |

Colours Claude sends: `red blue green yellow magenta cyan colour208 colour205`
(`colour208` → `orange`, `colour205` → `pink`, the rest pass through
unchanged).

Commands sent via `respawn-pane -- <cmd>` are a single shell-command string;
Claude validates on its own side that it contains no control characters. The
teammate command itself is `claude --agent-id <id> --agent-name <name>
--team-name <team> --agent-color <color> --parent-session-id <sid>
[--agent-type ...] ...`, typed into the pane's shell rather than exec'd — a
real bounded readiness wait guards this (the `respawn-pane` row above), not
an `exec`; see risk R4 in `01-port-plan.md` §4 for why that distinction
matters.

⚠ **Found during W2 live probing, correcting the plan's suggested
implementation:** the readiness poll must read `source:"visible"` (the whole
viewport), not `source:"recent"`. `"recent"` is anchored on the cursor row
and returns empty for a pane that has not yet written a scrollback
row — exactly the state the poll has to observe on a freshly split pane, so
polling it never converges and every respawn burned the full ~1.35s budget.
Switching to `"visible"` makes the two-stable-samples rule fire as intended;
measured respawn latency dropped to ~263ms, with the teammate command
verifiably executed in the pane's shell.

The trailing `-- <cmd>` on `split-window` (see the table above) is
deliberately not run because of what it is *for*: it is a holder command
whose only job, against a real tmux, is keeping a plain pane alive until the
follow-up `respawn-pane` replaces it. A Karvex pane already keeps its own
shell alive on its own, so there is nothing for the holder command to do —
`respawn-pane` is what actually submits the real teammate command.

### External-session path: out of scope

Only reached when `TMUX` is unset: `has-session -t <name>`, `new-session -d
-s <name> -n <win> -P -F '#{pane_id}' -- <holder>`, `new-window`,
`list-windows -F '#{window_name}'`, `select-layout tiled`. A Karvex pane
never hits this path (it always has `TMUX` set once tmux-compat lands), and
it is not implemented.

### Entry helper (`claude --tmux`, worktree mode): out of scope

Uses `show-options -g prefix`, `new-session -A`, `switch-client -t`,
`attach-session -t`, plus dev-mode `send-keys` splits. Not needed for
teammate spawning; `01-port-plan.md` §1 excludes it explicitly. The shim
refuses `-L` and passes it through to a real tmux rather than attempting to
emulate this surface.

## Pane-id shape

Real tmux ids are `%N` (pane) and `@N` (window). Karvex emits its own
`w1:p3`/`w1:t2`-shaped ids, unchanged — Claude treats every id it receives
from `display-message`/`split-window`/`list-panes` as an opaque string and
hands it back verbatim via `-t`, so this is expected to work exactly as it
does for bakr, which ships the same way.

⚠ **Found during W2 live probing.** The pane number inside a `w1:pN` id is
Karvex's own base-32 encoding (`public_pane_id_for_number`), not a decimal
ordinal — pane 10 renders as `w1:pA`, not `w1:p10`. This matters for
ordering, not just display: the donor derives a pane's creation ordinal by
parsing the trailing run of *decimal* digits in the id, which is correct for
the donor's own ids but wrong for Karvex's — under that rule every pane from
10 onward parses as "no digits" and sorts to the end, which destroys the
leader-first ordering `list-panes` must produce (see
[Behavioral notes](#behavioral-notes)). The shim instead decodes the pane
number properly via `workspace::decode_public_number`, keeping a decimal
parse only as a fallback for other id shapes.

⚠ **Unverified at time of writing** — flagged as gate-phase probe Q1b in
`01-port-plan.md` §8: does anything in Claude's Agent Teams path validate the
`%N`/`@N` sigil shape rather than treating ids as opaque? If the gate probe
(`01-port-plan.md` §7 step 13) finds that it does, the fix is a bijective
`w1:p3 ↔ %<ordinal>` mapping behind a single pair of pure
`encode_pane_id`/`decode_pane_id` functions in the shim — every id must
already route through that pair rather than being formatted inline, so the
fix is contained if it is ever needed. **TBD: record the probe result here
once the gate phase runs.**

Similarly unverified (Q1c): whether anything parses the `-V` banner more
strictly than "exit 0". Karvex prints `tmux 3.5a (karvex-compat)`; real tmux
prints `tmux 3.5a`. If something on a user's `PATH` chokes on the suffix, drop
it. **TBD: record the probe result here once the gate phase runs.**

## Behavioral notes

- All operations must be fast (well under 2s); Claude uses short timeouts on
  its probes. The shim's own request timeout is 1500ms — see
  [D3](#d3-shim-transport).
- `list-panes` output feeds Claude's own `.slice(1)`, so element 0 **must**
  be the leader pane. Pane enumeration is ordered by each pane's *decoded*
  pane number (`workspace::decode_public_number`, not a raw string or
  decimal-suffix sort — see [Pane-id shape](#pane-id-shape)), which is
  creation order: leader first, then teammates in creation order.
- After every spawn, Claude issues the layout rebalance sequence
  (`select-layout main-vertical` then `resize-pane -x 30%` on the leader).
  Karvex accepts and drops both; see [Residual gaps](#residual-gaps).
- Failure text matters on `pane.split` failure: stderr contains `no space for
  new pane (too small)` (or `too small`) so Claude's friendlier UI path is
  reachable. This applies on **both** a genuine API error and an unexpected
  response shape — bakr only covers the latter branch; Karvex covers both.
  Karvex has no minimum-pane-size guard, so in practice a split essentially
  always succeeds and this text rarely fires; it is still correct to wire up.
- Exit codes: 0 on success, non-zero with a stderr line on failure. stdout is
  parsed exactly (trimmed) by Claude — the shim prints only what the `-F`
  format asks for, never a JSON envelope on either stream.

## Deviations from bakr

These are Karvex-specific decisions the port makes that bakr does not, or
does differently. Full rationale for each is in `01-port-plan.md` §3; this
is the condensed version for anyone integrating against the shim.

### D1: shim install gates the identity export

bakr exports `TMUX`/`TMUX_PANE` unconditionally and only then tries to
install the shim, so a platform or filesystem where the shim can't be
installed still points Claude at a tmux backend that isn't there. Karvex
calls `platform::ensure_tmux_shim_dir()` first and only exports the identity
env on success. On Windows, and on any install failure, pane env is
unchanged and Claude falls back to its own backends — see
[D10](#d10-windows).

### D3: shim transport

The shim owns its own transport rather than reusing Karvex's CLI request
helpers (`cli::send_request`/`send_request_unchecked`). Two reasons: those
helpers perform an extra protocol-compatibility round trip the shim can't
afford under Claude's short probe timeouts, and — more importantly — *both*
of them resolve their socket through `session::active_api_socket_path()`,
which silently falls back to the default session's socket when
`$KARVEX_SOCKET_PATH` is unset. That is exactly the wrong-server hazard
[D5](#d5-socket-resolution) exists to prevent, so the shim instead builds an
explicitly targeted `ApiClient::for_target(ConnectionTarget::SocketPath(..))`
with a 1500ms request timeout, and maps every error itself to one plain
stderr line — never a JSON envelope on stdout or stderr. A connect failure
against a resolvable-but-dead socket reports `no server running` on stderr,
exit 1, rather than falling through to passthrough (passthrough would exec a
real tmux with `-S <karvex socket>`, which produces confusing output for a
socket a real tmux never owned). Measured live against a stopped server: the
failure surfaces in ~6ms, stderr is exactly `no server running`, stdout is
empty, and neither stream carries a JSON envelope. `tmux -V` short-circuits
before any socket work and keeps exiting 0 in the same condition, so
Claude's `isAvailable` check keeps passing against a stopped server.

### D5: socket resolution

The shim never falls back to the default session. It resolves its socket, in
order: `$KARVEX_SOCKET_PATH` (non-empty after trim); else the first
comma-field of `$TMUX` (non-empty after trim); else the command is not
serviced and passes through. A `-S <path>` argument is compared against the
resolved path (canonicalized where possible, else a trimmed string compare)
and passes through on mismatch.

### D6: shim ownership guard

The shim symlink at `<data_dir>/shims/tmux` (and its macOS
`~/.local/bin/tmux` mirror) is only ever installed or re-pointed by a binary
whose `current_exe()` file stem is exactly `kvx` — not `kvx-<hash>` or
`karvex-<hash>` (Cargo's test binary naming), so `cargo nextest` can never
hijack a user's real `tmux`. A pre-existing link is only replaced when its
recorded target's stem also passes that check (including a **dangling**
link left by a package-manager upgrade, inspected via `symlink_metadata`); a
real file, or a foreign symlink, is never touched — only logged.

### D7: hook reconciliation

Karvex keeps its own `karvex-agent-state.sh` hook (handles `session` **and**
`stop`) rather than adopting bakr's `SessionStart`-only version, because the
`stop → idle` report is consumed by the workflow engine
(`EngineInput::TurnEnded`) and bakr's version regressed it. No hook change,
no `CLAUDE_INTEGRATION_VERSION` bump — **unless** the gate-phase probe
(`01-port-plan.md` §7 step 8) finds that Agent-Teams teammate hook events
carry `agent_id`, which the hook's existing `is_subagent` guard treats as an
in-process subagent and ignores. **TBD — record the probe's raw JSON result
and verdict here** once the gate phase runs; see
[Hook probe result](#hook-probe-result) below.

### D8: teammate accent colour

Which colour Claude assigned a teammate is a shared runtime fact (chosen by
an external agent runtime, identifies the teammate, useful to any future
client), so it goes through the existing `pane.report_metadata` channel
rather than a new API field or an accept-and-drop. The shim writes
`pane.report_metadata{source:"karvex:tmux-compat", tokens:{"agent_accent":
"<colour>"}}` with a monotonic `seq`, no TTL.

⚠ **The token key is `agent_accent`, not `agent.accent`.** Metadata token
keys are validated as `[A-Za-z0-9_-]` only (`normalize_metadata_tokens`,
`src/app/api_helpers.rs`) — a dot is rejected with `invalid_metadata_token`.
This dotted form must not appear anywhere else in these docs or in the
shim's code; it does not work. (The `source` field is a different validator
and does allow dots and colons, so `karvex:tmux-compat` is fine.)

**Shipped in full — both halves landed, no fallback needed.** Writing the
token is the shared-fact half; the sidebar also tints the teammate's name in
the agent panel with it, so `kvx pane get` and the sidebar agree. Edge cases
as shipped: an unknown colour value is dropped and never written (no garbage
token reaches the API), and an explicit `fg=` with an empty value clears the
token (a `None` patch) rather than leaving a stale colour behind.

The parse gates on the **option name** before ever looking at the value:
`plan_accent(option, value)` (`src/cli/tmux_compat.rs`) returns
`AccentPlan::NotAccent` immediately unless `option` is one of
`ACCENT_STYLE_OPTIONS` (`window-style`, `pane-border-style`,
`pane-active-border-style`) — only then does it split `value` looking for an
`fg=` segment. This is why Claude's `pane-border-format
"#[fg=<c>,bold] #{pane_title} #[default]"` call, whose *value* string
contains a literal `fg=colour208,bold` inside a format string rather than a
colour assignment, never produces a spurious accent: `pane-border-format` is
not in `ACCENT_STYLE_OPTIONS`, so the function returns on its first line and
the value is never scanned. Covered by
`set_option_pane_border_format_does_not_yield_an_accent`
(`src/cli/tmux_compat.rs`, green).

### D9: unconditional

tmux-compat has nothing to do with the `workflow` cargo feature. All of it
compiles and passes under both `--features workflow` and
`--no-default-features`.

⚠ **Consequence the plan did not call out: workflow node panes get `TMUX`
too.** `apply_tmux_compat_env` hangs off the `Managed` arm of
`apply_pane_launch_env`, and the workflow binding's pane spawns
(`src/workflow/binding/spawn.rs` → `Workspace::split_pane_argv_command` →
`launch_env_for_new_pane`) are ordinary Managed panes, so a `claude` running
as a workflow node sees `TMUX` set exactly like a hand-opened pane. This is
deliberate and was audited rather than changed:

- Backend selection only *changes* for a node that actually asks for Agent
  Teams teammates. A normal (non-teams) `claude` node is unaffected by
  `TMUX` being set.
- If a node does spawn teammates, they become sibling panes in the same tab.
  The workflow binding is keyed by `PublicPaneId`
  (`src/workflow/binding/observe.rs`) and never enumerates a tab's panes, so
  unbound sibling panes produce no engine input and cannot be mistaken for
  node panes.
- The residual cost is cosmetic and containment-shaped: `close_node_pane`
  closes only the node's own pane, so teammate panes a node spawned outlive
  the node, and the run's split geometry is sized without them. Suppressing
  the export per-node would be the alternative, and was rejected — it would
  make one class of Managed pane silently different from every other, which
  is the inconsistency this codebase's guardrails exist to avoid.

### D10: Windows

`ensure_tmux_shim_dir()` returns `None` on Windows; by D1, nothing is
exported. This is a documented non-goal, not a silent gap — see
`integrations.mdx`'s Windows note.

### D11: DCS passthrough unwrap is a prerequisite, not an enhancement

See [Risk: TMUX export vs. terminal passthrough](#risk-tmux-export-vs-terminal-passthrough)
below. Exporting `TMUX` without first unwrapping `\ePtmux;…\e\\` passthrough
is not a smaller/later version of this feature — it is a user-visible
regression (broken clipboard, broken colour queries) in every pane, not just
teammate panes.

## Risk: `TMUX` export vs. terminal passthrough

The near-universal convention among terminal apps (neovim's OSC-52 clipboard
provider, fzf, yazi, lazygit, tmux-aware shell prompts, and Karvex's own
`terminal_notify.rs`) is: *if `$TMUX` is set, wrap OSC/DCS sequences in
`\ePtmux;<escaped>\e\\` passthrough.* The moment Karvex exports `TMUX` for
tmux-compat, every such app starts wrapping — and Karvex's terminal
(libghostty-vt) drops that wrapper wholesale rather than unwrapping it: a
`\ePtmux;…` sequence parses as a DCS with no hook, gets ignored until unhook,
and every payload byte inside is discarded. Net effect without a fix:
copy-to-clipboard from inside pane apps stops working, and
background/foreground colour queries stop resolving, for every pane — a
broad regression triggered by a feature most users aren't using.

The fix is a streaming unwrap filter that sits at the very top of the pane's
inbound byte path, before any of Karvex's own OSC/DCS observers see the
bytes, so both the clipboard write path and Karvex's own synthesized
OSC 11/OSC 4/XTGETTCAP responses see unwrapped bytes. This is mandatory and
must land before or together with the `TMUX` export — never after.

**Acceptance is two-part** and both halves are hard gates
(`01-port-plan.md` §7 step 7, §9): (a) an OSC 52 clipboard write made through
tmux-wrapped passthrough reaches the system clipboard, and (b) a
tmux-wrapped OSC 11 background-colour query produces a response. If either
fails, the fallback is to invert the `TMUX` export from default-on to
opt-in (`KARVEX_TMUX_COMPAT=1`) rather than ship the regression. **TBD:
record which outcome shipped here** once the gate phase runs.

## Residual gaps

Recorded here rather than left implicit, per `01-port-plan.md`'s definition
of done:

- **No macOS end-to-end coverage.** The crate's `tests/cli/` tree (where the
  real-shim-binary tests live) is compiled out on macOS
  (`#![cfg(not(target_os = "macos"))]` on `tests/cli.rs`), which is exactly
  the platform where the riskiest write — the `~/.local/bin/tmux` mirror —
  lives. The mirror's guarantees (never created if `~/.local/bin` doesn't
  already exist, ownership rules, dangling-link handling) are instead
  covered by `src/`-level unit tests, which do run on macOS, plus a
  by-hand pass in the gate phase on a macOS box when one is available.
- **`select-layout` and `resize-pane` are accept-and-drop.** Karvex does not
  map Claude's leader-30%/teammates-stacked layout request onto its own
  split geometry; Karvex's own layout stands in instead. A real mapping
  (`main-vertical` → `layout.apply`) is a follow-up, not part of this
  landing. **TBD: record here how bad this looks in practice** once the gate
  phase runs (`01-port-plan.md` §8 Q3).
- **No general tmux replacement.** tmux control mode (`DCS 1000 p`), the
  external-session path, and the `claude --tmux` entry helper are all out of
  scope; a real tmux still wins for anything the shim doesn't service or
  passes through to.

## Verified so far (build-time, pre-gate-phase)

W2 drove the shim directly through a real pty session (spawned, exercised,
torn down) rather than through an actual Claude Code process. This is real
evidence, not a substitute for `01-port-plan.md` §7's gate phase — it
confirms the shim's own behavior in isolation, not that Claude Code's Agent
Teams feature actually detects and drives it end to end. Confirmed this way:
`tmux -V`; both `#{pane_id}`/`#{window_id}` id formats; the
`#{client_control_mode}`/`#{client_termtype}` startup probes; `split-window
-d -h -l 70% -P` producing `splits[0].ratio == 0.3` with focus staying on the
leader; leader-first `list-panes` ordering; `select-pane -T`; `respawn-pane`
actually executing the submitted command in the pane's shell; `send-keys`;
the accept-and-succeed verbs (`set-option`, `select-layout`, `resize-pane`,
`show-options`); `kill-pane`; `tmux -L foo` and a mismatched `-S` both
passing through to the system tmux untouched; and the dead-server behavior
recorded under [D3](#d3-shim-transport).

Still open, and still requiring the real gate phase against a live Claude
Code session: Q1 (hook `agent_id`, see
[Hook probe result](#hook-probe-result) below), Q1b (pane-id sigil
validation), Q1c (`-V` banner strictness), Q2 (`respawn-pane` reliability
under real teammate commands), Q3 (how the accept-and-drop layout looks in
practice), and the two-part R1 clipboard/colour-query acceptance.

## Hook probe result

**TBD.** `01-port-plan.md` §7 step 8 / §8 Q1: does a live Agent-Teams
teammate pane's hook event carry `agent_id`, which
`karvex-agent-state.sh`'s existing `is_subagent` guard would treat as an
in-process subagent and ignore (meaning the teammate pane never reports
`agent_session` back to `kvx pane get`)? This decides whether W9 (hook
reconciliation) runs and whether `CLAUDE_INTEGRATION_VERSION` moves from 8 to
9. Record the raw hook JSON and the verdict here once the gate phase runs —
a clean probe (teammate pane does report a session) is the evidence that "no
hook change" was correct, not merely convenient.

## Removal

There is no `kvx uninstall` command, and `KARVEX_NO_TMUX_COMPAT`
deliberately does not delete anything it already installed — it only stops
exporting the identity env going forward (and, since the opt-out is checked
before `ensure_tmux_shim_dir` runs, stops *new* installs too). To remove the
shim entirely:

- `rm <data_dir>/shims/tmux` — `<data_dir>` is `session::data_dir()`, i.e.
  `~/.config/karvex` for the default session and
  `~/.config/karvex/sessions/<name>` for a named one, so a machine that has
  used named sessions has one link per session. Set `KARVEX_NO_TMUX_COMPAT`
  first, or the next Managed pane launch recreates it.
- On macOS, the `~/.local/bin/tmux` mirror **outlives an uninstall of
  Karvex** and keeps shadowing the user's real tmux until removed by hand:
  check that it points at Karvex before deleting it (`readlink
  ~/.local/bin/tmux`), then `rm ~/.local/bin/tmux` if so. Karvex never
  creates `~/.local/bin` itself and never touches a foreign file or symlink
  there — only its own link.

The user-facing version of this is in `integrations.mdx`'s Claude Agent
Teams section; keep the two in sync if either changes.
