# Architecture & Design Details

This document explains the core architectural decisions, design choices, and security considerations behind `gha-runner-ctl`.

---

## 1. Core Architecture: On-Demand Scaling

`gha-runner-ctl` is designed to provide high-performance, single-instance self-hosted execution.

```
  +--------------------------------------------------+
  |              gha-runner-ctl listen               |
  |  (Continuous lightweight Rust polling loop)      |
  +---------------------------------------+----------+
                                          |
                                          | (If jobs queued)
                                          v
                         +---------------------------------+
                         |  Ephemerally spins up Runner    |
                         |  (Podman Container, UID 1001)   |
                         +---------------------------------+
```

### The "Why"
* **Traditional Runners:** Usually run 24/7 as full system services, wasting RAM and CPU, and maintaining persistent authentication tokens that pose a long-term target for attackers.
* **On-Demand ephemeral model:** By polling the GitHub API and dynamically checking if self-hosted jobs are queued or in progress, `gha-runner-ctl` only starts the heavy container when compute is actually needed. Once the job completes, the container is destroyed and unregistered.

---

## 2. Token Security & Process Isolation

We apply strict security boundaries to token management:

### Command Line Interception & Masking
To prevent secrets from leaking into shell command histories (`.bash_history`, `.zsh_history`), system logging (`syslog`), or standard process lists (`ps aux`), we intercept and reject any raw token strings passed as arguments.
Users can never pass `ghp_...` in CLI parameters. Instead:
1. We query **Git Credential Manager** or the standard configured helper via secure stdin pipes.
2. We fallback to **GitHub CLI** authenticated tokens.
3. We securely read from `~/.config/gha-runner-ctl/config.json` with `0600` permissions.
4. If all else fails, we use **masked interactive entry** (`rpassword`).

### Staged Env File Isolation
When starting the runner:
1. A temporary file containing the generated short-lived registration token is created under `XDG_RUNTIME_DIR`.
2. Permissions are restricted to `0600` (readable/writeable only by the owner).
3. We pass this file to Podman using `--env-file`.
4. As soon as the container begins initialization, the host process overwrites the temp file with random noise (shredding) and deletes (unlinks) it.

---

## 3. Container Mirror Hardening

The base runner image (built from Ubuntu 24.04) is configured to enforce secure connections for package management:
* **The "Why":** Standard container environments often perform `apt-get update` over plain HTTP. In compromised or shared networks, this allows man-in-the-middle package tampering or vulnerability injection.
* **The Mitigation:** We pre-install `ca-certificates` and `apt-transport-https` first, and rewrite the default mirror sources in `/etc/apt/sources.list.d/ubuntu.sources` to enforce secure `https://` URIs for all Ubuntu repository fetches.

---

## 4. Multi-Repository Polling & User-level Batch Scope

Using standard GitHub Org or Repo scopes is simple. However, personal developer accounts cannot register an "org-wide" runner for multiple distinct repositories.

To solve this, `gha-runner-ctl` implements **User Batch Scope**:
1. It polls all owned personal repositories.
2. If Repository A gets a queued job, it registers the runner dynamically to Repository A.
3. If demand shifts to Repository B, it automatically terminates the container, unregisters from A, and launches a fresh registration for B.
This allows a developer to service 100 personal repositories with a single, resource-efficient local worker!
