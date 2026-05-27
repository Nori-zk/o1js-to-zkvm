//! Variable-base MSM for the Pasta curves using SP1's executor-level
//! `PALLAS_ADD/DOUBLE` and `VESTA_ADD/DOUBLE` precompiles.
//!
//! Each precompile call costs **256 cycles** in the SP1 minimal executor
//! (single `ecall`), regardless of the underlying field-mul count. Compare
//! to a native Pasta short-Weierstrass `+= affine` on this guest, which is
//! ~9 field muls × 2 `sys_bigint` calls + arkworks-side dispatch ≈ ~2500
//! cycles per op. The precompile is ~10× cheaper *per* point op; the MSM
//! win is bigger because Pippenger turns `n` scalar-muls into `O(n)` adds.
//!
//! Layout: points are represented as canonical-form `[u64; 8]`
//! (`[x_le_4u64s, y_le_4u64s]`), matching what the SP1 executor's
//! `AffinePoint::<E>::from_words_le` / `to_words_le` round-trip expects.
//! `arkworks` stores `x`, `y` in Montgomery form, so we convert at the MSM
//! boundary only (`into_bigint`/`from_bigint`).
//!
//! Identity is encoded as `None` in the working buckets — the precompile
//! has no infinity representation. First base into an empty bucket is a
//! copy; subsequent ones use the precompile add.
//!
//! Edge case `P + (-P)` (zero denominator inside the precompile's affine
//! add formula) is not handled — for random SRS bases and Fiat-Shamir
//! scalars the probability is negligible. The executor would produce
//! garbage rather than panic.

use alloc::vec;
use alloc::vec::Vec;

use ark_ec::short_weierstrass::{Affine, SWCurveConfig};
use ark_ec::AffineRepr;
use ark_ff::{BigInt, BigInteger, Fp, MontBackend, MontConfig, PrimeField};
use mina_curves::pasta::{PallasParameters, VestaParameters};

/// Canonical affine-point layout the SP1 precompile reads/writes:
/// `[x_le_4u64s, y_le_4u64s]` (little-endian per coord, x then y).
pub type Pt = [u64; 8];

// ---------------------------------------------------------------------------
// Affine ↔ canonical conversions.
//
// `arkworks` `Fp<MontBackend<C, 4>, 4>` stores values in **Montgomery form**
// (`a · R mod p`); the SP1 precompile reads/writes canonical (non-Mont)
// limbs. `into_bigint`/`from_bigint` strip/apply R⁻¹ on a single Montgomery
// reduction, which on the guest is one `sys_bigint`-shaped op per coord
// (~50–100 cycles). Cheap; only done once per base at MSM entry and once at
// MSM exit.
// ---------------------------------------------------------------------------

#[inline]
fn affine_to_canonical<C: MontConfig<4>>(p: &Affine<impl SWCurveConfig<BaseField = Fp<MontBackend<C, 4>, 4>>>) -> Pt {
    let mut out = [0u64; 8];
    let x_bigint = p.x.into_bigint();
    let y_bigint = p.y.into_bigint();
    out[..4].copy_from_slice(&x_bigint.0);
    out[4..].copy_from_slice(&y_bigint.0);
    out
}

#[inline]
fn canonical_to_affine<P: SWCurveConfig<BaseField = Fp<MontBackend<C, 4>, 4>>, C: MontConfig<4>>(
    words: &Pt,
) -> Affine<P> {
    let x_bigint = BigInt::<4>(words[..4].try_into().unwrap());
    let y_bigint = BigInt::<4>(words[4..].try_into().unwrap());
    let x = Fp::<MontBackend<C, 4>, 4>::from_bigint(x_bigint).expect("x in field");
    let y = Fp::<MontBackend<C, 4>, 4>::from_bigint(y_bigint).expect("y in field");
    Affine::new_unchecked(x, y)
}

