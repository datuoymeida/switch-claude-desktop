#!/bin/sh
# Cross-build the Windows programs (x64) from macOS or Linux, inside a Docker container
# that holds the MinGW-w64 toolchain. Nothing is installed on this machine.
#
#   ./scripts/build-windows.sh
#   dist/windows/AppSwitchClaude.exe          # the app
#   dist/windows/AppSwitchClaude-probe.exe    # read-only diagnostic, for a terminal
#
# On Windows itself none of this is needed: `cargo build --release`.
set -eu
cd "$(dirname "$0")/.."

IMAGE=app-switch-claude-windows-builder
TARGET=x86_64-pc-windows-gnu
WORK=target/windows-cross # the container's build directory and crate cache
# Compiler processes at a time. Docker Desktop's virtual machine is small by default (4 GB)
# and more than two of them run it out of memory; raise it with JOBS=8 on a bigger one.
JOBS=${JOBS:-2}

# The "posix" flavour of MinGW-w64 is the one the Rust standard library is built with.
docker build --tag "$IMAGE" - <<'DOCKERFILE'
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64-posix \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-pc-windows-gnu
DOCKERFILE

mkdir -p "$WORK/registry" dist/windows
docker run --rm \
    --volume "$PWD:/src:ro" \
    --volume "$PWD/$WORK:/target" \
    --volume "$PWD/$WORK/registry:/usr/local/cargo/registry" \
    --env CARGO_TARGET_DIR=/target \
    --env CARGO_BUILD_JOBS="$JOBS" \
    --workdir /src \
    "$IMAGE" \
    cargo build --release --locked --target "$TARGET" --bin app-switch-claude --example probe

cp "$WORK/$TARGET/release/app-switch-claude.exe" dist/windows/AppSwitchClaude.exe
cp "$WORK/$TARGET/release/examples/probe.exe" dist/windows/AppSwitchClaude-probe.exe
echo "Built dist/windows/AppSwitchClaude.exe and dist/windows/AppSwitchClaude-probe.exe"
