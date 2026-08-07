#!/bin/sh
# A `claude`-shaped stub for the seed-prompt end-to-end test
# (`docs/design/workflow-builder/04-kvdag-and-execution.md` §4.2).
#
# The harness puts this on the server's `PATH` as `claude`, so karvex builds
# the real agent argv and opens the real managed-agent launch window against
# it. Nothing in karvex is special-cased for it: detection is screen-driven, so
# rendering what `src/detect/manifests/claude.toml`'s `live_prompt_box` rule
# matches is enough to be a real idle `claude` as far as the runtime is
# concerned.
#
# It reproduces the failure mode exactly: it SWALLOWS the seed prompt it was
# given as its trailing positional, which is what claude's first-run
# workspace-trust dialog does to it on the first run in every fresh workspace,
# and then settles at its prompt and never works. karvex has to notice the
# agent never acted and re-deliver the seed.
#
# Everything it observes is appended to $AGENT_STUB_LOG so the test can assert
# on what the *process* received, not on what the API reported.

set -u

log="${AGENT_STUB_LOG:-/dev/null}"
: > "$log"

# The seed prompt is the argv's trailing positional. Logged on its own tagged
# line so a multi-line --append-system-prompt cannot be mistaken for it.
for a in "$@"; do
  printf 'ARG\t%s\n' "$a" >> "$log"
  last="$a"
done
printf 'SEED\t%s\n' "${last:-}" >> "$log"

# Swallowed: never read, never acted on.
printf 'SWALLOWED_SEED\n' >> "$log"

# The two horizontal rules plus a `❯` body line are what `prompt_box_body`
# extracts and `live_prompt_box` matches, i.e. an idle claude at its prompt.
draw_idle() {
  printf '\033[2J\033[H'
  printf 'agent-stub ready\n'
  printf '\n'
  printf '────────────────────────────────────────────────────\n'
  printf ' ❯ \n'
  printf '────────────────────────────────────────────────────\n'
  printf '  ? for shortcuts\n'
}

draw_idle

# Anything karvex delivers into this pane arrives on stdin; logging it is how a
# re-delivered seed becomes observable. The stub deliberately never goes
# Working: the point is an agent that has received something and still has not
# acted, so a second idle streak must surface the node rather than loop.
while IFS= read -r line; do
  printf 'STDIN\t%s\n' "$line" >> "$log"
  draw_idle
  printf 'received: %s\n' "$line"
done

# stdin closed; stay in the foreground so the pane binding stays observable.
while :; do sleep 1; done