/// In-place `(x, y) → (x, p - y)` on canonical limbs (`p` = `C::MODULUS`).
/// Subtracts the y limbs from the modulus limbs using a borrow chain — no
/// field-op precompile call needed, this is just 4× u64 sub-with-borrow.
#[inline]
fn negate_y_canonical<C: MontConfig<4>>(pt: &mut Pt) {
    let modulus = C::MODULUS.0;
    let y = [pt[4], pt[5], pt[6], pt[7]];
    let mut borrow: u128 = 0;
    for i in 0..4 {
        let lhs = modulus[i] as u128;
        let rhs = (y[i] as u128) + borrow;
        if lhs >= rhs {
            pt[4 + i] = (lhs - rhs) as u64;
            borrow = 0;
        } else {
            pt[4 + i] = ((lhs + (1u128 << 64)) - rhs) as u64;
            borrow = 1;
        }
    }
    // `0 - 0 mod p` would give 0 still; if y == 0, we'd write p which is
    // out-of-canonical-range, but y == 0 only on the curve's 2-torsion (which
    // Pasta's prime-order group doesn't contain), so this never happens for
    // bases on the SRS.
}

// ---------------------------------------------------------------------------
// Curve-op shims. zkVM target uses the precompile (one `ecall` = 256 cycles);
// host falls back to arkworks arithmetic so unit tests still pass.
// ---------------------------------------------------------------------------

#[inline]
fn vesta_add_inplace(p: &mut Pt, q: &Pt) {
    #[cfg(target_os = "zkvm")]
    unsafe {
        sp1_zkvm::syscalls::syscall_vesta_add(
            p as *mut [u64; 8],
            q as *const [u64; 8],
        );
    }
    #[cfg(not(target_os = "zkvm"))]
    {
        let p_aff: Affine<VestaParameters> = canonical_to_affine(p);
        let q_aff: Affine<VestaParameters> = canonical_to_affine(q);
        let sum = p_aff + q_aff;
        let sum_aff: Affine<VestaParameters> = ark_ec::CurveGroup::into_affine(sum);
        *p = affine_to_canonical(&sum_aff);
    }
}

#[inline]
fn vesta_double_inplace(p: &mut Pt) {
    #[cfg(target_os = "zkvm")]
    unsafe {
        sp1_zkvm::syscalls::syscall_vesta_double(p as *mut [u64; 8]);
    }
    #[cfg(not(target_os = "zkvm"))]
    {
        let p_aff: Affine<VestaParameters> = canonical_to_affine(p);
        let doubled = p_aff + p_aff;
        let doubled_aff: Affine<VestaParameters> = ark_ec::CurveGroup::into_affine(doubled);
        *p = affine_to_canonical(&doubled_aff);
    }
}

#[inline]
#[allow(dead_code)]
fn pallas_add_inplace(p: &mut Pt, q: &Pt) {
    #[cfg(target_os = "zkvm")]
    unsafe {
        sp1_zkvm::syscalls::syscall_pallas_add(
            p as *mut [u64; 8],
            q as *const [u64; 8],
        );
    }
    #[cfg(not(target_os = "zkvm"))]
    {
        let p_aff: Affine<PallasParameters> = canonical_to_affine(p);
        let q_aff: Affine<PallasParameters> = canonical_to_affine(q);
        let sum = p_aff + q_aff;
        let sum_aff: Affine<PallasParameters> = ark_ec::CurveGroup::into_affine(sum);
        *p = affine_to_canonical(&sum_aff);
    }
}

#[inline]
#[allow(dead_code)]
fn pallas_double_inplace(p: &mut Pt) {
    #[cfg(target_os = "zkvm")]
    unsafe {
        sp1_zkvm::syscalls::syscall_pallas_double(p as *mut [u64; 8]);
    }
    #[cfg(not(target_os = "zkvm"))]
    {
        let p_aff: Affine<PallasParameters> = canonical_to_affine(p);
        let doubled = p_aff + p_aff;
        let doubled_aff: Affine<PallasParameters> = ark_ec::CurveGroup::into_affine(doubled);
        *p = affine_to_canonical(&doubled_aff);
    }
}

// ---------------------------------------------------------------------------
// Pippenger driver, generic over the curve via add/double fn pointers.
// ---------------------------------------------------------------------------

