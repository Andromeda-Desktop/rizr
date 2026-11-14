#!/usr/bin/env bash
# Agent-eval baseline: Haiku with bare docker compose + shell, no rizr. One run per task.
# Usage: eval/baseline.sh [task-number...]   Output: $OUT/<task>/ (transcript, result, check).
set -uo pipefail

ROOT=${ROOT:-/tmp/rizr-eval}
ARM=${ARM:-bare} # bare: compose + shell; rizr: the same, plus rizr on PATH and one line in CLAUDE.md
RIZR=${RIZR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/rizr}
OUT=${OUT:-$(cd "$(dirname "$0")/.." && pwd)/.agent/scratch/eval-runs$([[ $ARM == rizr ]] && echo -rizr)}
LIMIT=${LIMIT:-1200}
MODEL=${MODEL:-haiku}
mkdir -p "$ROOT" "$OUT"

declare -A REPO=(
  [1]="dockersamples/example-voting-app 63e9150ca17af4ed05880d4245e486481f73fcb4"
  [2]="fastapi/full-stack-fastapi-template aabb90126964a164e02aff39e7c385bb0598236d"
  [3]="knadh/listmonk 32078ede6d87c83f4948cc592c76bee91d629f0b"
  [4]="saleor/saleor-platform ab6315bd59c58b4815175df4c679107ff9695be4"
  [5]="frappe/frappe_docker 85ad653360e4670c2e885c4283d293e7b9efeda9"
  [6]="dockersamples/example-voting-app 63e9150ca17af4ed05880d4245e486481f73fcb4"
  [7]="knadh/listmonk 32078ede6d87c83f4948cc592c76bee91d629f0b"
  [8]="dockersamples/example-voting-app 63e9150ca17af4ed05880d4245e486481f73fcb4"
  [9]="saleor/saleor-platform ab6315bd59c58b4815175df4c679107ff9695be4"
  [10]="knadh/listmonk 32078ede6d87c83f4948cc592c76bee91d629f0b"
)
MARK=eval-marker@example.com # task 10: a row in the first copy's data that the second copy must have
OTHER=rizr-eval-other # task 7: a server outside the stack that holds the stack's port
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
  docker rm -f "$OTHER" >/dev/null 2>&1
  # rizr's shared servers of eval repos (every eval repo dir is named repo) and their volumes
  docker ps -aq --filter name='^rizr-repo-' | xargs -r docker rm -f >/dev/null 2>&1
  docker volume ls -q --filter name='^rizr-repo-' | xargs -r docker volume rm >/dev/null 2>&1
}

