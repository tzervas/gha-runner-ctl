# gha-fleet-debug-20260722T031027Z

Pre-drain troubleshooting capture for improving **gha-runner-ctl**.

See **[ANALYSIS.md](./ANALYSIS.md)** first.

```
00-identity.txt
01-systemd.txt
02-config.txt
02-prefer-repos.list
03-process.txt
04-pool-rr-state.txt
05-containers.txt
06-github-queues.tsv
07-mycelium-lang-detail.txt
08-runners-online.jsonl
09-logs.txt
10-rate-limit.txt
ANALYSIS.md
INDEX.md
```

No secrets: tokens redacted in `02-config.txt`. Do not re-add `warm-auth.env`.
