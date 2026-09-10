#!/bin/bash
# Compares the wall-clock time of the test jobs of the `Test` workflow between a pull request run
# and the newest successful run on the base branch, then keeps one comment on the pull request up
# to date with the result.
#
# Required environment:
#   REPO      - repository in "owner/name" form.
#   RUN_ID    - identifier of the `Test` workflow run of the pull request.
#   GH_TOKEN  - token with `actions: read` and `pull-requests: write` scope.
#
# Optional environment:
#   DRY_RUN   - when set to "1", write the comment to stdout and do not call the comments API.
#   PR_NUMBER - number of the pull request. Without it, the script resolves the pull request from
#               the run.
set -euo pipefail

: "${REPO:?REPO is required}"
: "${RUN_ID:?RUN_ID is required}"
DRY_RUN="${DRY_RUN:-0}"
pr_number="${PR_NUMBER:-}"

# Hidden anchor that identifies the comment this script owns.
MARKER="<!-- ci-test-timings -->"

# Jobs to compare. The remaining jobs of the workflow build shared artifacts, so their time
# depends on cache hits instead of on the change under review.
JOB_NAMES='["Unit tests","Integration tests","AggLayer tests","miden-bench smoke tests"]'

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

gh api "/repos/${REPO}/actions/runs/${RUN_ID}" > "${workdir}/run.json"

head_sha="$(jq -r '.head_sha' "${workdir}/run.json")"
workflow_id="$(jq -r '.workflow_id' "${workdir}/run.json")"

if [ -z "${pr_number}" ]; then
    pr_number="$(jq -r '.pull_requests[0].number // empty' "${workdir}/run.json")"
fi

# A run that a fork pull request starts carries no `pull_requests` entry, so resolve the pull
# request from the head commit instead.
if [ -z "${pr_number}" ]; then
    pr_number="$(gh api "/repos/${REPO}/commits/${head_sha}/pulls" --jq '.[0].number // empty')"
fi

if [ -z "${pr_number}" ]; then
    echo "Run ${RUN_ID} belongs to no pull request. Nothing to comment." >&2
    exit 0
fi

base_ref="$(gh api "/repos/${REPO}/pulls/${pr_number}" --jq '.base.ref')"

# Identifier of the newest successful run of the same workflow that a push to $1 produced.
newest_green_run() {
    gh api -X GET "/repos/${REPO}/actions/workflows/${workflow_id}/runs" \
        -f "branch=$1" -f event=push -f status=success -f per_page=1 \
        --jq '.workflow_runs[0].id // empty'
}

baseline_ref="${base_ref}"
base_run_id="$(newest_green_run "${baseline_ref}")"

# A pull request stacked on another feature branch has no successful push run on its base, so
# compare against the default branch instead.
if [ -z "${base_run_id}" ]; then
    baseline_ref="$(gh api "/repos/${REPO}" --jq '.default_branch')"
    if [ "${baseline_ref}" != "${base_ref}" ]; then
        base_run_id="$(newest_green_run "${baseline_ref}")"
    fi
fi

gh api -X GET "/repos/${REPO}/actions/runs/${RUN_ID}/jobs" -f per_page=100 \
    > "${workdir}/pr-jobs.json"

if [ -n "${base_run_id}" ]; then
    gh api -X GET "/repos/${REPO}/actions/runs/${base_run_id}/jobs" -f per_page=100 \
        > "${workdir}/base-jobs.json"
    gh api "/repos/${REPO}/actions/runs/${base_run_id}" > "${workdir}/base-run.json"
    base_sha="$(jq -r '.head_sha' "${workdir}/base-run.json")"
    # The date makes a stale baseline visible. A branch with no recent push has an old newest
    # successful run, and its times say little about the change under review.
    base_date="$(jq -r '.created_at | .[0:10]' "${workdir}/base-run.json")"
    base_link="[\`${base_sha:0:7}\`](https://github.com/${REPO}/actions/runs/${base_run_id}) of ${base_date}"
else
    echo '{"jobs":[]}' > "${workdir}/base-jobs.json"
    base_link="none found"
fi

read -r -d '' JQ_PROGRAM <<'JQ' || true
# Seconds a job spent running. Queue time is not part of it because `started_at` marks the moment
# the runner picked the job up.
def duration:
    if (.started_at != null and .completed_at != null)
    then ((.completed_at | fromdateiso8601) - (.started_at | fromdateiso8601))
    else null
    end;

