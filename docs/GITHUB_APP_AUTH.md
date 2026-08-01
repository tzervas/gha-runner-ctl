# GitHub App authentication

Closes [#41](https://github.com/tzervas/gha-runner-ctl/issues/41).

A first-class, parameterised alternative to the long-lived `GH_TOKEN`/`GITHUB_TOKEN` PAT
path: three `Cli` flags (with matching env vars, exactly like every other option this
tool has), installation-id auto-discovery, and a `doctor` subcommand that checks it all
without ever printing a secret.

## Why

`listen` re-scans every repo in `GHA_PRIORITY_REPOS` every tick — on the homelab
instance that's 21–22 repos, `GHA_API_MAX_PER_POLL=60`, a 45s poll interval, ≈80 GETs/min,
≈4,800 GETs/hour. A classic PAT is capped at 5,000 requests/hour, so the controller runs
at ~96% of its budget and `listen: list_demand_jobs: budget exhausted mid-scan` fires on
nearly every tick. The poll interval can't be lowered further — the credential is the
throttle, not the config.

A **GitHub App installation token** raises that ceiling. GitHub scales an installation's
limit with the size of the installation, so there is no single universal figure —
measured on the `tzervas` personal-account installation (all repositories,
2026-07-31): **12,500 requests/hour**, 2.5x a classic PAT's 5,000/hour. Limits scale with
installation size — always check the live figure with `gha-runner-ctl doctor` or
`GET /rate_limit` rather than assuming a number; this tool never hardcodes one either.

## CLI flags

| Flag | Env var | Required | Notes |
|---|---|---|---|
| `--app-id` | `GHA_APP_ID` | yes, to enable App auth | the App's numeric ID, **not** the Client ID (`Iv1.…`/`Iv23…`) |
| `--app-installation-id` | `GHA_APP_INSTALLATION_ID` | **no** | omit it and it's auto-discovered — see below |
| `--app-private-key` | `GHA_APP_PRIVATE_KEY` | yes, to enable App auth | `secret:<group>/<key>` (recommended), `file:<path>`, or a bare path — never inline PEM |

Like every other option in this tool, each is declared on the `Cli` struct as
`#[arg(long, env = "...", global = true)]`: an explicit flag wins over the env var, the
env var is used when the flag is absent, and both are visible in `--help`. Nothing here
is special-cased — `gha-runner-ctl` never reads `GHA_APP_*` via `std::env::var` directly,
it reads the already-resolved `Cli` fields.

**Selection is all-or-nothing, and never a silent downgrade.** If neither `--app-id`/
`GHA_APP_ID` nor `--app-private-key`/`GHA_APP_PRIVATE_KEY` (nor `--app-installation-id`)
is set, App auth is off and the existing `GH_TOKEN`/`GITHUB_TOKEN`/GCM/`gh auth
token`/config-file/interactive-prompt chain runs exactly as before — **existing
deployments need zero config changes**. If *any* App-auth flag/env is set but `--app-id`
and `--app-private-key` aren't both present, or once fully configured the mint itself
fails (bad key, wrong installation id, revoked App), that's a **hard error** — the run
does not fall back to the PAT path. A typo'd env var name silently reverting to PAT auth
would look like a working setup right up until the rate limit runs out; refusing to
guess is the whole point.

## The private key: three accepted forms

```
--app-private-key secret:runner/gha-app-key   # recommended
--app-private-key file:/path/to/key.pem       # explicit path
--app-private-key /path/to/key.pem            # bare path (same as file:)
```

**`secret:<group>/<key>` (recommended).** `gha-runner-ctl` retrieves the key itself by
shelling out to the existing `secret` CLI (`secret get <group>/<key>`) — it never
vendors, forks, or reimplements SOPS/age/the `secret` script, only consumes it, the same
way it already shells out to `openssl` for JWT signing. The vault stores the PEM
base64-encoded on a single line (`secret set` refuses values containing whitespace), so
the tool auto-detects the encoding: if the retrieved blob starts with `-----BEGIN` it's
used as-is, otherwise it's base64-decoded and used if *that* starts with `-----BEGIN`;
neither shape working is a clear "vault entry is neither raw PEM nor base64-encoded PEM"
error. The decoded PEM is written to a `0600` file on tmpfs (`/dev/shm`, falling back to
`$TMPDIR`/`/tmp` with a warning if `/dev/shm` doesn't exist) for the span of a single
signing operation, then shredded (overwritten, then removed) — including on error paths.
`secret` not being on `PATH` is a named, actionable error, not a silent failure.

**`file:<path>` / a bare path.** An existing key file on disk. Either way, the file must
be `0600` (or `0400`) — group- or world-readable key files are refused outright, with the
offending mode and the `chmod 600 <path>` fix in the error.

**Inline PEM content is refused, always.** If the flag or env var value contains
`-----BEGIN` — with or without a `file:`/`secret:` prefix — `gha-runner-ctl` errors
immediately, before even trying to use it, and `prevent_raw_token_args` additionally
scans argv for the same pattern at process start (the same defense already in place for
raw `ghp_`/`gho_`/etc. tokens). An env var is readable via `/proc/<pid>/environ` by
anyone who can already read the process, is far more likely to leak into shell history /
`env` dumps / CI logs than a `0600` file, and can't be `chmod`-restricted the way a file
can — so this isn't offered as an option at all, on either the flag or the env var.

Key material is never passed on argv (RS256 signing shells out to
`openssl dgst -sha256 -sign <path>` with the signing input over stdin and the signature
back over stdout — neither the JWT nor the key ever appears in `/proc/<pid>/cmdline`) and
never logged (secret-carrying values use hand-written `Debug` impls that redact
themselves, as a second line of defense beyond call-site discipline).

## Installation id: auto-discovery

`--app-installation-id`/`GHA_APP_INSTALLATION_ID` is **optional**. Looking it up by hand
(Settings → Installations → Configure → read the id out of the URL) is a manual,
easy-to-mistype step; when it's omitted (with `--app-id`/`--app-private-key` set),
`gha-runner-ctl` calls `GET /app/installations` with the App JWT and picks one:

1. **An explicit `--app-installation-id` always wins** — no network call at all.
2. If an **owner hint** is available (`--owner`, `--user`, or the owner half of
   `--repo owner/name`) and it matches exactly one installation's account, use that one.
3. If there is **exactly one installation total**, use it (regardless of any owner hint).
4. Otherwise:
   - **zero installations** — the App exists but isn't installed anywhere. Error names
     the install URL: `https://github.com/settings/apps/<slug>/installations`.
   - **more than one, with no unique owner match** — error listing every `id`/account
     pair found, telling you to pass `--app-installation-id` explicitly.

The resolved id is cached in memory (keyed by App id + owner hint) for the process
lifetime — auto-discovery costs one extra request per process, never per mint. A
successful auto-discovery logs (to stderr, no secrets): `appauth: auto-discovered
installation id=150429495 account=tzervas`.

## `doctor`: check it all without printing secrets

```
$ gha-runner-ctl doctor
gha-runner-ctl doctor
======================
[PASS] auth path: GitHub App (app_id=4451176)
[PASS] app: tzervas-fleet-runner-ctl (id=4451176, slug=tzervas-fleet-runner-ctl)
[PASS] installation: id=150429495 account=tzervas repository_selection=all
[PASS] permissions granted: actions=read, administration=write, metadata=read
[PASS] permissions: cover the documented set (actions:read, administration:write, metadata:read)
[PASS] token acquired via GitHub App installation token
[PASS] rate limit (core, live): 12499/12500 remaining
======================
all checks passed
```
(Real output from a live run against the `tzervas-fleet-runner-ctl` App / installation
150429495, with the auth path forced to App auth. No token, JWT, or key material is ever
printed — installation ids and account logins aren't secret.)

`doctor` is a separate subcommand rather than folded into `status`, because `status`
requires a resolved scope (`--repo`/`--owner`/`--user`/`--auto`) and reports
container/volume/registration state; `doctor` is the thing to run *first*, with no other
flags, before any of that is set up — "is the credential even going to work." It reports:

- which auth path is active: GitHub App, or which link in the PAT chain (`GH_TOKEN`/
  `GITHUB_TOKEN` env, git credential helper, `gh auth token`, config file, interactive)
- for App auth: app id, slug, name, installation id, account, `repository_selection`,
  and the granted permissions — checked against the documented set below, so a
  mis-scoped App fails with `[FAIL] permissions: missing/under-scoped: ...` and a link to
  fix it, instead of a confusing 403 mid-`listen`
- the **live** `GET /rate_limit` core budget (limit + remaining) — measured, never a
  constant, for the same reason the 12,500 figure above is stated as *measured*, not
  promised

Every failed check prints `[FAIL]` with text naming the fix, not just what broke — a bad
key, a missing permission, and a scope-vs-rate-limit 403 are different failures with
different fixes, and the message says which one you're looking at. `doctor` exits
non-zero if anything failed.

## Setting up the App (owner action — the App does not exist yet)

### 1. Create the App

GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**.

- **Homepage URL**: anything valid (e.g. `https://github.com/tzervas/gha-runner-ctl`)
- **Webhook**: uncheck "Active" — this tool polls, it doesn't need webhook delivery
- **Where can this GitHub App be installed?**: "Only on this account"

### 2. Permissions — set exactly these, nothing broader

The controller only ever calls three endpoint families (see `src/lib.rs`:
`registration_token`, the `actions/runs` demand-poll GETs, and the `actions/runs/{id}/jobs`
GETs), so the permission set is narrow and specific — this is also exactly the set
`doctor` checks for:

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

### 3. Install the App on the target account

App settings page → **Install App** → select the account (personal or org) the runners
should act on behalf of.

Choose repository access:
- **All repositories** — simplest; matches `--scope user` batch polling of "whichever
  owned repo has demand," and needs no maintenance when repos are added.
- **Only select repositories** — if you want to scope tightly, the selected list **must**
  cover everything in `GHA_PRIORITY_REPOS` / `GHA_PREFER_REPOS_FILE` / `GHA_ALLOWLIST_REPOS`,
  or those repos will silently stop being pollable (a 404/403 from the App's own installation
  scope, not a rate-limit issue — `doctor`/the demand-poll error text now say so explicitly,
  don't confuse the two failure modes).

### 4. Obtain the App ID and private key (installation id is optional — see auto-discovery)

- **App ID**: shown at the top of the App's settings page
  (`github.com/settings/apps/<your-app-slug>`).
- **Private key**: same App settings page → **Private keys** → **Generate a private key**.
  This downloads a `.pem` once — GitHub does not retain a copy. Treat the download as the
  only copy until it's in the vault.
- **Installation ID** (optional): if you want to skip auto-discovery, Settings →
  Installations → click **Configure** next to the installation → read it from the URL:
  `https://github.com/settings/installations/<INSTALLATION_ID>`. Most setups can just
  omit `--app-installation-id` and let `gha-runner-ctl` find it.

### 5. Store the private key (ciphertext only — never commit plaintext)

Encrypt the downloaded `.pem` with whatever the existing `secret` vault toolchain uses on
this fleet (SOPS/age-backed, per `docs/SECURITY.md`), e.g.:

```bash
base64 -w0 downloaded-key.pem | secret set runner/gha-app-key
shred -u downloaded-key.pem   # or: rm -P / a secure-delete equivalent
```

then point `gha-runner-ctl` at it directly — no manual decrypt/tmpfs/trap dance needed,
the tool does that internally:

```bash
gha-runner-ctl --app-id 123456 --app-private-key secret:runner/gha-app-key listen …
# or, equivalently, via env:
GHA_APP_ID=123456 GHA_APP_PRIVATE_KEY=secret:runner/gha-app-key gha-runner-ctl listen …
```

If this fleet's `secret` tool isn't what's managing the key, or you're not using this
vault at all, `file:<path>` still works exactly as before: decrypt to a `0600` file
(ideally on tmpfs, e.g. `/dev/shm`) and point `--app-private-key`/`GHA_APP_PRIVATE_KEY`
at it yourself:

```bash
secret exec GHA_APP_PRIVATE_KEY_B64=runner/gha-app-key -- bash -c '
  key="$(mktemp /dev/shm/gha-app-key.XXXXXX.pem)"
  trap "rm -f \"$key\"" EXIT
  umask 077
  printf %s "$GHA_APP_PRIVATE_KEY_B64" | base64 -d > "$key"
  GHA_APP_ID=123456 GHA_APP_PRIVATE_KEY="file:$key" exec gha-runner-ctl listen …
'
```

### 6. Verify

```bash
gha-runner-ctl --app-id 123456 --app-private-key secret:runner/gha-app-key doctor
```

should print `[PASS]` for every check and exit `0` (see the sample output above). If
`--app-id`/`--app-private-key` are only partially set (e.g. a typo dropped one), or the
key/installation is wrong, `doctor` prints exactly which check failed and why — that's
the point of running it first, before `--scope repo --auto detect` or `listen`.

## Requirements on the host running `gha-runner-ctl`

- **`openssl`** must be on `PATH`. RS256 JWT signing shells out to
  `openssl dgst -sha256 -sign <key-path>` (stdin/stdout only — the signing input and the
  key never appear in `/proc/<pid>/cmdline`) instead of adding a new crate dependency
  (`jsonwebtoken`/`ring`/`rsa`) just to mint a JWT roughly once an hour.
- **`secret`** must be on `PATH` if (and only if) you use the `secret:<group>/<key>`
  form. `file:<path>`/bare-path setups don't need it.

See `src/appauth.rs` for the full implementation rationale.

## What did *not* change

- `GHA_APP_PRIVATE_KEY_PATH` (a separate env var some earlier drafts of this feature
  used for the key path) was deliberately **not** adopted — there is exactly one flag/env
  var for the key, `--app-private-key`/`GHA_APP_PRIVATE_KEY`, which now accepts
  `secret:`/`file:`/bare-path forms instead of needing a second name.
- No compiled-in hourly rate-limit figure exists anywhere in this codebase, on purpose —
  see the note in `src/appauth.rs` next to where such a constant used to live. Installation
  limits scale with installation size; `doctor`/`GET /rate_limit` report the real number.
- No workflow files were touched.
- `GH_TOKEN`/`GITHUB_TOKEN` and every other existing auth path are untouched and remain
  the default when no `--app-*`/`GHA_APP_*` flags or env vars are set — verified live by
  running `doctor` with only `GH_TOKEN` set: it reports `auth path: not using GitHub App
  auth` and `token acquired via GH_TOKEN environment variable`, exactly as before this
  feature existed.
