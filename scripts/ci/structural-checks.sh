#!/usr/bin/env bash
# Source-tree checks: `mod.rs` placement, review-marker and external-reference
# comments, and decorative separators. Every violation is printed as
# `path:line:text`, and the script exits non-zero when there is any.
set -Eeuo pipefail

cd "$(git rev-parse --show-toplevel)"

# Writes every matching line as `path:line:text` to stdout.
# Exit status:
#   0 = at least one match
#   1 = no matches
#  >1 = search tool error
match_rust_sources() {
  local pattern="$1"
  shift
  if command -v rg >/dev/null 2>&1; then
    rg -n --no-heading -e "$pattern" "$@"
  else
    grep -En -e "$pattern" "$@"
  fi
}

report_structural_matches() {
  local label="$1"
  local pattern="$2"
  shift 2

  local matches status=0
  matches="$(match_rust_sources "$pattern" "$@" 2>&1)" || status=$?

  case "$status" in
    0)
      printf '%s: %d violation(s)\n%s\n' \
        "$label" "$(wc -l <<<"$matches")" "$matches" >&2
      ;;
    1) ;;
    *)
      printf '%s search failed (exit=%d).\n%s\n' "$label" "$status" "$matches" >&2
      ;;
  esac
  return "$status"
}

main() {
  # Untracked files are listed as well, so a file is checked before it is ever
  # staged. A tracked file deleted from the working tree is left out: it has
  # nothing left to check, and the search would fail on it. The `./` prefix
  # keeps each path in the form the `mod.rs` rule below is written against,
  # which a crate at the repository root needs.
  local rust_files=()
  local path
  while IFS= read -r -d '' path; do
    if [[ -f "$path" ]]; then
      rust_files+=("./$path")
    fi
  done < <(git ls-files -z --cached --others --exclude-standard -- '*.rs')

  if ((${#rust_files[@]} == 0)); then
    echo "No Rust source files were found." >&2
    exit 1
  fi

  # A `mod.rs` under a crate's `tests/` or `benches/` directory is left alone:
  # a file directly under those is compiled as a binary of its own, so a
  # helper module shared between them has to live in a subdirectory. The
  # `no_mod_rs` dylint draws the same line. A `src/tests/mod.rs` is an
  # ordinary module and is still reported.
  local mod_rs_files=()
  local f
  for f in "${rust_files[@]}"; do
    if [[ "$(basename "$f")" == "mod.rs" ]] \
      && ! [[ "$f" =~ /(tests|benches)/ && ! "$f" =~ /src/(tests|benches)/ ]]; then
      mod_rs_files+=("$f")
    fi
  done

  local violations=0

  if ((${#mod_rs_files[@]} > 0)); then
    printf 'no_mod_rs: %d violation(s)\n' "${#mod_rs_files[@]}" >&2
    printf '%s:1:mod.rs is not allowed here\n' "${mod_rs_files[@]}" >&2
    violations=1
  fi

  local ai_marker_pattern
  ai_marker_pattern='^[[:space:]]*//[[:space:]]*(REVIEW:|TODO\(review\):|FIXME\(review\):|NOTE\(ai\):|AI:)'

  local external_reference_pattern
  external_reference_pattern='§'
  external_reference_pattern+='|order\.md'
  external_reference_pattern+='|[Ii]nvariants?[[:space:]_-]*#[0-9]'
  external_reference_pattern+='|^[[:space:]]*//[/!]?[[:space:]]*-[[:space:]]*#[0-9]'
  external_reference_pattern+='|[Aa]ppendix'
  external_reference_pattern+='|contract[[:space:]]*#[0-9]'
  external_reference_pattern+='|ARCH-[0-9]'
  external_reference_pattern+='|[Ii]ssue[[:space:]]*#[0-9]'
  external_reference_pattern+='|(^|[^a-zA-Z])[Ss]ection'
  external_reference_pattern+='|[Ss]pecification|SPECIFICATION'
  external_reference_pattern+='|[Tt]he spec[^a-zA-Z]'
  external_reference_pattern+='|`draft`[[:space:]]+step'
  external_reference_pattern+='|documented in the[[:space:]]+(test[[:space:]]+)?report'

  local separator_pattern
  separator_pattern='^[[:space:]]*//[[:space:]]*[-=]{4,}[[:space:]]*$'

  local status

  status=0
  report_structural_matches \
    "AI review-marker comments" \
    "$ai_marker_pattern" \
    "${rust_files[@]}" || status=$?
  if ((status == 0)); then
    violations=1
  elif ((status > 1)); then
    exit "$status"
  fi

  status=0
  report_structural_matches \
    "External document references" \
    "$external_reference_pattern" \
    "${rust_files[@]}" || status=$?
  if ((status == 0)); then
    echo "Comments and messages must state the contract without external references." >&2
    violations=1
  elif ((status > 1)); then
    exit "$status"
  fi

  status=0
  report_structural_matches \
    "Decorative separator comments" \
    "$separator_pattern" \
    "${rust_files[@]}" || status=$?
  if ((status == 0)); then
    violations=1
  elif ((status > 1)); then
    exit "$status"
  fi

  ((violations == 0)) || exit 1
}

main
