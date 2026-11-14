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
  expect socket out 'usermod -aG docker|unset DOCKER_HOST'
}

up_rc() { # case: up with a time limit; the exit code goes into out
  timeout 120 "$RIZR" up >out 2>&1; echo "rc=$?" >>out
}

portstray() { # a plain compose copy of the same repo holds the port
  stack portstray 'services:
  a: {image: busybox:1.36-musl, command: [httpd, -f, -p, "80"], ports: ["18097:80"]}'
  docker compose -p portstray-old up -d >/dev/null 2>&1
  up_rc
  expect portstray out 'docker compose -p portstray-old down --remove-orphans'
  docker compose -p portstray-old down >/dev/null 2>&1
  done_case portstray
}

image() { # the image does not exist
  stack image 'services:
  a: {image: "rizr-no-such-image:1", command: sleep 600}'
  up_rc
  expect image out 'try: docker login'
  done_case image
}

daemon() { # docker does not run
  stack daemon 'services:
  a: {image: alpine, command: sleep 600}'
  DOCKER_HOST=unix://$WORK/none.sock up_rc
  expect daemon out 'rc=4'
  expect daemon out 'try: systemctl (start docker|--user enable --now podman.socket)'
}

variable() { # a required compose variable has no value: rizr names it and where to set it
  stack variable 'services:
  a: {image: alpine, command: sleep 600, environment: {X: "${RIZR_EDGE_NEEDED:?set it}"}}'
  up_rc
  expect variable out 'add `RIZR_EDGE_NEEDED=<value>` to'
  done_case variable
}

yaml() { # the compose file is not valid YAML
  mkdir -p "$WORK/yaml" && cd "$WORK/yaml" && printf 'services:\n  a: {image: alpine\n' >compose.yaml
  script -qec "$RIZR trust" /dev/null <<<y >out 2>&1
  expect yaml out 'is not valid YAML'
}

job() { # a migration job fails: its output and the fix
  stack job 'services:
  migrate: {image: alpine, command: [sh, -c, "echo relation users does not exist; exit 3"], restart: "no"}
  app: {image: alpine, command: sleep 600, depends_on: {migrate: {condition: service_completed_successfully}}}'
  up_rc
  expect job out 'relation users does not exist'
  expect job out 'try: rizr logs migrate'
  done_case job
}

health() { # a dependency never turns healthy: its healthcheck output
  stack health 'services:
  db: {image: alpine, command: sleep 600, healthcheck: {test: [CMD, sh, -c, "echo db not ready; exit 1"], interval: 1s, retries: 2}}
  app: {image: alpine, command: sleep 600, depends_on: {db: {condition: service_healthy}}}'
  up_rc
  expect health out 'db not ready'
  done_case health
}

build() { # a Dockerfile step fails
  stack build 'services:
  a: {build: ., command: sleep 600}'
  printf 'FROM alpine\nRUN echo compiling; exit 2\n' >Dockerfile
  up_rc
  expect build out 'try: docker compose build a'
  done_case build
}

disk() { # a tmpfs mount fills up
  stack disk 'services:
  a: {image: alpine, command: [sh, -c, "dd if=/dev/zero of=/data/x bs=1M count=8 && sleep 600"], restart: "no", tmpfs: ["/data:size=1m"]}'
  up_rc
  expect disk out 'ran out of disk space'
  done_case disk
}

oom() { # the kernel kills a service at its memory limit
  stack oom 'services:
  a: {image: alpine, command: [sh, -c, "x=$$(head -c 64000000 /dev/zero | tr \"\\000\" a); sleep 600"], mem_limit: 16m, memswap_limit: 16m}'
  up_rc
  expect oom out 'ran out of memory'
  done_case oom
}

tool() { # the healthcheck calls a tool the image does not have
  stack tool 'services:
  a: {image: alpine, command: sleep 600, healthcheck: {test: [CMD, curl, -f, "http://localhost/"], interval: 1s, retries: 2}}'
  up_rc
  expect tool out 'runs curl, which the image does not have'
  done_case tool
}

nocmd() { # the start command is not in the image
  stack nocmd 'services:
  a: {image: alpine, command: [httpd, -f]}'
  up_rc
  expect nocmd out 'starts with httpd, which its image does not have'
  done_case nocmd
}

envfile() { # an env_file with no example to copy
  stack envfile 'services:
  a: {image: alpine, command: sleep 600, env_file: [./secrets.env]}'
  up_rc
  expect envfile out 'try: touch .*secrets.env'
}

undefined() { # depends_on names a service no file defines
  stack undefined 'services:
  a: {image: alpine, command: sleep 600, depends_on: [nosuch]}'
  up_rc
  expect undefined out 'a depends on nosuch, which no compose file in use defines'
}

