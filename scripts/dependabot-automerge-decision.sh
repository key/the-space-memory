#!/usr/bin/env bash
# Decide whether a Dependabot pull request may be merged unattended.
#
# Why: `.github/workflows/dependabot-auto-merge.yml` merges to main with no
# human in the loop, so the "which bumps qualify" rule must be one auditable,
# testable place rather than an expression buried in YAML. This script is pure:
# it reads only the head branch and the PR title, touches no network, and never
# merges anything itself.
#
# Policy:
#   github_actions — always eligible. A broken action bump shows up as a red
#                    check, and the merge is gated on every check being green.
#   cargo          — eligible only while the bump stays inside Cargo's caret
#                    compatibility range, i.e. the leading non-zero component
#                    is unchanged (^1.2.3 allows <2.0.0, ^0.11.14 allows
#                    <0.12.0, ^0.0.3 allows nothing else). A range-crossing
#                    bump can break the API even when it still compiles, so a
#                    human reviews it.
#   anything else  — held.
#
# Usage: dependabot-automerge-decision.sh <head-branch> <pr-title>
# Prints `merge` or `hold` on stdout and the reason on stderr; exits 0 for a
# verdict, 2 for a usage error.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: $(basename "$0") <head-branch> <pr-title>" >&2
    exit 2
fi

BRANCH="$1"
TITLE="$2"

verdict() {
    printf '%s\n' "$1"
    printf '%s\n' "$2" >&2
    exit 0
}

# compat_key <version> — print the identifier of the Cargo compatibility range
# the version belongs to. Two versions are caret-compatible when their keys
# match. Fails (exit 1) for anything that is not a plain numeric version, which
# includes pre-releases: their ordering rules are subtler than the caret rule,
# so they are held rather than approximated.
compat_key() {
    local version="${1%%+*}"  # build metadata is not part of the range
    if [[ ! "$version" =~ ^([0-9]+)(\.([0-9]+))?(\.([0-9]+))?$ ]]; then
        return 1
    fi
    local major="${BASH_REMATCH[1]}"
    local minor="${BASH_REMATCH[3]:-0}"
    local patch="${BASH_REMATCH[5]:-0}"

    if [[ "$major" -ne 0 ]]; then
        printf '%s' "$major"
    elif [[ "$minor" -ne 0 ]]; then
        printf '0.%s' "$minor"
    else
        printf '0.0.%s' "$patch"
    fi
}

if [[ "$BRANCH" != dependabot/* ]]; then
    verdict hold "not a Dependabot branch: $BRANCH"
fi

# Dependabot branches are `dependabot/<ecosystem>/<dependency>-<version>`.
rest="${BRANCH#dependabot/}"
ecosystem="${rest%%/*}"

case "$ecosystem" in
    github_actions)
        verdict merge "github_actions bumps are eligible regardless of version"
        ;;
    cargo) ;;
    *)
        verdict hold "ecosystem not enabled for auto-merge: ${ecosystem:-<none>}"
        ;;
esac

# Dependabot titles a single-dependency bump `bump <name> from <old> to <new>`.
# Grouped updates ("bump the <group> group with N updates") carry no single
# pair and fall through to the hold below.
if [[ ! "$TITLE" =~ bump[[:space:]]+[^[:space:]]+[[:space:]]+from[[:space:]]+([^[:space:]]+)[[:space:]]+to[[:space:]]+([^[:space:]]+) ]]; then
    verdict hold "cannot read a single from/to version pair from the title"
fi
old_version="${BASH_REMATCH[1]}"
new_version="${BASH_REMATCH[2]}"

if ! old_key="$(compat_key "$old_version")"; then
    verdict hold "unsupported version format: $old_version"
fi
if ! new_key="$(compat_key "$new_version")"; then
    verdict hold "unsupported version format: $new_version"
fi

if [[ "$old_key" != "$new_key" ]]; then
    verdict hold "leaves the ^$old_version compatibility range: $old_version -> $new_version"
fi

verdict merge "stays within the ^$old_version compatibility range: $old_version -> $new_version"
