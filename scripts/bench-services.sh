#!/usr/bin/env bash
# Measure what a service job pays before its first step, three ways (ADR-0046):
#
#   dind     today's path: a fresh privileged dind with an empty image store,
#            which pulls and starts postgres, redis and mailpit itself
#   per-job  faber starts the three on the job's network from the engine's
#            cached images (ADR-0046's per-job path, applied to Postgres too)
#   shared   Postgres is already running; the job gets a role and a database
#            cloned from a template, and redis and mailpit start per job
#
# Run against any throwaway Docker engine (it creates and removes its own
# containers, networks and volumes, all named faber-bench-*):
#
#   scripts/bench-services.sh [runs]
#
# Prints one line per run and a median per path. Never point it at faber's own
# engine: it is a benchmark, not a job.
set -euo pipefail

RUNS=${1:-5}
PG=postgres:16-alpine
REDIS=redis:7-alpine
MAILPIT=axllent/mailpit:v1.21
DIND=docker:dind

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
elapsed() { perl -e 'printf "%.2f\n", $ARGV[1] - $ARGV[0]' "$1" "$2"; }

# until_ok <seconds> <cmd...>: retry every 100 ms until cmd succeeds.
until_ok() {
  local limit=$1; shift
  local end; end=$(perl -e "print time + $limit")
  until "$@" >/dev/null 2>&1; do
    [ "$(perl -e 'print time')" -lt "$end" ] || { echo "timed out: $*" >&2; return 1; }
    sleep 0.1
  done
}

cleanup() {
  local id
  for id in $(docker ps -aq --filter name=faber-bench-); do docker rm -f "$id" >/dev/null 2>&1 || true; done
  for id in $(docker network ls -q --filter name=faber-bench-); do docker network rm "$id" >/dev/null 2>&1 || true; done
  for id in $(docker volume ls -q --filter name=faber-bench-); do docker volume rm "$id" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT
cleanup

# The engine's own cache is warm in both new paths: that is the point of them.
for i in "$PG" "$REDIS" "$MAILPIT" "$DIND"; do docker pull -q "$i" >/dev/null; done

pg_flags=(-c fsync=off -c synchronous_commit=off -c full_page_writes=off)

run_dind() {
  local n=faber-bench-d$1
  docker network create "$n" >/dev/null
  docker volume create "$n" >/dev/null
  docker run -d --privileged --name "$n" --network "$n" --network-alias dind \
    -e DOCKER_TLS_CERTDIR= -v "$n":/var/lib/docker "$DIND" >/dev/null
  until_ok 60 docker exec "$n" docker info
  docker exec "$n" sh -c "
    docker pull -q $PG >/dev/null & docker pull -q $REDIS >/dev/null & docker pull -q $MAILPIT >/dev/null & wait
    docker run -d --name pg -e POSTGRES_PASSWORD=x $PG ${pg_flags[*]} >/dev/null
    docker run -d --name redis $REDIS >/dev/null
    docker run -d --name mailpit $MAILPIT >/dev/null"
  until_ok 60 docker exec "$n" docker exec pg pg_isready -U postgres -h 127.0.0.1
  until_ok 30 docker exec "$n" docker exec redis redis-cli ping
}

run_per_job() {
  local n=faber-bench-p$1
  docker network create "$n" >/dev/null
  docker run -d --name "$n-pg" --network "$n" --network-alias postgres \
    -e POSTGRES_PASSWORD=x "$PG" "${pg_flags[@]}" >/dev/null
  docker run -d --name "$n-redis" --network "$n" --network-alias redis "$REDIS" >/dev/null
  docker run -d --name "$n-mailpit" --network "$n" --network-alias mailpit "$MAILPIT" >/dev/null
  until_ok 60 docker exec "$n-pg" pg_isready -U postgres -h 127.0.0.1
  until_ok 30 docker exec "$n-redis" redis-cli ping
}

SERVER=faber-bench-server
start_server() {
  docker network create --internal faber-bench-svc >/dev/null
  docker run -d --name "$SERVER" --network faber-bench-svc \
    -e POSTGRES_PASSWORD=x "$PG" "${pg_flags[@]}" -c max_connections=500 >/dev/null
  until_ok 60 docker exec "$SERVER" pg_isready -U postgres -h 127.0.0.1
  docker exec -i "$SERVER" psql -q -v ON_ERROR_STOP=1 -U postgres >/dev/null <<'SQL'
CREATE DATABASE faber_tpl IS_TEMPLATE true;
REVOKE CONNECT ON DATABASE faber_tpl FROM PUBLIC;
SQL
}

run_shared() {
  local n=faber-bench-s$1 role=faber_r_bench_$1
  docker network create "$n" >/dev/null
  docker network connect --alias postgres "$n" "$SERVER"
  docker exec -i "$SERVER" psql -q -v ON_ERROR_STOP=1 -U postgres >/dev/null <<SQL
CREATE ROLE $role LOGIN CREATEDB PASSWORD 'x';
CREATE DATABASE $role TEMPLATE faber_tpl OWNER $role;
REVOKE CONNECT ON DATABASE $role FROM PUBLIC;
GRANT CONNECT ON DATABASE $role TO $role;
SQL
  docker run -d --name "$n-redis" --network "$n" --network-alias redis "$REDIS" >/dev/null
  docker run -d --name "$n-mailpit" --network "$n" --network-alias mailpit "$MAILPIT" >/dev/null
  until_ok 30 docker exec "$n-redis" redis-cli ping
}

median() { sort -n | awk '{a[NR]=$1} END {print (NR%2 ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2)}'; }

t0=$(now); start_server; t1=$(now)
echo "shared server cold start (once per host, then idle-stopped): $(elapsed "$t0" "$t1") s"

D=(); P=(); S=()
for r in $(seq 1 "$RUNS"); do
  t0=$(now); run_dind "$r"; t1=$(now); d=$(elapsed "$t0" "$t1"); D+=("$d")
  t0=$(now); run_per_job "$r"; t1=$(now); p=$(elapsed "$t0" "$t1"); P+=("$p")
  t0=$(now); run_shared "$r"; t1=$(now); s=$(elapsed "$t0" "$t1"); S+=("$s")
  echo "run $r: dind $d s   per-job $p s   shared $s s"
done

# Memory the services cost while idle, as the engine accounts it.
mem() { docker stats --no-stream --format '{{.MemUsage}}' "$@" | awk '{print $1}' | tr '\n' ' '; }
echo "memory, dind (daemon + its 3 services, one job):  $(mem faber-bench-d1)"
echo "memory, per-job (pg, redis, mailpit, one job):     $(mem faber-bench-p1-pg faber-bench-p1-redis faber-bench-p1-mailpit)"
echo "memory, shared server ($RUNS jobs attached):        $(mem "$SERVER")"

echo
echo "median over $RUNS runs:"
echo "  dind     $(printf '%s\n' "${D[@]}" | median) s"
echo "  per-job  $(printf '%s\n' "${P[@]}" | median) s"
echo "  shared   $(printf '%s\n' "${S[@]}" | median) s"
