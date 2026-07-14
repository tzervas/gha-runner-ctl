# gha-runner-ctl

**One** GitHub Actions self-hosted runner on Podman: pre-seeded snapshot, short-lived auto-registration, on-demand up/down.

Not a fleet. Not 140 processes. One listener on your workstation, shared by every repo that targets its labels.

[MIT](LICENSE) · [NOTICE](NOTICE) (cites [actions/runner](https://github.com/actions/runner), also MIT)

## Why

| Goal | Approach |
|---|---|
| Fast start | Image + volume snapshot (`prepare`) — no tarball download on the hot path |
| Secure register | Mint registration token via API / `gh auth`; never commit UI tokens |
| Idle cost | Ephemeral mode + idle timeout tears the container down |
| Many repos | Prefer **org-level** registration (one runner, GitHub dispatches jobs) |
| Queues | GitHub queues jobs to matching labels; one runner takes one job at a time; `listen` brings it back for the next |

## Requirements

- Rust **1.96+** (build), **Podman**, **`gh`** logged in  
- Token that can create runner registration tokens:
  - **Repo runner:** admin on that repo  
  - **Org runner:** org owner / runner admin  

Personal GitHub **user** accounts only get **repo-scoped** runners. For one runner across many repos, use a **GitHub Organization** and `--scope org`.

## Install

```bash
git clone https://github.com/tzervas/gha-runner-ctl.git
cd gha-runner-ctl
bash packaging/install-ctl.sh
export PATH="$HOME/.local/bin:$PATH"
```

## Quick start (one shared org runner)

```bash
# Once
gha-runner-ctl prepare

# Org-wide (recommended for many repos under one org)
export GHA_SCOPE=org
export GHA_OWNER=your-org          # e.g. tzervas-labs
export GHA_RUNNER_NAME=shared-podman-1
export GHA_LABELS=self-hosted,linux,x64,podman

gha-runner-ctl listen --interval 30 --idle-secs 180
```

Repo-only (single repository):

```bash
export GHA_SCOPE=repo
export GHA_REPO=tzervas/tg-agent-relay
gha-runner-ctl listen --interval 30 --idle-secs 180
```

Manual:

```bash
gha-runner-ctl up
gha-runner-ctl status
gha-runner-ctl down
```

## Consumer workflows

In **any** repo that should use this host:

```yaml
jobs:
  ci:
    runs-on: [self-hosted, linux, x64, podman]
    steps:
      - uses: actions/checkout@v4
      # …
```

Use the **same labels** the runner registered with. GitHub matches labels and queues work; you do not start a runner per repo.

See [docs/CONSUMERS.md](docs/CONSUMERS.md).

## Modes

| Mode | Behavior |
|---|---|
| `ephemeral` (default) | Fresh registration each `up`; runner drops after one job |
| `retain` | Keep `.runner` on the snapshot volume across restarts |

```bash
gha-runner-ctl --mode retain up
```

## Hardening (summary)

- Identity allowlists (no shell metacharacters into Podman/API)
- Short-lived registration tokens; scrubbed logs; private env file shredded after start
- `no-new-privileges`, `--pull=never` on run path; resource caps
- One controller instance via flock; wake endpoint needs `GHA_WAKE_TOKEN`
- Prefer **private** repos on self-hosted runners  

Details: [docs/SECURITY.md](docs/SECURITY.md).

## What `config.sh` is

That script ships **inside** the official runner package. This tool runs it for you in the container. You do not install the runner by hand or paste UI tokens.

## Citation / license

- This project: **MIT** (Tyler Zervas), see [LICENSE](LICENSE).  
- Official runner binary: **MIT** ([actions/runner](https://github.com/actions/runner)), see [NOTICE](NOTICE).  

## Commands

```
gha-runner-ctl prepare   # build image + seed volume
gha-runner-ctl up        # register + start
gha-runner-ctl down      # stop
gha-runner-ctl status
gha-runner-ctl listen    # poll for demand; idle down
```
