# Fleet standards (tzervas)

Applied from the workstation pack under `plans/fleet-standards/pack/`.

## Honest CI (normative)

See **[HONEST_CI.md](./HONEST_CI.md)** — outcomes are only **pass / fail / skip**;
failures must classify **`FAIL_PRODUCT` | `FAIL_NOT_IMPLEMENTED` | `FAIL_RUNNER` |
`FAIL_ENV` | `FAIL_INFRA`** so runner/env problems are never mistaken for product
or for “not implemented yet.” No fake green (missing toolchain → exit 0 is banned
on required jobs).

## Workflows

| Workflow | When | Runner |
|----------|------|--------|
| `fleet-ci.yml` | push/PR to main|dev | self-hosted linux x64 podman |
| `fleet-security.yml` | push/PR + weekly | same |
| `close-issues-on-main.yml` | PR closed→main | same |
| `reopen-issues-closed-off-main.yml` | PR merged off-main with Closes | same |

## Issue close policy

- **`dev` / feature merges:** `Refs #n` only — issues stay open
- **`main` merges:** `Closes #n` / `Fixes #n`
- **Epics:** close only when delivery PR to main includes `Closes #<epic>`

## Badges

README badges use GitHub Actions SVG for **trunk** branch — live status, not static green.

## Copilot

Automatic Copilot code reviews are **disabled** for fleet-managed repos. Do not request Copilot on PRs.

## Permissions

Workflows use minimum `permissions:` blocks (contents read; issues write only for close/reopen jobs).
