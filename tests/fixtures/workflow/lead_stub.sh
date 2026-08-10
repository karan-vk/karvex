#!/bin/sh
# A team-lead-shaped `claude` stub for the agent-teams run
# (`docs/design/workflow-builder/09-agent-teams-rework.md` §3.1, §3.4).
#
# `tests/workflow_lead_headless.rs` puts this on the server's PATH as `claude`,
# so karvex builds the real lead argv, runs the real `claude --version`
# preflight against it, opens the real managed-agent launch window, and binds
# the run by recognising the team config this script writes. Nothing in karvex
# is special-cased for it.
#
# It is a separate file from `agent_stub.sh` on purpose. `agent_stub.sh` models
# a *node* agent that swallows its seed; this models a *lead* that comes up,
# registers a Claude Code agent team, and populates the shared task list. Their
# argv contracts differ (a lead is launched with no positional prompt at all),
# and `agent_stub.sh` answers `--version` by hanging, which would wedge the
# lead preflight.
#
# ## What it does, in order
#
#  1. Answers `--version` and exits, before touching anything else. karvex runs
#     `claude --version` as a plain subprocess before it will spawn a lead, so
#     this branch must not truncate the argv log the real launch writes.
#  2. Logs its whole argv, one `ARG<TAB><value>` line per argument, plus the
#     facts the test needs to prove the launch environment: the argument count,
#     the cwd, the karvex pane id, the run id, and the agent-teams flag.
#  3. Writes a Claude Code team config and a shared task list, in the layout
#     §3.4 describes and against the live shapes captured 2026-08-11.
#  4. Renders what `src/detect/manifests/claude.toml`'s `live_prompt_box` rule
#     matches, so karvex's screen-driven detection sees a live idle agent and
#     the managed-agent phase reaches `Active` — which is what unlocks the
#     `agent.prompt` seed delivery (§3.1 step 3's "NO positional prompt").
#  5. Stays in the foreground logging whatever is delivered into its pane, so
#     the seed is observable as something the *process* received.
#
# ## Environment
#
#   CLAUDE_CONFIG_DIR    required. Where the team and task files go. There is
#                        no default and no fallback: this script must never be
#                        able to write into a developer's real ~/.claude.
#   LEAD_STUB_LOG        argv/stdin log path (default: /dev/null).
#   LEAD_STUB_VERSION    what `--version` reports (default: 2.1.226, which
#                        clears `lead::MIN_CLAUDE_VERSION`). A test sets this
#                        to an old version to drive the preflight refusal.
#   LEAD_STUB_SESSION    the lead session id recorded in the team config.
#   LEAD_STUB_TEAM       the team name (default: `session-` + the session id's
#                        first eight characters, Claude Code's own rule).
#   LEAD_STUB_CWD        the leader member's recorded cwd. Defaults to this
#                        process's real cwd, which is what `match_team`'s
#                        tier-1 rule compares against the lead pane's cwd.
#   LEAD_STUB_TEAMMATE   the tmux-backed teammate's name.
#   LEAD_STUB_TEAMMATE_MODEL   that teammate's model.
#   LEAD_STUB_TEAMMATE_PANE    that teammate's `tmuxPaneId`. Defaults to this
#                        pane's own `KARVEX_PANE_ID`, which is a pane karvex
#                        provably owns — that is what makes `match_team`
#                        answer with the strong `OwnPane` rule rather than
#                        only the cwd rule, and it keeps the fixture from
#                        depending on a second pane spawn it does not control.
#   LEAD_STUB_NO_TEAM    when set to 1, skip writing the team and task files
#                        entirely, so a test can observe an unbound lead.

set -u

# 1. The preflight. Handled before the log is truncated and before any file is
#    written: this is a throwaway subprocess of the server, not the lead.
for arg in "$@"; do
  case "$arg" in
    --version|-v)
      printf '%s (Claude Code)\n' "${LEAD_STUB_VERSION:-2.1.226}"
      exit 0
      ;;
  esac
done

log="${LEAD_STUB_LOG:-/dev/null}"
: > "$log"

# 2. The argv, verbatim. `ARGC` is logged too so the test can assert the
#    *absence* of a trailing positional prompt without having to guess which
#    flag values are prompts — a positional would make the count odd against a
#    flags-only argv, and the tagged lines make the shape readable on failure.
for arg in "$@"; do
  printf 'ARG\t%s\n' "$arg" >> "$log"
done
printf 'ARGC\t%s\n' "$#" >> "$log"

cwd="${LEAD_STUB_CWD:-$(pwd -P)}"
printf 'CWD\t%s\n' "$cwd" >> "$log"
printf 'PANE\t%s\n' "${KARVEX_PANE_ID:-}" >> "$log"
printf 'RUN\t%s\n' "${KARVEX_WORKFLOW_RUN_ID:-}" >> "$log"
printf 'TEAMS\t%s\n' "${CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS:-}" >> "$log"

# The isolation interlock. An unset CLAUDE_CONFIG_DIR would make karvex read
# the developer's real ~/.claude, so refusing here is the difference between a
# failing test and a test that quietly edits a real agent team.
if [ -z "${CLAUDE_CONFIG_DIR:-}" ]; then
  printf 'FATAL\tCLAUDE_CONFIG_DIR is unset\n' >> "$log"
  echo "lead_stub.sh refuses to run without CLAUDE_CONFIG_DIR" >&2
  exit 64
