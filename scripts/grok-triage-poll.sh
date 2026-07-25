#!/usr/bin/env bash
# grok-triage-poll.sh -- host-side worker for tier 2 of the auto-fix loop.
#
# Tier 1 (.github/workflows/auto-fix.yml) applies the closed list of mechanical
# fixes inside CI. Tier 2 (.github/workflows/ci-triage.yml) diagnoses everything
# else and labels the PR `autofix:needs-grok`. THIS script is what acts on that
# label: it runs the grok CLI against a checkout of the PR branch, lets it review,
# test and patch, and pushes the result back to the PR's own head branch.
#
# ---------------------------------------------------------------------------
# WHY THIS IS A HOST POLLER AND NOT A WORKFLOW STEP
#
# A GitHub Actions job on this fleet runs inside a rootless podman container. The
# grok CLI and its credentials live on the HOST at /root/.grok. A workflow step
# therefore CANNOT invoke grok -- and the obvious fix, bind-mounting /root/.grok
# into the runner container, would put a model credential inside the same container
# that executes pull-request-authored code. That is a credential-exposure path, so
# it is not done.
#
# The split that results is the point:
#   * in CI, with a repo-scoped token: diagnose and label. Never patch.
#   * on the host, with the operator's own credentials: patch. Never merge.
# No agent credential ever enters a container running PR code, and no long-lived
# fleet-wide credential is ever added to a repository secret store.
#
# HONEST LIMITATION: this is a POLLER. There is no push from GitHub to this host,
# so a PR waits up to one poll interval. That is a deliberate trade against opening
# an inbound path to the workstation, and it is not a hidden cost -- say it out
# loud rather than pretending the pipeline is event-driven.
# ---------------------------------------------------------------------------
#
# USAGE
#   bash scripts/grok-triage-poll.sh --repo tzervas/gha-runner-ctl            # dry run
#   bash scripts/grok-triage-poll.sh --repo tzervas/gha-runner-ctl --apply
#   bash scripts/grok-triage-poll.sh --repo A/B --repo C/D --apply --loop 600
#
# Dry run is the DEFAULT. Nothing is cloned, patched or pushed without --apply.
#
# REQUIREMENTS (host)
#   gh, authenticated as a user with push access to the target repos
#   git
#   /root/.grok/bin/grok  (override with GROK_BIN)
#
# WHAT IT WILL NOT DO, EVER
#   * push to main, master, dev, sec or release/**
#   * touch a pull request whose head is a fork
#   * merge, or arm auto-merge -- see MERGING below
#   * report success from a queued or in-progress check
#   * exceed MAX_ATTEMPTS rounds on one pull request
#
# MERGING
#   This script never merges and never enables auto-merge. Most repos in this fleet
#   have rulesets with ZERO required checks; arming auto-merge there merges
#   unverified work while looking green. That hole existed on 35 repos and was
#   closed deliberately. Green-and-gated, then a human merges. Never the reverse.

set -euo pipefail

GROK_BIN="${GROK_BIN:-/root/.grok/bin/grok}"
WORKDIR="${WORKDIR:-${TMPDIR:-/tmp}/grok-triage}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-3}"          # keep in step with ci-triage.yml
LABEL_QUEUE='autofix:needs-grok'
LABEL_HUMAN='autofix:needs-human'
MARKER='<!-- autofix-report -->'
APPLY=0
LOOP_SECS=0
REPOS=()

die()  { printf 'grok-triage-poll: ERROR: %s\n' "$*" >&2; exit 1; }
say()  { printf '\n== %s\n' "$*"; }
note() { printf '   %s\n' "$*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --repo)  [ $# -ge 2 ] || die "--repo needs owner/name"; REPOS+=("$2"); shift 2 ;;
    --apply) APPLY=1; shift ;;
    --loop)  [ $# -ge 2 ] || die "--loop needs seconds"; LOOP_SECS="$2"; shift 2 ;;
    -h|--help) sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ "${#REPOS[@]}" -gt 0 ] || die "at least one --repo owner/name is required"
command -v gh  >/dev/null || die "gh is required"
command -v git >/dev/null || die "git is required"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated on this host"
if [ "$APPLY" = 1 ] && [ ! -x "$GROK_BIN" ]; then
  # UNKNOWN, not empty. Exiting 0 here would look like "no work to do".
  die "grok CLI not found or not executable at $GROK_BIN (set GROK_BIN)"
