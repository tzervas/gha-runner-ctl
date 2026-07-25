# Automated CI fixing — the two-tier loop

When CI fails on a pull request, one of two things is true.

**Either the failure is mechanical** — formatting drift, an import ordering nit, a
lockfile that no longer matches its manifest — in which case a human reading the
diff learns nothing and a machine can fix it exactly. **Or it is a real defect**, in
which case a red PR is the *correct* outcome and automation must not paper over it.

This repo separates those two cases explicitly:

| tier | what | where it runs | may change code? |
|---|---|---|---|
| **1** | apply the closed list of mechanical fixes and push to the PR's own branch | `.github/workflows/auto-fix.yml` (in CI) | **yes**, closed list only |
| **2** | diagnose everything else into one sticky comment and queue a review agent | `.github/workflows/ci-triage.yml` (in CI) + `grok-triage-poll.sh` (on the fleet host) | **never in CI**; only the host worker patches, and only after it has run the gate |

Both tiers ship **inert**. Nothing happens until an operator sets the
`AUTOFIX_ENABLED` repository variable — see [Enabling it](#enabling-it).

---

## Tier 1 — the closed safe-fix list

These four, and nothing else, may ever be applied automatically:

| # | fix | command |
|---|---|---|
| 1 | Rust formatting | `cargo fmt --all` |
| 2 | Python formatting | `ruff format .` |
| 3 | Python safe lint fixes | `ruff check --fix --no-unsafe-fixes .` |
| 4 | Lockfile refresh matching a manifest change **in this PR** | `cargo metadata` / `uv lock` |

### Never auto-applied

Semantic code changes · test logic · version bumps · dependency upgrades of any
size · workflow, permission or action changes · anything at all that alters
behaviour.

If a failure is not on the list above, tier 1 does **nothing** and says so. That is
not a gap; it is the design. A red pull request is the right answer to a real
defect, and automation that hides one is worse than automation that does not exist.

### Why *these* four, specifically

Each is a **total function from source to source with no semantic degrees of
freedom**: given the same input and the same tool version, there is exactly one
output, and that output is behaviour-equivalent to the input. `ruff check --fix`
qualifies only because `--no-unsafe-fixes` is passed — ruff's own definition of an
*unsafe* fix is precisely "may not retain the original intent of the code", so
that single flag is the boundary between tier 1 and tier 2. It is stated explicitly
in the workflow rather than relied on as a default.

Tool versions are pinned (`RUFF_VERSION` in the workflow) so "the same tool
version" is a fact rather than a hope.

**A note on test files.** The fixer *will* reformat a file under `tests/`, because
`ruff format` does not distinguish. That is not a "test edit": only formatters run,
so a test's semantics cannot change. No tool in tier 1 can delete a test, skip one,
mark one `xfail`, or alter an assertion.

### The enforcement point

The tool invocations above express *intent*. The **allowlist check** is what makes
it true. Before anything is committed, every staged path is matched against:

```
DENY  .github/**        (checked first — .github/scripts/x.py would otherwise
                         match the *.py rule and let the fixer edit CI itself)
ALLOW *.rs *.py *.pyi
ALLOW Cargo.lock        only if Cargo.toml     changed in this PR
ALLOW uv.lock           only if pyproject.toml changed in this PR
ELSE  abort without pushing
```

Staging uses `git add -u`, which updates **tracked files only** — so the auto-fixer
cannot introduce a new file under any circumstance.

A lockfile is refreshed only to match a manifest change *made by this PR*.
Refreshing an untouched lock is a dependency bump wearing a hat, and the refresh
commands are chosen to be conservative: `cargo metadata` and `uv lock` keep existing
pins wherever they still satisfy the manifest. `cargo update` and `uv lock
--upgrade` are deliberately **not** used.

---

## The credential — and the trap it exists to avoid

> **A push made with `GITHUB_TOKEN` does not trigger workflows.**

That is deliberate GitHub behaviour to prevent infinite loops, and it is the single
thing this design is built around. An auto-fix pushed with the default token lands
on the branch and **the checks never re-run**. The PR sits red with a fix already
applied — which is *worse* than not fixing it, because it looks handled and is not.

So the push uses a **non-default credential**.

### What to create

| | |
|---|---|
| **Secret name** | `AUTOFIX_TOKEN` |
| **Kind** | fine-grained personal access token |
| **Repository access** | **this repository only** |
| **Repository permissions** | **Contents: Read and write** |
| **Everything else** | **none** |
| **Stored as** | a repository **Actions secret** |

```sh
# Create the fine-grained PAT in the GitHub UI (Settings -> Developer settings ->
# Fine-grained tokens), scoped to this repository, Contents: Read and write only.
# Then, reading from stdin so it never lands in shell history or in argv:
gh secret set AUTOFIX_TOKEN -R <owner>/<repo>
```

**Contents: write is genuinely sufficient — that is the whole scope.** The brief
that produced this workflow allowed *Pull requests: write* as well; it is not
needed and is therefore not requested. Pushing a commit to a branch requires
Contents: write and nothing more. Tier 1 never calls the pull request API — it
reports through the job summary instead, precisely so it can stay at one
permission. Narrower is provable; broader would only have been convenient.

### Why a fine-grained PAT and not a GitHub App

A GitHub App is the better shape *in general*: installation tokens expire in under
an hour, and revocation is one place. It is the wrong shape **here**, for a reason
this fleet already wrote down in `mycelium-workflows/GITHUB_APP.md`:

> Do **not** push the private key into component repos to let them mint their own
> tokens. That is strictly worse than a narrow PAT: it would put a train-wide
> credential-minting key in 46 places.

The fleet App's private key lives in `mycelium-lang` alone, and component repos hold
**zero** credentials by design. Using it here would mean putting a PEM that can mint
tokens for the whole train into a component repo — trading a blast radius of *one
repository* for a blast radius of *the entire fleet*, to save a rotation.

So: a fine-grained PAT, scoped to exactly one repository, with exactly one
permission. **Upgrade path:** if the operator later registers a *separate* App
installed only on the repos that run this loop, swapping the push credential is a
change to two steps in `auto-fix.yml` and nothing else. The App would be strictly
better; the *fleet-wide* App would be strictly worse.

### How the token is handled

The token is passed to `git` through a **credential helper that prints it on a
pipe**, reading the value from the process environment:

```sh
git -c 'credential.helper=' \
    -c 'credential.helper=!f() { printf "username=x-access-token\npassword=%s\n" "$AUTOFIX_TOKEN"; }; f' \
    push origin "HEAD:refs/heads/${HEAD_REF}"
```

The single quotes matter: the outer shell never expands `$AUTOFIX_TOKEN`, so what
lands in `argv` is the *literal string*. Git's helper subshell expands it from the
environment and writes it to git's stdin. Verified:

```
argv[4]=credential.helper=!f() { printf "username=x-access-token\npassword=%s\n" "$AUTOFIX_TOKEN"; }; f
```

That distinction is not cosmetic:

```
-r--r--r--  /proc/<pid>/cmdline      world-readable
-r--------  /proc/<pid>/environ      owner only
```

The leading `-c 'credential.helper='` clears any inherited helper so ours is the
only one consulted.

### `persist-credentials: false` is load-bearing

`actions/checkout` by default writes `http.https://github.com/.extraheader` into
`.git/config`, carrying basic auth for `GITHUB_TOKEN`. **That header takes
precedence over any credential helper.** Left in place, the push would silently
succeed *as `GITHUB_TOKEN`* — and the checks would never re-run. The exact trap,
re-opened by a default.

So checkout runs with `persist-credentials: false`, and the workflow then
**asserts** it: a dedicated step fails the job if any auth `extraheader` or any
credentialed remote URL survived checkout. It is checked, not assumed.

---

## The blast-radius guard

The workflow pushes to **one thing only**: the pull request's own head branch.

It **hard-refuses** to push to a trunk — `main`, `master`, `dev`, `sec`, or
`release/**` — and refuses any head ref that is not a plain branch name (empty,
leading `-`, containing whitespace, `..`, `:`, `~`, `^`, `\`, or a glob character).
A ref beginning with `-` would otherwise be reinterpreted by `git push` as an
option.

This is asserted **three times**, not once:

1. in the job-level `if:`, against the event payload;
2. in the first step, before checkout;
3. in the push step, immediately before the write.

Once is not enough because an `if:` expression is evaluated against a payload
snapshot that the job then never re-reads.

### Fork pull requests

The workflow runs on `pull_request`, **not** `pull_request_target`, and requires
`head.repo.full_name == github.repository`.

A fork PR's head branch is attacker-controlled. Pushing to it with our token is a
credential-exposure path, so it never happens. `pull_request` also withholds
secrets from fork runs, which makes this belt *and* braces — and is exactly why
`pull_request_target` is avoided rather than "guarded". Tier 2 does use
`workflow_run`, which has the same privileges as `pull_request_target`; see
[below](#why-workflow_run-is-safe-in-tier-2) for the one property that makes it
safe there.

The consequence is that a fork PR gets **no** auto-fix. That is correct. The CI
format gate still reports the drift and the contributor fixes it themselves.

---

## The loop bound

Because the push uses a non-default token, it **does** re-trigger the workflow —
that is the entire point — so an unbounded design would ping-pong forever.

**Bound 1 (load-bearing, purely local, always evaluable).** If `HEAD` already
carries the `Auto-Fix-Bot: safe-fixes` trailer, stop. The consequence is exact:
**at most one auto-fix commit per human push, ever.**

```
human pushes A  ->  run 1 fixes, pushes B
run 2 sees B    ->  HEAD is an auto-fix commit  ->  stop
```

**Bound 2 (secondary net).** Refuse a 4th auto-fix commit between the merge base
and `HEAD`. Bound 1 already makes a ping-pong impossible; bound 2 catches a
formatter that oscillates between two outputs across several human pushes. It needs
the base branch, and if the base cannot be resolved the run emits a `::warning::`
and continues on bound 1 alone. That is stated here rather than hidden, because it
is the one place the design degrades: the load-bearing bound is local and cannot
degrade, and the net that can is the redundant one.

**No empty commits.** Nothing is committed when there is nothing to fix, and the
workflow never force-pushes. If the branch moved while the job was working, the
plain push is rejected, the run reports `superseded`, and exits **0** — a newer run
already owns that newer SHA.

---

## Tier 2 — diagnosis and handoff

Everything tier 1 will not touch reaches `.github/workflows/ci-triage.yml`. It runs
on `workflow_run` when a gate workflow concludes `failure`, and it **never changes
code**.

### What the sticky comment carries

Not "CI failed". It is the handoff, so it carries:

- the failing **workflow** and a link to the run;
- the failing **job names**;
- the **real error excerpt, pulled from the job log** (`GET
  /repos/{o}/{r}/actions/jobs/{id}/logs`) — not a restatement of the check name;
- **whether the same workflow is also failing on the base branch** — i.e. is this
  the PR's defect, or pre-existing drift the PR merely exposed? That distinction
  has cost real time on this fleet twice. It is answered from the base branch's own
  most recent completed run, so it costs no extra queue time;
- the **attempt count** out of the maximum;
- if the budget is exhausted, **what a human should look at**.

If the run failed but exposes no failed job, the comment says the failure is
**UNKNOWN, not empty**, and points at the most common cause on this fleet: a
`python3 <<'PY'` heredoc at column 0 terminating a `run: |` block scalar, which
makes the YAML invalid and produces a permanent `startup_failure` where the
workflow never ran at all. Empty and unknown are different answers and are reported
differently.

### One comment, not a stream

The comment is **sticky**: found by the HTML marker `<!-- autofix-report -->` and
updated in place, never appended. A comment per run floods the PR and buries the
signal; the newest state is the only state anyone wants. The attempt counter lives
in a second marker, `<!-- autofix-state attempts=N max=M -->`, so the bound survives
without a database.

### Why `workflow_run` is safe in tier 2

`workflow_run` runs the **default branch's** workflow file with full repository
secrets and a writable token, on an event caused by a pull request. That is the same
shape as `pull_request_target` and dangerous for the same reason.

It is safe in `ci-triage.yml` for exactly one reason:

> **That workflow never checks out and never executes pull request code.**

There is no `actions/checkout` step in it. Every step is an API call. Adding a
checkout there re-opens the hole — the file says so in a comment at the top.

### The escalation bound

`MAX_ATTEMPTS: 3`. After three triage rounds on one PR the loop **stops**: the
`autofix:needs-grok` label is removed, `autofix:needs-human` is applied, and the
comment records what was tried and what to look at.

An unbounded patch/fail/patch retry is the single most likely way this system burns
a model subscription and an API budget, so the bound is the first thing in the file.
Re-arm by removing the `autofix:needs-human` label and deleting the sticky comment —
the counter lives in its marker, so deleting the comment resets it.

If the failure is classified as *safe-fixable* (the failing step was a formatter),
tier 2 does **not** queue an agent — spending a model on `cargo fmt` is waste. The
comment instead tells you to check whether `AUTOFIX_ENABLED` is set and whether
`AUTOFIX_TOKEN` is present, because tier 1 should already have handled it.

### The host-side worker

`ci-triage.yml` labels; it does not patch. The patching is done on the fleet host by
[`scripts/grok-triage-poll.sh`](../scripts/grok-triage-poll.sh), which polls for
`autofix:needs-grok`, runs the grok CLI against a checkout of the PR branch, and
pushes the result to that branch.

**Why a host poller rather than a workflow step.** A job on this fleet runs inside a
rootless podman container. The grok CLI and its credentials live on the *host* at
`/root/.grok`. A workflow step cannot reach it — and the obvious fix, bind-mounting
`/root/.grok` into the runner container, would put a model credential inside the
same container that executes pull-request-authored code.

The resulting split is the point:

- **in CI**, with a repo-scoped token: diagnose and label. Never patch.
- **on the host**, with the operator's own credentials: patch. Never merge.

No agent credential ever enters a container running PR code, and no long-lived
fleet-wide credential is ever added to a repository secret store.

**Honest limitation:** it is a *poller*. Nothing pushes from GitHub to the
workstation, so a PR waits up to one poll interval. That is a deliberate trade
against opening an inbound path to the host, and it is stated rather than dressed up
as an event-driven pipeline.

The worker's own guards mirror tier 1's — fork refusal, trunk refusal, live re-read
before acting, plain push never `--force` — plus three of its own:

- it **refuses to push** if the agent touched `.github/**` or a `VERSION` file, and
  leaves the worktree on disk for inspection;
- it **never merges and never arms auto-merge.** Most repos in this fleet have
  rulesets with *zero* required checks; arming auto-merge there merges unverified
  work while looking green. That hole existed on 35 repos and was closed
  deliberately. Green-and-gated, then a human merges — never the reverse;
- it **never reports success from a queued check.** Self-hosted jobs here queue for
  real periods — one PR's build sat 17.8 hours and then passed in 43 seconds. The
  worker prints the current check state and explicitly labels pending as *not*
  success.

The prompt it hands to the agent requires it to **state why the previous attempt
failed before trying again** and to **build on the branch rather than re-solve from
scratch**, which is what stops a second attempt from producing a differently-wrong
patch and a noisy branch.

---

## The exit contract

Per `BRANCH-AND-RELEASE-CONTRACT.md` §4a, and honoured in both tiers:

| situation | outcome |
|---|---|
| something is broken and not safely fixable | **fail** — correct |
| a safe fix was applied and pushed | **pass** |
| nothing to fix | **pass**, no empty commit, no false red |
| branch moved under us | **pass**, reported as `superseded` |
| tool crashed, or a fix is needed and `AUTOFIX_TOKEN` is missing | **fail**, loudly |

The ordering of that last row is deliberate. A missing secret is a failure **only
once a fix actually needs pushing** — that is the "could not tell whether it landed"
case, and it must be loud. Failing on a PR with nothing to fix would be a false red,
and false reds train everyone to ignore red.

### Do not make these required checks

`auto-fix` legitimately **skips** on fork PRs, on drafts, and when the loop bound
trips. `ci-triage` legitimately skips when there is no open PR. A skipped job is not
a passing job — so requiring either as a status check would block those PRs
forever with nothing red to diagnose. They are *fixers and reporters*, not gates.
The real gates stay required; these must not be.

Related trap, for whoever wires the ruleset: adopting a reusable workflow **prefixes
job names** (`detect stack` becomes `fleet-ci / detect stack`) and the prefix cannot
be suppressed. Update the caller first, then the ruleset — the reverse order leaves
a window where nothing is required at all.

---

## Runner sizing

Both jobs pin an **explicit tier label**:

```yaml
runs-on: [self-hosted, linux, x64, podman, small]
```

`gha-runner-ctl`'s `size_for_job()` checks **labels before** the job-name heuristic,
so this is deterministic. Without a label, the words `fix`, `fmt`, `format`, `lint`
and `security` in a job name all route to **micro (0.25 cpu / 512 MiB)**.

Undersizing is not theoretical here: `cargo clippy --all-targets` was measured at
**1225 MiB** against micro's 512 MiB cap, producing SIGKILLs and a 30-minute stall
that looked like a hang.

`small` (0.5 cpu / 1 GiB) is correct for these two jobs because **nothing in either
compiles**: rustfmt and ruff only, plus a conservative `cargo metadata` for the lock
refresh, and API calls in tier 2. There is no `cargo build`, no `cargo check` and no
`clippy`. `small` is also the only tier label currently registered on an online
worker in this fleet. Anything that *does* compile must carry `medium` or larger.

---

## Enabling it

Both workflows ship **inert**. Two steps, in this order:

```sh
# 1. the credential — created in the GitHub UI as a fine-grained PAT scoped to
#    THIS repository with Contents: Read and write, then piped in on stdin:
gh secret set AUTOFIX_TOKEN -R <owner>/<repo>

# 2. the switch
gh variable set AUTOFIX_ENABLED -R <owner>/<repo> --body true
```

`AUTOFIX_ENABLED` is a repository **variable**, not a secret — it holds no value
worth protecting, and keeping it out of the secret store means the kill switch can
be flipped by anyone who can read the repo settings.

### Disabling it

```sh
gh variable set AUTOFIX_ENABLED -R <owner>/<repo> --body false   # instant, both tiers
```

Any value other than the exact string `true` disables both tiers, so deleting the
variable also works:

```sh
gh variable delete AUTOFIX_ENABLED -R <owner>/<repo>
```

To keep tier 1 but stop tier 2 from queueing an agent, delete the
`autofix:needs-grok` label from the repo, or simply do not run the host poller —
`ci-triage.yml` will still post its diagnosis comment.

To remove the loop entirely, delete `.github/workflows/auto-fix.yml` and
`.github/workflows/ci-triage.yml` and revoke the PAT.

---

## What has *not* been verified

Stated plainly, because a design document that overclaims is worse than none:

- **The re-trigger itself is unproven.** That a push authenticated with
  `AUTOFIX_TOKEN` re-runs the checks — the single behaviour this whole design
  exists for — **cannot be demonstrated without a live run on a real PR with the
  secret present.** It follows from documented GitHub behaviour and from this
  fleet's own `GITHUB_APP.md`, and the `persist-credentials: false` assertion
  removes the most likely way to get it wrong silently. It is still not the same as
  having watched it happen. **Verify this first**, on a throwaway PR, before
  trusting the loop.
- **`workflow_run` only fires for workflow files that exist on the default
  branch.** Until `ci-triage.yml` reaches the default branch, tier 2 does not run at
  all — including on the pull request that introduces it.
- The YAML parses and every embedded `run:` block passes `bash -n`; the guard,
  allowlist and credential-argv behaviours were tested in isolation. None of that is
  a live end-to-end run.
