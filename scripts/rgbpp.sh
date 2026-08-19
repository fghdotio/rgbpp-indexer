#!/usr/bin/env bash
#
# Docker helper for the RGB++ indexer.
#
# Everything here is a thin, predictable wrapper over `docker compose`. The value it
# adds is the two things compose makes awkward: telling apart "stop" from "delete my
# data", and having one command that rebuilds and restarts without stale images.
#
#   scripts/rgbpp.sh <command> [args...]

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

COMPOSE=(docker compose)
DB_USER="${POSTGRES_USER:-rgbpp}"
DB_NAME="${POSTGRES_DB:-rgbpp}"
API_PORT="${API_PORT:-8080}"

red()  { printf '\033[31m%s\033[0m\n' "$*"; }
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
die()  { red "error: $*" >&2; exit 1; }

require_docker() {
  docker info >/dev/null 2>&1 || die "the Docker daemon is not reachable"
}

# Destructive actions ask first, unless -y was passed or FORCE=1 is set.
confirm() {
  [[ "${FORCE:-0}" == "1" ]] && return 0
  local answer
  read -r -p "$1 [y/N] " answer
  [[ "$answer" == "y" || "$answer" == "Y" ]] || { echo "aborted"; exit 1; }
}

wait_for_db() {
  printf 'waiting for postgres'
  for _ in $(seq 1 60); do
    if "${COMPOSE[@]}" exec -T postgres pg_isready -U "$DB_USER" >/dev/null 2>&1; then
      echo ' ready'
      return 0
    fi
    printf '.'
    sleep 1
  done
  echo
  die "postgres did not become ready"
}

usage() {
  cat <<'USAGE'
RGB++ indexer — Docker helper

  Running
    up               Start Postgres and the indexer
    db               Start Postgres only (for `cargo run` from the host)
    down             Stop everything; data is kept
    restart          Restart the indexer only
    ps               Container status

  Building
    build            Build the indexer image (cached)
    rebuild          Build with --no-cache, then restart the indexer

  Data                                                    [DESTRUCTIVE]
    reset-db         Drop and recreate the schema, keeping containers running
    destroy          Stop everything and delete the database volume
    clean            destroy, plus remove the built image

  Inspecting
    logs [-f|N]      Indexer logs (follow, or last N lines)
    logs-db [-f|N]   Postgres logs
    status           GET /status from the running indexer
    psql [sql]       psql shell, or run one statement
    sh               Shell inside the indexer container
    exec <args...>   Run the indexer binary with arbitrary arguments

  Development
    migrate          Apply migrations and exit
    test-db          Run the store schema tests against the dev database

  Destructive commands prompt for confirmation; pass -y or set FORCE=1 to skip.
USAGE
}

cmd="${1:-help}"
[[ $# -gt 0 ]] && shift || true

# Allow `-y` anywhere in the remaining arguments.
args=()
for arg in "$@"; do
  if [[ "$arg" == "-y" ]]; then FORCE=1; else args+=("$arg"); fi
done
set -- ${args[@]+"${args[@]}"}

case "$cmd" in
  up)
    require_docker
    "${COMPOSE[@]}" up -d --build
    wait_for_db
    bold "indexer:  http://localhost:${API_PORT}/status"
    bold "logs:     scripts/rgbpp.sh logs -f"
    ;;

  db)
    require_docker
    "${COMPOSE[@]}" up -d postgres
    wait_for_db
    bold "DATABASE_URL=postgres://${DB_USER}:${POSTGRES_PASSWORD:-rgbpp}@localhost:${POSTGRES_PORT:-5432}/${DB_NAME}"
    ;;

  down)
    require_docker
    "${COMPOSE[@]}" down
    ;;

  restart)
    require_docker
    "${COMPOSE[@]}" restart indexer
    ;;

  ps)
    require_docker
    "${COMPOSE[@]}" ps
    ;;

  build)
    require_docker
    "${COMPOSE[@]}" build indexer
    ;;

  rebuild)
    require_docker
    "${COMPOSE[@]}" build --no-cache indexer
    "${COMPOSE[@]}" up -d --force-recreate indexer
    ;;

  reset-db)
    require_docker
    confirm "Drop and recreate the '${DB_NAME}' schema? All indexed data is lost."
    "${COMPOSE[@]}" up -d postgres
    wait_for_db
    # The indexer must not be writing while the schema is swapped out from under it.
    "${COMPOSE[@]}" stop indexer >/dev/null 2>&1 || true
    "${COMPOSE[@]}" exec -T postgres psql -U "$DB_USER" -d "$DB_NAME" \
      -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;"
    bold "schema reset; migrations re-apply on the next start"
    ;;

  destroy)
    require_docker
    confirm "Stop all containers and DELETE the database volume?"
    "${COMPOSE[@]}" down --volumes
    bold "containers and data volume removed"
    ;;

  clean)
    require_docker
    confirm "Remove containers, the data volume, AND the built image?"
    "${COMPOSE[@]}" down --volumes --rmi local
    bold "removed"
    ;;

  logs)
    require_docker
    if [[ "${1:-}" == "-f" ]]; then
      "${COMPOSE[@]}" logs -f indexer
    else
      "${COMPOSE[@]}" logs --tail "${1:-200}" indexer
    fi
    ;;

  logs-db)
    require_docker
    if [[ "${1:-}" == "-f" ]]; then
      "${COMPOSE[@]}" logs -f postgres
    else
      "${COMPOSE[@]}" logs --tail "${1:-200}" postgres
    fi
    ;;

  status)
    curl -fsS "http://localhost:${API_PORT}/status" \
      | { python3 -m json.tool 2>/dev/null || cat; }
    ;;

  psql)
    require_docker
    if [[ $# -gt 0 ]]; then
      "${COMPOSE[@]}" exec -T postgres psql -U "$DB_USER" -d "$DB_NAME" -c "$*"
    else
      "${COMPOSE[@]}" exec postgres psql -U "$DB_USER" -d "$DB_NAME"
    fi
    ;;

  sh)
    require_docker
    "${COMPOSE[@]}" exec indexer /bin/bash
    ;;

  exec)
    require_docker
    [[ $# -gt 0 ]] || die "usage: scripts/rgbpp.sh exec <indexer arguments>"
    "${COMPOSE[@]}" run --rm indexer "$@"
    ;;

  migrate)
    require_docker
    "${COMPOSE[@]}" run --rm indexer migrate
    ;;

  test-db)
    require_docker
    "${COMPOSE[@]}" up -d postgres
    wait_for_db
    TEST_DATABASE_URL="postgres://${DB_USER}:${POSTGRES_PASSWORD:-rgbpp}@localhost:${POSTGRES_PORT:-5432}/${DB_NAME}" \
      cargo test -p rgbpp-store --test schema
    ;;

  help|--help|-h)
    usage
    ;;

  *)
    red "unknown command: $cmd"
    echo
    usage
    exit 1
    ;;
esac
