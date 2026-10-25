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

step "checkout $tag"
git -C "$repo" fetch --force --tags origin
git -C "$repo" checkout --force --detach "refs/tags/$tag"
cd "$repo"
git log --oneline -1

if $run_tests; then
  step "workspace tests"
  cargo nextest run --workspace --locked
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
