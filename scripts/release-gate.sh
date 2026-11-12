#!/usr/bin/env bash
# Whether the Runner release of a commit may go out, by the rule the console rolls Runners out by: a commit of main
# whose push ran CI green, and Image green too when that push made an Image run, the newest run of each counting. Its
# programs are those the push's "Runner build" made, which must be green as well.
#
#   scripts/release-gate.sh <repository API, https://api.github.com/repos/codefusion-cc/runner> <whole commit>
#
# Its one line on standard output, with status 0:
#   publish <run id>   the release may go out, with the programs of that "Runner build" run
#   wait <why>         a run the release waits for has not ended: the run that ends it starts this check again
#   skip <why>         the commit gets no release: it is not on main, or a run it needs ended other than green
# A status other than 0 when GitHub could not say, after host-setup.sh's retries of 403, 408, 429, 5xx and no answer.
# GITHUB_TOKEN, when set, goes with each read.
set -Eeuo pipefail

api=${1:?the repository API}
commit=${2:?the commit}

# github_get, with its retries, from the script hosts run; sourced, it only defines it.
# shellcheck source=scripts/host-setup.sh
source "$(dirname "${BASH_SOURCE[0]}")/host-setup.sh"
trap - ERR EXIT
step=release-gate

CI=ci.yml
IMAGE=image.yml
BUILD=runner-build.yml

if [[ ! $commit =~ ^[0-9a-f]{40}$ ]]; then
  echo "release-gate: $commit is no whole commit" >&2
  exit 2
fi

main=$(main_head "$api")
if [[ -z $main ]]; then
  echo "release-gate: GitHub could not say which commit main is at" >&2
  exit 1
fi
status=$(compare_status "$api" "$commit" "$main")
case $status in
  ahead | identical) ;;
  behind | diverged)
    echo "skip $commit is not on main"
    exit 0
    ;;
  *)
    echo "release-gate: the compare of $commit with main says '$status'" >&2
    exit 1
    ;;
esac

# Every run of the push of the commit to main, of every workflow, in one read.
runs=$(github_get "$api/actions/runs?head_sha=$commit&event=push&branch=main&per_page=100")

# The newest run of the workflow file $1 among them: `<id> <status> <conclusion>`, or nothing for none.
newest_run() {
  jq -r --arg commit "$commit" --arg path ".github/workflows/$1" '
    [.workflow_runs[] | select(.path == $path and .head_sha == $commit and .event == "push" and .head_branch == "main")]
    | if length == 0 then empty else max_by(.run_number, .run_attempt)
      | "\(.id) \(.status) \(.conclusion // "none")" end' <<<"$runs"
}

# `wait` or `skip` for a run that is not green; nothing for a green one. $1 names the workflow, $2 its run, $3 whether
# no run is needed.
judge() {
  local name=$1 run=$2 optional=$3 id state conclusion
  if [[ -z $run ]]; then
    ((optional)) || echo "wait $name has not run for this push"
    return 0
  fi
  read -r id state conclusion <<<"$run"
  if [[ ! $id =~ ^[0-9]+$ ]]; then
    echo "release-gate: $name's run is not as GitHub lists runs" >&2
    return 1
  fi
  if [[ $state != completed ]]; then
    echo "wait $name run $id is $state"
  elif [[ $conclusion != success ]]; then
    echo "skip $name run $id ended $conclusion"
  fi
}

build=''
for workflow in "$CI" "$IMAGE" "$BUILD"; do
  case $workflow in
    "$CI") name=CI optional=0 ;;
    "$IMAGE") name=Image optional=1 ;;
    *) name='Runner build' optional=0 ;;
  esac
  run=$(newest_run "$workflow")
  said=$(judge "$name" "$run" "$optional")
  if [[ -n $said ]]; then
    echo "$said"
    exit 0
  fi
  [[ $workflow != "$BUILD" ]] || build=${run%% *}
done
echo "publish $build"
