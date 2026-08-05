#!/bin/sh
# Stub teammate for the Phase 1 workflow end-to-end tests.
#
# This is what a `runner = "command"` node runs: a plain process in a pane, no
# `claude`, no network. It honours the node prompt/output contract of
# `docs/design/workflow-builder/04-kvdag-and-execution.md` §4.1/§4.3 — read the
# task from the node dir, write `result.json`, then self-report with
# `kvx workflow node complete`.
#
# usage: node_stub.sh <mode> [payload]
#   report  <json>  write <json> as result.json, then self-report once
#   report2 <json>  same, but self-report twice — the second report is what
#                   turns a schema-invalid result from a corrective re-prompt
#                   into NeedsAttention (§4.3)
#   idle            never write result.json and never report; just sit there
#
# After the mode's work is done the process stays alive so the pane binding
# stays observable and so `pane.read` can see anything steered into the pane
# (`04` §5 delivers a `runner = "command"` steer as `pane.send_text`, which the
# tty echoes).

set -u

mode="${1:-idle}"
payload="${2:-}"

node_dir="${KARVEX_WORKFLOW_NODE_DIR:-}"
if [ -z "$node_dir" ]; then
  printf 'node_stub: KARVEX_WORKFLOW_NODE_DIR is not set\n' >&2
  exit 64
fi

printf 'node-stub ready path=%s mode=%s\n' "${KARVEX_WORKFLOW_NODE_PATH:-?}" "$mode"

case "$mode" in
  report|report2)
    printf '%s' "$payload" > "$node_dir/result.json"
    kvx workflow node complete || printf 'node-stub: first report failed\n'
    if [ "$mode" = report2 ]; then
      # A second report of the same invalid document; the engine allows exactly
      # one corrective re-prompt before the node goes to NeedsAttention.
      sleep 1
      kvx workflow node complete || printf 'node-stub: second report failed\n'
    fi
    printf 'node-stub reported\n'
    ;;
  idle)
    printf 'node-stub idle\n'
    ;;
  *)
    printf 'node_stub: unknown mode %s\n' "$mode" >&2
    exit 64
    ;;
esac

# Stay in the foreground. Exiting here would look like "the pane exited before a
# valid result", which §4.3 defines as a node failure.
while :; do
  sleep 1
done
