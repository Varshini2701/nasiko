#!/usr/bin/env bash
# Copies this crate's source into a dependent agent's own directory, at
# vendor/coding-policy. Needed because `nasiko push`/`deploy` runs `docker
# build` with the agent's own directory as the ONLY build context (see
# oss/cli/src/commands/build.rs) — a Cargo path dependency reaching outside
# that directory (e.g. `../coding-policy`) compiles fine on the host but
# can't resolve inside the container, since nothing outside the agent's
# directory is ever COPYed in.
#
# So each dependent agent keeps a real, committed copy under its own
# vendor/coding-policy — same reasoning as these crates already pinning
# dependency versions literally instead of `workspace = true`: buildable in
# isolation, from just that one directory, matching the existing
# oss/agents/<agent> convention.
#
# Usage: run this after editing oss/coding-policy/src/lib.rs, once
# per dependent agent:
#   oss/coding-policy/sync-vendor.sh oss/agents/coding
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <path-to-dependent-agent-directory>" >&2
  exit 1
fi

src_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target_dir="$1/vendor/coding-policy"

rm -rf "$target_dir"
mkdir -p "$target_dir"
# Drop the `[workspace]` table: the canonical crate declares it so it's
# buildable standalone, but a vendored copy is nested INSIDE the dependent
# agent's own workspace root — Cargo errors on "multiple workspace roots
# found in the same workspace" if both declare one.
grep -v '^\[workspace\]$' "$src_dir/Cargo.toml" > "$target_dir/Cargo.toml"
cp -r "$src_dir/src" "$target_dir/"

echo "synced $src_dir -> $target_dir"
