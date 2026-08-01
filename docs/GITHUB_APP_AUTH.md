# GitHub App authentication (opt-in)

Closes [#41](https://github.com/tzervas/gha-runner-ctl/issues/41).

## Why

`listen` re-scans every repo in `GHA_PRIORITY_REPOS` every tick — on the homelab
instance that's 21–22 repos, `GHA_API_MAX_PER_POLL=60`, a 45s poll interval, ≈80 GETs/min,
≈4,800 GETs/hour. A classic PAT is capped at 5,000 requests/hour, so the controller runs
at ~96% of its budget and `listen: list_demand_jobs: budget exhausted mid-scan` fires on
nearly every tick. The poll interval can't be lowered further — the credential is the
throttle, not the config.

A **GitHub App installation token** raises that ceiling. GitHub scales an installation's limit
with the size of the installation, so there is no single universal figure — measured on the
`tzervas` personal-account installation (all repositories, 2026-07-31): **12,500 requests/hour**,
2.5x a classic PAT's 5,000/hour. That is what
what makes a faster, steadier poll sustainable.

## What changed (source)

`github_token()` in `src/lib.rs` now checks for GitHub App configuration first
(`src/appauth.rs`). If `GHA_APP_ID`, `GHA_APP_INSTALLATION_ID`, and `GHA_APP_PRIVATE_KEY`
are **all** set, it mints (and caches) an installation token instead of falling through to
the existing `GH_TOKEN`/`GITHUB_TOKEN`/GCM/`gh auth token`/config-file/interactive-prompt
chain. If none, or only some, are set, behavior is **unchanged** — existing deployments
need zero config changes. If they're fully set but broken (bad key, wrong installation id,
revoked App), the mint fails loudly; there is no silent fall-back to the PAT path, since
that could mask a real misconfiguration or mint against the wrong identity.

The installation token is cached in memory and re-minted only when within ~5 minutes of
its ~1h expiry — never per-request, which would defeat the purpose.

## Setting up the App (owner action — the App does not exist yet)

### 1. Create the App

GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**.

- **Homepage URL**: anything valid (e.g. `https://github.com/tzervas/gha-runner-ctl`)
- **Webhook**: uncheck "Active" — this tool polls, it doesn't need webhook delivery
- **Where can this GitHub App be installed?**: "Only on this account"

### 2. Permissions — set exactly these, nothing broader

The controller only ever calls three endpoint families (see `src/lib.rs`:
`registration_token`, the `actions/runs` demand-poll GETs, and the `actions/runs/{id}/jobs`
GETs), so the permission set is narrow and specific:

| Permission | Level | Why |
|---|---|---|
| **Repository → Actions** | Read-only | `GET /repos/{owner}/{repo}/actions/runs` and `.../jobs` — the demand-polling GETs that dominate the rate-limit budget |
| **Repository → Administration** | Read and write | `POST /repos/{owner}/{repo}/actions/runners/registration-token` — minting a runner registration token is an Administration-scoped operation on GitHub's API, not an Actions one |
| **Repository → Metadata** | Read-only | Mandatory baseline for any GitHub App; also covers repo-listing endpoints (`GET /orgs/{owner}/repos`, `GET /users/{user}/repos`) used by `--scope user`/`--scope org` |

**If `--scope org` is used** (org-level runner registration via
`POST /orgs/{org}/actions/runners/registration-token`), also grant:

| Permission | Level | Why |
|---|---|---|
| **Organization → Self-hosted runners** | Read and write | Required for the org-level registration-token endpoint; this is a separate, org-scoped permission from the repo-level Administration one above |

Do **not** grant Contents, Issues, Pull requests, Workflows, or anything else — the
controller never touches them, and least-privilege here bounds the blast radius of a
leaked installation token (which is already short-lived, but still).

### 3. Install the App on the `tzervas` account

App settings page → **Install App** → select the `tzervas` account (this must be a
**personal-account** installation, not an org — the App is created under `tzervas`'s
GitHub identity to act on `tzervas`-owned repos).

Choose repository access:
- **All repositories** — simplest; matches `--scope user` batch polling of "whichever
  owned repo has demand," and needs no maintenance when repos are added.