served_by() { # container: the container that answers DNS name $2 on a network of container $1, by any tool
  local n c
  for n in $(docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$1"); do
    for c in $(docker ps -q --filter "network=$n"); do
      docker inspect -f "{{with index .NetworkSettings.Networks \"$n\"}}{{join .Aliases \" \"}} {{join .DNSNames \" \"}}{{end}}" "$c" |
        tr ' ' '\n' | grep -qx "$2" && { echo "$c"; return 0; }
    done
  done
  return 1
}

service_ctr() { # repo service: the running compose container of that service
  docker ps -q --filter "label=com.docker.compose.project.working_dir=$1" --filter "label=com.docker.compose.service=$2" | head -n1
}

seal() { # fold a prepared change into the only commit, so `git log` does not show it
  git -C "$1" commit -qa --amend --no-edit --no-gpg-sign
}

final() { # the agent's last message
  jq -rs 'map(select(.type=="result")) | last | .result // ""' "$OUT/$1/transcript.jsonl"
}

clone() { # task -> $ROOT/<task>/repo at the pinned commit
  local t=$1 slug sha dir
  read -r slug sha <<<"${REPO[$t]}"
  dir=$ROOT/$t
  # containers write root-owned files into bind mounts (task 2 backend/htmlcov)
  rm -rf "$dir" 2>/dev/null || docker run --rm -v "$ROOT:/r" alpine rm -rf "/r/$t"
  [[ ! -e $dir ]] || { log "task $t: cannot remove $dir"; return 1; }
  mkdir -p "$dir/repo"
  git -C "$dir/repo" init -q
  git -C "$dir/repo" fetch -q --depth 1 "https://github.com/$slug" "$sha"
  git -C "$dir/repo" checkout -q FETCH_HEAD
}

prep() { # task: pull and build images, so the run measures the agent, not the network
  local t=$1 d=$ROOT/$1/repo
  : >"$OUT/$t/prep.log"
  # Package downloads inside image builds time out now and then (task 2: PyPI); the cache keeps finished layers.
  # bun.sh installs from GitHub releases (~30 KB/s here); the npm registry serves the same binary fast.
  if [[ $t == 2 ]]; then
    sed -i 's#^RUN curl -fsSL https://bun.sh/install | bash$#RUN npm install -g bun#' "$d/frontend/Dockerfile.playwright"
    # PyPI downloads stall here now and then; uv's default 30 s timeout then fails the whole image build.
    sed -i '/^FROM /a ENV UV_HTTP_TIMEOUT=300' "$d/backend/Dockerfile"
    git -C "$d" commit -qam "bun from npm, longer uv timeout" --no-gpg-sign
  fi
  local try
  for try in 1 2 3; do
    (cd "$d" && docker compose ${COMPOSE[$t]:-} pull -q --ignore-buildable && docker compose ${COMPOSE[$t]:-} build --progress plain) >>"$OUT/$t/prep.log" 2>&1 && break
    [ "$try" = 3 ] && return 1
    log "task $t: prep try $try failed, retrying"
  done
  case $t in
    3|10) # first copy runs before the agent starts; the agent must add a second one
      git -C "$d" worktree add -q --detach "$ROOT/$t/wt2" HEAD
      (cd "$d" && docker compose up -d) >>"$OUT/$t/prep.log" 2>&1
      wait_http http://localhost:9000/ 180 || log "task $t: first copy not ready"
      [[ $t == 10 ]] && docker exec listmonk_db psql -U listmonk -qc "insert into subscribers (uuid, email, name, status) values (gen_random_uuid(), '$MARK', 'Eval Marker', 'enabled')" >>"$OUT/$t/prep.log" 2>&1
      docker ps -q --filter label=com.docker.compose.project=repo | sort >"$OUT/$t/first-ids"
      docker inspect -f '{{.State.StartedAt}}' $(cat "$OUT/$t/first-ids") | sort >"$OUT/$t/first-start" ;;
    4) # migrate with the correct env, stop, then inject the fault the agent must find
      (cd "$d" && docker compose run --rm api python3 manage.py migrate && docker compose down) >>"$OUT/$t/prep.log" 2>&1
      sed -i 's|^DATABASE_URL=postgres://saleor:saleor@db/saleor$|DATABASE_URL=postgres://saleor:saleor@database/saleor|' "$d/backend.env"
      git -C "$d" commit -qam "env" --no-gpg-sign ;;
    6) # a stack left behind by older work: an old result port, a removed service still running, a stopped worker
      printf 'services:\n  adminer:\n    image: adminer:latest\n    networks: [back-tier]\n' >"$ROOT/6/old.yml"
      (cd "$d" && docker compose -f docker-compose.yml -f "$ROOT/6/old.yml" up -d --wait && docker compose stop worker) >>"$OUT/$t/prep.log" 2>&1
      sed -i 's|"8081:80"|"8091:80"|' "$d/docker-compose.yml"
      seal "$d" ;;
    7) # another server on the host already listens on the stack's port 9000
      docker run -d --name "$OTHER" -p 9000:80 nginx:1.30-alpine >>"$OUT/$t/prep.log" 2>&1
      docker inspect -f '{{.State.StartedAt}}' "$OTHER" >"$OUT/7/other-start" ;;
    8) # the worker joins the wrong network: it waits for redis and db forever, and every container looks fine
      sed -i '/^  worker:/,/^  redis:/s/- back-tier/- front-tier/' "$d/docker-compose.yml"
      seal "$d" ;;
  esac
}