fi

# --- guards -----------------------------------------------------------------

is_trunk() {
  case "$1" in
    main|master|dev|sec|release/*) return 0 ;;
    *) return 1 ;;
  esac
}

is_sane_branch() {
  case "$1" in
    ''|-*|*' '*|*'..'*|*':'*|*'~'*|*'^'*|*'\'*) return 1 ;;
    *) return 0 ;;
  esac
}

# --- one pull request -------------------------------------------------------

handle_pr() {
  local repo="$1" pr="$2"
  local head_ref head_sha head_repo base_ref state attempts cid body

  # RE-READ LIVE STATE. Never act on the listing snapshot: by the time we get here
  # the PR may have been merged, closed, pushed to, or re-labelled. This fleet has
  # already produced a case where an agent asserted a tag "does not exist" while it
  # did, because it never re-checked after its own attempt was blocked.
  local tsv
  tsv="$(gh api "repos/${repo}/pulls/${pr}" \
           --jq '[.state, .head.ref, .head.sha, .head.repo.full_name, .base.ref] | @tsv')" \
    || { note "PR #$pr: could not read -- skipping"; return 0; }
  IFS="$(printf '\t')" read -r state head_ref head_sha head_repo base_ref <<TSV
$tsv
TSV

  [ "$state" = open ] || { note "PR #$pr: state=$state -- skipping"; return 0; }

  # FORK GUARD. A fork PR's head is attacker-controlled; checking it out on the
  # host and running an agent with host credentials over it is not acceptable.
  if [ "$head_repo" != "$repo" ]; then
    note "PR #$pr: head is fork $head_repo -- refusing"
    return 0
  fi
  # TRUNK GUARD.
  if is_trunk "$head_ref" || ! is_sane_branch "$head_ref"; then
    note "PR #$pr: head '$head_ref' is a trunk or is not a plain branch name -- refusing"
    return 0
  fi

  # ESCALATION BOUND, read from the sticky comment the workflow maintains.
  cid="$(gh api "repos/${repo}/issues/${pr}/comments" --paginate \
          --jq '.[] | select(.body | contains("'"$MARKER"'")) | .id' | head -1 || true)"
  if [ -z "$cid" ]; then
    note "PR #$pr: labelled but has no triage comment -- refusing to guess. Re-run ci-triage."
    return 0
  fi
  body="$(gh api "repos/${repo}/issues/comments/${cid}" --jq .body)"
  attempts="$(printf '%s' "$body" | sed -n 's/.*autofix-state attempts=\([0-9]*\).*/\1/p' | head -1)"
  attempts="${attempts:-1}"
  if [ "$attempts" -gt "$MAX_ATTEMPTS" ]; then
    note "PR #$pr: attempt $attempts exceeds MAX_ATTEMPTS=$MAX_ATTEMPTS -- handing to a human"
    gh api --method DELETE "repos/${repo}/issues/${pr}/labels/${LABEL_QUEUE}" --silent 2>/dev/null || true
    gh api --method POST   "repos/${repo}/issues/${pr}/labels" -f "labels[]=${LABEL_HUMAN}" --silent || true
    return 0
  fi

  say "PR ${repo}#${pr}  branch=${head_ref}  head=${head_sha}  attempt=${attempts}/${MAX_ATTEMPTS}"

  if [ "$APPLY" != 1 ]; then
    note "dry run -- would run grok here. Re-run with --apply."
    return 0
  fi

  # --- workspace ------------------------------------------------------------
  local wt="${WORKDIR}/${repo//\//_}-pr${pr}"
  rm -rf "$wt"
  mkdir -p "$(dirname "$wt")"
  # Shallow-ish clone of the PR branch only. gh clone uses the host credential;
  # the token is never written into the remote URL by this script.
  git clone --quiet --branch "$head_ref" --depth 50 \
    "https://github.com/${repo}.git" "$wt" || { note "clone failed"; return 0; }
  git -C "$wt" fetch --quiet --depth 50 origin "$base_ref" || true

  local actual
  actual="$(git -C "$wt" rev-parse HEAD)"
  if [ "$actual" != "$head_sha" ]; then
    note "branch moved ${head_sha} -> ${actual} while we were starting -- skipping this round"
    return 0
  fi

  # --- gate command ---------------------------------------------------------
  local gate='(no repo-local gate script found; use the language toolchain directly)'
  [ -f "$wt/scripts/local-ci.sh" ] && gate='bash scripts/local-ci.sh'

  # --- prompt ---------------------------------------------------------------
  # NON-DETERMINISM GUARD: grok is required to state why the PREVIOUS attempt
  # failed and to build on the branch, not re-solve from scratch. Without that a
  # second attempt produces a differently-wrong patch and a noisy branch.
  local prompt_file="${wt}/.grok-task.md"
  {
    printf 'You are fixing CI on an existing pull request. Work ONLY in this checkout.\n\n'
    printf '## Repository\n%s, pull request #%s, branch `%s` (base `%s`), attempt %s of %s.\n\n' \
      "$repo" "$pr" "$head_ref" "$base_ref" "$attempts" "$MAX_ATTEMPTS"
    printf '## The current CI diagnosis (regenerated from the LATEST failing run)\n\n'
    printf '%s\n\n' "$body"
    printf '## Rules -- these are not suggestions\n\n'
    printf '1. BUILD ON THIS BRANCH. Do not revert it and re-solve from scratch. If an\n'
    printf '   earlier automated attempt is in the history, FIRST state in your commit\n'
    printf '   message why that attempt failed, then change only what that reason implies.\n'
    printf '2. RE-READ LIVE STATE before concluding anything. Do not assert that a file,\n'
    printf '   tag or check does not exist without looking again.\n'
    printf '3. Run the gate and make it pass locally before you stop:  %s\n' "$gate"
    printf '4. DO NOT weaken, skip, xfail or delete a test to make the gate pass. If the\n'
    printf '   test is correct and the code is wrong, fix the code. If the test itself is\n'
    printf '   wrong, say so in the commit message and explain why.\n'
    printf '5. DO NOT edit anything under .github/ -- workflows, permissions, actions.\n'
    printf '6. DO NOT bump any version, and do not touch VERSION, CHANGELOG.md or the\n'
    printf '   commitizen config. These repos stay 0.x.x; releases are a human decision.\n'
    printf '7. DO NOT upgrade dependencies to fix a failure unless the diagnosis above\n'
    printf '   names the dependency as the cause. Never a major bump.\n'
    printf '8. DO NOT merge, and do not enable auto-merge. Pushing is handled outside\n'
    printf '   this session; a human decides when this lands.\n'
    printf '9. Update the documentation for exactly what you changed, in this same change.\n'
    printf '10. If the diagnosis says the failure ALSO reproduces on the base branch, the\n'
    printf '    defect is probably not this PR. Say so and change nothing rather than\n'
    printf '    inventing a local workaround.\n'
    printf '11. If you cannot fix this safely, STOP and leave the tree clean. Handing back\n'
    printf '    a red PR with an explanation is a correct outcome. A speculative patch is\n'
    printf '    not.\n\n'
    printf '## Commit\n'
    printf 'Conventional commits. Include the trailer line `Auto-Fix-Bot: grok` in the\n'
    printf 'commit message so the automation can recognise its own work.\n'
  } > "$prompt_file"

  say "running grok (cwd=$wt)"
  local grok_rc=0
  ( cd "$wt" && "$GROK_BIN" -p "$(cat "$prompt_file")" --always-approve --cwd "$wt" ) || grok_rc=$?
  note "grok exit=$grok_rc"

  rm -f "$prompt_file"
  git -C "$wt" add -A -- ':!.grok-task.md' >/dev/null 2>&1 || true

  if git -C "$wt" diff --cached --quiet && git -C "$wt" diff --quiet; then
    note "grok produced no change -- nothing to push"
    post_update "$repo" "$pr" "$cid" "$body" \
      "grok ran (exit ${grok_rc}) and produced **no change**. That is a legitimate outcome: it means no safe patch was found. A human should take this."
    gh api --method DELETE "repos/${repo}/issues/${pr}/labels/${LABEL_QUEUE}" --silent 2>/dev/null || true
    gh api --method POST   "repos/${repo}/issues/${pr}/labels" -f "labels[]=${LABEL_HUMAN}" --silent || true
    return 0
  fi

  # SAFETY NET on what grok touched. The prompt says not to edit CI or bump
  # versions; this enforces it rather than trusting it.
  local staged
  staged="$(git -C "$wt" status --porcelain --untracked-files=all | cut -c4-)"
  local bad=0
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
      .github/*|*/.github/*|VERSION|*/VERSION)
        note "REFUSING: grok modified $f"; bad=1 ;;
    esac
  done <<< "$staged"
  if [ "$bad" = 1 ]; then
    note "not pushing; leaving the worktree at $wt for inspection"
    post_update "$repo" "$pr" "$cid" "$body" \
      "grok modified CI configuration or a version file, which is outside what automation may change. **Nothing was pushed.** A human must review; the worktree is at \`${wt}\` on the fleet host."
    gh api --method DELETE "repos/${repo}/issues/${pr}/labels/${LABEL_QUEUE}" --silent 2>/dev/null || true
    gh api --method POST   "repos/${repo}/issues/${pr}/labels" -f "labels[]=${LABEL_HUMAN}" --silent || true
    return 0
  fi

  # Commit anything grok left unstaged, then push. TRUNK GUARD once more, against
  # the ref we are actually about to write.
  if is_trunk "$head_ref"; then die "refusing to push to trunk '$head_ref'"; fi
  git -C "$wt" -c user.name='grok-triage[bot]' \
                -c user.email='grok-triage@users.noreply.github.com' \
                commit -q -m 'fix(ci-triage): patch from automated review' \
                          -m "Attempt ${attempts}/${MAX_ATTEMPTS} on PR #${pr}." \
                          -m 'Auto-Fix-Bot: grok' 2>/dev/null || true

  # Plain push, never --force: if the branch moved we lose the race and say so.
  if ! git -C "$wt" push origin "HEAD:refs/heads/${head_ref}"; then
    note "push rejected (branch moved, or no write access) -- not forcing"
    post_update "$repo" "$pr" "$cid" "$body" \
      "grok produced a patch but the push was rejected -- the branch moved, or this host lacks write access. **Nothing was applied.**"
    return 0
  fi

  # De-queue immediately. ci-triage re-labels only if the NEXT run also fails, and
  # that is what keeps the escalation bounded: one label, one attempt.
  gh api --method DELETE "repos/${repo}/issues/${pr}/labels/${LABEL_QUEUE}" --silent 2>/dev/null || true
  post_update "$repo" "$pr" "$cid" "$body" \
    "grok pushed a patch (attempt ${attempts}/${MAX_ATTEMPTS}). Checks re-run on the new commit. **Nothing is merged** -- a human decides once the gates are genuinely green."

  # CHECK STATE, reported honestly. QUEUED IS NOT SUCCESS. Self-hosted jobs on this
  # fleet queue for real periods -- one PR's build sat 17.8 hours and then passed in
  # 43 seconds. So this reports what the API says right now and nothing more; it
  # does not wait, and it never concludes "fixed".
  sleep 5
  note "check state right now (informational only -- pending is NOT success):"
  gh pr checks "$pr" --repo "$repo" 2>/dev/null | sed 's/^/     /' || note "(no checks reporting yet)"
}