#[inline]
fn ln_without_floats(a: usize) -> usize {
    let log2 = (usize::BITS - 1 - a.leading_zeros()) as usize;
    log2 * 69 / 100
}

/// Fill `out` (length must be `num_bits.div_ceil(c)`) with signed Pippenger
/// digits of `scalar` in `[-2^(c-1), 2^(c-1))`. Same encoding as arkworks's
/// `make_digits`. Writes one digit per window in the slice.
fn fill_signed_digits(scalar: &impl BigInteger, c: usize, num_bits: usize, out: &mut [i32]) {
    let raw = scalar.as_ref();
    let radix: u64 = 1 << c;
    let window_mask: u64 = radix - 1;
    let digits_count = out.len();
    let mut carry: u64 = 0;

    for (i, slot) in out.iter_mut().enumerate() {
        let bit_offset = i * c;
        let u64_idx = bit_offset / 64;
        let bit_idx = bit_offset % 64;
        let bit_buf = if bit_idx < 64 - c || u64_idx == raw.len() - 1 {
            raw[u64_idx] >> bit_idx
        } else {
            (raw[u64_idx] >> bit_idx) | (raw[u64_idx + 1] << (64 - bit_idx))
        };
        let coef = carry + (bit_buf & window_mask);
        carry = (coef + radix / 2) >> c;
        let mut digit = (coef as i64) - ((carry << c) as i64);
        if i == digits_count - 1 {
            digit += (carry << c) as i64;
        }
        *slot = digit as i32;
    }
}

/// Pippenger MSM that uses `add_in_place`/`double_in_place` callbacks for
/// every point operation. On the SP1 guest these are the Pasta precompile
/// syscalls (~256 cycles each); on host they're arkworks affine ops.
///
/// `negate_y` flips the y coordinate sign in place (used for w-NAF signed
/// digits). `bases` and `scalars` must be the same length.
fn pippenger_with_ops<F1, F2, F3>(
    bases: &[Pt],
    scalars: &[[u64; 4]],
    num_bits: usize,
    add_in_place: F1,
    double_in_place: F2,
    mut negate_y: F3,
) -> Pt
where
    F1: Fn(&mut Pt, &Pt),
    F2: Fn(&mut Pt),
    F3: FnMut(&mut Pt),
{
    let n = bases.len().min(scalars.len());
    if n == 0 {
        return [0u64; 8];
    }
    let bases = &bases[..n];
    let scalars = &scalars[..n];

    let c = if n < 32 { 3 } else { ln_without_floats(n) + 2 };
    let num_windows = num_bits.div_ceil(c);
    let num_buckets = 1usize << (c - 1);

    // Flat per-scalar digit table (n × num_windows i32s), filled once.
    let mut all_digits: Vec<i32> = vec![0; n * num_windows];
    for (i, s) in scalars.iter().enumerate() {
        let slot = &mut all_digits[i * num_windows..(i + 1) * num_windows];
        // `BigInt::<4>` is `[u64; 4]` little-endian; `fill_signed_digits` reads
        // `BigInteger::as_ref() -> &[u64]`, so wrap in a BigInt to get that.
        let bigint = BigInt::<4>(*s);
        fill_signed_digits(&bigint, c, num_bits, slot);
    }

    // Per-window scratch: one slot per bucket, plus a parallel "occupied" flag
    // (the precompile has no identity / infinity, so we track empty buckets
    // explicitly and skip the syscall on first-add-to-empty).
    let mut buckets: Vec<Pt> = vec![[0u64; 8]; num_buckets];
    let mut occupied: Vec<bool> = vec![false; num_buckets];
    let mut window_sums: Vec<Option<Pt>> = Vec::with_capacity(num_windows);

    for w in 0..num_windows {
        // Reset buckets for this window.
        for o in occupied.iter_mut() {
            *o = false;
        }

        // Distribute bases into buckets keyed by this window's signed digit.
        for (i, base) in bases.iter().enumerate() {
            let d = all_digits[i * num_windows + w];
            if d == 0 {
                continue;
            }
            let (idx, neg) = if d > 0 {
                (d as usize - 1, false)
            } else {
                ((-d) as usize - 1, true)
            };
            let mut tmp = *base;
            if neg {
                negate_y(&mut tmp);
            }
            if occupied[idx] {
                add_in_place(&mut buckets[idx], &tmp);
            } else {
                buckets[idx] = tmp;
                occupied[idx] = true;
            }
        }

        // Running-sum reduction: `window = Σᵢ i · bucket[i-1]`.
        // Walk buckets high to low, accumulating `running += bucket; window += running`.
        let mut running: Option<Pt> = None;
        let mut window: Option<Pt> = None;
        for j in (0..num_buckets).rev() {
            if occupied[j] {
                match &mut running {
                    None => running = Some(buckets[j]),
                    Some(r) => add_in_place(r, &buckets[j]),
                }
            }
            if let Some(r) = &running {
                match &mut window {
                    None => window = Some(*r),
                    Some(w) => add_in_place(w, r),
                }
            }
        }
        window_sums.push(window);
    }

    // Combine windows: `total = Σᵢ window_sums[i] · 2^(c·i)`, computed high to
    // low as `total = total·2^c + window_sums[i]`.
    let mut total: Option<Pt> = None;
    for (i, ws) in window_sums.iter().enumerate().rev() {
        if let Some(t) = &mut total {
            // total *= 2^c
            for _ in 0..c {
                double_in_place(t);
            }
        }
        if let Some(w) = ws.as_ref() {
            match &mut total {
                None => total = Some(*w),
                Some(t) => add_in_place(t, w),
            }
        }
        let _ = i;
    }

    total.unwrap_or([0u64; 8])
}

