#!/bin/sh
# Epilogue summariser stub for the Phase 3 end-to-end tests.
#
# `docs/design/workflow-builder/07-phase3-plan.md` §4 D2 / §6 A4: the epilogue
# runs `claude` in production and the argv named by
# `KARVEX_WORKFLOW_SUMMARY_COMMAND` when one is declared. That override is a
# first-class command binding, not a test hook, so the summariser this script
# stands in for reads and writes exactly the same node contract every
# `runner = "command"` node does: write `result.json` into the node dir, then
# self-report with `kvx workflow node complete`.
#
# usage: summary_stub.sh <mode>
#   ok    write a summary that validates against `summary_output_schema()`
#   over  write a `text` past the 4,000-character budget, so the schema's
#         `maxLength` rejects it, the one corrective re-prompt is consumed, and
#         the epilogue lands in `GaveUp` with the run's own status untouched
#         (§4 D1's bounded failure ladder)
#
# The `ok` summary names the run it summarises, so a test can prove the summary
# it read back belongs to the run it asked about rather than to whichever run
# happened to summarise last.

set -u

mode="${1:-ok}"

node_dir="${KARVEX_WORKFLOW_NODE_DIR:-}"
if [ -z "$node_dir" ]; then
  printf 'summary_stub: KARVEX_WORKFLOW_NODE_DIR is not set\n' >&2
  exit 64
fi

run_id="${KARVEX_WORKFLOW_RUN_ID:-unknown-run}"

printf 'summary-stub ready path=%s mode=%s\n' "${KARVEX_WORKFLOW_NODE_PATH:-?}" "$mode"

case "$mode" in
  ok)
    text="the stub summariser saw this run reach its terminal status"
    ;;
  over)
    # 36 characters × 250 = 9,000, comfortably past `SUMMARY_TEXT_BUDGET`.
    text=""
    i=0
    while [ "$i" -lt 250 ]; do
      text="${text}the summariser wrote far too much. "
      i=$((i + 1))
    done
    ;;
  *)
    printf 'summary_stub: unknown mode %s\n' "$mode" >&2
    exit 64
    ;;
esac

# No quotes, backslashes, or newlines in either field, so `printf` produces
# valid JSON without an escaper.
printf '{"text":"%s","outcome":"stub summary of %s","highlights":["the stub summariser ran"],"open_gaps":[],"per_node":[]}' \
  "$text" "$run_id" > "$node_dir/result.json"

kvx workflow node complete || printf 'summary-stub: report exited non-zero\n'
printf 'summary-stub reported\n'

if [ "$mode" = over ]; then
  # The failure ladder is *bounded*, not immediate: an over-budget result is
  # first answered with one corrective re-prompt, and only a second unusable
  # result lands the epilogue in `GaveUp` (§4 D1). A command-runner node cannot
  # act on a re-prompt — the text goes into a pane nothing reads — so the stub
  # reports the same over-budget document again, exactly as `node_stub.sh`'s
  # `report2` mode does for the ordinary-node version of this ladder.
  sleep 1
  kvx workflow node complete || printf 'summary-stub: second report exited non-zero\n'
  printf 'summary-stub reported twice\n'
fi

# Stay in the foreground like every other node stub: exiting before the engine
# has accepted the report would look like a summariser pane that died, which is
# a different branch of the same failure ladder.
while :; do
  sleep 1
done
