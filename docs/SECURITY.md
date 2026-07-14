# Security model

## Threats we design for

| Threat | Mitigation |
|---|---|
| Shell injection via labels/names/repo | Allowlist charset validation before Podman/API |
| Registration / PAT leakage in logs | Redact secrets; never print tokens; scrub API errors |
| Env-file residue | `0600` under `XDG_RUNTIME_DIR`, overwrite + unlink after `up` |
| Privilege escalation in container | `--security-opt no-new-privileges`, no docker.sock mount |
| Surprise image pulls on `up` | `--pull=never` after `prepare` |
| Unauthenticated wake endpoint | Loopback only; **requires** `GHA_WAKE_TOKEN` (≥16 chars) |
| Twin controllers racing | Exclusive flock on `listen` / `up` |
| Public fork abuse of self-hosted | Prefer private repos; documented warning on `up` |
| Stale registration after ephemeral job | Wipe `.runner` / credentials on `down` in ephemeral mode |

## Org vs personal repos

Organization runners only serve **repositories in that org**. Personal
`user/repo` workflows cannot use `vectorweighttechnologies` runners while
staying outside the org. See README.

## Operator checklist

- [ ] `gh auth` / `GH_TOKEN` with least privilege for registration only  
- [ ] Runner groups in org UI limited to intended repos  
- [ ] Prefer **private** repos on self-hosted compute  
- [ ] Do not commit registration tokens or `GHA_WAKE_TOKEN`  
- [ ] Keep `gha-runner-ctl` and the image pin current (runner sha256 in Containerfile)  

## Reporting

Open a private security advisory on the GitHub repo if you find a vulnerability.
