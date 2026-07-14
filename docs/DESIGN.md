# Design notes

## Decision: single runner

We intentionally avoid autoscaling fleets. One Podman container, one registration identity, labels shared by consumers. Demand is “is there a queued/in-progress self-hosted job?”; idle timeout stops the container.

## Decision: snapshot baseline

Official install downloads a large tarball every cold start. `prepare` bakes [actions/runner](https://github.com/actions/runner) (MIT) into an image and copies binaries into a volume. `up` only needs a registration token + `config.sh` + `run.sh`.

## Decision: short-lived registration tokens

UI tokens expire and must not be stored. The controller uses `GH_TOKEN` / `GITHUB_TOKEN` / `gh auth token` to call:

- `POST /repos/{owner}/{repo}/actions/runners/registration-token`, or  
- `POST /orgs/{org}/actions/runners/registration-token`

Token is written to a `0600` env file, passed to Podman, then deleted.

## Alternatives rejected

| Idea | Why not |
|---|---|
| One runner process per repo | Scales with repo count; defeats “one host” |
| Always-on VM without idle down | Wastes RAM/CPU when idle |
| Embed UI registration token in git | Secret leak; short-lived tokens exist for a reason |
| Full ARC / Kubernetes | Overkill for a single workstation |

## Attribution

Controller and packaging: original work, MIT.  
Runner binary: GitHub’s `actions/runner`, MIT, cited in NOTICE.
