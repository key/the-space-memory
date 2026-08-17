#!/usr/bin/env bash
# Keep the Dependabot auto-merge trigger list in sync with the workflows that
# can attach a check to a Dependabot pull request.
#
# Why: dependabot-auto-merge.yml waits for every `pull_request`-triggered
# workflow run on the head commit, and it re-evaluates only when a workflow in
# its `workflows:` trigger list finishes. Those two sets must be the same one.
# A workflow that is waited on but not listed can never resume the evaluation
# it is blocking — whichever listed workflow finished last already looked, saw
# it still running, and exited 0. The pull request goes green and sits there
# forever while every auto-merge run reports success. Nothing in GitHub couples
# the two, so this check does.
#
# The parse is deliberately strict: anything it cannot read is an error, never
# a pass, because a false green here is exactly the failure being prevented.
#
# Usage: lint-automerge-triggers.sh [workflow-dir]
# Exits 0 when in sync, 1 on a mismatch, 2 when the input cannot be parsed.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW_DIR="${1:-$REPO_ROOT/.github/workflows}"
AUTOMERGE="$WORKFLOW_DIR/dependabot-auto-merge.yml"

if [[ ! -d "$WORKFLOW_DIR" ]]; then
    echo "ERROR: workflow dir not found at $WORKFLOW_DIR" >&2
    exit 2
fi
if [[ ! -f "$AUTOMERGE" ]]; then
    echo "ERROR: auto-merge workflow not found at $AUTOMERGE" >&2
    exit 2
fi

# in_list <needle> <newline-delimited haystack>
in_list() {
    printf '%s\n' "$2" | grep -Fxq -- "$1"
}

# The trigger list is a single-line flow sequence: `workflows: [CI, Lint, ...]`.
listed_line="$(grep -E '^[[:space:]]+workflows: \[' "$AUTOMERGE" || true)"
if [[ -z "$listed_line" ]]; then
    echo "ERROR: no 'workflows: [...]' trigger list found in $AUTOMERGE" >&2
    exit 2
fi
listed="$(printf '%s\n' "$listed_line" \
    | sed -E 's/^[^[]*\[//; s/\].*$//' \
    | tr ',' '\n' \
    | sed -E 's/^[[:space:]]*//; s/[[:space:]]*$//' \
    | grep -v '^$' || true)"
if [[ -z "$listed" ]]; then
    echo "ERROR: the 'workflows:' trigger list in $AUTOMERGE is empty" >&2
    exit 2
fi

# A workflow reachable through `pull_request` reports its checks on the pull
# request head and fires a `workflow_run` carrying the pull request branch, so
# it must be listed. One reachable only through `pull_request_target` runs in
# the base context: its `workflow_run` payload carries the base branch and
# would never match the auto-merge branch filter, so listing it would be a lie.
required=""
target_only=""
known=""

for file in "$WORKFLOW_DIR"/*.yml; do
    [[ "$file" == "$AUTOMERGE" ]] && continue

    name_line="$(grep -m1 '^name:' "$file" || true)"
    if [[ -z "$name_line" ]]; then
        echo "ERROR: $file has no top-level 'name:'" >&2
        exit 2
    fi
    name="${name_line#name:}"
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%\"}"; name="${name#\"}"
    name="${name%\'}"; name="${name#\'}"
    known="$known$name"$'\n'

    if grep -qE '^  pull_request:[[:space:]]*$' "$file"; then
        required="$required$name"$'\n'
    elif grep -qE '^  pull_request_target:[[:space:]]*$' "$file"; then
        target_only="$target_only$name"$'\n'
    fi
done

status=0

while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    if ! in_list "$name" "$listed"; then
        echo "ERROR: workflow '$name' runs on pull_request but is missing from the" >&2
        echo "       auto-merge trigger list; its completion could never resume an" >&2
        echo "       evaluation that is waiting on its check." >&2
        status=1
    fi
done <<EOF
$required
EOF

while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    if in_list "$name" "$listed"; then
        echo "ERROR: workflow '$name' runs only on pull_request_target, so its" >&2
        echo "       workflow_run payload carries the base branch and can never" >&2
        echo "       match the auto-merge branch filter. Remove it from the list." >&2
        status=1
    fi
done <<EOF
$target_only
EOF

while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    if ! in_list "$name" "$known"; then
        echo "ERROR: the auto-merge trigger list names '$name', which is not the" >&2
        echo "       name of any workflow. A typo here silently disables a trigger." >&2
        status=1
    fi
done <<EOF
$listed
EOF

if [[ "$status" -ne 0 ]]; then
    echo "" >&2
    echo "Fix the 'workflows:' list in $AUTOMERGE." >&2
    exit 1
fi

echo "OK: auto-merge trigger list covers every pull_request workflow"
