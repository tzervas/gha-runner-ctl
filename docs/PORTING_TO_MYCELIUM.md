# Porting `gha-runner-ctl` to Mycelium — readiness & gap analysis

**Status:** staging / planning. Part of the `claude/mycelium-readiness-gaps` review
(2026-07-22), measured against the Mycelium Rust train **`v0.464.0`**
(`mycelium-lang` `components.lock`).

`gha-runner-ctl` is one of two designated first-порt targets for Mycelium (the other
is `tg-agent-relay`). This document records what a **native Mycelium** port requires,
what is portable **today**, and what must stay in **Rust** until specific language
gaps close. A working proof-of-concept of the portable fragment lives in
[`../mycelium-port/`](../mycelium-port/) — it type-checks and runs under `myc`.

## TL;DR

- This tool is an unusually **good** first target: ~4.6k LOC, 5 dependencies,
  **fully synchronous** (no `async`/tokio), no `unsafe`, light on generics/traits.
  Its concurrency is a blocking poll loop + `sleep` — so Mycelium's lack of `async`
  is **not** on the critical path.
- The **pure-logic core** (resource-tier accounting, budget fitting, backoff pacing,
  enum/`match` validation, constant-time compare) ports cleanly and is **already
  demonstrated** natively — see `mycelium-port/`.
- ~80–90% of the tool's *value*, however, lives in **host effects the language cannot
  express or bridge today**: subprocess spawning (`podman`/`git`/`gh`), HTTPS+TLS to
  the GitHub API, JSON, and CLI parsing.
- **The blocker is not "add an HTTP library."** The extracted train has **no FFI host**
  (`wild {}` parses/type-checks but does not execute — `ElabError::Residual`, "no FFI
  host in v0"), so even a Rust-backed shim has **no seam to call through** yet. The
  linchpin gap is upstream, in the language (see the planning doc referenced below).

## Empirical measurement (transpiler `--vet`)

`mycelium-transpile --vet src/ …` (the Rust→Mycelium gap **profiler**, run against
this repo at the `v0.464.0` train):

| Metric | Value |
|---|---|
| Top-level items (non-test) | 192 |
| Items emitted (some `.myc` draft) — `expressible_fraction` | **32 / 192 = 16.7%** |
| Items that actually type-check (`myc check`) — `checked_fraction` | **0 / 192 = 0.0%** (file-gated) |
| Files with any fully-clean emission | 0 / 2 |

Gap categories (union over `src/`), most-common first:

| Category | Count | Meaning for this port |
|---|---:|---|
| `Other` — method calls w/ no free-fn referent | 31 | Rust `x.method()` has no Mycelium mapping (free functions only) |
| `MultiStmtBody` | 38 | multi-statement / imperative fn bodies not expressible |
| `Import` (`use`) | 22 | no cross-module symbol resolution in the transpiler |
| `Other` — non-unsigned types | 14 | only plain unsigned ints co-emit; `String`/structs/`f64` gap |
| `Other` — unit-returning fns | 13 | **"no unit value is representable in this grammar"** — side-effecting fns have no home |
| `DeriveSatisfied`/`DeriveAttr` | 20 | `#[derive(...)]` |
| `Struct` | 11 | named-field records |
| `MacroInvocation` | 6 | `println!`/`format!`/`vec!` … |
| `Impl` / `NamedFieldDrop` | 10 | inherent/trait impls; named→positional field loss |

Read honestly: **0.0% `checked_fraction` does not mean "0% portable."** It means the
*automatic* transpiler emits nothing that type-checks unmodified — because idiomatic
Rust here is imperative, method-call-heavy, string-heavy, and unit-returning, none of
which the pure Mycelium fragment expresses. Hand-porting the *pure* logic works fine
(the `mycelium-port/` PoC is proof); the transpiler is a **measurement instrument**,
not a porter.

## Capability map: native-now vs. new-stdlib vs. Rust-bridge

| Capability (this tool) | Class | Detail |
|---|---|---|
| Enum/`match` mode/scope/policy resolution, allowlist checks, pacing/backoff math, resource-tier accounting, constant-time compare | **(a) native today** | Pure, total, finite/unsigned — demonstrated in `mycelium-port/` |
| Config/lock file read/write, `/proc` & `/dev/null` reads | (a/b) | Thin real-OS `std-sys` fs floor exists; richer `std-fs` is in-memory only (M-541) |
| Env-var **reads**, `sleep`, monotonic/wall clocks | (a/b) | Present in `std-sys`; env **mutation** / cwd absent |
| **JSON** (serde_json) | **(b) new stdlib** | pure computation, writable in-language; today only `Value`↔JSON exists |
| **CLI parsing** (clap: 54 attrs) | **(b) new stdlib** | argv→struct is pure logic; the derive/env/subcommand surface must be rebuilt |
| Unix perms (`0o600`), termios no-echo, char-device check | (b) | needs mode-setting + terminal APIs in stdlib |
| **Subprocess** spawn `podman`/`git`/`gh`/`kill` (~20 sites — the tool's core mechanism) | **(c) Rust-bridge** | no process API in stdlib **and** no FFI host to bridge through |
| **HTTPS + TLS** to `api.github.com` (registration tokens, demand polling, downloads) | **(c) Rust-bridge** | no sockets, no TLS **and** no FFI host |
| Inbound TCP wake server (127.0.0.1) + 1 background thread | (c) | optional feature; can be dropped for a first port |

## Ranked blockers for this port

1. **FFI / host-effect execution path in the language** (upstream; `mycelium-l1` /
   interp). Until `wild {}` executes against a host-function table, no Rust shim can be
   called — this gates every (c) row. **This is the linchpin.**
2. **Subprocess/exec host capability** — the tool *is* a `podman`/`git`/`gh` orchestrator.
3. **HTTPS + TLS host capability** — the GitHub API half of the core loop.
4. **JSON codec for arbitrary values** (buildable natively).
5. **CLI argument parsing** (buildable natively).

Items 2–3 become thin Rust-backed host functions *once item 1 exists*; 4–5 can be pure
Mycelium. See the umbrella planning doc in `mycelium-lang`
(`docs/planning/PORT-READINESS-2026-07-22.md`) for the new phyla proposed to close
these, and the per-component gap notes in `mycelium-std-sys`, `mycelium-std-io`,
and `mycelium-l1`.

## Suggested sequencing

1. **Now:** keep/extend `mycelium-port/` — port more of the pure core (mode/scope/
   image-mode/pull-policy resolution, the full `SizeTier` label logic minus string
   parsing) with differential tests vs. the Rust. Zero dependence on upstream gaps;
   real dogfood that surfaces frontend/stdlib bugs.
2. **Upstream unlock:** track the FFI-host + real-OS-floor work (blocker #1/#2/#3).
3. **Then:** replace the Rust host-effect surface with Mycelium calling Rust-backed
   `spawn` / `https` host functions; author JSON + CLI natively. At that point an
   end-to-end Rust-bridged port of this tool is the right **first full** Mycelium port.
