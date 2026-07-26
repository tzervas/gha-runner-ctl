# Branch sync: keeping `dev` and `sec` current with `main`

Automated, non-destructive reconciliation of downstream branches after every land on
`main`. Implemented by [`.github/workflows/branch-sync.yml`](../.github/workflows/branch-sync.yml).

## Root cause this prevents (squash-merge of `dev` → `main`)

When release PR #37 (`dev` → `main`) was **squash-merged**, `main` received a single new
commit instead of `dev`'s history. After that:

- `dev` and `main` had **identical trees** but **disjoint histories**
- Their merge base stayed stuck at an older commit (`ace4fe3`), days behind the tip
- Every feature branch cut from `main` computed its PR diff against that stale merge base
- Retargeting those branches to `dev` showed ~3,600 lines across 25 files of work `dev`
  **already contained** — a phantom diff that makes the UI unusable and invites bad merges

Squash destroys the shared ancestry that would have advanced the merge base. A normal
**merge commit** would have made `main` contain `dev`'s commits as ancestors, and
retargeted PRs would show only real deltas.

The one-off repair for that incident is PR #47 (`chore/sync-main-into-dev` → `dev`). This
document and workflow are the **automation that prevents recurrence**, not that manual fix.

## Recommendation (documentation only — do not change settings here)

**Promote PRs from `dev` into `main` must be merged with a MERGE COMMIT, not SQUASH.**

| Merge style on `dev` → `main` | Effect on ancestry | Effect on later PRs |
|---|---|---|
| **Merge commit** (recommended) | `main` gains `dev`'s commits as ancestors; merge base advances | Feature branches retargeted to `dev` show only their own delta |
| **Squash** (root cause) | `main` gets one new commit; shared history stops at the old base | Phantom multi-thousand-line diffs; every lower looks dirty |
| **Rebase merge** | Rewrites commits onto `main`; `dev` still does not share those SHAs | Similar merge-base confusion unless `dev` is reset (which we never do) |

This is a **process recommendation**. Repository rulesets and allowed merge methods are
owned by humans; this doc does not change them. If squash remains enabled for convenience
on feature PRs, at minimum the **release PR into `main`** should use a merge commit.

## How `dev` and `sec` stay synced

On every push to `main`, on a daily schedule, and on `workflow_dispatch`, the
`branch-sync` workflow runs one job per downstream branch in `{dev, sec}`:

1. **Full-history checkout** (`fetch-depth: 0`) so a real merge is possible.
2. If the downstream branch **does not exist** (e.g. `sec` today): **create it from
   `main`** and exit success (work done).
3. If the branch **already contains `main`** (`main` is an ancestor): print that clearly
   and exit 0 — **nothing to do is success**. No empty PR.
4. Otherwise: cut a disposable head `chore/sync-main-into-<branch>-<short-sha>` from the
   downstream tip, run `git merge --no-ff origin/main`, and:
   - **Clean merge** → push the head and **open or update** a PR into the downstream
     branch. The workflow **never merges** that PR.
   - **Conflict or tool/API failure** → fail loudly with the branch name, conflicting
     paths, and exact human recovery commands.

### Non-destructive guarantee

Reconciliation is **always a merge**. The workflow will **never**:

- force-push `main`, `dev`, or `sec`
- `git reset` a protected/downstream branch
- rebase `dev` or `sec`

Downstream-only commits that `main` does not have are preserved. The only force-free
rewrite of history that could lose work is deliberately out of scope.

If an open sync PR for the same downstream base already exists (head matching
`chore/sync-main-into-<branch>*`), the workflow **updates** that lane: same head refreshes
title/body; a newer `main` tip closes the stale sync PR and opens one from the new head so
exactly one equivalent sync PR stays open.

## Exit contract

Matches the fleet contract ("red must mean broken"):

| Situation | Outcome | Example |
|---|---|---|
| The thing is broken (merge conflict; push/API/auth failure) | **FAIL loudly** | Conflict paths printed; human recovery commands printed |
| Work was done successfully | **PASS** | Created missing `sec`; opened/updated a sync PR after a clean merge |
| **Nothing to do** | **PASS** (exit 0) | Downstream already contains `main` — no empty PR, not red |
| Could not tell (fetch failed, `gh` missing, no merge-base, list/create PR error) | **FAIL loudly** | Named branch + what failed + what a human should run |

**Empty and unknown are different code paths.** "Already up to date" is empty/nothing-to-do
and passes. "Could not query open PRs" is unknown and fails.

## Runner tier and sizing basis

```yaml
runs-on: [self-hosted, linux, x64, podman, small]
```

| Choice | Why |
|---|---|
| Self-hosted podman fleet | Same fleet as the rest of this repo's CI |
| Explicit **`small`** (0.5 cpu / 1 GiB) | Label takes precedence over job-name heuristics |
| Not `micro` | Micro is 512 MiB; fine for tiny scanners but not required here |
| Not `large` / no compile tier | **This workflow compiles nothing** — checkout + `git merge` + `gh` only |

Measured fleet fact: OOM risk comes from **cargo compiles** (e.g. `cargo clippy
--all-targets` peaking ~1225 MiB against micro's 512 MiB cap → SIGKILL 137). Git
plumbing does not need a compile tier. The job name is also chosen so it does **not**
collide with required main contexts (`cargo check/test`, `detect stack`, `gate`,
`gitleaks`, `trivy filesystem (vuln+secret+license)`, `build`).

Scheduled runs use a concurrency group **without** `cancel-in-progress`. On self-hosted
runners a scheduled job can sit queued; the next schedule must not cancel it into a silent
`cancelled` that never alerts.

## Manual bootstrap (automation unavailable)

If the workflow cannot run, a human can reconcile one downstream branch locally:

```bash
git fetch origin

# Create sec if missing
git push origin origin/main:refs/heads/sec   # only when sec does not exist

# Back-merge main into dev (or sec) with a merge commit — never squash this step
git checkout dev
git pull --ff-only origin dev
git merge --no-ff origin/main -m "chore(sync): merge main into dev"
# If conflicts: fix files, git add, git commit
git push origin dev
# Or push a branch and open a PR:
#   git push origin HEAD:chore/sync-main-into-dev-manual
#   gh pr create --base dev --head chore/sync-main-into-dev-manual \
#     --title "chore(sync): merge main into dev"
```

Verify the phantom-diff class of bug is gone:

```bash
git fetch origin
git merge-base origin/dev origin/main    # should be near tip of main after a proper merge
git diff origin/main origin/dev --stat   # real content delta only
```

## Related

- One-off history repair after the #37 squash: PR #47
- Branch / release contract (red = broken, required-check naming trap): fleet
  `BRANCH-AND-RELEASE-CONTRACT.md`
- Maintenance and security automation (separate worker): [maintenance-and-security.md](maintenance-and-security.md)
