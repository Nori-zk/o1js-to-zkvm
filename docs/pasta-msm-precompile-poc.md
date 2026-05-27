# Pasta MSM precompile POC

A working proof-of-concept that routes the Vesta-side MSM in `accumulator_check` through a new Pasta executor-level precompile in a local SP1 fork. **Execute-only:** the prover-side chip is not implemented in this POC. The goal was to measure the realised cycle reduction and de-risk the path to a production precompile.

## Problem

`make rust-e2e-tests` against `fixtures/mainnet-blockchain-snark`, with `sp1-sdk` built with the `profiling` feature so the cycle tracker is populated:

```
Execution used 4329210424 cycles
  accumulator_check: 2,610,059,324  (60.3%)
  kimchi_dlog_check: 1,561,684,022  (36.1%)
  public_inputs:       118,205,648  ( 2.7%)
  setup:                34,843,447  ( 0.8%)
  batching_rng:            743,080  ( ~0%)
```

Each native Pasta short-Weierstrass `+= affine` runs ~9 base-field muls via the `sys_bigint` precompile + arkworks dispatch overhead = roughly **2,000–2,500 cycles per add**. The Vesta MSM in `accumulator_check` does ~1.5M such adds (Pippenger over the 2^16-base Vesta SRS), totalling 2.6B cycles — 60% of the guest.

Algorithmic MSM optimizations on top of `sys_bigint` (batched-affine, GLV) tap out around 10–15% because the field-mul count is already minimised. The real win is to shrink the per-EC-op cost, which means an EC precompile.

## What this POC does

Adds Pallas + Vesta to a local SP1 fork (`sp1-pasta-fork/`) and re-routes `accumulator_check`'s MSM through new `syscall_vesta_add`/`syscall_vesta_double` syscalls. Each precompile call is billed **256 cycles** (one `ecall`) regardless of the underlying field-mul count.

This is the "executor-only precompile" path Succinct's @tamirhemo suggested in the contribution discussion: "you could experiment with just precompile on the executor layer and see if it gives sufficient reduction in cycles." It works in **execute mode** (cycle measurement, public-input commit), but cannot **prove** — the prover-side chip is not in this POC.

## Results

Mainnet blockchain SNARK fixture, execute mode, profiling on:

| stage              | baseline    | with precompile | Δ                |
| ------------------ | ----------- | --------------- | ---------------- |
| accumulator_check  | 2,610M      | **159M**        | **−94%** on stage |
| kimchi_dlog_check  | 1,562M      | 1,562M          | — (not patched)  |
| public_inputs      | 118M        | 118M            | —                |
| setup              | 35M         | 35M             | —                |
| batching_rng       | 0.7M        | 0.7M            | —                |
| **total**          | **4,329M**  | **1,878M**      | **−57%**         |

The Vesta MSM collapses from 2.6B to 159M cycles — a 16× speedup on that stage. Total guest cycles drop from 4.33B to 1.88B (57% reduction).

## What's in this POC

### SP1 fork — `Nori-zk/sp1` @ [`FEAT/msm-precompile-try-1`](https://github.com/Nori-zk/sp1/tree/FEAT/msm-precompile-try-1)