// ---------------------------------------------------------------------------
// Public MSM entrypoints. Convert at the arkworks ↔ canonical boundary,
// dispatch to `pippenger_with_ops` with the curve-specific syscall shims.
// ---------------------------------------------------------------------------

/// Smoke test: convert two arkworks Vesta affine points to canonical limbs,
/// call the VESTA_ADD precompile, convert back, and verify against the
/// arkworks-computed sum. Returns true iff they match. Used by the guest as
/// a single-precompile-call sanity check that the syscall plumbing works.
pub fn smoke_test_vesta_add(
    a: &Affine<VestaParameters>,
    b: &Affine<VestaParameters>,
) -> bool {
    let mut p = affine_to_canonical_vesta(a);
    let q = affine_to_canonical_vesta(b);
    vesta_add_inplace(&mut p, &q);
    let got = canonical_to_affine_vesta(&p);
    let expected: Affine<VestaParameters> = ark_ec::CurveGroup::into_affine(*a + *b);
    got == expected
}

/// Reference smoke test: call the EXISTING (upstream) `BN254_ADD` syscall
/// with arbitrary in-range bn254 points. Used to verify that the syscall
/// plumbing works at all on this patched SP1, independent of the new
/// Pasta-specific code. The point math doesn't have to be correct; we only
/// care that the executor doesn't panic on the call.
#[cfg(target_os = "zkvm")]
pub fn smoke_test_bn254_add() -> bool {
    let mut p: [u64; 8] = [1, 0, 0, 0, 2, 0, 0, 0];
    let q: [u64; 8] = [3, 0, 0, 0, 4, 0, 0, 0];
    unsafe {
        sp1_zkvm::syscalls::syscall_bn254_add(
            &mut p as *mut [u64; 8],
            &q as *const [u64; 8],
        );
    }
    true
}

#[cfg(not(target_os = "zkvm"))]
pub fn smoke_test_bn254_add() -> bool {
    true
}

/// Like [`smoke_test_bn254_add`] but for our new `VESTA_ADD` syscall.
/// Uses arbitrary distinct dummy values for `p`/`q` so we exercise only
/// the syscall plumbing, not curve math correctness.
#[cfg(target_os = "zkvm")]
pub fn smoke_test_vesta_add_dummy() -> bool {
    let mut p: [u64; 8] = [1, 0, 0, 0, 2, 0, 0, 0];
    let q: [u64; 8] = [3, 0, 0, 0, 4, 0, 0, 0];
    unsafe {
        sp1_zkvm::syscalls::syscall_vesta_add(
            &mut p as *mut [u64; 8],
            &q as *const [u64; 8],
        );
    }
    true
}

