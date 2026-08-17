#!/usr/bin/env bash
# Merge eligible, green Dependabot pull requests.
#
# Called two ways by .github/workflows/dependabot-auto-merge.yml:
#   with a branch  — the fast path, from a completed workflow run
#   with none      — the sweep, from a schedule, re-examining every open
#                    Dependabot pull request
#
# Why both: every "not yet" below returns without merging and relies on a later
# look. The fast path only happens when a `pull_request` workflow finishes, so
# any wait for something else — a base that moved, a scanner that has not
# answered, a mergeability recompute — has nothing to resume it. The sweep is
# that resumption, and it is what lets each individual wait stay simple.
#
# Usage: dependabot-automerge.sh [<head-branch>...]
# Env:   REPO (or GITHUB_REPOSITORY), GH_TOKEN
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DECIDE="$SCRIPT_DIR/dependabot-automerge-decision.sh"
REPO="${REPO:-${GITHUB_REPOSITORY:-}}"

# How long to wait for checks that cannot wake the fast path (see evaluate()).
EXTERNAL_POLL_ATTEMPTS="${EXTERNAL_POLL_ATTEMPTS:-4}"
EXTERNAL_POLL_INTERVAL="${EXTERNAL_POLL_INTERVAL:-15}"
# How long to wait for GitHub to finish computing mergeability.
MERGEABLE_POLL_ATTEMPTS="${MERGEABLE_POLL_ATTEMPTS:-3}"
MERGEABLE_POLL_INTERVAL="${MERGEABLE_POLL_INTERVAL:-5}"

if [[ -z "$REPO" ]]; then
    echo "ERROR: set REPO or GITHUB_REPOSITORY" >&2
    exit 2
fi
if [[ ! -x "$DECIDE" ]]; then
    echo "ERROR: $DECIDE not found or not executable" >&2
    exit 2
fi

note() { echo "::notice::$*"; }
fail() { echo "::error::$*" >&2; }

