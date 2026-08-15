# Claude Code teammate tmux backend — Karvex spec

Status: describes the code as built and merged into the working tree
(W1–W5, W7, W8 complete; adversarially audited 2026-08-09). The live gate
phase (`01-port-plan.md` §7) ran on 2026-08-09 against Claude Code
v2.1.226 with real Agent Teams teammates; its results are recorded inline
below in place of the former TBD markers. Everything here reflects shipped
behavior and is backed by tests, live evidence, or both.

Amended after the shim was found unreachable on machines whose shell startup
re-orders `PATH` past it (the fix that made teammate spawning work on Linux
and macOS regardless of `PATH` order): see
[Shim visibility](#shim-visibility-on-path), and the socket-ownership rule in
[D5](#d5-socket-resolution) that had to land with it.

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

## Shim visibility on PATH

Everything below only happens if Claude's `tmux` lookup lands on Karvex's
shim. That lookup is an ordinary `PATH` search (Claude spawns `tmux` by
name), and `PATH` is not Karvex's to control: Karvex prepends
`<data_dir>/shims` when it launches the pane, and the pane's **shell startup
then re-orders `PATH`**. `path_helper` (macOS `/etc/zprofile`) moves every
inherited entry behind `/usr/local/bin` and `/etc/paths.d`; `brew shellenv`
prepends the Homebrew prefix; `fish_add_path`, mise, asdf, nix and a plain
`export PATH="...:$PATH"` all prepend too.

Measured on a stock fish setup: the shims directory Karvex handed the pane as
`PATH[0]` came out of shell startup at **index 9**, behind eight directories
the shell had prepended.

That is harmless until one of those directories holds a real `tmux` — the
Homebrew prefix being the common case, which is why this bit macOS hardest.
Then Claude runs the real tmux with `-S <karvex socket>`, it fails, and
Claude's backend registry marks `inProcessFallbackActive` and **silently
falls back to in-process teammates**. The user's report is "teammates don't
spawn as panes", with nothing in Karvex's own logs to explain it.

So the prepend is necessary but not sufficient, and Karvex adds one mirror:

- **Only when it can lose.** If no foreign `tmux` is anywhere on `PATH`, the
  prepend cannot be beaten and Karvex writes nothing outside its data
  directory.
- **Only beside the binary that owns it.** The mirror goes next to Karvex's
  own `kvx` on `PATH` — the directory the user installed Karvex into, found by
  canonicalizing `<path entry>/kvx` against `current_exe()`, so symlinked
  installs recognise themselves. Nowhere else is Karvex's to write to, and
  nowhere else needs guessing. An install directory that sits *behind* the
  foreign tmux is skipped rather than shadowed for nothing.
- **Never in someone else's prefix.** `/nix/store`, `/usr`, `/opt/homebrew`,
  `/opt/local`, `/home/linuxbrew`, `/snap`, Homebrew Cellar paths and friends
  are excluded even when Karvex itself was installed there.
- **Never by an uninstalled binary.** See [D6](#d6-shim-ownership-guard).
- **Always refreshed.** A mirror answers `tmux` for the whole machine, so an
  existing one is repointed at the current binary on every install pass; a
  Karvex upgrade cannot leave a dangling `tmux` behind.
- **Never created directories, never foreign files.** Same rules as the
  primary shim, one implementation (`link::install_shim_symlink`).

When the install directory sits behind the foreign tmux — or is a package
manager's prefix, so a Homebrew- or Nix-installed Karvex cannot put a `tmux`
next to itself — Karvex installs no mirror and logs one warning per process
naming the tmux that wins. The
identity export is deliberately **not** withheld in that case: the prediction
is made from the server's `PATH` and can be wrong in the user's favour, and
Claude degrades to in-process teammates on its own.

The policy lives in `src/platform/tmux_shim/plan.rs` as a pure function over
an injected `ShimFacts`, so every case above is a unit test with no
filesystem involved; the symlink mechanics are `src/platform/tmux_shim/link.rs`.

⚠ Putting a `tmux` on the user's `PATH` is only safe because passthrough is
airtight — see [D5](#d5-socket-resolution) for the socket-ownership rule that
had to land with it.

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

✅ **Q1b answered by the live gate (2026-08-09, Claude Code v2.1.226).**
Claude treats the ids as opaque. Across a full teammate lifecycle — 15 shim
invocations covering `display-message`, `list-panes`, `split-window`,
`set-option` ×5, `select-pane -T`, `respawn-pane`, and `kill-pane` — every
`-t` target Claude sent back was a Karvex-shaped `w1:pN`/`w1:tN` id it had
received from us, with zero errors, zero retries, and no re-probing. The
`w1:p3 ↔ %<ordinal>` mapping is **not** needed; the
`encode_pane_id`/`decode_pane_id` seam stays in place as insurance only.

✅ **Q1c answered: the banner is never parsed, because `-V` is never
called.** `insideTmux` (a synchronous `process.env.TMUX` check) short-circuits
backend selection before `isAvailable` is consulted, so a pane that exports
`TMUX` never reaches the `tmux -V` availability probe at all. The live
invocation log contains no `-V` call. The `(karvex-compat)` suffix is
therefore unexercised by Claude on this path; it is kept because it is the
honest answer for anything else on `PATH` that asks.

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
[D10](#d10-windows). The same rule applies one level down: a `PATH` the shim
directory cannot be prepended to (`prepend_path_once` returning `None`)
exports nothing either.

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

The shim never falls back to the default session, and never answers for a
multiplexer that is not Karvex.

`$TMUX` is the authority on which multiplexer the process is running inside,
so it is consulted first: a socket it names is serviced only when that socket
is demonstrably Karvex's own — it matches `$KARVEX_SOCKET_PATH`, or it
carries Karvex's own socket file name (`session::API_SOCKET_FILE_NAME`).
Anything else is a real tmux server's socket and passes through. When `$TMUX`
is unset, `$KARVEX_SOCKET_PATH` (non-empty after trim) still resolves on its
own: that is an unambiguous request to talk to one named Karvex server.
Otherwise the command is not serviced and passes through. A `-S <path>`
argument is compared against the resolved path (canonicalized where possible,
else a trimmed string compare) and passes through on mismatch.

⚠ **The ownership check is load-bearing, not defensive tidying.** The earlier
rule preferred `$KARVEX_SOCKET_PATH` and otherwise trusted `$TMUX` outright,
which mis-serviced two real cases now that the shim is reachable from the
user's own `PATH` (see [Shim visibility](#shim-visibility-on-path)): a user
running a real tmux *inside* a Karvex pane still inherits
`$KARVEX_SOCKET_PATH`, and a user running one outside Karvex has `$TMUX`
pointing at `/tmp/tmux-<uid>/default`. Both were answered out of Karvex's
pane tree instead of reaching the tmux they named.

### D6: shim ownership guard

The shim symlink at `<data_dir>/shims/tmux` (and any `PATH` mirror) is only
ever installed or re-pointed by a binary whose `current_exe()` file stem is
exactly `kvx` — not `kvx-<hash>` or `karvex-<hash>` (Cargo's test binary
naming), so `cargo nextest` can never hijack a user's real `tmux`. A
pre-existing link is only replaced when its recorded target's stem also
passes that check (including a **dangling** link left by a package-manager
upgrade, inspected via `symlink_metadata`); a real file, or a foreign
symlink, is never touched — only logged.

A **second** gate applies to mirrors only, because a mirror is machine-visible
state rather than something inside Karvex's own data directory: the only
directory Karvex will write one into is the one that makes the *running*
binary runnable by name (`<path entry>/kvx` canonicalizes to `current_exe()`,
so symlinked installs — `~/.local/bin/kvx`, a Homebrew `bin` entry into the
Cellar, a Nix profile link into the store — all recognise themselves), and
never when that directory belongs to a package manager.

The exact-stem rule alone does not cover this, and neither does a
home-relative candidate list. Karvex's own test suites spawn the *real* `kvx`
binary with the developer's `$PATH` inherited — `tests/cli/` straight out of
`target/debug`, and `tests/workflow_headless.rs` / `tests/cli/workflow.rs`
through a `<test base>/bin/kvx` symlink they prepend to that `PATH` so a
node's own `kvx workflow node complete` resolves to the binary under test. An
earlier version of this rule mirrored into `~/.local/bin` when it appeared on
`PATH` ahead of a real tmux, and those workflow suites promptly installed a
`tmux` into the developer's home. Binding the mirror to the directory the
running binary is reachable from makes that structurally impossible: a
`target/debug` build writes nothing, and a test's `<test base>/bin` build
writes only into its own sandbox. Covered by
`a_binary_that_is_not_installed_on_path_writes_nothing_outside_its_data_dir`,
`an_uninstalled_binary_never_writes_outside_its_data_dir`, and
`a_mirror_only_ever_lands_next_to_the_binary_that_owns_it`.

### D7: hook reconciliation

Karvex keeps its own `karvex-agent-state.sh` hook (handles `session` **and**
`stop`) rather than adopting bakr's `SessionStart`-only version, because the
`stop → idle` report is consumed by the workflow engine
(`EngineInput::TurnEnded`) and bakr's version regressed it. No hook change,
no `CLAUDE_INTEGRATION_VERSION` bump. The gate-phase probe
(`01-port-plan.md` §7 step 8) confirmed this was correct rather than merely
convenient: teammate hook events do **not** carry `agent_id`, so the
`is_subagent` guard never fires for them and a teammate pane reports its own
session normally. W9 did not run and the constant stays at 8. See
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
tmux-wrapped colour query produces a response identical to the bare form. If
either fails, the fallback is to invert the `TMUX` export from default-on to
opt-in (`KARVEX_TMUX_COMPAT=1`) rather than ship the regression.

✅ **Both halves passed live (2026-08-09); the default-on export shipped.**

- **(a) Clipboard.** From inside a pane with `TMUX` exported,
  `\ePtmux;\e\e]52;c;<base64>\a\e\\` landed in the real system clipboard
  (`wl-paste` read back the exact payload). The write travels
  pane → `ProcessBytesResult.clipboard_writes` → `AppEvent::ClipboardWrite`
  → `ServerMessage::Clipboard` to the foreground client → the host clipboard,
  so on a desktop with a working native clipboard it never appears as an
  OSC 52 re-emission on the client's stdout — check the clipboard, not the
  client's output stream, when verifying this by hand.
- **(b) Colour/capability queries.** A tmux-wrapped `OSC 4;1;?` palette query
  answered `\e]4;1;rgb:cccc/6666/6666\e\\` — **byte-identical** to the bare
  query — and a wrapped `CSI 6n` answered identically to the bare form too.
  Wrapped `OSC 11`/`OSC 10` returned empty in that harness, but so did the
  *bare* form: Karvex only answers a default fg/bg query once it has learned
  a host terminal theme, and the gate harness (detached tmux → `script` → a
  pipe) had no terminal to learn one from. Identical bare/wrapped behavior in
  every case, and
  `tmux_passthrough_unwraps_osc11_query_and_a_response_is_produced` pins the
  OSC 11 equality with a host theme applied.

## Residual gaps

Recorded here rather than left implicit, per `01-port-plan.md`'s definition
of done:

- **No macOS end-to-end coverage.** The crate's `tests/cli/` tree (where the
  real-shim-binary tests live) is compiled out on macOS
  (`#![cfg(not(target_os = "macos"))]` on `tests/cli.rs`), which is also the
  platform the `PATH` mirror matters most on. The mirror's guarantees
  (installed only when something could shadow the shim, only ahead of it,
  only next to the binary that owns it, never in a package manager's prefix,
  ownership rules, dangling-link handling) are covered by `src/`-level unit
  tests, which do run on macOS, plus live verification on Linux against a
  real server with a real competing `tmux` on `PATH`.
- **A `tmux` ahead of Karvex's own install directory cannot be beaten**, and
  neither can one competing with a Karvex installed inside a package manager's
  prefix (Homebrew, Nix). Karvex logs a warning and leaves the export in
  place; teammates may then spawn in-process. The user-facing remedy is to put
  the directory holding `kvx` ahead of the competing one on `PATH`. The alternative — re-asserting the
  prepend from inside the pane's shell startup, the way editors inject shell
  integration (`ZDOTDIR`, `bash --init-file`, `fish -C`) — would cover this
  case too, at the cost of Karvex taking over every user's shell startup;
  that is a deliberate non-goal here.
- **`select-layout` and `resize-pane` are accept-and-drop.** Karvex does not
  map Claude's leader-30%/teammates-stacked layout request onto its own
  split geometry; Karvex's own layout stands in instead. A real mapping
  (`main-vertical` → `layout.apply`) is a follow-up, not part of this
  landing.

  ✅ **Q3 answered for one teammate: it looks fine, because `split-window`
  already carries the geometry.** With a single teammate Claude sends
  `split-window -d -t <leader> -h -l 70%`, which the shim inverts to a 0.3
  ratio for the leader — measured live at 52/174 columns for the leader
  against 122 for the teammate, with focus staying on the leader. The
  `select-layout main-vertical` / `resize-pane -x 30%` pair that follows is
  then genuinely redundant. It is only at 3+ teammates, where the stacking
  half of `main-vertical` carries information the per-split ratios do not,
  that dropping it is expected to bite; that case was not exercised at
  the gate and remains the reason the follow-up mapping is worth doing.

- **`list-panes` treats the tab's first pane as the leader.** Claude's own
  `.slice(1)` assumes element 0 is the leader, and the shim's leader-first
  ordering is creation order within the tab — so a Claude started in a tab
  that *already* contains an older pane will have that unrelated pane
  treated as the leader (observed at the gate: `resize-pane -x 30%` was
  aimed at a bystander pane). Every verb this affects is accept-and-drop, so
  the observed impact was nil, but the assumption is worth knowing: the
  intended shape is Claude owning its tab.

- **A teammate that finishes its own task is not always reaped.** Reaping is
  driven entirely by Claude issuing `kill-pane`, which it does on an explicit
  teammate stop (verified live: `kill-pane -t w1:p3` closed the pane). At the
  gate one teammate reported "finished" on its own and the leader's roster
  dropped it *without* issuing `kill-pane`, leaving a live teammate `claude`
  process in its Karvex pane. Nothing in the shim can detect this — Karvex
  faithfully executed every command it was given — but users may see an
  orphaned teammate pane they need to close by hand.
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

## Verified by the live gate phase (2026-08-09)

`01-port-plan.md` §7 ran against Claude Code v2.1.226 with
`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`, driving real teammates in a
throwaway named session. Every open question above is now answered: Q1
(hook `agent_id` — clean), Q1b (id sigil — opaque), Q1c (`-V` — never
called), Q2 (`respawn-pane` — reliable, see below), Q3 (layout — fine at one
teammate), and the two-part R1 acceptance (both halves passed).

The exact command sequence Claude issued to spawn one teammate, in order,
was:

```
show -Av mouse                              → passthrough (real tmux)
show -gv focus-events                       → passthrough (real tmux)
display-message -p -t w1:p1 '#{session_name}:#{window_id}.#{pane_id}'
display-message -t w1:p1 -p '#{window_id}'  → w1:t1
list-panes -t w1:t1 -F '#{pane_id}'         → w1:p1
split-window -d -t w1:p1 -h -l 70% -P -F '#{pane_id}' -- cat  → w1:p2
set-option -p -t w1:p2 window-style bg=default,fg=blue
set-option -p -t w1:p2 pane-border-style fg=blue
set-option -p -t w1:p2 pane-active-border-style fg=blue
select-pane -t w1:p2 -T gate-mate
set-option -p -t w1:p2 pane-border-format '#[fg=blue,bold] #{pane_title} #[default]'
list-panes -t w1:t1 -F '#{pane_id}'         → w1:p1, w1:p2
set-option -w -t w1:t1 pane-border-status top
set-option -p -t w1:p2 remain-on-exit failed
respawn-pane -k -t w1:p2 -- '<teammate claude command>'
```

and, on an explicit teammate stop, `kill-pane -t <pane>`. `Q2` is answered by
that `respawn-pane` line working every time it was issued: the teammate
command reached the pane's shell and started, with no mangling from the
readiness wait.

Two surface gaps were observed and deliberately left as-is:

- **Composite `-F` formats are not interpolated.** Claude's startup probe
  `display-message -p '#{session_name}:#{window_id}.#{pane_id}'` is not one
  of the recognized single-token formats, so the shim prints an empty line
  and exits 0, per the unknown-format contract. Claude proceeded normally and
  the whole teammate lifecycle worked, so this is non-fatal — but a small
  format interpolator over the tokens the shim already answers would be
  strictly more faithful.
- **`show -Av mouse` and `show -gv focus-events` fall through to
  passthrough.** Only `show-options … prefix` is serviced. Against a machine
  with a real tmux installed these reach it and fail with tmux's own "no
  server running", which Claude tolerates. On a machine with no tmux at all
  the shim emulates the same failure. Neither affected teammate spawning.

Also confirmed live: `tmux -L foo ls`, an unknown verb, and a mismatched
`-S <path>` all passed through to the system `/usr/bin/tmux` (each failing
with real tmux's own error text, never Karvex's); the
`KARVEX_NO_TMUX_COMPAT=1` opt-out left panes with no `TMUX`, no `TMUX_PANE`,
an unmodified `PATH`, `tmux` resolving to `/usr/bin/tmux`, and no `shims/`
directory created at all; the shims directory contained exactly one entry,
`tmux`, symlinked to the running binary; and with the server stopped
`tmux -V` still exited 0 while `list-panes` failed in 14ms with exactly
`no server running` on stderr, an empty stdout, and no JSON envelope.

## Hook probe result

**Clean — no hook change, no version bump.** Live gate, 2026-08-09, Claude
Code v2.1.226. A teammate pane reports its own session identity through the
unmodified `karvex-agent-state.sh` exactly like a hand-started Claude pane:

```
$ kvx pane get w1:p2
  "label": "gate-mate",
  "agent": "claude",
  "agent_status": "idle",
  "agent_session": {
    "agent": "claude", "kind": "id", "source": "karvex:claude",
    "value": "68dcbedf-af8a-4766-b2be-9a3b4ee2b54d"
  },
  "tokens": { "agent_accent": "blue" }
```

The value differs from the leader pane's session id, so this is the
teammate's own `SessionStart`, not the leader's leaking across. The hook's
`is_subagent` guard (`bool(hook_input.get("agent_id"))`) is aimed at
*in-process* subagents; an Agent-Teams teammate is a separate `claude`
process owning its own pane and its own session, and its hook payload carries
no `agent_id`. W9 did not run; `CLAUDE_INTEGRATION_VERSION` stays at 8.

Lifecycle state was verified live end to end as well: the teammate pane
transitions `idle → working → idle` as it receives and answers a
`SendMessage` from the leader, and appears as a first-class entry in
`kvx agent list` with its own session id.

⚠ **One gate-phase fix was required to get there** — see
[Teammate process identity](#teammate-process-identity) below. The hook was
always fine; agent *identification* was not.

## Teammate process identity

⚠ **Found by the live gate; fixed in `src/detect/mod.rs`.** Claude's native
installer keeps version-pinned binaries at
`<data dir>/claude/versions/<version>` with a `claude` symlink to the active
one on `PATH`. A hand-started Claude therefore has `claude` as its argv[0] —
but the teammate command Claude submits through `respawn-pane` invokes the
**absolute versioned path**:

```
cd <cwd> && env CLAUDECODE=1 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 \
  /home/<user>/.local/share/claude/versions/2.1.226 \
  --agent-id gate-mate@session-<sid> --agent-name gate-mate \
  --team-name session-<sid> --agent-color blue \
  --parent-session-id <sid> --permission-mode auto --effort high --model haiku
```

so the teammate process's argv[0] basename is a bare version string
(`2.1.226`) and every basename-based rule in `identify_agent_in_job` missed
it. The observed consequence was that a teammate pane reported `agent: null`
and `agent_status: "unknown"`, no manifest rule ever ran against its screen,
and it never appeared in `kvx agent list` or the sidebar's agent panel — even
though its hook was reporting a session correctly the whole time.

The fix is a narrow path-shape rule in `agent_name_from_known_package_path`:
the last three path components must be exactly `claude`, `versions`, and a
component starting with a digit. A numeric basename anywhere else is never
treated as an agent, and `claude/versions/<v>/bin/<other>` does not match.
Covered by `identify_agent_in_job_detects_claude_teammate_versioned_binary`
and `claude_versioned_install_rule_is_narrow`.

**No agent-detection manifest change was needed.** Once identity resolves,
the teammate's screen is the ordinary Claude Code UI and the existing
`src/detect/manifests/claude.toml` rules classify it correctly with no
tuning: `osc_title_idle` (`^\x{2733} `) matches the teammate's idle OSC title
and `osc_title_working` (braille spinner) matches while it works.

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
- A `PATH` mirror, if one was needed (see
  [Shim visibility](#shim-visibility-on-path)), **outlives an uninstall of
  Karvex** and keeps shadowing the user's real tmux until removed by hand:
  check that it points at Karvex before deleting it (`readlink
  ~/.local/bin/tmux`), then `rm ~/.local/bin/tmux` if so. Karvex never
  creates those directories itself and never touches a foreign file or
  symlink there — only its own link.

The user-facing version of this is in `integrations.mdx`'s Claude Agent
Teams section; keep the two in sync if either changes.
