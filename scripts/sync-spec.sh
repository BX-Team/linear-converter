#!/usr/bin/env bash
# Copies the Linear format spec, dictionaries and golden files from a DivineMC checkout into spec/.
set -euo pipefail
src="${1:?usage: sync-spec.sh <DivineMC checkout>}/linear-format"
dst="$(cd "$(dirname "$0")/.." && pwd)/spec"
[ -f "$src/SPEC.md" ] || { echo "no linear-format/SPEC.md under $1" >&2; exit 1; }
rm -rf "$dst"
mkdir -p "$dst"
cp -r "$src/SPEC.md" "$src/dicts" "$src/golden" "$dst/"
echo "synced $(find "$dst" -type f | wc -l) files from $src"
