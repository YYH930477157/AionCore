#!/usr/bin/env bash
# Build an AionCore release on a remote Linux host and publish it to GitHub Releases.
#
# Usage: scripts/remote-release.sh <tag> [--test]
#
# The tag must already be pushed to origin. The host builds the Windows x64 and
# Linux x64 archives with scripts/remote-build.sh, keeping its cargo target dir
# warm between releases, so only crates that changed since the last release are
# recompiled. `--test` runs the workspace tests on the host first. The archives
# are then published from this machine with the local gh login, so the host
# holds no GitHub credentials.
#
# Environment:
#   AIONCORE_BUILD_HOST  ssh alias of the build host (default: perf)
#   AIONCORE_BUILD_USER  unprivileged build account on the host (default: aionbuild)
#
# One-time host setup: a Rust toolchain matching rust-toolchain.toml, cargo-xwin,
# cargo-nextest, clang/lld, zip, and a clone of this repo at ~/AionCore for the
# build account.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 || ( $# -eq 2 && "$2" != "--test" ) ]]; then
  echo "Usage: $0 <tag> [--test]" >&2
  exit 2
fi

tag="$1"
test_flag="${2:-}"
host="${AIONCORE_BUILD_HOST:-perf}"
user="${AIONCORE_BUILD_USER:-aionbuild}"
script_dir="$(cd "$(dirname "$0")" && pwd)"
home="/home/${user}"
unit="aion-build"
log="${home}/dist/logs/${tag}.log"

if [[ ! "${tag}" =~ ^v[0-9A-Za-z.+-]+$ ]]; then
  echo "Invalid tag: ${tag}" >&2
  exit 2
fi

origin_url="$(git -C "${script_dir}" remote get-url origin)"
repo="$(sed -E 's#^(https://github.com/|git@github.com:)##; s#\.git$##' <<<"${origin_url}")"

if ! git -C "${script_dir}" ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null; then
  echo "Tag ${tag} is not on origin; push it first." >&2
  exit 1
fi

echo "Building ${tag} on ${host}..."
ssh "${host}" "cat > ${home}/bin/aion-build.sh && chmod 755 ${home}/bin/aion-build.sh \
  && chown ${user}: ${home}/bin/aion-build.sh" <"${script_dir}/remote-build.sh"

# Run as a transient unit so the build survives a dropped ssh session, and cap
# it so it cannot starve the other services on the host.
ssh "${host}" "systemctl is-active -q ${unit} && { echo 'Another build is running.' >&2; exit 1; }
  systemd-run --quiet --unit=${unit} --collect --uid=${user} --gid=${user} \
    -p MemoryHigh=10G -p MemoryMax=12G -p CPUWeight=20 -p IOWeight=20 -p Nice=10 \
    -p WorkingDirectory=${home} \
    bash -lc '${home}/bin/aion-build.sh ${tag} ${test_flag} > ${log} 2>&1'
  while systemctl is-active -q ${unit}; do sleep 10; done
  grep '^== ' ${log}
  tail -n 1 ${log} | grep -qx '== OK' || { tail -n 40 ${log}; exit 1; }"

# Pull the archives back in parallel byte ranges: a single ssh stream from the
# host can be several times slower than the link allows.
dist="$(mktemp -d)"
trap 'rm -rf "${dist}"' EXIT
streams=8
sizes="$(ssh "${host}" "cd ${home}/dist/${tag} && stat -c '%s %n' aioncore-*")"
while read -r size name; do
  chunk=$(( (size + streams - 1) / streams ))
  pids=()
  for i in $(seq 0 $((streams - 1))); do
    ssh -n "${host}" "tail -c +$((i * chunk + 1)) ${home}/dist/${tag}/${name} | head -c ${chunk}" \
      >"${dist}/${name}.part${i}" &
    pids+=("$!")
  done
  for pid in "${pids[@]}"; do wait "${pid}"; done
  for i in $(seq 0 $((streams - 1))); do cat "${dist}/${name}.part${i}"; done >"${dist}/${name}"
  rm -f "${dist}/${name}".part*
done <<<"${sizes}"
(cd "${dist}" && sha256sum -c --quiet aioncore-checksums.txt)

if gh release view "${tag}" --repo "${repo}" >/dev/null 2>&1; then
  gh release upload "${tag}" "${dist}"/* --repo "${repo}" --clobber
else
  gh release create "${tag}" "${dist}"/* --repo "${repo}" --title "${tag}" --generate-notes --verify-tag
fi
echo "Published https://github.com/${repo}/releases/tag/${tag}"