# Append a status line to the sticky comment rather than posting a new one.
post_update() {
  local repo="$1" pr="$2" cid="$3" body="$4" msg="$5"
  local f; f="$(mktemp)"
  {
    printf '%s\n\n' "$body"
    printf -- '---\n\n**Host worker, %s:** %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$msg"
  } > "$f"
  gh api --method PATCH "repos/${repo}/issues/comments/${cid}" -F body=@"$f" --silent || true
  rm -f "$f"
}

# --- main -------------------------------------------------------------------

one_pass() {
  local repo
  for repo in "${REPOS[@]}"; do
    say "polling ${repo} for ${LABEL_QUEUE}"
    local prs
    prs="$(gh pr list --repo "$repo" --state open --label "$LABEL_QUEUE" \
             --json number --jq '.[].number' 2>/dev/null || true)"
    if [ -z "$prs" ]; then
      # EMPTY IS A REAL ANSWER, and it is a success. Only UNKNOWN is a failure.
      note "no PRs queued -- nothing to do"
      continue
    fi
    local pr
    while IFS= read -r pr; do
      [ -n "$pr" ] || continue
      handle_pr "$repo" "$pr"
      sleep 1   # GitHub API pacing
    done <<< "$prs"
  done
}

if [ "$LOOP_SECS" -gt 0 ]; then
  while :; do
    one_pass
    say "sleeping ${LOOP_SECS}s"
    sleep "$LOOP_SECS"
  done
else
  one_pass
fi