# evaluate <head-branch> — merge the pull request on that branch if everything
# below holds. Returns 0 for any decision reached, including "not yet"; returns
# 1 only for a state that should not be possible, which is worth a red run.
evaluate() {
    local branch="$1"

    # Pull request text is untrusted: read it from JSON into variables, never
    # interpolate it into a command line.
    local pr_json
    pr_json=$(gh pr list --repo "$REPO" --head "$branch" --state open \
        --json number,title,author,headRefOid,baseRefName --limit 1)
    if [[ $(jq 'length' <<<"$pr_json") -ne 1 ]]; then
        note "no open pull request for $branch"
        return 0
    fi

    local author
    author=$(jq -r '.[0].author.login' <<<"$pr_json")
    if [[ "$author" != "app/dependabot" ]]; then
        note "$branch is not authored by Dependabot (author: $author)"
        return 0
    fi

    local number title head base
    number=$(jq -r '.[0].number' <<<"$pr_json")
    title=$(jq -r '.[0].title' <<<"$pr_json")
    head=$(jq -r '.[0].headRefOid' <<<"$pr_json")
    base=$(jq -r '.[0].baseRefName' <<<"$pr_json")

    # Always judge the pull request's current head. A caller's event may name an
    # older commit; that event is simply stale, not a reason to skip the branch.
    local reason_file verdict reason
    reason_file=$(mktemp)
    verdict=$("$DECIDE" "$branch" "$title" 2>"$reason_file")
    reason=$(<"$reason_file")
    rm -f "$reason_file"
    if [[ "$verdict" != "merge" ]]; then
        note "PR #$number held for review: $reason"
        return 0
    fi

    # Wait only on things whose completion can wake the fast path.
    #
    # `event=pull_request` is exactly that set. A run from any other event
    # cannot re-trigger an evaluation: `pull_request_target` (PR Labeler)
    # reports its checks on this commit, but its workflow_run payload carries
    # the base branch and never matches the branch filter in the workflow.
    # `scripts/lint-automerge-triggers.sh` closes the other half of this: it
    # guarantees every pull_request workflow appears in that trigger list, so
    # everything counted here can wake the fast path.
    #
    # Only the runs that exist are inspected, which is what makes the path
    # filters on CI/E2E/Quality/Bench harmless: a workflow that never ran is
    # nothing to wait on.
    local runs
    runs=$(gh api --paginate \
        "repos/$REPO/actions/runs?head_sha=$head&event=pull_request&per_page=100" \
        --jq '.workflow_runs[] | [.name, .status, (.conclusion // "")] | @tsv')
    if [[ -z "$runs" ]]; then
        # The sweep can legitimately see this on a pull request whose runs have
        # been deleted by retention; the fast path cannot, since it only runs
        # because such a workflow completed. Either way there is nothing to
        # merge on, and merging anyway would defeat the point.
        note "PR #$number has no pull_request workflow runs at $head; nothing to merge on"
        return 0
    fi

    local incomplete unsuccessful
    incomplete=$(awk -F'\t' '$2 != "completed" { printf "%s ", $1 }' <<<"$runs")
    if [[ -n "$incomplete" ]]; then
        note "PR #$number still running: $incomplete"
        return 0
    fi
    # `skipped` and `neutral` are how a conditional workflow reports "nothing to
    # do" — they are not failures.
    unsuccessful=$(awk -F'\t' \
        '$3 != "success" && $3 != "skipped" && $3 != "neutral" { printf "%s(%s) ", $1, $3 }' <<<"$runs")
    if [[ -n "$unsuccessful" ]]; then
        note "PR #$number is not green: $unsuccessful"
        return 0
    fi

    # Every gated run is complete by now, and a workflow run completes only once
    # its jobs do — so a check still pending on this commit came from outside
    # that set: PR Labeler, or a third-party app. No classification is needed;
    # being pending here is the definition.
    #
    # Such a check cannot wake the fast path, so it cannot be waited on the way
    # the gated runs are. It must not be ignored either: a policy check that
    # reports after the merge reports too late. Poll briefly, because these
    # checks take seconds and because this holds the shared concurrency queue,
    # then hand the wait to the sweep rather than failing — a red run here would
    # be one nothing ever retries.
    # The Checks API is not the only surface. The older Statuses API is a
    # separate mechanism whose contexts appear in neither `check-runs` nor
    # `actions/runs`, and plenty of third-party tools still post through it, so
    # ignoring it would quietly undo the paragraph above. Fold its contexts into
    # the same three columns and one rule covers both.
    #
    # Read `.statuses[]`, never the combined `.state`: that field reports
    # "pending" for a commit carrying no statuses at all, which would hold every
    # pull request forever. An empty array contributes nothing, which is right.
    local attempts="$EXTERNAL_POLL_ATTEMPTS" checks statuses pending objections
    while true; do
        checks=$(gh api --paginate "repos/$REPO/commits/$head/check-runs" \
            --jq '.check_runs[] | [.name, .status, (.conclusion // "")] | @tsv')
        statuses=$(gh api "repos/$REPO/commits/$head/status" \
            --jq '.statuses[]
                  | [.context,
                     (if .state == "pending" then "in_progress" else "completed" end),
                     .state]
                  | @tsv')
        checks="$checks"$'\n'"$statuses"
        # `NF` skips the blank line left when either query returns nothing;
        # without it an empty field would read as a nameless pending check.
        pending=$(awk -F'\t' 'NF && $2 != "completed" { printf "%s ", $1 }' <<<"$checks")
        if [[ -z "$pending" ]]; then
            break
        fi
        attempts=$((attempts - 1))
        if [[ "$attempts" -le 0 ]]; then
            note "PR #$number waiting on checks outside the trigger set: $pending"
            return 0
        fi
        sleep "$EXTERNAL_POLL_INTERVAL"
    done
    objections=$(awk -F'\t' \
        'NF && $3 != "success" && $3 != "skipped" && $3 != "neutral" { printf "%s(%s) ", $1, $3 }' <<<"$checks")
    if [[ -n "$objections" ]]; then
        note "PR #$number held by a check outside the trigger set: $objections"
        return 0
    fi

    # GitHub computes mergeability asynchronously and reports UNKNOWN until it
    # settles; give it a moment before believing the answer.
    local mergeable=UNKNOWN attempt
    for ((attempt = 0; attempt < MERGEABLE_POLL_ATTEMPTS; attempt++)); do
        mergeable=$(gh pr view "$number" --repo "$REPO" --json mergeable --jq '.mergeable')
        if [[ "$mergeable" != "UNKNOWN" ]]; then
            break
        fi
        sleep "$MERGEABLE_POLL_INTERVAL"
    done
    if [[ "$mergeable" != "MERGEABLE" ]]; then
        note "PR #$number is not mergeable ($mergeable); waiting for Dependabot to rebase"
        return 0
    fi

    # A `pull_request` run tests the merge ref — this head merged into the base
    # as it stood then. Once the base moves, the combination that would land is
    # one nothing ever built, which is what "require branches to be up to date"
    # prevents on repositories that enforce it.
    local behind
    behind=$(gh api "repos/$REPO/compare/$base...$head" --jq '.behind_by')
    if [[ "$behind" -ne 0 ]]; then
        note "PR #$number is $behind commit(s) behind $base, so its checks tested a stale merge"
        return 0
    fi

    # `--match-head-commit` pins the head: what lands is what was inspected.
    # Everything above reads the head at a point in time and then does more I/O,
    # so a rebase in between would otherwise merge a revision nothing checked —
    # and Dependabot rebases exactly when a sibling lands, which this script
    # causes. GitHub rejects the merge on mismatch.
    #
    # Nothing pins the *base* that way. The workflow's global concurrency group
    # closes that window against sibling auto-merges, the only concurrent writer
    # here; a human merging in the same second is not closable without a merge
    # queue, which this repository has deliberately not adopted. If that ever
    # lands an untested combination, main's push-triggered CI goes red on it.
    #
    # The branch is removed by the repository's delete-branch-on-merge setting,
    # and there is no review requirement to approve away.
    if gh pr merge "$number" --repo "$REPO" --squash --match-head-commit "$head"; then
        note "merged PR #$number: $reason"
        return 0
    fi

    # Tell the expected rejection from a real failure by re-reading the head
    # rather than by matching on GitHub's error text.
    local current
    current=$(gh pr view "$number" --repo "$REPO" --json headRefOid --jq '.headRefOid')
    if [[ "$current" != "$head" ]]; then
        note "PR #$number moved to $current mid-merge; waiting for its new checks"
        return 0
    fi
    fail "merging PR #$number failed while its head was still $head"
    return 1
}

branches=""
if [[ $# -gt 0 ]]; then
    for arg in "$@"; do
        branches="$branches$arg"$'\n'
    done
else
    branches=$(gh pr list --repo "$REPO" --state open --limit 100 \
        --json headRefName,author \
        --jq '.[] | select(.author.login == "app/dependabot") | .headRefName')
fi

status=0
while IFS= read -r head_branch; do
    [[ -z "$head_branch" ]] && continue
    # One unmergeable pull request must not stop the sweep from reaching the
    # rest; collect the failure and carry on.
    evaluate "$head_branch" || status=1
done <<EOF
$branches
EOF

exit "$status"
