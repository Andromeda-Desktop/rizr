#!/usr/bin/env bash
# Agent-eval baseline: Haiku with bare docker compose + shell, no rizr. One run per task.
# Usage: eval/baseline.sh [task-number...]   Output: $OUT/<task>/ (transcript, result, check).
set -uo pipefail

ROOT=${ROOT:-/tmp/rizr-eval}
OUT=${OUT:-$(cd "$(dirname "$0")/.." && pwd)/.agent/scratch/eval-runs}
LIMIT=${LIMIT:-1200}
MODEL=${MODEL:-haiku}
mkdir -p "$ROOT" "$OUT"

declare -A REPO=(
  [1]="dockersamples/example-voting-app 63e9150ca17af4ed05880d4245e486481f73fcb4"
  [2]="fastapi/full-stack-fastapi-template aabb90126964a164e02aff39e7c385bb0598236d"
  [3]="knadh/listmonk 32078ede6d87c83f4948cc592c76bee91d629f0b"
  [4]="saleor/saleor-platform ab6315bd59c58b4815175df4c679107ff9695be4"
  [5]="frappe/frappe_docker 85ad653360e4670c2e885c4283d293e7b9efeda9"
)
declare -A COMPOSE=([5]="-f pwd.yml")

log() { printf '%s %s\n' "$(date +%T)" "$*"; }

http_ok() { # url [body]
  local code
  if [[ -n ${2:-} ]]; then
    code=$(curl -s -m 10 -o /tmp/rizr-eval-body -w '%{http_code}' -H 'content-type: application/json' -d "$2" "$1")
  else
    code=$(curl -s -m 10 -o /tmp/rizr-eval-body -w '%{http_code}' -L "$1")
  fi
  [[ $code == 200 ]]
}

wait_http() { # url seconds
  local end=$((SECONDS + $2))
  while ((SECONDS < end)); do http_ok "$1" && return 0; sleep 3; done
  return 1
}

cleanup() { # remove every compose project started from $ROOT
  docker compose ls -a --format json 2>/dev/null |
    jq -r --arg r "$ROOT" '.[] | select(.ConfigFiles | contains($r)) | .Name' |
    while read -r p; do docker compose -p "$p" down -v --remove-orphans >/dev/null 2>&1; done
}

clone() { # task -> $ROOT/<task>/repo at the pinned commit
  local t=$1 slug sha dir
  read -r slug sha <<<"${REPO[$t]}"
  dir=$ROOT/$t
  rm -rf "$dir" && mkdir -p "$dir/repo"
  git -C "$dir/repo" init -q
  git -C "$dir/repo" fetch -q --depth 1 "https://github.com/$slug" "$sha"
  git -C "$dir/repo" checkout -q FETCH_HEAD
}

prep() { # task: pull and build images, so the run measures the agent, not the network
  local t=$1 d=$ROOT/$1/repo
  : >"$OUT/$t/prep.log"
  # Package downloads inside image builds time out now and then (task 2: PyPI); the cache keeps finished layers.
  local try
  for try in 1 2 3; do
    (cd "$d" && docker compose ${COMPOSE[$t]:-} pull -q --ignore-buildable && docker compose ${COMPOSE[$t]:-} build --progress plain) >>"$OUT/$t/prep.log" 2>&1 && break
    [ "$try" = 3 ] && return 1
    log "task $t: prep try $try failed, retrying"
  done
  case $t in
    3) # first copy runs before the agent starts; the agent must add a second one
      git -C "$d" worktree add -q --detach "$ROOT/3/wt2" HEAD
      (cd "$d" && docker compose up -d) >>"$OUT/$t/prep.log" 2>&1
      wait_http http://localhost:9000/ 180 || log "task 3: first copy not ready"
      docker ps -q --filter label=com.docker.compose.project=repo | sort >"$OUT/3/first-ids"
      docker inspect -f '{{.State.StartedAt}}' $(cat "$OUT/3/first-ids") | sort >"$OUT/3/first-start" ;;
    4) # migrate with the correct env, stop, then inject the fault the agent must find
      (cd "$d" && docker compose run --rm api python3 manage.py migrate && docker compose down) >>"$OUT/$t/prep.log" 2>&1
      sed -i 's|^DATABASE_URL=postgres://saleor:saleor@db/saleor$|DATABASE_URL=postgres://saleor:saleor@database/saleor|' "$d/backend.env"
      git -C "$d" commit -qam "env" --no-gpg-sign ;;
  esac
}