Branched off upstream `succinctlabs/sp1` at tag `v6.1.0` (single commit `b31e2292d`, +415 LOC across 9 files). Pasta added with no chip changes — the existing `WeierstrassAddAssignChip` is already generic over `EllipticCurve`. Lives locally at `../sp1-pasta-fork/` (this repo's `[patch."https://github.com/succinctlabs/sp1"]` block points there).

- `crates/curves/src/weierstrass/pasta.rs` (new) — `PallasParameters`, `VestaParameters`, `PallasBaseField`, `VestaBaseField`. Modulus LE bytes mirror `mina-curves` `Fp::MODULUS` / `Fq::MODULUS`. A=0, B=5. Generators copied from `mina-curves/src/pasta/curves/{pallas,vesta}.rs`. Unit tests check the LE-bytes modulus matches `BigUint::from_str_radix(...)` and that the generators satisfy `y² ≡ x³ + 5`.
- `crates/curves/src/lib.rs` — `CurveType::Pallas`, `CurveType::Vesta` enum variants.
- `crates/curves/src/weierstrass/mod.rs` — `impl_generic_ec_ops!(pasta::PallasParameters)` + same for Vesta. Uses the same `sw_add`/`sw_double` dashu-modpow path as bn254/bls12-381.
- `crates/core/executor/src/syscall_code.rs` — `PALLAS_ADD=0x...34`, `PALLAS_DOUBLE=0x...35`, `VESTA_ADD=0x...36`, `VESTA_DOUBLE=0x...37`, plus their `from_u32` entries and `None` in `as_air_id` (no chip yet).
- `crates/core/executor/src/minimal/ecall.rs` — dispatch arms for the four codes, generic over `Pallas`/`Vesta`.
- `crates/core/executor/src/vm/syscall.rs` — same dispatch arms (this is the actual path `client.execute()` takes, see "Pitfalls" below).
- `crates/zkvm/entrypoint/src/syscalls/pasta.rs` (new) + `mod.rs` entries — `extern "C" fn syscall_pallas_add(p, q)` etc., one `ecall` each with `in("t0") <code>, in("a0") p, in("a1") q`. Mirrors `bn254.rs` exactly.
- `crates/zkvm/lib/src/lib.rs` — `extern "C"` declarations for the four `syscall_*` functions.

### POC repo changes

- `Cargo.toml` — `[patch."https://github.com/succinctlabs/sp1"]` block pointing the seven SP1 crates we use at the local `../sp1-pasta-fork/` paths.
- `crates/pickles-verifier/Cargo.toml` — `[target.'cfg(target_os = "zkvm")'.dependencies]` adds `sp1-zkvm` so the precompile module can issue syscalls on the guest target while building cleanly on the host.
- `crates/pickles-verifier/src/precompile_msm.rs` (new) — Pippenger MSM that calls `syscall_vesta_add`/`double` (`#[cfg(target_os = "zkvm")]`) and falls back to arkworks point arithmetic on the host (so unit tests still pass). Points are kept in canonical `[u64; 8]` (non-Montgomery) layout matching what the SP1 executor's `AffinePoint::from_words_le` reads; arkworks ↔ canonical conversion happens once at MSM entry/exit via `into_bigint`/`from_bigint`. Identity is `Option<Pt>` (the precompile has no infinity).
- `crates/pickles-verifier/src/lib.rs` — `accumulator_check` now calls `precompile_msm::msm_vesta` instead of `<<Vesta as AffineRepr>::Group as VariableBaseMSM>::msm`. The staged cycle-tracker markers in `verify_batch` (`check_accumulators` / `compute_public_inputs` / `batching_rng` / `kimchi_dlog_check`) are also from this POC and are useful independent of the precompile.
- `crates/o1-verifier-host/Cargo.toml` — `sp1-sdk` `profiling` feature enabled (without it the cycle tracker is silent).

The host-side `cargo test -p pickles-verifier` still passes against the full fixture matrix — the precompile module has an arkworks fallback for non-zkvm targets.

## Pitfalls hit (read this if you reproduce)

1. **Two executor dispatch paths, both must be wired.** SP1 has `minimal/ecall.rs` (used by the JIT executor when profiling is off) and `vm/syscall.rs` (used by the gas estimator and `client.execute()` when profiling is on). The first build of this POC only wired `minimal/ecall.rs`, so `client.execute()` silently returned `None` for `VESTA_ADD`, the guest's `[u64; 8]` result buffer was never written, and subsequent code read garbage and crashed with `invalid memory access for opcode ld and address <random huge value>`. Fix: dispatch in both files. The `vm/syscall.rs` path will `panic!("Unsupported curve")` if `RT::TRACING = true`, but that branch is only reached during proving — execute mode (`TRACING = false`) skips it.
2. **Workspace `[patch]` must live below `[workspace.dependencies]`.** Cargo treats `[patch.<source>]` as a new table; injecting it mid-`[workspace.dependencies]` silently truncates the dependency list.
3. **Pasta moduli encoding.** Both base fields are 256-bit primes that fit in `U32` limbs / `U62` witness, same as bn254/secp256r1. The LE byte form must match the OCaml/arkworks `MODULUS` exactly — a `biguint_from_limbs` round-trip unit test catches typos.
4. **Montgomery vs canonical limbs at the syscall boundary.** Arkworks stores field elements in Montgomery form; the SP1 executor's `from_words_le`/`to_words_le` round-trips canonical limbs. Call `into_bigint()` / `from_bigint()` at the MSM boundary, never pass `(p.x.0).0` straight to the syscall.

## What this POC does NOT include

- **No prover-side chip.** `sp1-core-machine`'s `WeierstrassAddAssignChip` is generic over `EllipticCurve`, but registering Pasta instantiations in the machine config + their air-IDs + tests is not done here. With the current POC, **proving will fail** for the wrap fixture — only `client.execute()` works. The cycle-tracker numbers above are valid measurements of what the proven cost would be if the chip were wired.
- **No Pallas precompile in the kimchi wrap MSM.** `kimchi_dlog_check` (1.56B cycles, ~36% of original) still runs arkworks-Pippenger inside `kimchi::verifier::batch_verify_with_rng` → `OpeningProof::verify` → `G::Group::msm_bigint`. The Pallas syscalls (`syscall_pallas_add`/`double`) are already wired in the fork — the remaining work is to fork-edit `mina/src/lib/crypto/proof-systems/poly-commitment/src/ipa.rs` to call `pickles_verifier::precompile_msm::msm_pallas` (which already exists) instead. Estimated additional saving: ~80% of the kimchi MSM portion, bringing total to ~700–900M cycles (~80% off baseline).
- **No upstream PR to Succinct.** See the "Path to production" section.

## Path to production

In rough order:

1. **Patch kimchi's wrap MSM (local).** A one-call swap in `poly-commitment/src/ipa.rs` to `precompile_msm::msm_pallas`. Should land ~1.2–1.4B more cycles. Verify with the fixture matrix and the e2e.
2. **Implement the prover-side chip wiring.** `WeierstrassAddAssignChip<Pallas>` and `<Vesta>` instantiations in `sp1-core-machine`, plus the corresponding `RiscvAirId` variants + machine config registration. The chip arithmetic itself is generic; no constraint changes needed. Test against a small proving run, not just execute mode.
3. **Upstream PR to `succinctlabs/sp1`.** The diff is small and self-contained: the current [`Nori-zk/sp1@FEAT/msm-precompile-try-1`](https://github.com/Nori-zk/sp1/tree/FEAT/msm-precompile-try-1) commit is 9 files / +415 LOC (~300 LOC for `sp1-curves/src/weierstrass/pasta.rs`, ~30 LOC of `CurveType`/`SyscallCode` plumbing, ~80 LOC for the entrypoint syscall stubs); the chip-side additions from step 2 add roughly another ~100 LOC. Reference Tamir's contribution-welcome message in the GitHub discussion.
4. **Network deployment.** Once merged + released by Succinct, the Pasta precompiles are available on the prover network. Until then, local proving (`SP1_PROVER=cpu` / `cuda`) works with our fork; `SP1_PROVER=network` does not (the network proves the upstream SP1 VK; our fork's VK is rejected).

## Reproducing the measurement

The fork lives at `../sp1-pasta-fork/` relative to the repo root, on branch
[`FEAT/msm-precompile-try-1`](https://github.com/Nori-zk/sp1/tree/FEAT/msm-precompile-try-1)
of `Nori-zk/sp1` (one commit on top of upstream `v6.1.0`). From the repo root:

```bash
# 1. Clone the fork at the POC branch (one-time).
git clone -b FEAT/msm-precompile-try-1 --single-branch \
    git@github.com:Nori-zk/sp1.git ../sp1-pasta-fork

# 2. Build (the workspace [patch] in this repo's Cargo.toml points at
#    ../sp1-pasta-fork; nothing else to wire up).
VK_JSON=fixtures/mainnet-blockchain-snark/vk.serde.json make build-rust

# 3. Run (with profiling feature already on in o1-verifier-host's Cargo.toml).
./target/release/o1zkvm --fixture-dir fixtures/mainnet-blockchain-snark
```

Expected output:

```
Pickles proof verified successfully inside SP1 zkVM!
Execution used 1878152386 cycles
  accumulator_check:   159M cycles
  kimchi_dlog_check: 1,562M cycles
  ...
```

## Files

POC code:

- `crates/pickles-verifier/src/precompile_msm.rs` — Pippenger MSM over the new syscalls.
- `crates/pickles-verifier/src/lib.rs` — `accumulator_check` re-routed; staged cycle-tracker markers.
- `crates/o1-verifier-host/Cargo.toml` — `sp1-sdk` profiling feature.
- `Cargo.toml` — `[patch]` block.

SP1 fork (in `../sp1-pasta-fork/`):

- `crates/curves/src/weierstrass/pasta.rs` (new)
- `crates/curves/src/lib.rs`, `crates/curves/src/weierstrass/mod.rs`
- `crates/core/executor/src/syscall_code.rs`
- `crates/core/executor/src/minimal/ecall.rs`
- `crates/core/executor/src/vm/syscall.rs`
- `crates/zkvm/entrypoint/src/syscalls/pasta.rs` (new) + `mod.rs`
- `crates/zkvm/lib/src/lib.rs`
