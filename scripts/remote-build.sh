#!/usr/bin/env bash
# Build AionCore release archives for one tag on the build host (runs as the
# unprivileged build account; started by scripts/remote-release.sh).
# Usage: remote-build.sh <tag> [--test]
set -euo pipefail

tag="$1"
run_tests=false
[[ "${2:-}" == "--test" ]] && run_tests=true

. "$HOME/.cargo/env"
export CARGO_TERM_COLOR=never CARGO_HTTP_TIMEOUT=600 CARGO_NET_RETRY=10

repo="$HOME/AionCore"
dist="$HOME/dist/$tag"
step() { echo "== $1 ($(date +%T), ${SECONDS}s)"; }

trap 'echo "== FAILED at line $LINENO (${SECONDS}s)"' ERR

# Fail now rather than at the final link when the disk is nearly full.
step "disk space"
need_gb=10
$run_tests && need_gb=40
free_gb=$(($(df --output=avail -k "$HOME" | tail -n 1) / 1024 / 1024))
echo "free ${free_gb} GB, need ${need_gb} GB"
if ((free_gb < need_gb)); then
  echo "== FAILED: only ${free_gb} GB free on the build host, need ${need_gb} GB (${SECONDS}s)"
  exit 1
fi

step "checkout $tag"
git -C "$repo" fetch --force --tags origin
git -C "$repo" checkout --force --detach "refs/tags/$tag"
cd "$repo"
git log --oneline -1

if $run_tests; then
  step "workspace tests"
  tests_passed=true
  cargo nextest run --workspace --locked || tests_passed=false
  # The test build in target/debug grows by many GB per release; left alone it
  # reached 105 GB and filled the host's disk (woody.11). Remove it whether or
  # not the tests passed. The next --test run rebuilds it from scratch, while
  # the release targets below stay warm.
  step "remove test build"
  cargo clean --profile dev
  if ! $tests_passed; then
    echo "== FAILED: workspace tests (${SECONDS}s)"
    exit 1
  fi
fi

rm -rf "$dist"
mkdir -p "$dist"

step "windows x64"
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo xwin build --release --locked --target x86_64-pc-windows-msvc -p aionui-app
(cd target/x86_64-pc-windows-msvc/release && zip -q -X "$dist/aioncore-$tag-x86_64-pc-windows-msvc.zip" aioncore.exe)

step "linux x64"
cargo build --release --locked --target x86_64-unknown-linux-gnu -p aionui-app
scripts/check-glibc-baseline.sh target/x86_64-unknown-linux-gnu/release/aioncore GLIBC_2.34
tar -C target/x86_64-unknown-linux-gnu/release -czf "$dist/aioncore-$tag-x86_64-unknown-linux-gnu.tar.gz" aioncore

(cd "$dist" && sha256sum aioncore-* > aioncore-checksums.txt && ls -la)
step "done"
echo "== OK"