redis() { # a redis is ready by its log line, answers PING, and reset empties it
  stack redis 'services:
  cache: {image: "redis:7-alpine", ports: ["18096:6379"]}'
  up_rc
  "$RIZR" status >>out 2>&1
  docker exec redis-cache-1 redis-cli set k v >/dev/null
  "$RIZR" reset cache >>out 2>&1
  echo "after reset: [$(docker exec redis-cache-1 redis-cli get k)]" >>out
  expect redis out 'log line "Ready to accept connections"'
  expect redis out 'redis://localhost:18096 -> PONG'
  expect redis out 'after reset: \[\]'
  done_case redis
}

otherfile() { # up runs compose.yaml, but a self-contained pwd.yml beside it is the stack: the error's try says so
  stack otherfile 'services:
  app: {image: alpine, command: [sh, -c, "echo cannot reach localhost:6379; exit 1"]}'
  printf 'services:\n  app: {image: alpine, command: sleep 600}\n  cache: {image: alpine, command: sleep 600}\n' >pwd.yml
  up_rc
  expect otherfile out 'pwd.yml beside it also defines cache'
  expect otherfile out 'try: rizr -f pwd.yml up'
  done_case otherfile
}

unsynced() { # exec misses a path the image lacks and develop.watch syncs: the try copies the tree and reruns
  stack unsynced 'services:
  a: {build: ., command: sleep 600, develop: {watch: [{path: ., action: sync, target: /app, ignore: [.venv]}]}}'
  printf 'FROM alpine\nWORKDIR /app\nCOPY compose.yaml .\n' >Dockerfile
  mkdir -p tests .venv && echo t >tests/test_a && echo v >.venv/marker
  "$RIZR" trust --yes >/dev/null 2>&1
  up_rc
  "$RIZR" exec a ls tests/ >>out 2>&1
  expect unsynced out "image lacks tests"
  try=$(sed -n 's/^try: //p' out | tail -1)
  bash -c "$try" >>out 2>&1
  expect unsynced out '^test_a$'
  [[ -z $(docker compose exec -T a ls /app/.venv 2>/dev/null) ]] || { echo "FAIL unsynced: .venv was copied"; fails=$((fails + 1)); }
  done_case unsynced
}

subnet() { # two worktrees of a repo that pins a subnet and a fixed address run up at once, each on its own subnet
  mkdir -p "$WORK/subnet" && cd "$WORK/subnet"
  printf 'services:\n  a: {image: alpine, command: sleep 600, networks: {back: {ipv4_address: 10.213.7.10}}}\nnetworks:\n  back: {ipam: {config: [{subnet: 10.213.7.0/24, gateway: 10.213.7.1}]}}\n' >compose.yaml
  git init -q && git add -A && git -c user.email=e@x -c user.name=e commit -qm init && git worktree add -q ../subnet-wt
  for d in . ../subnet-wt; do (cd "$d" && script -qec "$RIZR trust" /dev/null <<<y >/dev/null); done
  (timeout 120 "$RIZR" up >main.out 2>&1; echo "rc=$?" >>main.out) &
  (cd ../subnet-wt && timeout 120 "$RIZR" up >../subnet/wt.out 2>&1; echo "rc=$?" >>../subnet/wt.out) &
  wait
  expect subnet main.out 'rc=0'
  expect subnet wt.out 'rc=0'
  docker ps -q --filter label=com.docker.compose.service=a --filter name=subnet |
    xargs -r docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' >ips
  expect subnet ips '^10\.213\.7\.10 $'
  expect subnet ips '^10\.213\.8\.10 $'
  (cd ../subnet-wt && "$RIZR" down >/dev/null 2>&1)
  grep -q subnet-wt "$XDG_DATA_HOME"/rizr/*.subnets.json || { echo "FAIL subnet: no claim for the worktree"; fails=$((fails + 1)); }
  git worktree remove --force ../subnet-wt && "$RIZR" gc >gc.out 2>&1
  expect subnet gc.out 'released subnet-wt'
  ! grep -q subnet-wt "$XDG_DATA_HOME"/rizr/*.subnets.json && echo "ok   subnet: gc released the claim" ||
    { echo "FAIL subnet: gc kept the claim"; fails=$((fails + 1)); }
  done_case subnet
}

cases=("${@:-orphan crashloop slowjob server500 credentials namecheck downplain socket portstray image daemon variable yaml job health build disk oom tool nocmd envfile undefined redis otherfile unsynced subnet}")
# Each case takes a docker network; docker has about 30 address pools for every stack on the machine.
for c in ${cases[*]}; do
  while (($(jobs -rp | wc -l) >= 4)); do wait -n; done
  ("$c") >"$WORK/$c.log" 2>&1 &
done
wait
cat "$WORK"/*.log
grep -q '^FAIL' "$WORK"/*.log && { echo "edge: failures (work dir $WORK)"; exit 1; }
echo "edge: all passed"
rm -rf "$WORK"
