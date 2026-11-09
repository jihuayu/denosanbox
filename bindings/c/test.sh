#!/bin/sh
set -eu
repository=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
cargo build --locked --manifest-path "$repository/bindings/c/Cargo.toml"
artifact_dir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$repository/bindings/c/Cargo.toml" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"] + "/debug")')
task_tmpdir=$(mktemp -d)
trap 'rm -rf -- "$task_tmpdir"' EXIT HUP INT TERM
for example in two_sandboxes abi_edge; do
  cc -std=c11 -Wall -Wextra -Werror -I"$repository/bindings/c/include" \
    "$repository/examples/c/$example.c" -L"$artifact_dir" -ldenosanbox_c \
    -Wl,-rpath,"$artifact_dir" -o "$task_tmpdir/$example"
  "$task_tmpdir/$example"
done
export CGO_LDFLAGS="${CGO_LDFLAGS:+$CGO_LDFLAGS }-L$artifact_dir"
export LD_LIBRARY_PATH="$artifact_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
(cd "$repository/bindings/go" && go test -race -count=1 ./...)
(cd "$repository/examples/go" && go run .)