# Map of job name to the longest attempt of that job, so a matrix or a retry gives one row.
def index_jobs:
    [ .jobs[] | { name: .name, conclusion: .conclusion, secs: duration } ]
    | group_by(.name)
    | map(sort_by(.secs // -1) | last)
    | INDEX(.name);

def clock:
    if . == null then "n/a"
    else . as $s
        | ($s / 60 | floor) as $m
        | ($s - $m * 60) as $r
        | if $m > 0
          then "\($m)m \(if $r < 10 then "0" else "" end)\($r)s"
          else "\($r)s"
          end
    end;

def one_decimal:
    ((. * 10 | round) / 10)
    | tostring
    | if test("\\.") then . else . + ".0" end;

def percent($before; $after):
    if ($before == null or $after == null or $before == 0) then "n/a"
    else (($after - $before) * 100 / $before) as $p
        | (if $p > 0 then "+" else "" end) + ($p | one_decimal) + "%"
    end;

# A job that did not succeed stopped early or was killed, so its time means nothing.
def suspect($row):
    (($row.before | . != null and .conclusion != "success")
     or ($row.after | . != null and .conclusion != "success"));

($base_jobs[0] | index_jobs) as $before
| ($pr_jobs[0] | index_jobs) as $after
| [ $names[] | { name: ., before: $before[.], after: $after[.] } ] as $rows
| [ $rows[] | select(.before.secs != null and .after.secs != null) ] as $paired
| ($paired | map(.before.secs) | add) as $total_before
| ($paired | map(.after.secs) | add) as $total_after
| [ $marker,
    "### CI test timings",
    "",
    "| Test type | `\($base)` | This PR | % diff |",
    "| --- | ---: | ---: | ---: |" ]
  + [ $rows[]
      | "| \(.name)\(if suspect(.) then " \\*" else "" end) | \(.before.secs | clock) | \(.after.secs | clock) | \(percent(.before.secs; .after.secs)) |" ]
  + [ "| **Total** | **\($total_before | clock)** | **\($total_after | clock)** | **\(percent($total_before; $total_after))** |",
      "",
      "Base run: \($base_run) on `\($base)`. This run: [`\($head[0:7])`](\($pr_run)).",
      "Job wall-clock time, queue time excluded. The total adds up only the rows that ran on both sides." ]
  + (if ($rows | map(suspect(.)) | any)
     then [ "A row marked with `*` holds a job that did not succeed, so its time is not comparable." ]
     else [] end)
| join("\n")
JQ

jq -nr \
    --slurpfile pr_jobs "${workdir}/pr-jobs.json" \
    --slurpfile base_jobs "${workdir}/base-jobs.json" \
    --argjson names "${JOB_NAMES}" \
    --arg marker "${MARKER}" \
    --arg base "${baseline_ref}" \
    --arg base_run "${base_link}" \
    --arg head "${head_sha}" \
    --arg pr_run "https://github.com/${REPO}/actions/runs/${RUN_ID}" \
    "${JQ_PROGRAM}" > "${workdir}/comment.md"

if [ "${DRY_RUN}" = "1" ]; then
    cat "${workdir}/comment.md"
    exit 0
fi

# Identifier of the comment this script wrote before, if the pull request already holds one.
gh api --paginate "/repos/${REPO}/issues/${pr_number}/comments" --jq '.[] | { id, body }' \
    > "${workdir}/comments.json"
comment_id="$(jq -rs --arg marker "${MARKER}" \
    '[ .[] | select(.body | startswith($marker)) | .id ] | first // empty' \
    "${workdir}/comments.json")"

jq -n --rawfile body "${workdir}/comment.md" '{ body: $body }' > "${workdir}/payload.json"

if [ -n "${comment_id}" ]; then
    gh api -X PATCH "/repos/${REPO}/issues/comments/${comment_id}" \
        --input "${workdir}/payload.json" --silent
    echo "Updated comment ${comment_id} on pull request #${pr_number}."
else
    gh api -X POST "/repos/${REPO}/issues/${pr_number}/comments" \
        --input "${workdir}/payload.json" --silent
    echo "Created a timings comment on pull request #${pr_number}."
fi
