#!/usr/bin/env bash
# Live edge cases against docker: each case is a small stack, a setup, `rizr up`, and what an agent must see.
# Usage: tests/edge.sh [case...]   Needs docker, alpine and mariadb:11.8 images, and port 18099 free.
set -uo pipefail
RIZR=${RIZR:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/rizr}
WORK=$(mktemp -d /tmp/rizr-edge.XXXX)
export XDG_DATA_HOME=$WORK/data
fails=0

stack() { # name, compose text -> cd into a trusted stack dir
  mkdir -p "$WORK/$1" && cd "$WORK/$1" && printf '%s\n' "$2" >compose.yaml
  script -qec "$RIZR trust" /dev/null <<<y >/dev/null
}
expect() { # case, file, extended regex
  grep -Eq "$3" "$2" && echo "ok   $1: /$3/" || { echo "FAIL $1: no /$3/ in:"; sed 's/^/     /' "$2"; fails=$((fails + 1)); }
}
done_case() { "$RIZR" down >/dev/null 2>&1; docker volume ls -q --filter "label=com.docker.compose.project=$1" | xargs -r docker volume rm >/dev/null; }

orphan() { # a container of the project that no compose file runs any more
  stack orphan 'services:
  a: {image: alpine, command: sleep 600}'
  printf 'services:\n  a: {image: alpine, command: sleep 600}\n  b: {image: alpine, command: sleep 600}\n' >"$WORK/full.yaml"
  docker compose -p orphan -f "$WORK/full.yaml" --project-directory . up -d >/dev/null 2>&1
  "$RIZR" up >out 2>&1
  expect orphan out 'removed container orphan-b-1'
  [[ -z $(docker ps -aq --filter name=orphan-b-1) ]] || { echo "FAIL orphan: orphan-b-1 still exists"; fails=$((fails + 1)); }
  done_case orphan
}

crashloop() { # restart: always and a crash at start: up must fail in time, with the crash output
  stack crashloop 'services:
  bad: {image: alpine, command: [sh, -c, "echo boom; exit 1"], restart: always}'
  timeout 110 "$RIZR" up >out 2>&1; echo "rc=$?" >>out
  expect crashloop out 'rc=5'
  expect crashloop out 'boom'
  done_case crashloop
}

slowjob() { # a job longer than the 100 s budget: up returns in time, names it, and the next up finishes
  stack slowjob 'services:
  seed: {image: alpine, command: [sh, -c, "echo seeding step 1; sleep 115; echo seeded"], restart: "no"}
  web: {image: alpine, command: [sleep, "600"], depends_on: {seed: {condition: service_completed_successfully}}}'
  timeout 110 "$RIZR" up >out 2>&1; echo "rc=$?" >>out
  expect slowjob out 'still starting after 100 s: seed running, last log: seeding step 1'
  timeout 110 "$RIZR" up >out2 2>&1; echo "rc=$?" >>out2
  expect slowjob out2 'slowjob: Up'
  done_case slowjob
}

server500() { # the app answers 5xx: up, status, why and context say degraded with the cause
  stack server500 'services:
  web:
    image: alpine
    command: [sh, -c, "echo \"OperationalError: no such table: users\" >&2; while true; do printf \"HTTP/1.0 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nerror\" | nc -l -p 80 >/dev/null; done"]
    ports: ["18099:80"]'
  "$RIZR" up >out 2>&1
  "$RIZR" status >st 2>&1
  "$RIZR" why web >why 2>&1
  "$RIZR" context --json >ctx 2>&1
  expect server500 out 'Degraded'
  expect server500 st 'Degraded'
  expect server500 st 'database has no tables'
  expect server500 why 'problem: web http://localhost:18099/ -> HTTP 500'
  expect server500 ctx '"status":"degraded"|"status": "degraded"'
  done_case server500
}

credentials() { # a volume made with another password: name the cause and the reset command
  local db='    image: mariadb:11.8
    volumes: [dbdata:/var/lib/mysql]
    healthcheck: {test: [CMD, healthcheck.sh, --connect], interval: 2s, retries: 30}'
  stack credentials "services:
  db:
$db
    environment: {MARIADB_ROOT_PASSWORD: first}
volumes: {dbdata: {}}"
  "$RIZR" up >/dev/null 2>&1
  "$RIZR" down >/dev/null 2>&1
  printf 'services:\n  db:\n%s\n    environment: {MARIADB_ROOT_PASSWORD: second}\n  app:\n    image: mariadb:11.8\n    command: [sh, -c, "mariadb -h db -uroot -psecond -e \\"select 1\\" && sleep 600"]\n    restart: "on-failure:3"\n    depends_on: {db: {condition: service_healthy}}\nvolumes: {dbdata: {}}\n' "$db" >compose.yaml
  "$RIZR" trust --yes >/dev/null 2>&1
  "$RIZR" up >out 2>&1
  expect credentials out 'keeps the user and password it was first made with'
  done_case credentials
}

namecheck() { # container_name held by a container outside the stack
  docker run -d --name rizr-edge-fixed alpine sleep 600 >/dev/null
  stack namecheck 'services:
  a: {image: alpine, command: sleep 600, container_name: rizr-edge-fixed}'
  "$RIZR" up >out 2>&1
  expect namecheck out 'try: docker rm -f rizr-edge-fixed'
  docker rm -f rizr-edge-fixed >/dev/null
  done_case namecheck
}

downplain() { # down removes what a plain compose run left in the same project
  stack downplain 'services:
  a: {image: alpine, command: sleep 600}'
  docker compose -p downplain up -d >/dev/null 2>&1
  "$RIZR" down >out 2>&1
  expect downplain out 'removed container downplain-a-1'
  done_case downplain
}

socket() { # this user may not use the docker socket: up names the docker group fix
  stack socket 'services:
  a: {image: alpine, command: sleep 600}'
  python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$WORK/denied.sock"
  chmod 000 "$WORK/denied.sock"
  DOCKER_HOST=unix://$WORK/denied.sock timeout 60 "$RIZR" up >out 2>&1; echo "rc=$?" >>out
  expect socket out 'rc=4'
  expect socket out 'usermod -aG docker'
}

cases=("${@:-orphan crashloop slowjob server500 credentials namecheck downplain socket}")
for c in ${cases[*]}; do ("$c") >"$WORK/$c.log" 2>&1 & done
wait
cat "$WORK"/*.log
grep -q '^FAIL' "$WORK"/*.log && { echo "edge: failures (work dir $WORK)"; exit 1; }
echo "edge: all passed"
rm -rf "$WORK"