#[cfg(not(target_os = "zkvm"))]
pub fn smoke_test_vesta_add_dummy() -> bool {
    true
}

/// Variable-base MSM on the Vesta curve, using the SP1 `VESTA_ADD/DOUBLE`
/// executor precompile on the guest (and arkworks fallback on host).
pub fn msm_vesta(
    bases: &[Affine<VestaParameters>],
    scalars: &[<<VestaParameters as ark_ec::CurveConfig>::ScalarField as PrimeField>::BigInt],
) -> Affine<VestaParameters> {
    let n = bases.len().min(scalars.len());
    if n == 0 {
        return Affine::<VestaParameters>::zero();
    }
    // Convert bases from arkworks Montgomery → canonical limbs once.
    let canonical_bases: Vec<Pt> = bases[..n].iter().map(affine_to_canonical_vesta).collect();
    // BigInt<4>.0 is [u64; 4] little-endian.
    let scalar_limbs: Vec<[u64; 4]> = scalars[..n].iter().map(|s| s.0).collect();
    let num_bits = <<VestaParameters as ark_ec::CurveConfig>::ScalarField as PrimeField>::MODULUS_BIT_SIZE as usize;

    let result = pippenger_with_ops(
        &canonical_bases,
        &scalar_limbs,
        num_bits,
        vesta_add_inplace,
        vesta_double_inplace,
        negate_y_vesta,
    );

    if result == [0u64; 8] {
        Affine::<VestaParameters>::zero()
    } else {
        canonical_to_affine_vesta(&result)
    }
}

/// Variable-base MSM on the Pallas curve, using the SP1 `PALLAS_ADD/DOUBLE`
/// precompile on the guest.
#[allow(dead_code)]
pub fn msm_pallas(
    bases: &[Affine<PallasParameters>],
    scalars: &[<<PallasParameters as ark_ec::CurveConfig>::ScalarField as PrimeField>::BigInt],
) -> Affine<PallasParameters> {
    let n = bases.len().min(scalars.len());
    if n == 0 {
        return Affine::<PallasParameters>::zero();
    }
    let canonical_bases: Vec<Pt> = bases[..n].iter().map(affine_to_canonical_pallas).collect();
    let scalar_limbs: Vec<[u64; 4]> = scalars[..n].iter().map(|s| s.0).collect();
    let num_bits = <<PallasParameters as ark_ec::CurveConfig>::ScalarField as PrimeField>::MODULUS_BIT_SIZE as usize;

    let result = pippenger_with_ops(
        &canonical_bases,
        &scalar_limbs,
        num_bits,
        pallas_add_inplace,
        pallas_double_inplace,
        negate_y_pallas,
    );

    if result == [0u64; 8] {
        Affine::<PallasParameters>::zero()
    } else {
        canonical_to_affine_pallas(&result)
    }
}

// Curve-specific conversion wrappers (the generic versions above need an
// explicit `C: MontConfig<4>` that's awkward to surface at call sites).
// `Fp` for Vesta = `mina_curves::pasta::FqConfig`, base field of Pallas =
// `FqConfig` as well? No — Pallas base field = `Fp` (mina-curves's `FpConfig`),
// Vesta base field = `Fq` (mina-curves's `FqConfig` from fp.rs naming —
// historically the file's named after the OCaml convention).
//
// Concretely:
//   * `Affine<VestaParameters>::x: Fp<MontBackend<FqConfig, 4>, 4>` (= mina-curves `Fq`)
//   * `Affine<PallasParameters>::x: Fp<MontBackend<FqConfig, 4>, 4>` (= mina-curves `Fp`)
// Both use the type-alias `Fp = Fp256<MontBackend<FqConfig, 4>>` /
// `Fq = Fp256<MontBackend<FrConfig, 4>>` from mina-curves's fp.rs/fq.rs.

