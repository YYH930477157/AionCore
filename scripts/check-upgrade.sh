#!/usr/bin/env bash
# Prove a new Windows backend opens a database the previous one created.
#
# Usage: scripts/check-upgrade.sh <previous.exe> <new.exe>
#
# Migration checksums are embedded at compile time, so a build that behaves the
# same on a fresh data dir can still refuse every existing database. Only a real
# upgrade shows that.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: $0 <previous.exe> <new.exe>" >&2
  exit 2
fi

port="${AIONCORE_UPGRADE_PORT:-18799}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
data="$(cygpath -m "${work}/data")"
mkdir -p "${work}/data"

# Run a backend on the shared data dir until it serves a request. The private
# image name keeps taskkill away from any backend the user is running.
serve_once() {
  local source="$1" label="$2"
  local image="aioncore-upgrade-${label}.exe"
  local log="${work}/${label}.log"
  cp "${source}" "${work}/${image}"

  "${work}/${image}" --port "${port}" --data-dir "${data}" --work-dir "${data}" --local >"${log}" 2>&1 &
  local pid=$!
  local served=false
  for _ in $(seq 1 60); do
    kill -0 "${pid}" 2>/dev/null || break
    if curl -sf -o /dev/null --max-time 2 "http://127.0.0.1:${port}/api/providers"; then
      served=true
      break
    fi
    sleep 1
  done

  taskkill //F //IM "${image}" >/dev/null 2>&1 || true
  wait "${pid}" 2>/dev/null || true
  if ! "${served}"; then
    echo "The ${label} backend did not start:" >&2
    tail -n 5 "${log}" >&2
    return 1
  fi
}

serve_once "$1" previous
serve_once "$2" new
echo "Upgrade check passed."
