#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
  echo 'unsupported_platform: native gate covers Linux x86_64' >&2
  exit 1
fi
cargo build --manifest-path Cargo.toml
# An explicit artifact copy; no runtime sidecar is used.
artifact_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
cp "$artifact_dir/debug/libdenosanbox_nodejs.so" denosanbox.node