arm() { # task: give the agent rizr the way a user would: on PATH, trusted once, named in CLAUDE.md
  [[ $ARM == rizr ]] || { rm -rf "$ROOT/bin"; return 0; }
  local dir
  mkdir -p "$ROOT/bin" && rm -rf "$ROOT/rizr-data"
  cp "$RIZR" "$ROOT/bin/rizr.real"
  printf '#!/bin/sh\nXDG_DATA_HOME=%s exec %s "$@"\n' "$ROOT/rizr-data" "$ROOT/bin/rizr.real" >"$ROOT/bin/rizr"
  chmod +x "$ROOT/bin/rizr"
  for dir in "$ROOT/$1/repo" "$ROOT/$1/wt2"; do
    [[ -d $dir ]] || continue
    echo 'This stack runs with rizr (on PATH). `rizr up` starts it and prints each URL with a check result and the cause of a failure; `rizr context --json` gives the graph, the state and every verb.' >"$dir/CLAUDE.md"
    (cd "$dir" && script -qec "$ROOT/bin/rizr ${COMPOSE[$1]:-} trust" /dev/null <<<y) >>"$OUT/$1/prep.log" 2>&1 || { log "task $1: rizr trust failed in $dir"; return 1; }
  done
  # preflight: the agent must start with the task's files trusted, as a user who ran them once
  (cd "$ROOT/$1/repo" && "$ROOT/bin/rizr" ${COMPOSE[$1]:-} plan) >>"$OUT/$1/prep.log" 2>&1 || { log "task $1: rizr plan fails before the agent"; return 1; }
}

prompt() {
  local d=$ROOT/$1/repo
  case $1 in
    1) echo "You are in a repository with a docker compose stack ($d). Start the stack. Tell me when the vote page answers in the browser." ;;
    2) echo "You are in a repository with a docker compose stack ($d). Start the stack and run the backend tests. Report the test result." ;;
    3) echo "A copy of this docker compose stack already runs from $d. Run a second copy of the stack from the git worktree at $ROOT/3/wt2, while the first copy keeps running unchanged. Tell me the URL of the second copy." ;;
    4) echo "You are in a repository with a docker compose stack ($d). The API returns errors. Find the cause and fix it, so that the GraphQL API at http://localhost:8000/graphql/ answers." ;;
    5) echo "You are in a repository with a docker compose stack ($d; use pwd.yml). Start the stack. Tell me when the login page answers in the browser." ;;
    6) echo "You are in a repository with a docker compose stack ($d). Someone worked on it before you. Make the running stack match the current compose file. Tell me the URLs of the vote page and the result page." ;;
    7|9) echo "You are in a repository with a docker compose stack ($d). Start the stack. Tell me the URL where $([[ $1 == 7 ]] && echo 'the listmonk admin page answers' || echo 'the GraphQL API answers a shop query') when it is ready." ;;
    10) echo "A copy of this docker compose stack already runs from $d, with data in its database. Run a second copy from the git worktree at $ROOT/10/wt2 that starts with a copy of the first copy's data. The first copy must keep running unchanged, and changes in the second copy must not reach the first. Tell me the URL of the second copy." ;;
    8) echo "You are in a repository with a docker compose stack ($d). Start the stack. Votes cast on the vote page never show up on the result page. Find the cause and fix it." ;;
  esac
}