fi

session="${LEAD_STUB_SESSION:-7f3c9a12-0b64-4d8e-9a11-2c5f6d7e8a90}"
# Claude Code names a team `session-` plus the first eight characters of the
# lead session id; the default mirrors that so the fixture is shaped like the
# thing it stands in for.
default_team="session-$(printf '%s' "$session" | cut -c1-8)"
team="${LEAD_STUB_TEAM:-$default_team}"
teammate="${LEAD_STUB_TEAMMATE:-build-hand}"
teammate_model="${LEAD_STUB_TEAMMATE_MODEL:-sonnet}"
teammate_pane="${LEAD_STUB_TEAMMATE_PANE:-${KARVEX_PANE_ID:-}}"

printf 'TEAM\t%s\n' "$team" >> "$log"
printf 'SESSION\t%s\n' "$session" >> "$log"

teams_dir="$CLAUDE_CONFIG_DIR/teams/$team"
tasks_dir="$CLAUDE_CONFIG_DIR/tasks/$team"

# Written through a temp file and renamed. Claude Code rewrites these files
# concurrently and karvex's readers are built to skip a half-written one, but a
# fixture that produced torn reads on purpose would be testing the retry loop
# rather than the projection.
write_atomic() {
  target="$1"
  tmp="$target.tmp.$$"
  cat > "$tmp"
  mv "$tmp" "$target"
}

if [ "${LEAD_STUB_NO_TEAM:-0}" != "1" ]; then
  mkdir -p "$teams_dir" "$tasks_dir"

  # `createdAt` is *now*, in milliseconds. Freshness is mandatory in
  # `lead::match_team` — a stale config is correctly refused — so a fixture
  # with a hardcoded timestamp would pin the wrong behaviour.
  now_ms="$(date +%s)000"

  write_atomic "$teams_dir/config.json" <<EOF
{
  "name": "$team",
  "leadSessionId": "$session",
  "createdAt": $now_ms,
  "members": [
    {
      "name": "team-lead",
      "agentId": "team-lead@$session",
      "agentType": "team-lead",
      "tmuxPaneId": "leader",
      "backendType": "in-process",
      "isActive": true,
      "cwd": "$cwd",
      "joinedAt": $now_ms
    },
    {
      "name": "$teammate",
      "agentId": "$teammate@$session",
      "agentType": "general-purpose",
      "model": "$teammate_model",
      "tmuxPaneId": "$teammate_pane",
      "backendType": "tmux",
      "isActive": true,
      "cwd": "$cwd",
      "joinedAt": $now_ms,
      "color": "cyan",
      "planModeRequired": false,
      "subscriptions": []
    }
  ]
}
EOF

  # The shared task list. Subjects 1 and 2 carry the `lead_run.toml` node keys
  # as their prefix, which is the whole matching contract (§3.2); subject 3
  # carries no key at all and must therefore be recorded as emergent.
  # Task 2 has no `owner` key — an unclaimed task genuinely omits the field,
  # and karvex must not default it to an empty owner at the parse layer.
  write_atomic "$tasks_dir/1.json" <<EOF
{
  "id": "1",
  "subject": "plan: Draft the approach",
  "description": "Write the implementation plan the build task will carry out.",
  "activeForm": "Drafting the approach",
  "owner": "$teammate",
  "status": "in_progress",
  "blocks": ["2"],
  "blockedBy": []
}
EOF

  write_atomic "$tasks_dir/2.json" <<EOF
{
  "id": "2",
  "subject": "build: Carry out the approach",
  "description": "Carry out the plan the planning task produced.",
  "activeForm": "Carrying out the approach",
  "status": "pending",
  "blocks": [],
  "blockedBy": ["1"]
}
EOF

  write_atomic "$tasks_dir/3.json" <<EOF
{
  "id": "3",
  "subject": "chase down the flaky fixture nobody planned for",
  "description": "Work the team invented; the definition never asked for it.",
  "activeForm": "Chasing down the flaky fixture",
  "owner": "$teammate",
  "status": "pending",
  "blocks": [],
  "blockedBy": []
}
EOF

  printf 'WROTE_TEAM\t%s\n' "$teams_dir/config.json" >> "$log"
fi

# 4. Two horizontal rules around a `❯` body line is what `prompt_box_body`
#    extracts and `live_prompt_box` matches: an idle claude at its prompt.
draw_idle() {
  printf '\033[2J\033[H'
  printf 'lead-stub ready\n'
  printf '\n'
  printf '────────────────────────────────────────────────────\n'
  printf ' ❯ \n'
  printf '────────────────────────────────────────────────────\n'
  printf '  ? for shortcuts\n'
}

draw_idle

# 5. Whatever karvex delivers into this pane arrives on stdin. The lead's plan
#    is delivered here and nowhere else (§3.1: the argv carries no prompt), so
#    this log is the only place the seed is observable as received rather than
#    merely as acknowledged by the control plane.
while IFS= read -r line; do
  printf 'STDIN\t%s\n' "$line" >> "$log"
  draw_idle
  printf 'received: %s\n' "$line"
done

# stdin closed; stay in the foreground so the pane binding stays observable.
while :; do sleep 1; done
