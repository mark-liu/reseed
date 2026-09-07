#!/usr/bin/env sh
# Build the shipped binary without the build machine's absolute paths in it.
#
# rustc compiles the path of every source file it touches into panic metadata,
# including the dependency sources under $CARGO_HOME, so a plain `cargo build
# --release` ships the builder's home directory to whoever runs `strings` on the
# result. Measured on macOS 2026-09-07: 147 such paths before, 0 after.
set -eu
RUSTFLAGS="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/src${RUSTFLAGS:+ $RUSTFLAGS}" \
  exec cargo build --release "$@"
