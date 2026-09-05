#!/usr/bin/env bash
# Fails if `daemon/tests/` gains (or loses) a `target_os` gate without the
# allowlist being updated to match.
#
# Why this exists. A `#[cfg(target_os = "...")]` on a test silently removes
# coverage from a first-class platform, and this repo has twice shipped one
# whose stated reason did not survive contact with real hardware:
#
#   - #25: three fixtures gated off macOS "pending measurement on real
#     hardware" turned out to be one wrong queue bound (#29/#32), wrong on
#     Linux too and merely invisible there.
#   - #36: `reconnect_storm.rs` shipped Linux-only citing those same three
#     fixtures as precedent -- four commits after that precedent was
#     overturned. It passed 20/20 on macOS the first time anyone ran it.
#
# Note what would NOT have caught either one: a check demanding a rationale
# or an issue link near the gate. Both had a rationale, and both cited real
# documents. What was missing was a deliberate, reviewable decision at the
# moment of gating. So the rule is an allowlist -- adding a gate means
# editing `scripts/allowed-target-os-gates.tsv` in the same PR, where a
# reviewer sees it as a line of its own rather than as one attribute buried
# in a diff.
#
# `cfg(unix)` / `cfg(windows)` are deliberately NOT checked: those are the
# portable idiom for "this drives /bin/sh" or "this drives ConPTY", not a
# platform being dropped. Only `target_os` narrows to a single OS.
#
# Scope is `daemon/tests/` only. Product code under `daemon/src/` may branch
# on `target_os` freely -- that is real platform logic, not absent coverage.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tests_dir="$repo_root/daemon/tests"
allowlist="$repo_root/scripts/allowed-target-os-gates.tsv"

if [[ ! -f "$allowlist" ]]; then
  echo "error: allowlist not found at $allowlist" >&2
  exit 1
fi

actual="$(mktemp)"
expected="$(mktemp)"
trap 'rm -f "$actual" "$expected"' EXIT

# Every non-comment line mentioning `target_os`, counted per (file, attribute).
#
# The comment skip is load-bearing and not hypothetical: `shutdown_endpoint.rs`
# documents its own *absence* of a gate with the literal text
# "No `#![cfg(unix)]` gate here", and a naive grep reads that as a gate.
find "$tests_dir" -name '*.rs' | sort | while read -r file; do
  awk -v rel="${file#"$repo_root"/}" '
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (line ~ /^\/\//) next
      if (line !~ /target_os/) next
      count[rel "\t" line]++
    }
    END { for (key in count) print count[key] "\t" key }
  ' "$file"
done | sort -t$'\t' -k2,3 > "$actual"

# Allowlist: count, file, attribute, tracking, why. Only the first three
# participate in the comparison; the last two are for the human reading it.
grep -v '^[[:space:]]*#' "$allowlist" | grep -v '^[[:space:]]*$' \
  | awk -F'\t' 'NF >= 3 { print $1 "\t" $2 "\t" $3 }' \
  | sort -t$'\t' -k2,3 > "$expected"

if diff -u "$expected" "$actual" > /dev/null; then
  # Sum the counts, not the rows -- a single row can allowlist two gates.
  gates="$(awk -F'\t' '{ n += $1 } END { print n + 0 }' "$actual")"
  echo "target_os gate check: ok ($gates allowlisted gate(s) in daemon/tests/)"
  exit 0
fi

echo "error: the target_os gates in daemon/tests/ do not match the allowlist." >&2
echo >&2
echo "  -- expected (scripts/allowed-target-os-gates.tsv)" >&2
echo "  ++ actual   (daemon/tests/)" >&2
echo >&2
diff -u "$expected" "$actual" | tail -n +3 >&2
echo >&2
cat >&2 <<'MSG'
If you ADDED a gate: say so in scripts/allowed-target-os-gates.tsv, with the
issue that tracks removing it again. A gate is a platform losing coverage, so
it wants a tracking issue and a reason that is measured rather than assumed --
this repo has shipped two whose reasons did not survive being run (see the
header of scripts/check-target-os-gates.sh). If you have not actually run the
test on the platform you are gating it off, that is the cheaper next step.

If you REMOVED a gate: delete its row. A stale row is how an un-gating loses
the record of why the gate was there.
MSG
exit 1