check() { # task -> exit 0 on pass; prints the reason
  local t=$1 d=$ROOT/$1/repo
  case $t in
    1) http_ok http://localhost:8080/ && echo "vote page 200" ;;
    2) jq -se 'any(.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | .input.command? // ""; test("pytest|tests-start|test\\.sh"))' "$OUT/2/transcript.jsonl" >/dev/null || { echo "no test command run"; return 1; }
       [[ $(docker inspect -f '{{.State.Health.Status}}' "$(service_ctr "$d" backend)" 2>/dev/null) == healthy ]] || { echo "backend not healthy"; return 1; }
       # as the repo's scripts/test.sh: the image has no tests/ and no service migrates, so the check does both itself
       (cd "$d" && docker compose cp backend/tests backend:/app/backend/ && docker compose exec -T backend bash scripts/prestart.sh && docker compose exec -T backend bash scripts/tests-start.sh) >"$OUT/2/check-tests.log" 2>&1 && echo "tests pass" ;;
    3) [[ -s $OUT/3/first-ids ]] || { echo "no first copy"; return 1; }
       [[ $(docker inspect -f '{{.State.StartedAt}}' $(cat "$OUT/3/first-ids") 2>/dev/null | sort) == "$(cat "$OUT/3/first-start")" ]] || { echo "first copy changed"; return 1; }
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
    6) http_ok http://localhost:8080/ || { echo "vote page fails"; return 1; }
       http_ok http://localhost:8091/ || { echo "result page not on 8091"; return 1; }
       ! http_ok http://localhost:8081/ || { echo "the old result port 8081 still answers"; return 1; }
       local got w # every container started from this repo, by any tool: each compose service once, all running
       got=$(docker ps -a --filter "label=com.docker.compose.project.working_dir=$d" --format '{{.Label "com.docker.compose.service"}} {{.State}}' | sort | tr '\n' ' ')
       # a tool may serve db from its own container: count it when the worker reaches a running `db`
       w=$(service_ctr "$d" worker)
       [[ $got != *"db "* && -n $w ]] && served_by "$w" db >/dev/null && got=$(printf '%s\n' $got db running | paste -d' ' - - | sort | tr '\n' ' ')
       [[ $got == "db running redis running result running vote running worker running " ]] || { echo "containers of this repo: $got"; return 1; }
       echo "stack matches the compose file" ;;
    7) [[ $(docker inspect -f '{{.State.StartedAt}}' "$OTHER" 2>/dev/null) == "$(cat "$OUT/7/other-start")" ]] && http_ok http://localhost:9000/ && grep -qi nginx /tmp/rizr-eval-body || { echo "the other server on 9000 was touched"; return 1; }
       local p
       for p in $(docker ps --filter ancestor=listmonk/listmonk:latest --format '{{.Ports}}' | grep -oE '0\.0\.0\.0:[0-9]+' | cut -d: -f2 | sort -u); do
         http_ok "http://localhost:$p/admin/login" && final 7 | grep -q ":$p" && { echo "listmonk on $p, named in the reply"; return 0; }
       done
       echo "no listmonk answers on the port the reply names"; return 1 ;;
    8) local i n=0
       for i in 1 2 3; do curl -s -m 10 -o /dev/null -b "voter_id=eval$i" -d "vote=a" http://localhost:8080/; done
       for i in $(seq 10); do
         n=$(docker exec "$(served_by "$(service_ctr "$d" worker)" db)" psql -U postgres -tAc 'select count(*) from votes' 2>/dev/null | tr -dc 0-9)
         ((${n:-0} > 0)) && break; sleep 3
       done
       ((${n:-0} > 0)) || { echo "votes do not reach the database"; return 1; }
       [[ $(git -C "$d" diff --name-only HEAD | tr '\n' ' ') == "docker-compose.yml " ]] || { echo "diff touches: $(git -C "$d" diff --name-only HEAD | tr '\n' ' ')"; return 1; }
       echo "votes counted, fix in docker-compose.yml only" ;;
    10) [[ -s $OUT/10/first-ids ]] || { echo "no first copy"; return 1; }
       [[ $(docker inspect -f '{{.State.StartedAt}}' $(cat "$OUT/10/first-ids") 2>/dev/null | sort) == "$(cat "$OUT/10/first-start")" ]] && http_ok http://localhost:9000/ || { echo "first copy changed or down"; return 1; }
       local app db name q='select count(*) from subscribers where email' n
       # the second app and the database it uses, wherever a tool runs it
       app=$(docker ps -q --filter ancestor=listmonk/listmonk:latest | grep -vxFf <(cut -c1-12 "$OUT/10/first-ids") | head -n1)
       [[ -n $app ]] && db=$(served_by "$app" db) || { echo "no second copy with a database"; return 1; }
       name=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$app" | sed -n 's/^LISTMONK_db__database=//p')
       n=$(docker exec "$db" psql -U listmonk -d "${name:-listmonk}" -tAc "$q = '$MARK'" 2>/dev/null)
       [[ $n == 1 ]] || { echo "second copy lacks the first copy's data"; return 1; }
       docker exec "$db" psql -U listmonk -d "${name:-listmonk}" -qc "insert into subscribers (uuid, email, name, status) values (gen_random_uuid(), 'second-$MARK', 'Second', 'enabled')" >/dev/null 2>&1
       [[ $(docker exec listmonk_db psql -U listmonk -tAc "$q = 'second-$MARK'") == 0 ]] || { echo "second copy writes into the first"; return 1; }
       for p in $(docker ps --filter ancestor=listmonk/listmonk:latest --format '{{.Ports}}' | grep -oE '0\.0\.0\.0:[0-9]+' | cut -d: -f2 | sort -u); do
         [[ $p != 9000 ]] && http_ok "http://localhost:$p/" && final 10 | grep -q ":$p" && { echo "second copy on $p with the data, isolated"; return 0; }
       done
       echo "no second copy answers on the port the reply names"; return 1 ;;
    9) http_ok http://localhost:8000/graphql/ '{"query":"{ shop { name } }"}' && ! grep -q '"errors"' /tmp/rizr-eval-body || { echo "graphql fails"; return 1; }
       echo "graphql shop query ok" ;;
  esac
}

