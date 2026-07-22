# `gha_pool` — native Mycelium port (staging dogfood)

This is a **staging** proof-of-concept: the *pure-logic* core of `gha-runner-ctl`
reimplemented as a native Mycelium phylum and validated against the Rust semantics.
It exists to dogfood the Mycelium language on the port-able fragment of this tool and
to make the readiness gaps concrete. See [`../docs/PORTING_TO_MYCELIUM.md`](../docs/PORTING_TO_MYCELIUM.md)
for the full gap analysis.

## What is ported (native `.myc`, type-checks + runs today)

Mirrors `src/pool.rs` and the `ApiPacer` in `src/lib.rs`:

| Mycelium fn | Rust origin | Notes |
|---|---|---|
| `tier_cpus_milli` / `tier_mem_mib` | `resources_for_tier` | CPU as **milli-cores**, RAM as **MiB** (integer model — Mycelium's numeric family is `Binary{N}`, no floats/strings in this fragment) |
| `tier_fits` | pool budget check | whole-tier fit against a free budget |
| `fit_to_budget` | `fit_to_budget` | clamp want→free, never below min; `NoFit` when free < min |
| `backoff_init` / `backoff_next` | `ApiPacer` backoff | init clamps to `[5, 900]s`; next doubles with a 900s ceiling. Doubling uses `add_u` — **never-silent** (overflow refuses, never wraps) |

`check_all` is a differential battery: each case equals the value the Rust code
produces for the same input (the oracle constants were computed from the Rust
semantics at generation time). `main` returns `Binary{1}` = `0b1` iff all agree.

## What stayed in Rust (and why — these are the real gaps)

Everything that touches the host or Rust's imperative/string surface, i.e. the
bulk of the tool's value:

- `size_for_job` / `tier_from_labels` — operate on **`String` labels**; Mycelium
  has no `String` in this fragment (only `Binary{N}` + `Bytes` in the compiler lib).
- `parse_cpus_f64` / `parse_memory_mib` — **`f64`** and string parsing; no float family here.
- `PoolState::try_claim`, config/lock I/O — **filesystem + mutation**; the rich fs
  API is in-memory only today (real-OS floor deferred, flag M-541).
- All GitHub API calls, `podman`/`git`/`gh` spawning — **networking + subprocess**;
  neither exists in the stdlib, and there is no FFI host to bridge to Rust yet
  (`wild {}` parses but does not execute — `ElabError::Residual`, "no FFI host in v0").

## Run it

```bash
# with `myc` on PATH (built from tzervas/mycelium-cli @ the components.lock pin)
cd mycelium-port
myc check      # => 1 nodule(s) checked clean
myc run        # => evaluates `main` to Binary{1} Bits([true])
```

Built/verified against the `v0.464.0` Rust train (toolchain pin `1.96.1`).
