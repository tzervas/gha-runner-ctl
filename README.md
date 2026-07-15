# gha-runner-ctl

**One** hardened, highly performant Rust controller for a GitHub Actions self-hosted runner on Podman. It features pre-seeded snapshot volumes, short-lived auto-registration, and automatic, load-aware horizontal scaling up and down.

This is a production-grade **1-click deployment** solution with advanced Git Credential Manager (GCM) integration, interactive secret guarding, dynamic multi-repo retargeting, and automatic disk/cache pruning.

---

## Why (Design Philosophy)

Unlike heavy Kubernetes operators or idle VMs that consume resources 24/7, `gha-runner-ctl` operates on a simple, hardened principle:

* **Idle Zero-Cost:** The only component running continuously is the thin, lightweight Rust listener/agent. The runner container is started ephemerally when a job is dispatched, and torn down after completion or upon idling.
* **Fast Start via Seeded Volumes:** Instead of downloading a massive official runner tarball on every cold start, `gha-runner-ctl prepare` bakes the binary baseline into a reusable Podman volume. Cold starts happen in under 2 seconds.
* **Secure Registration Guard:** Self-hosted runner registration tokens are short-lived. We dynamically query GitHub API, write credentials to a locked `0600` runtime file, start the container, and immediately shred/unlink the secret from the host.
* **Multi-Repo Capability (Scope User/Org):** One single controller process polls all repositories in your user or organization namespace, ephemerally starting the runner and registering it to whichever repository has queued self-hosted work.

---

## Key Features

1. **1-Click `--full-auto` Mode:** Detects if you are inside a Git checkout (targeting that repository) or defaults to user-level batch polling. Instantly prepares the Podman snapshot if missing, and initiates the polling listener.
2. **Secure Interactive Fallback & Secret Guarding:** Never pass secrets in CLI parameters or environment variables directly. If missing, `gha-runner-ctl` prompts for your token via a masked prompt. Any raw token pattern detected in raw command arguments is intercepted and scrubbed to prevent shell/TTY history leaks.
3. **Git Credential Manager (GCM) Integration:** Automatically queries GCM (using standard `git credential fill` protocol) to safely retrieve your GitHub Personal Access Token without human intervention. Offers automatic interactive GCM installation if missing on Debian/Ubuntu-based machines.
4. **Visibilities Filtering:** Keep control of security with `--public-only`, `--private-only`, or `--all-repos` (defaults safely to the user's public repositories).
5. **Secure Container Environment:** Pre-configures the Ubuntu 24.04 runner container to only pull packages from trusted mirrors over secure HTTPS connections.
6. **Dynamic Horizontal Scaling:** Scale out automatically to multiple concurrent runner containers using `--max-runners`, controlled by queued job counts and configured with `--queue-depth-threshold` per runner.
7. **Host Load average Throttling:** Protect host workstations from CPU saturation. Setting `--max-load` automatically delays scaling up if the 1-minute system load average exceeds the configured threshold.
8. **Intelligent Cache & Storage Pruning:** Setting `--max-cache-size` and `--min-disk-free-pct` automatically inspects storage usage on the persistent Podman baseline volume and auto-prunes older workspaces inside `_work/` first if disk space runs low.

---

## Prerequisites

* **Podman** installed on the workstation.
* A GitHub Personal Access Token (PAT) with `admin:org` (for org scope) or repository-level admin permissions.

---

## Quick Start & Installation

### 1-Click Install & Run

Compile and install `gha-runner-ctl` to `~/.local/bin` from source:

```bash
git clone https://github.com/tzervas/gha-runner-ctl.git
cd gha-runner-ctl
bash packaging/install-ctl.sh
export PATH="$HOME/.local/bin:$PATH"
```

To run a fully automated deployment with 1-click:

```bash
# Automatically detects current directory repo context, prepares snapshot, and starts polling
gha-runner-ctl --full-auto
```

### Manual Execution

If you prefer to invoke commands individually:

```bash
# 1. Build container image and seed the snapshot volume
gha-runner-ctl prepare

# 2. Start polling for a specific repository with custom visibility filter
gha-runner-ctl --scope repo --repo your-username/your-repo --private-only listen --interval 30 --idle-secs 180
```

---

## Scaling & Cache Parameters

| Option | Default | Description |
|---|---|---|
| `--max-runners` | `1` | Maximum number of concurrent runner containers to spawn (horizontal scaling). |
| `--queue-depth-threshold` | `1` | Number of queued jobs required per idle runner before starting another runner. |
| `--max-load` | `0.0` | Maximum 1-minute system load average allowed before scaling up is throttled (0.0 to disable). |
| `--max-cache-size` | `10240` | Maximum size of the build cache/`_work` directory in MB before auto-pruning. |
| `--min-disk-free-pct` | `15` | Minimum percentage of free disk space required on the volume mount before pruning oldest workspaces. |

---

## Commands

| Command | Description |
|---|---|
| `prepare` | Builds Podman container image and seeds the persistent baseline volume. |
| `up` | Dynamically fetches registration token, seeds env, and brings up the Podman runner. |
| `down` | Safely stops the runner container, deletes container instance, and cleans up volume registration. |
| `status` | Checks active scope, container state, and registration details. |
| `listen` | Runs the main polling daemon, monitoring queues and scaling the runner on-demand. |

---

## Consumer Workflows

Configure any repository workflow file to target your workstation runner by adding the appropriate `runs-on` labels:

```yaml
jobs:
  ci:
    runs-on: [self-hosted, linux, x64, podman]
    steps:
      - uses: actions/checkout@v4
      - name: Verify Environment
        run: |
          echo "Running on secure, on-demand self-hosted compute!"
```

---

## Security Model

The controller enforces robust, fail-closed security guarantees:
* **Shell Injection Block:** Strict allowlist validation is applied to all repository names, labels, and CPU/memory specifications to eliminate shell metacharacters before calling APIs or Podman.
* **Token Redaction:** Log traces and errors are automatically parsed to filter and redact `ghp_`, `github_pat_`, and `Bearer` secret strings.
* **User Privileges:** The runner container runs completely under a non-root user (`runner` UID 1001) with `no-new-privileges` enabled, ensuring complete sandboxing from the host system.

For details, refer to `docs/SECURITY.md`.
