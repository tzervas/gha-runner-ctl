# GitLab CE adapter MVP (gha-runner-ctl)

**Status:** design + live API spike (2026-07-29). Not full listen-loop parity.  
**Issues:** [#87](https://github.com/tzervas/gha-runner-ctl/issues/87) · fleet-ops [#126](https://github.com/tzervas/fleet-ops/issues/126)  
**Live forge:** `https://git.vectorweight.com` (GitLab CE **19.2.0**, rootful podman unit `gitlab`, Caddy edge)

## Honest binary boundary

| | GitHub Actions | GitLab CI |
|---|---|---|
| Job agent binary | `actions/runner` (`config.sh` / `run.sh`) | **`gitlab-runner`** (not the same binary) |
| Registration credential | Short-lived **registration token** (~1 h) from REST | Durable **`glrt-…` runner auth token** from `POST /api/v4/user/runners` |
| One-job-then-exit | `--ephemeral` | `--max-builds 1` |
| Demand signal | `actions/runs?status=queued` | CE: **per-project** `…/jobs?scope=pending`; **no** instance-wide pending endpoint |

The fleet agent stays a **control plane**. It does **not** speak the job protocol itself. For GitLab it will:

1. Mint / revoke runners via the **GitLab Runner API** (this MVP).
2. Spawn a **resource-capped Podman worker** that runs the official `gitlab-runner` image/binary with `max-builds=1` (listen-loop parity — next phase).
3. Never mount host `docker.sock` / rootful god-mode into the worker.

Smallest working path for CE: **API mint + podman `gitlab/gitlab-runner` register/run/unregister**, not a reimplementation of the job wire protocol.

## Lifecycle (same ideas as GH path)

```text
  long-lived listen/control plane
       │  paced mint (reg budget shared host file lock — reuse GH pacing files or forge-scoped)
       │  secret exec ONLY for long-lived PAT (gitlab/api-token)
       ▼
  POST /api/v4/user/runners  →  (id, glrt-…)
       │  glrt never in argv; 0600 runtime file or env file shredded after inject
       ▼
  podman run --rm  resource caps (fleet slice / tier)
       gitlab-runner register --non-interactive --token <glrt> --url <url> …
       gitlab-runner run --max-builds 1
       │  exit after one job (or idle timeout)
       ▼
  DELETE /api/v4/runners/:id   (always — glrt does not expire)
```

### Critical security difference

GitHub registration tokens expire in ~1 hour. GitLab `glrt-` tokens **do not expire** (`token_expires_at: null` measured live). A leaked glrt is durable job-execution authority. Therefore:

- Extend `prevent_raw_token_args` + `redact` for `glpat-` / `glrt-` / `PRIVATE-TOKEN:`.
- Revoke on every worker teardown (success **and** failure paths).
- Prefer never writing glrt into a long-lived volume; if retained for warm mode, bind 0600 and shred on down.

## MVP scope (this PR)

| In | Out |
|---|---|
| Design doc (this file) | Full `listen` loop for GitLab |
| Spike script: mint / get / delete against live CE | Full `listen` loop |
| Worker script: capped podman `gitlab-runner` register/run + always DELETE | Warm multi-worker pool |
| Secret key name: `gitlab/api-token` via `secret exec` | Gitea/Forgejo |
| Redaction / argv guards for GL token shapes | Renaming crate to `ap-runner-ctl` |
| `run_untagged=false` default (documented) | Instance-wide demand polling (CE unsupported) |

## Secret keys

| Vault key | Purpose | How created | Consume |
|---|---|---|---|
| `gitlab/api-token` | Root (or least-priv) PAT for Runner API | Rails mint or UI; `secret set gitlab/api-token` (stdin) | `secret exec GITLAB_TOKEN=gitlab/api-token -- …` |
| *(not stored long-term)* `glrt-…` | Per-worker runner auth | `POST /user/runners` at claim time | Inject into worker only; delete runner after job |
| Config (non-secret) | API base URL | e.g. `GHA_GITLAB_URL=https://git.vectorweight.com` | Env / instance unit |

**Scopes measured on live CE PAT** (root, expires ~90d): `api`, `create_runner`, `manage_runner`, `read_api`.

**Never print values.** Dual-home: WSL vault has the key today; enroll Pi later with `fleet-sops-add-recipient` / age rekey (same pattern as GH runner keys). Do not copy plaintext between hosts.

## API surface (CE 19.2 proven)

Base: `https://git.vectorweight.com/api/v4`  
Auth header: `PRIVATE-TOKEN: <pat>` (or `Authorization: Bearer <pat>`).

| Call | Purpose | Live result (2026-07-29) |
|---|---|---|
| `GET /version` | Sanity | **200** `{version: 19.2.0, enterprise: false}` |
| `GET /user` | Identity | **200** `root` admin |
| `GET /runners/all` | Inventory | **200** list |
| `POST /user/runners` | Mint runner + `glrt-` | **201** `instance_type`, tags set, `run_untagged=false`, `token_expires_at=null` |
| `GET /runners/:id` | Inspect | **200** `status=never_contacted` until agent connects |
| `DELETE /runners/:id` | Deprovision / revoke glrt | **204** |

### Mint body (MVP defaults)

```json
{
  "runner_type": "instance_type",
  "description": "gha-runner-ctl-<host>-<id>",
  "tag_list": ["self-hosted", "linux", "x64", "podman", "tier-micro"],
  "run_untagged": false,
  "locked": false,
  "maximum_timeout": 3600,
  "access_level": "not_protected"
}
```

`run_untagged` **must default false**. Untagged jobs bypass tier tags and can land a `rustc` build on a micro worker (fleet has already paid for this class of failure on GH).

`runner_type` choices: `instance_type` | `group_type` | `project_type`. MVP uses instance for the supplemental forge; project/group come later with scope config.

### Demand (honest CE limit)

There is **no** instance-wide pending-jobs API on CE. Options for scale-up:

1. Floor + PSI pressure (keep N warm; scale on host pressure) — recommended for instance runners.
2. Enumerate allowlisted projects: `GET /projects/:id/jobs?scope=pending` (paced).
3. Return `Unsupported` for pure instance scope rather than lying with “queue empty”.

## Worker shape (next phase — not in spike)

```text
podman run --rm \
  --name gl-worker-… \
  --cpus <tier> --memory <tier> \
  --cap-drop ALL --security-opt no-new-privileges \
  --network <egress-ok, no host docker.sock> \
  -e CI_SERVER_URL=https://git.vectorweight.com \
  -v <0600 token file>:/runner-token:ro \
  docker.io/gitlab/gitlab-runner:alpine \
  … register + run --max-builds 1 …
```

Resource caps must match existing fleet slices (`micro` → `large`), not an unbounded gitlab-runner unit.

**Do not** install a long-lived host `gitlab-runner` with docker.sock. That is the anti-pattern issue #123 considered and this design rejects for the multi-forge path.

## Config sketch (future CLI — non-breaking)

Preserve all existing GH flags. Additive only:

| Env / flag | Meaning |
|---|---|
| `GHA_FORGE=github\|gitlab` | Default `github` — **existing `gha-runner-ctl@cpu` unchanged** |
| `GHA_GITLAB_URL` | e.g. `https://git.vectorweight.com` |
| `GHA_GITLAB_TOKEN` / secret exec | Long-lived PAT (never argv) |
| `GHA_GITLAB_RUNNER_TYPE` | `instance` / `group` / `project` |
| `GHA_GITLAB_TAGS` | Comma tags (map from GH labels) |

Suggested unit name when enabled: `gha-runner-ctl@gitlab-cpu` (separate instance lock + env), not a second binary.

## Spike

```bash
# requires: secret vault key gitlab/api-token, reachability to the forge
# (WSL→host:443 may be firewalled; script supports SSH tunnel via HOMELAB_SSH)
./scripts/gitlab-ce-spike.sh
```

See script header for tunnel + proof steps. It mints one runner, prints **only** non-secret metadata, deletes it.

## Sequencing to listen-loop parity

1. **Done (spike):** API mint/delete + secrets + docs + token redaction.
2. Trait split (optional intermediate): `Forge` / `RunnerAgent` per `DESIGN-runner-ctl-forges` in fleet-config — **or** thin `gitlab` module without full rename.
3. Podman worker image pin (`gitlab/gitlab-runner` digest) + register/run once.
4. Paced mint budget (reuse host reg budget files, forge-scoped keys).
5. `listen` tick: floor workers + optional per-project pending poll.
6. Enable `shared_runners` / project CI only when a real worker path is green (app settings today prefer CI off).
7. Rename consideration (`ap-runner-ctl`) before crates.io `publish-new` lands.

## Related

- fleet-config design: `DESIGN-runner-ctl-forges.md` (Forge + RunnerAgent traits, `run_untagged`, CE demand honesty)
- fleet-ops GitLab unit + supplemental posture (`gitlab/` on server lanes)
- Do **not** break existing GH `gha-runner-ctl@cpu`


## Worker script (phase 1b)

`scripts/gitlab-ce-worker.sh`:

1. `secret exec` + `POST /user/runners` → `glrt` (0600 temp)
2. SSH to forge host → `podman run` **resource-capped** `gitlab/gitlab-runner` (no docker.sock)
3. `register --non-interactive --executor shell --run-untagged=false`
4. `run --max-builds 1` until online (or timeout)
5. Stop container; trap always `DELETE /runners/:id`

Env: `HOMELAB_SSH`, `HOMELAB_SSH_KEY`, `GHA_GITLAB_URL`, `GITLAB_WORKER_MEMORY` (default 512m), `GITLAB_WORKER_CPUS` (default 1).
