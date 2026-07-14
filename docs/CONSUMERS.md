# Using the shared runner from other repos

## One host, many repos

```
  repo A  ──┐
  repo B  ──┼── labels: self-hosted,linux,x64,podman ──►  one gha-runner-ctl host
  repo C  ──┘                                                      │
                                                                   ▼
                                                         GitHub job queue
```

1. Run **one** `gha-runner-ctl listen` (or `up`) on the workstation.  
2. Register at **organization** scope when possible so every org repo can schedule jobs.  
3. In each consumer repo, set `runs-on` to those labels.  
4. Do **not** install a runner per repository.

## Queuing

GitHub maintains the job queue. With a single runner:

- One job runs at a time (default).  
- Further jobs stay **queued** until the runner is free.  
- In **ephemeral** mode the process exits after a job; `listen` detects remaining queue (poll interval) and starts the runner again.  
- You do not implement a custom dispatcher.

## Example job

```yaml
# .github/workflows/ci.yml
name: ci
on:
  workflow_dispatch:
  push:
    branches: [main, dev]

jobs:
  check:
    runs-on: [self-hosted, linux, x64, podman]
    steps:
      - uses: actions/checkout@v4
      - name: test
        run: |
          # your project’s local gate
          true
```

Match labels exactly (order does not matter; all listed labels must be present on the runner).

## Registration modes

| Scope | Flag / env | Who can use the runner |
|---|---|---|
| Organization | `--scope org --owner my-org` | All **org** repos (one registration; GitHub dispatches) |
| Repository | `--scope repo --repo owner/name` or `--auto` | That repository only |
| User batch | `--scope user --user tzervas` | Any **owned** personal repo: controller re-registers ephemerally to the repo that has demand (still one process) |

Personal accounts cannot use a single org-level registration for `user/*` repos
outside the org. Use **user batch** (re-target) or move CI-heavy repos into the org.

## Checklist for a new consumer repo

- [ ] Runner host is online (`gha-runner-ctl status` → `online` or `listen` running)  
- [ ] Workflow `runs-on` includes the shared labels  
- [ ] No duplicate self-hosted install in that repo  
- [ ] Secrets/permissions for the job are set on the **consumer** repo (the runner only provides compute)  