prompt() {
  local d=$ROOT/$1/repo
  case $1 in
    1) echo "You are in a repository with a docker compose stack ($d). Start the stack. Tell me when the vote page answers in the browser." ;;
    2) echo "You are in a repository with a docker compose stack ($d). Start the stack and run the backend tests. Report the test result." ;;
    3) echo "A copy of this docker compose stack already runs from $d. Run a second copy of the stack from the git worktree at $ROOT/3/wt2, while the first copy keeps running unchanged. Tell me the URL of the second copy." ;;
    4) echo "You are in a repository with a docker compose stack ($d). The API returns errors. Find the cause and fix it, so that the GraphQL API at http://localhost:8000/graphql/ answers." ;;
    5) echo "You are in a repository with a docker compose stack ($d; use pwd.yml). Start the stack. Tell me when the login page answers in the browser." ;;
  esac
}

check() { # task -> exit 0 on pass; prints the reason
  local t=$1 d=$ROOT/$1/repo
  case $t in
    1) http_ok http://localhost:8080/ && echo "vote page 200" ;;
    2) jq -se 'any(.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | .input.command? // ""; test("pytest|tests-start|test\\.sh"))' "$OUT/2/transcript.jsonl" >/dev/null || { echo "no test command run"; return 1; }
       [[ $(docker ps -a --filter label=com.docker.compose.service=prestart --format '{{.Status}}' | head -n1) == Exited\ \(0\)* ]] || { echo "prestart did not exit 0"; return 1; }
       (cd "$d" && docker compose exec -T backend bash scripts/tests-start.sh) >"$OUT/2/check-tests.log" 2>&1 && echo "tests pass" ;;
    3) [[ $(docker inspect -f '{{.State.StartedAt}}' $(cat "$OUT/3/first-ids") 2>/dev/null | sort) == "$(cat "$OUT/3/first-start")" ]] || { echo "first copy changed"; return 1; }
       http_ok http://localhost:9000/ || { echo "first copy down"; return 1; }
       local p
       for p in $(docker ps --filter ancestor=listmonk/listmonk:latest --format '{{.Ports}}' | grep -oE '0\.0\.0\.0:[0-9]+' | cut -d: -f2 | sort -u); do
         [[ $p != 9000 ]] && http_ok "http://localhost:$p/" && { echo "second copy on $p"; return 0; }
       done
       echo "no second copy answers"; return 1 ;;
    4) http_ok http://localhost:8000/graphql/ '{"query":"{ shop { name } }"}' && ! grep -q '"errors"' /tmp/rizr-eval-body || { echo "graphql fails"; return 1; }
       [[ $(git -C "$d" diff --name-only HEAD | tr '\n' ' ') == "backend.env " ]] || { echo "diff touches: $(git -C "$d" diff --name-only HEAD | tr '\n' ' ')"; return 1; }
       echo "graphql ok, fix in backend.env only" ;;
    5) http_ok http://localhost:8080/login && echo "login page 200" ;;
  esac
}

run() {
  local t=$1 start rc
  mkdir -p "$OUT/$t"
  log "task $t: clone + prep"
  cleanup; clone "$t" && prep "$t" || { log "task $t: prep failed"; return; }
  log "task $t: agent ($MODEL, limit ${LIMIT}s)"
  start=$SECONDS
  (cd "$ROOT/$t/repo" && timeout "$LIMIT" claude -p "$(prompt "$t")" --model "$MODEL" \
    --setting-sources project --no-session-persistence --strict-mcp-config \
    --allowedTools "Bash Read Edit Write Glob Grep" \
    --output-format stream-json --verbose) >"$OUT/$t/transcript.jsonl" 2>"$OUT/$t/agent.err"
  rc=$?
  local secs=$((SECONDS - start)) verdict reason
  reason=$(check "$t") && verdict=pass || verdict=fail
  jq -s --arg t "$t" --arg v "$verdict" --arg r "$reason" --argjson rc "$rc" --argjson s "$secs" '
    (map(select(.type=="result")) | last) as $res
    | { task: ($t|tonumber), verdict: $v, reason: $r, agent_exit: $rc, seconds: $s,
        tool_calls: ([.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use")] | length),
        turns: $res.num_turns, cost_usd: $res.total_cost_usd,
        tokens: (($res.usage.input_tokens // 0) + ($res.usage.output_tokens // 0) + ($res.usage.cache_read_input_tokens // 0) + ($res.usage.cache_creation_input_tokens // 0)),
        final: ($res.result // "" | .[0:400]) }' "$OUT/$t/transcript.jsonl" >"$OUT/$t/result.json"
  log "task $t: $verdict ($reason) $(jq -c '{tool_calls,seconds,cost_usd}' "$OUT/$t/result.json")"
  cleanup
}

for t in "${@:-1 2 3 4 5}"; do for x in $t; do run "$x"; done; done
jq -s '.' "$OUT"/*/result.json >"$OUT/summary.json"
log "summary: $OUT/summary.json"