fn affine_to_canonical_vesta(p: &Affine<VestaParameters>) -> Pt {
    let mut out = [0u64; 8];
    out[..4].copy_from_slice(&p.x.into_bigint().0);
    out[4..].copy_from_slice(&p.y.into_bigint().0);
    out
}

fn canonical_to_affine_vesta(words: &Pt) -> Affine<VestaParameters> {
    use mina_curves::pasta::Fq;
    let x = Fq::from_bigint(BigInt::<4>(words[..4].try_into().unwrap()))
        .expect("vesta x in field");
    let y = Fq::from_bigint(BigInt::<4>(words[4..].try_into().unwrap()))
        .expect("vesta y in field");
    Affine::new_unchecked(x, y)
}

fn affine_to_canonical_pallas(p: &Affine<PallasParameters>) -> Pt {
    let mut out = [0u64; 8];
    out[..4].copy_from_slice(&p.x.into_bigint().0);
    out[4..].copy_from_slice(&p.y.into_bigint().0);
    out
}

fn canonical_to_affine_pallas(words: &Pt) -> Affine<PallasParameters> {
    use mina_curves::pasta::Fp;
    let x = Fp::from_bigint(BigInt::<4>(words[..4].try_into().unwrap()))
        .expect("pallas x in field");
    let y = Fp::from_bigint(BigInt::<4>(words[4..].try_into().unwrap()))
        .expect("pallas y in field");
    Affine::new_unchecked(x, y)
}

fn negate_y_vesta(pt: &mut Pt) {
    use mina_curves::pasta::Fq;
    let modulus = <Fq as PrimeField>::MODULUS.0;
    sub_y_into_modulus(pt, &modulus);
}

fn negate_y_pallas(pt: &mut Pt) {
    use mina_curves::pasta::Fp;
    let modulus = <Fp as PrimeField>::MODULUS.0;
    sub_y_into_modulus(pt, &modulus);
}

/// `y_new = modulus - y_old` on the y limbs of `pt` (`pt[4..8]`). 256-bit
/// borrow-chain subtraction — no precompile/sys_bigint call needed since
/// it's just `u128`-width subs in software.
fn sub_y_into_modulus(pt: &mut Pt, modulus: &[u64; 4]) {
    let y = [pt[4], pt[5], pt[6], pt[7]];
    let mut borrow: u128 = 0;
    for i in 0..4 {
        let lhs = modulus[i] as u128;
        let rhs = (y[i] as u128) + borrow;
        if lhs >= rhs {
            pt[4 + i] = (lhs - rhs) as u64;
            borrow = 0;
        } else {
            pt[4 + i] = ((lhs + (1u128 << 64)) - rhs) as u64;
            borrow = 1;
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use ark_ec::{CurveGroup, VariableBaseMSM};
    use ark_ff::UniformRand;
    use mina_curves::pasta::{Fp, Vesta};
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    fn vesta_msm_matches(n: usize, seed: u64) {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let bases: Vec<Vesta> = (0..n)
            .map(|_| <Vesta as AffineRepr>::Group::rand(&mut rng).into_affine())
            .collect();
        let scalars: Vec<Fp> = (0..n).map(|_| Fp::rand(&mut rng)).collect();
        let scalars_bigint: Vec<_> = scalars.iter().map(|s: &Fp| s.into_bigint()).collect();

        let expected =
            <<Vesta as AffineRepr>::Group as VariableBaseMSM>::msm(&bases, &scalars)
                .unwrap()
                .into_affine();
        let got = msm_vesta(&bases, &scalars_bigint);
        assert_eq!(expected, got, "n = {n}");
    }

    #[test]
    fn vesta_precompile_msm_matches_arkworks_small() {
        for n in [1usize, 2, 3, 4, 8, 16, 31, 32, 64] {
            vesta_msm_matches(n, 1);
        }
    }

    #[test]
    fn vesta_precompile_msm_matches_arkworks_medium() {
        vesta_msm_matches(1024, 7);
    }
}