run() {
  local t=$1 start rc
  mkdir -p "$OUT/$t"
  log "task $t: clone + prep"
  cleanup; clone "$t" && prep "$t" && arm "$t" || {
    log "task $t: prep failed"
    jq -n --arg arm "$ARM" --argjson t "$t" '{arm: $arm, task: $t, verdict: "prep_failed"}' >"$OUT/$t/result.json"
    return
  }
  log "task $t: agent ($MODEL, limit ${LIMIT}s)"
  start=$SECONDS
  # NOAGENT=1 runs prep and check only: each check must fail on the prepared state.
  [[ -n ${NOAGENT:-} ]] && : >"$OUT/$t/transcript.jsonl" ||
  (cd "$ROOT/$t/repo" && PATH="$ROOT/bin:$PATH" timeout "$LIMIT" claude -p "$(prompt "$t")" --model "$MODEL" \
    --setting-sources project --no-session-persistence --strict-mcp-config \
    --allowedTools "Bash Read Edit Write Glob Grep" \
    --output-format stream-json --verbose) >"$OUT/$t/transcript.jsonl" 2>"$OUT/$t/agent.err"
  rc=$?
  local secs=$((SECONDS - start)) verdict reason
  reason=$(check "$t") && verdict=pass || verdict=fail
  # held-out tasks are judged from this state, never from their transcripts
  [[ $verdict == fail ]] && { docker ps -a --format '{{.Names}} {{.Status}}'; [[ $ARM == rizr ]] &&
    (cd "$ROOT/$t/repo" && "$ROOT/bin/rizr" ${COMPOSE[$t]:-} status); } >"$OUT/$t/check.log" 2>&1
  jq -s --arg arm "$ARM" --arg t "$t" --arg v "$verdict" --arg r "$reason" --argjson rc "$rc" --argjson s "$secs" --arg bin "$([[ $ARM == rizr ]] && sha256sum "$RIZR" | cut -c1-12)" '
    (map(select(.type=="result")) | last) as $res
    | ([.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | {id, cmd: (.input.command? // "")}]) as $calls
    | ([.[] | select(.type=="user") | .message.content[]? | select(.type=="tool_result" and .is_error==true) | .tool_use_id]) as $errs
    | ($calls | map(select(.cmd | test("(^|[;&|( ])rizr( |$)")))) as $rz
    | { arm: $arm, task: ($t|tonumber), verdict: $v,
        rizr_calls: ($rz | length),
        rizr_errors: ([$rz[] | select(.id as $i | $errs | index($i))] | length),
        compose_calls: ([$calls[] | select(.cmd | test("docker"))] | length), reason: $r, agent_exit: $rc, seconds: $s,
        tool_calls: ([.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use")] | length),
        turns: $res.num_turns, cost_usd: $res.total_cost_usd,
        rizr_sha: $bin, uncached: (($res.usage.input_tokens // 0) + ($res.usage.output_tokens // 0) + ($res.usage.cache_creation_input_tokens // 0)),
        tokens: (($res.usage.input_tokens // 0) + ($res.usage.output_tokens // 0) + ($res.usage.cache_read_input_tokens // 0) + ($res.usage.cache_creation_input_tokens // 0)),
        final: ($res.result // "" | .[0:400]) }' "$OUT/$t/transcript.jsonl" >"$OUT/$t/result.json"
  log "task $t: $verdict ($reason) $(jq -c '{tool_calls,seconds,cost_usd}' "$OUT/$t/result.json")"
  cleanup
}

for t in "${@:-1 2 3 4 5}"; do for x in $t; do run "$x"; done; done
jq -s '.' "$OUT"/*/result.json >"$OUT/summary.json"
log "summary: $OUT/summary.json"
