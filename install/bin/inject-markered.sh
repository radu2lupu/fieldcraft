#!/usr/bin/env bash
# Inject (or replace) a markered block in a target file. Idempotent.
#
# Usage: inject-markered.sh <source-file> <target-file>
#
# The source-file must start with `<!-- fieldcraft:start -->` and end with
# `<!-- fieldcraft:end -->`. If the target-file already contains the same
# markers, the block between them is replaced. Otherwise the source-file
# content is appended (with a leading blank line).

set -e

source_file="$1"
target_file="$2"

if [[ -z "$source_file" || -z "$target_file" ]]; then
  echo "Usage: inject-markered.sh <source-file> <target-file>" >&2
  exit 1
fi

if [[ ! -f "$source_file" ]]; then
  echo "ERROR: source file does not exist: $source_file" >&2
  exit 1
fi

mkdir -p "$(dirname "$target_file")"
[[ -f "$target_file" ]] || touch "$target_file"

START="<!-- fieldcraft:start -->"
END="<!-- fieldcraft:end -->"

if grep -qF "$START" "$target_file" && grep -qF "$END" "$target_file"; then
  # Replace existing block.
  tmp="$(mktemp)"
  awk -v start="$START" -v end="$END" -v srcfile="$source_file" '
    BEGIN { in_block = 0; injected = 0 }
    {
      if (index($0, start) > 0) {
        in_block = 1
        if (!injected) {
          while ((getline line < srcfile) > 0) print line
          close(srcfile)
          injected = 1
        }
        next
      }
      if (in_block && index($0, end) > 0) {
        in_block = 0
        next
      }
      if (!in_block) print
    }
  ' "$target_file" > "$tmp"
  mv "$tmp" "$target_file"
  echo "  updated fieldcraft block in $target_file"
else
  # Append fresh.
  if [[ -s "$target_file" ]]; then
    echo "" >> "$target_file"
  fi
  cat "$source_file" >> "$target_file"
  echo "  appended fieldcraft block to $target_file"
fi
