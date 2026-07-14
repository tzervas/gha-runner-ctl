# Changelog

## 0.1.1

- Fail-closed validation for repo/owner/labels/names/image/cpus/memory
- Secret redaction on errors; registration env file overwrite+unlink
- HTTP timeouts; single-instance flock on `up` / `listen`
- Podman: `no-new-privileges`, `--pull=never` on hot path
- Wake server requires `GHA_WAKE_TOKEN`; constant-time compare
- Entrypoint validates `https://github.com/…` only; never logs tokens
- SECURITY.md operator checklist
- **Distributed release assets**: Linux x86_64 tarball + SHA256SUMS via
  `scripts/dist.sh` and tag workflow (required for host updates without cargo)

## 0.1.0

- Initial release: one Podman runner, snapshot `prepare`, auto-registration, `listen` up/down
- Repo and org registration scopes
- MIT license; NOTICE cites [actions/runner](https://github.com/actions/runner) (MIT)
