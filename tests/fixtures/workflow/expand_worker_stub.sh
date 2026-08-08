#!/bin/sh
# Template-child stub for the Phase 2 expansion end-to-end tests.
#
# `docs/design/workflow-builder/04-kvdag-and-execution.md` §3.4 and §4.1. This
# is what an *expansion child* of `expand.toml` runs. It exists because the
# Phase 1 stub (`node_stub.sh`) reports a payload handed to it on the command
# line, and every child of one template is spawned from the same authored argv:
# a fixed payload makes N children indistinguishable, which is precisely the
# thing the payload path is supposed to make impossible.
#
# So this stub reports what its own node directory says about it:
#
#   * the `{{goal}}` slot as rendered into its `task.md` — which is the accepted
#     `--input goal=…` override if the payload path works, and the run argument
#     if it was discarded (the retest's P0 1);
#   * its own instance path, so the fan-in node's `inputs/shard/` can be checked
#     for one file per child rather than one file per generation (P0 3).
#
# usage: expand_worker_stub.sh
#
# No arguments: everything it needs is in `KARVEX_WORKFLOW_NODE_DIR`, which is
# the point — a child that can only describe itself from its own node directory
# cannot accidentally describe itself from the fixture.

set -u

node_dir="${KARVEX_WORKFLOW_NODE_DIR:-}"
node_path="${KARVEX_WORKFLOW_NODE_PATH:-}"
if [ -z "$node_dir" ] || [ -z "$node_path" ]; then
  printf 'expand_worker_stub: KARVEX_WORKFLOW_NODE_DIR/NODE_PATH are not set\n' >&2
  exit 64
fi

printf 'expand-worker ready path=%s\n' "$node_path"

# The rendered prompt line. `expand.toml`'s template is
# `Work one shard of: {{goal}}`, so this is the filled slot verbatim.
shard="$(sed -n 's/^Work one shard of: //p' "$node_dir/task.md" | head -1)"
if [ -z "$shard" ]; then
  shard="<unfilled>"
fi
# The heading `task.md` opens with is the node's own label (§4.1), so a child
# that was named by its proposal reports a different title than its siblings.
title="$(sed -n 's/^# //p' "$node_dir/task.md" | head -1)"

printf '{"report":"%s reporting on %s","summary":"%s reporting on %s (%s)"}' \
  "$node_path" "$shard" "$node_path" "$shard" "$title" > "$node_dir/result.json"
kvx workflow node complete || printf 'expand-worker: report failed\n'
printf 'expand-worker reported shard=%s title=%s\n' "$shard" "$title"

# Stay in the foreground: exiting here would look like "the pane exited before a
# valid result", which §4.3 defines as a node failure.
while :; do
  sleep 1
done
