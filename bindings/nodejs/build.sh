#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'unsupported_platform: native gate covers Linux x86_64' >&2
  exit 1
fi
# `--locked` keeps the validated Cargo.lock; no dependency resolution here.
cargo build --locked --manifest-path Cargo.toml
# An explicit artifact copy; no runtime sidecar is used.
artifact_dir=$(cargo metadata --locked --format-version 1 --no-deps --manifest-path Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
cp "$artifact_dir/debug/libdenosanbox_nodejs.so" denosanbox.node
