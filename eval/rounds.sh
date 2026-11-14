#!/usr/bin/env bash
# Fair comparison: ROUNDS rounds, each runs every task with the bare arm and then the rizr arm on the same build.
# Usage: eval/rounds.sh [task-number...]   Output: $BASE/<arm>-r<round>/ and $BASE/median.json.
# ARMS=rizr runs the rizr arm only; the bare arm is a fixed baseline once measured (user, 2026-11-14).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
BASE=${BASE:-$here/../.agent/scratch/eval-rounds}
ROUNDS=${ROUNDS:-3}
ARMS=${ARMS:-bare rizr}
tasks=("${@:-1 2 3 4 5 6 7 8 9 10}")

for r in $(seq "$ROUNDS"); do
  for arm in $ARMS; do
    ARM=$arm OUT="$BASE/$arm-r$r" "$here/baseline.sh" "${tasks[@]}"
  done
done

# Per arm and task: median calls, tokens, seconds; pass count.
jq -s '
  map(select(.verdict != "prep_failed")) | group_by([.arm, .task]) | map({
    arm: .[0].arm, task: .[0].task, runs: length,
    passes: (map(select(.verdict == "pass")) | length),
    timeouts: (map(select(.agent_exit == 124)) | length),
    uncached: (map(.uncached // 0) | sort | .[length / 2 | floor]),
    cost_usd: (map(.cost_usd // 0) | sort | .[length / 2 | floor]),
    calls: (map(.tool_calls) | sort | .[length / 2 | floor]),
    tokens: (map(.tokens) | sort | .[length / 2 | floor]),
    seconds: (map(.seconds) | sort | .[length / 2 | floor]) })
' "$BASE"/*-r*/*/result.json >"$BASE/median.json"
echo "median: $BASE/median.json"