- **Only select repositories** — if you want to scope tightly, the selected list **must**
  cover everything in `GHA_PRIORITY_REPOS` / `GHA_PREFER_REPOS_FILE` / `GHA_ALLOWLIST_REPOS`,
  or those repos will silently stop being pollable (a 404/403 from the App's own installation
  scope, not a rate-limit issue — don't confuse the two failure modes).

### 4. Obtain App ID, Installation ID, private key

- **App ID**: shown at the top of the App's settings page
  (`github.com/settings/apps/<your-app-slug>`).
- **Installation ID**: Settings → Installations → click **Configure** next to the
  installation on `tzervas` → read it from the URL:
  `https://github.com/settings/installations/<INSTALLATION_ID>`.
- **Private key**: same App settings page → **Private keys** → **Generate a private key**.
  This downloads a `.pem` once — GitHub does not retain a copy. Treat the download as the
  only copy until it's in the vault.

### 5. Store the private key (ciphertext only — never commit plaintext)

Encrypt the downloaded `.pem` with whatever the existing `secret` vault toolchain uses on
this fleet (SOPS/age-backed, per `docs/SECURITY.md`) under a path such as
`runner/gha-app-key`, then delete the plaintext download.

`gha-runner-ctl` **only** accepts the key as a filesystem path (`GHA_APP_PRIVATE_KEY`,
optionally prefixed `file:`) — never inline PEM content in an env var. That's deliberate:
an env var is readable via `/proc/<pid>/environ` by anyone who can already read the
process, is far more likely to leak into shell history / `env` dumps / CI logs than a
`0600` file, and can't be `chmod`-restricted the way a file can.

At process start, decrypt to a `0600` file (ideally on tmpfs, e.g. `/dev/shm`, so nothing
touches persistent disk) and point `GHA_APP_PRIVATE_KEY` at it:

```bash
secret exec GHA_APP_PRIVATE_KEY_B64=runner/gha-app-key -- bash -c '
  key="$(mktemp /dev/shm/gha-app-key.XXXXXX.pem)"
  trap "rm -f \"$key\"" EXIT
  umask 077
  printf %s "$GHA_APP_PRIVATE_KEY_B64" | base64 -d > "$key"
  GHA_APP_ID=123456 \
  GHA_APP_INSTALLATION_ID=78901234 \
  GHA_APP_PRIVATE_KEY="file:$key" \
  exec gha-runner-ctl listen …
'
```

(`GHA_APP_PRIVATE_KEY_B64` above is illustrative of "get ciphertext out of the vault and
onto disk as a 0600 file, transiently, without ever printing it" — adapt the exact
encrypt/decrypt commands to however this fleet's `secret` tool already handles file-shaped
secrets, if it has a more direct "decrypt to file" mode. The important invariants are: (a)
ciphertext at rest in the vault, never plaintext in git; (b) the decrypted file is `0600`
and, ideally, on tmpfs; (c) `GHA_APP_PRIVATE_KEY` is set to a path, never to key content.)

### 6. Verify

```bash
gha-runner-ctl --scope repo --auto detect
```

A successful App-auth mint logs (to stderr, never the token itself):

```
auth: minted GitHub App installation token (app_id=123456, installation_id=78901234, expires in 3540s)
```

If `GHA_APP_ID`/`GHA_APP_INSTALLATION_ID`/`GHA_APP_PRIVATE_KEY` are only partially set
(e.g. a typo dropped one), you'll instead see a warning on stderr naming exactly which
var is missing, and the run falls back to `GH_TOKEN`/PAT discovery unchanged.

## Requirements on the host running `gha-runner-ctl`

- **`openssl`** must be on `PATH`. RS256 JWT signing shells out to
  `openssl dgst -sha256 -sign <key-path>` (stdin/stdout only — the signing input and the
  key never appear in `/proc/<pid>/cmdline`) instead of adding a new crate dependency
  (`jsonwebtoken`/`ring`/`rsa`) just to mint a JWT roughly once an hour. See
  `src/appauth.rs` for the full rationale.

## What did *not* change

- No CLI flags were added — App auth is env-var-only, matching how `GH_TOKEN` itself is
  read (not a `clap` arg either).
- No workflow files were touched.
- `GH_TOKEN`/`GITHUB_TOKEN` and every other existing auth path are untouched and remain
  the default when App-auth env vars are absent.
