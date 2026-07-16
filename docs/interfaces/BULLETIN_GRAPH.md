# Interface bulletin graph — gha-runner-ctl 0.2.6 ship

Status: STABLE (L0 frozen)

```text
W1b CLI/env STABLE ─────────────► W1c docs, W2 env, W3 PR test plan
  docs/interfaces/ctl-cli-env.md
  docs/interfaces/ctl-security-identity.md
  docs/interfaces/ctl-reg-api-budget.md
W1a cz/semver STABLE ───────────► W3 commits / PR title
scripts phase API STABLE (if touched) ► W2 setup-rootless
W2 host STABLE (units, runners) ─► L0 join / promote decision
```

Consumers must list `Depends-on: <contract-id>@STABLE` in PM briefs.
