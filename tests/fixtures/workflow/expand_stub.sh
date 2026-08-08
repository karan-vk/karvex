#!/bin/sh
# Fan-out stub for the Phase 2 expansion end-to-end tests.
#
# `docs/design/workflow-builder/06-phase2-plan.md` WS-J. This is what the
# proposing node of `expand.toml` runs: a plain process that makes one
# `kvx workflow node expand` proposal, records the server's verdict, and then
# completes itself the ordinary way. It is deliberately a *second* stub rather
# than a mode on `node_stub.sh`, because `node_stub.sh` is the Phase 1 contract
# stub and every Phase 1 scenario depends on its exact behaviour.
#
# usage: expand_stub.sh <template> <count> <result-json>
#
#   template      the kvdag key to propose. `expand.toml` declares `worker` in
#                 the proposer's `expand_allow` and `quarantined` outside it,
#                 so the same fixture drives both the accepted and the refused
#                 scenario by substitution alone.
#   count         `--count`; the proposal asks for this many children.
#   result-json   written to `result.json` before self-reporting, exactly as
#                 `node_stub.sh report` does.
#
# `04-kvdag-and-execution.md` §3.4: the proposal is made *mid-run*, before the
# node completes. That ordering is what the run graph depends on — a child
# hangs off a `sequence` edge from its parent, so a proposal from a node that
# has already settled is refused. Doing both in one shell, in this order, is
# what makes the ordering deterministic instead of a race.
#
# The verdict is written to `$KARVEX_WORKFLOW_NODE_DIR/expand.json` because the
# expand response is returned to the *node*, not to the operator: the test has
# no other way to assert what the proposing node was told. `--json` prints the
# whole response envelope, so the file is the response verbatim.

set -u

template="${1:-}"
count="${2:-}"
payload="${3:-}"

node_dir="${KARVEX_WORKFLOW_NODE_DIR:-}"
run_id="${KARVEX_WORKFLOW_RUN_ID:-}"
node_path="${KARVEX_WORKFLOW_NODE_PATH:-}"

if [ -z "$node_dir" ] || [ -z "$run_id" ] || [ -z "$node_path" ]; then
  printf 'expand_stub: KARVEX_WORKFLOW_NODE_DIR/RUN_ID/NODE_PATH are not all set\n' >&2
  exit 64
fi
if [ -z "$template" ] || [ -z "$count" ]; then
  printf 'expand_stub: usage: expand_stub.sh <template> <count> <result-json>\n' >&2
  exit 64
fi

printf 'expand-stub ready path=%s template=%s count=%s\n' "$node_path" "$template" "$count"

# Written to a temporary name and moved into place, so a test that polls for
# the file never reads a half-written document.
kvx workflow node expand "$run_id" "$node_path" \
  --template "$template" \
  --label "Shard" \
  --count "$count" \
  --json > "$node_dir/expand.json.partial" 2>"$node_dir/expand.err"
printf '%s' "$?" > "$node_dir/expand.status"
mv "$node_dir/expand.json.partial" "$node_dir/expand.json"
printf 'expand-stub proposed\n'

printf '%s' "$payload" > "$node_dir/result.json"
kvx workflow node complete || printf 'expand-stub: report failed\n'
printf 'expand-stub reported\n'

# Stay in the foreground: exiting here would look like "the pane exited before a
# valid result", which §4.3 defines as a node failure.
while :; do
  sleep 1
done
