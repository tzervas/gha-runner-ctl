# Release process — gha-runner-ctl

Portable release contract for this repo. Cross-repo process lives in
`~/documents/agent-dev-methodology.md` §7 (`framework/git-semver-pr`).
Orchestration roles: [ORCHESTRATION.md](ORCHESTRATION.md).

**Local workstation gates are the source of truth** for merge readiness. Remote
GitHub Actions are informational unless a human explicitly requires otherwise.

---

## Branch model

| Branch | Role |
|--------|------|
| **`main`** | Production line. **PR only** — no direct pushes for product work. |
| **`dev`** | Integration. All feature/fix/docs PRs **base `dev`**. |
| **`feat/*`**, **`fix/*`**, **`chore/*`**, **`docs/*`** | Cut from `origin/dev`; merge back into **`dev`**. |

After a release promote, keep **`main` and `dev` tips aligned** (merge-back PR
`main` → `dev` or equivalent) so integration does not drift.

```text
feat/foo ──PR──► dev ──promote PR──► main ──tag vX.Y.Z──► dist / GitHub Release
                     ▲
                     └── merge-back from main when needed
```

---

## Conventional Commits

All commits on product branches use [Conventional Commits](https://www.conventionalcommits.org/):

| Prefix | Use |
|--------|-----|
| `feat:` | User-visible capability |
| `fix:` | Bug fix |
| `docs:` | Documentation only |
| `chore:` | Tooling, deps, non-user-facing |
| `refactor:` | Behavior-preserving code change |
| `test:` | Tests only |
| `ci:` | CI/workflow |
| `perf:` | Performance |
| `build:` | Build/packaging |

**Breaking changes:** `feat!:` / `fix!:` subject, or a `BREAKING CHANGE:` footer.

**Agents** must emit compliant messages even without interactive prompts.
**Humans** may use Commitizen when installed (see below).

Examples:

```text
feat: add demand label filter for GPU listeners
fix: honor Retry-After on registration 429
docs: document RELEASE and cz bump checklist
chore: align VERSION with Cargo.toml for 0.2.7
```

---

## SemVer

This project follows [Semantic Versioning](https://semver.org/) while `0.y.z`
(**`major_version_zero = true`** in `.cz.toml`):

| Change | Bump | Examples |
|--------|------|----------|
| Breaking API/CLI behavior or security contract | **MAJOR** (or `0.y` → `0.(y+1)` while on 0.x) | Removed flag, changed default that breaks consumers |
| Compatible new capability | **MINOR** | New subcommand, new optional env |
| Fix, docs, chore, internal refactor | **PATCH** | Bugfix, typo, dependency patch |

**Tags:** `vX.Y.Z` (e.g. `v0.2.6`). **Never** force-push `main` or `dev`.

### Version files (lockstep)

On every release bump, these must agree:

| File | Field |
|------|--------|
| `VERSION` | Plain `X.Y.Z` |
| `Cargo.toml` | `[package].version` |
| `src/lib.rs` | `const UA: &str = "gha-runner-ctl/X.Y.Z"` |
| `CHANGELOG.md` | New `## X.Y.Z` section |

Commitizen (`.cz.toml`) updates `VERSION`, `Cargo.toml`, and the `UA` prefix in
`src/lib.rs` when `cz bump` is used. Always verify with:

```bash
grep -E '^(version =|0\.)' VERSION Cargo.toml
grep 'const UA' src/lib.rs
```

---

## Commitizen (`cz`)

Configuration: repository root **`.cz.toml`**.

The CLI is **optional** on the workstation. When available:

```bash
# Interactive commit (human)
cz commit

# Preview next version from commits since last tag
cz bump --dry-run

# Bump version + changelog + version files (pick increment explicitly when unsure)
cz bump --increment PATCH   # or MINOR, MAJOR

# After bump, run local gates, commit, tag
git tag -a "v$(tr -d '[:space:]' < VERSION)" -m "release: v$(cat VERSION)"
```

Install (one of):

```bash
pip install commitizen
# or
uv tool install commitizen
```

If `cz` is not installed, follow the same SemVer table and update version files
manually (or via a focused `chore:` commit).

---

## Pull requests

### Feature / fix work

1. Branch from `origin/dev`.
2. Implement; stay inside owned paths (see worker briefs / Interface Bulletins).
3. Run **local gates** (below).
4. Open PR with **base `dev`**.
5. PR title/body: conventional style; link issues (`Fixes #N` only when that
   issue should close on merge to the **target** branch per project policy).

Workers (L2) edit files only; **L1 / human owns** `git commit`, `push`, and PR.

### Promote to `main`

1. Ensure `dev` is green on local gates.
2. Open promote PR: **`dev` → `main`** (human review gate).
3. Merge via PR (no force-push). **Use a MERGE COMMIT, not a squash.**
4. On `main` tip: `release-on-merge.yml` tags `vX.Y.Z`, builds artifacts, cuts
   the GitHub release, then publishes to crates.io.
5. Open the merge-back PR **`main` → `dev`** immediately (step 4 of the
   checklist below is not optional).

#### Never squash the promote PR

Squashing `dev` → `main` gives `main` a single new commit instead of `dev`'s
history. The two branches then have **identical trees but disjoint histories**,
and their merge base stops advancing. Consequences, all of which happened:

- Every PR cut from `main` diffs against a stale merge base, so retargeting it
  to `dev` renders thousands of lines of already-merged work.
- `main` → `dev` merge-back conflicts on files both sides "added".
- "Which branch is upstream?" becomes unanswerable from the UI.

If it happens anyway, recover with a `main` → `dev` back-merge PR that resolves
conflicts in favour of `main`, and verify the result with
`git diff origin/main <branch> --stat` returning empty.

### Release artifacts vs registry publication

These are **separate artifacts with separate failure domains**, and
`release-on-merge.yml` deliberately orders them so one cannot block the other:

| Artifact | System | Blocking? |
|----------|--------|-----------|
| `vX.Y.Z` tag + GitHub release + assets | this repository | runs **first** |
| crates.io publish | crates.io | runs **after**, cannot prevent the above |

A failed publish still fails the job (loudly, with a job summary), but the tag
and release already exist by then, so the repository's own record of what
shipped is never held hostage by a registry problem.

The publish step is **not** gated on the tag. It relies on `cargo publish` being
idempotent — an already-published version reports "already exists" and is
treated as success — so a publish that failed for a registry-side reason is
retried by re-running the workflow, with no repo change. Gating it on the tag
would mean that once the tag existed the publish could never run again.

**Known blocker:** this crate has never been published. A scoped crates.io token
needs the `publish-new` permission (not just `publish-update`) to create a crate
for the first time. Until the token carries it, expect the publish step to fail
while the GitHub release still succeeds.

Task/epic issue closure policy matches org practice: prefer closing task issues
when their commits land on **`main`**, not on intermediate `dev` merges.

---

## Local gates (source of truth)

Run on the PR head commit before requesting merge:

```bash
cd /path/to/gha-runner-ctl
export PATH="${HOME}/.cargo/bin:${PATH}"

cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release
bash scripts/security-scan.sh   # when audit tooling available
```

Optional before tag:

```bash
bash scripts/dist.sh          # uses VERSION
# bash scripts/dist.sh --upload  # after gh auth + release exists
```

These mirror `.github/workflows/ci.yml`. Do not empty-commit to “kick” CI for
merge theater.

---

## Release checklist (maintainer)

- [ ] Conventional commits on the integration branch since last tag
- [ ] SemVer increment chosen (PATCH / MINOR / MAJOR)
- [ ] `cz bump` or manual bump: `VERSION`, `Cargo.toml`, `UA`, `CHANGELOG.md`
- [ ] Local gates green
- [ ] PR to `dev` merged (feature wave) or `dev` ready for promote
- [ ] Promote PR `dev` → `main` merged **with a merge commit, never a squash**
- [ ] Annotated tag `vX.Y.Z` on `main` (automated by `release-on-merge.yml`)
- [ ] GitHub release created with `dist/` assets attached
- [ ] crates.io publish succeeded **or** its failure is understood and recorded
      (it does not block the release — see "Release artifacts vs registry
      publication")
- [ ] Merge-back PR `main` → `dev` opened — **always**, not only "if tips
      diverged". A squash-merged promote leaves the trees identical while the
      histories diverge, so "tips look the same" is not evidence they are.

---

## Sister repos (templates)

| Repo | Release doc | Version files (follow-up `.cz.toml`) |
|------|-------------|----------------------------------------|
| `tg-agent-relay` | `docs/RELEASING.md` | `VERSION`, `pyproject.toml:version` |
| `agent-harness` | `docs/WORKFLOW.md` | `VERSION`, `pyproject.toml:version` |

Copy `.cz.toml` pattern from this repo and adjust `version_files` when enabling
Commitizen there.

---

## Contract

**Producer:** process worker (`framework/git-semver-pr`).  
**Consumers:** L1 git owner, agents opening PRs, maintainers tagging releases.