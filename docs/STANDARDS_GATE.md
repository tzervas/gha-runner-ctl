# The development-standards gate

`.github/workflows/standards.yml` is a thin caller for
[`tzervas/ap-workflows/.github/workflows/reusable-policy-contract.yml@v0.1`](https://github.com/tzervas/ap-workflows/blob/v0.1/.github/workflows/reusable-policy-contract.yml).
All policy lives centrally; this repo carries only its overrides. Full rule reference, the
operator commands, and the known limits are in
[`ap-workflows/docs/STANDARDS.md`](https://github.com/tzervas/ap-workflows/blob/main/docs/STANDARDS.md).

This page covers what is specific to **gha-runner-ctl**.

## What it reports

One status context: **`standards / standards`**.

It is `standards / standards` and not `standards` because a reusable workflow prefixes the job
name with the caller's job id, and **the prefix cannot be suppressed**. A ruleset that requires
the bare `standards` would require a context that never reports — the PR would not go red, it
would wait forever with nothing to diagnose.

**Do not add this to `protec-main` / `protec-dev` until this caller has landed and reported
once.** Confirm the exact spelling first:

```bash
gh pr checks <n> --repo tzervas/gha-runner-ctl
```

## Why this repo cares more than most

`gha-runner-ctl` is where the fleet's own damage was measured. At the time of adoption:

| measurement | value |
|---|---|
| `main` / `dev` merge base | `ace4fe32` |
| `main` commits not in `dev` | 2 |
| `dev` commits not in `main` | 8 |

The merge base being pinned that far back is the residue of a promote that was squashed rather
than merged. Rule 11 (`trunk-divergence`) runs on `push` as well as `pull_request` precisely
because that damage is done *at merge time*, after every PR check has already gone green.

## Runner sizing

The caller does **not** pass a size label. Verified against
`GET /repos/tzervas/gha-runner-ctl/actions/runners` on 2026-07-25, the only registered runner
advertises `[self-hosted, Linux, X64, podman]` and nothing else. Requiring `small` would queue
the job against a label nothing serves.

`pool::size_for_job` checks `runs-on` labels **before** the job-name heuristic, so once workers
register size labels the caller should pass:

```yaml
with:
  runner-labels: '["self-hosted","linux","x64","podman","small"]'
```

Until then the job name `standards` matches none of the Micro name signals in `src/pool.rs` and
lands on the Medium catch-all (2 CPU / 4 GiB) — see `resources_for_tier`. That is ample for a
Python linter and is *not* the 0.25 CPU / 512 MiB Micro tier that a job named `lint` would get.

## What changed to make this repo pass

`fleet-security.yml` had `cancel-in-progress: true` alongside an `on.schedule` trigger. On a
self-hosted pool the weekly full-history secret scan queues when no runner is free, the next
tick cancels it, and a cancelled run reports `cancelled` — not `failed`. Nothing alerts and the
scan silently stops running. It is now:

```yaml
cancel-in-progress: ${{ github.event_name != 'schedule' }}
```

PR pushes still supersede each other; only the schedule is exempt.

## Modes in force here

Everything is at the central default. Nothing is downgraded, so any rule that goes red here is a
real finding rather than a policy exception:

| rule | mode |
|---|---|
| `promote-merge-mode`, `branch-targeting`, `protected-refs`, `trunk-divergence` | enforce |
| `version-policy`, `yaml-validity`, `schedule-cancel`, `conventional-title` | enforce |
| `version-drift`, `exit-contract`, `python-floor`, `docs-with-change` | warn |
| `actionlint` | off |

`exit-contract` stays at the central `warn` default. It has no findings on this repo today —
the two `|| true` lines in `fleet-ci.yml` and `fleet-security.yml` are a `rustup component add`
and an `echo "$(gitleaks version || true)"` banner, and the checker deliberately does not flag
either, because it resolves the command the `||` actually binds to rather than matching tool
names anywhere on the line.
