//! SP1 guest: out-of-circuit Pickles verification.
//!
//! At build time, `build.rs` reads the wrap `vk.serde.json` (path via the
//! `VK_JSON` env var) and writes a serialized [`pickles_verifier::Verifier`]
//! blob to `OUT_DIR/verifier.bin`. The guest `include_bytes!`s it and
//! reinstantiates the verifier mostly zero-parse via
//! [`pickles_verifier::serialize::decode_verifier_blob`] (pod-cast SRSes +
//! postcard wrap VK).
//!
//! At runtime, the host driver writes one
//! [`pickles_verifier::types::VerifiableProof`] to the guest stdin; the guest
//! reads it, runs [`pickles_verifier::verify`], and commits the boolean
//! result.

#![no_main]
sp1_zkvm::entrypoint!(main);

use core::slice;

use pickles_verifier::serialize::decode_verifier_blob;
use pickles_verifier::types::VerifiableProof;
use pickles_verifier::{batching_rng, check_accumulators, compute_public_inputs, kimchi_dlog_check};

/// 8-byte aligned wrapper around `include_bytes!`. The blob's pod-cast
/// sections (`PodVesta` / `PodPallas`) require 8-byte alignment for the
/// `bytemuck::cast_slice` casts inside `decode_verifier_blob`; raw
/// `include_bytes!` data is 1-byte aligned.
#[repr(C, align(8))]
struct Aligned<T: ?Sized>(T);

static VERIFIER_BYTES: &Aligned<[u8]> =
    &Aligned(*include_bytes!(concat!(env!("OUT_DIR"), "/verifier.bin")));

fn tracker(line: &[u8]) {
    sp1_zkvm::io::write(1, line);
}

pub fn main() {
    tracker(b"cycle-tracker-report-start:setup\n");
    let verifier = decode_verifier_blob(&VERIFIER_BYTES.0);
    let proof: VerifiableProof = sp1_zkvm::io::read();
    tracker(b"cycle-tracker-report-end:setup\n");

    // Reborrow as a 1-element slice so the staged API (which takes
    // `&[VerifiableProof]`) doesn't require us to move/clone the proof.
    let proofs = slice::from_ref(&proof);

    tracker(b"cycle-tracker-report-start:verify\n");

    tracker(b"cycle-tracker-report-start:accumulator_check\n");
    let acc_ok = check_accumulators(&verifier, proofs);
    tracker(b"cycle-tracker-report-end:accumulator_check\n");

    let valid = if !acc_ok {
        false
    } else {
        tracker(b"cycle-tracker-report-start:public_inputs\n");
        let pis = compute_public_inputs(&verifier, proofs);
        tracker(b"cycle-tracker-report-end:public_inputs\n");

        tracker(b"cycle-tracker-report-start:batching_rng\n");
        let mut rng = batching_rng(proofs, &pis);
        tracker(b"cycle-tracker-report-end:batching_rng\n");

        tracker(b"cycle-tracker-report-start:kimchi_dlog_check\n");
        let ok = kimchi_dlog_check(&verifier, proofs, &pis, &mut rng);
        tracker(b"cycle-tracker-report-end:kimchi_dlog_check\n");
        ok
    };

    tracker(b"cycle-tracker-report-end:verify\n");

    sp1_zkvm::io::commit(&valid);
}
