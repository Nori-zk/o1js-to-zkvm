//! Host driver for the `o1-verifier` SP1 guest.
//!
//! Takes a fixture directory containing the OCaml-dumped pickles wire files —
//! ```
//!   <fixture_dir>/
//!     vk.serde.json
//!     proof.serde.json
//!     public_input_skeleton.json
//!     app_statement.json
//! ```
//! — assembles a [`pickles_verifier::types::VerifiableProof`] host-side (via
//! `pickles_verifier::wire` parsers + `OcamlProof::into_verifiable`), writes
//! it to the guest stdin, and runs the SP1 zkVM (execute or prove).
//!
//! The guest ELF has a wrap VK baked in at build time via `VK_JSON` (see
//! `o1-verifier/build.rs`). The fixture passed here MUST be against that
//! same VK — there is no runtime mismatch check yet.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use pickles_verifier::wire::{parse_app_statement, parse_wrap_proof, parse_wrap_vk, OcamlProof};
use sp1_sdk::network::proto::types::FulfillmentStrategy;
use sp1_sdk::network::{Address, NetworkMode};
use sp1_sdk::{
    include_elf, Elf, ProveRequest, Prover, ProverClient, ProvingKey, SP1ProofWithPublicValues,
    SP1Stdin,
};

const ELF: Elf = include_elf!("o1-verifier");

#[derive(Parser)]
#[command(name = "o1-verifier-host")]
#[command(about = "Run the o1-verifier guest program (pickles verification) in the SP1 zkVM")]
struct Cli {
    /// Path to a fixture directory containing vk.serde.json, proof.serde.json,
    /// public_input_skeleton.json, and app_statement.json. The VK must match
    /// the one the guest was built against (see o1-verifier's VK_JSON env var).
    #[arg(short, long)]
    fixture_dir: PathBuf,

    /// Generate a real SP1 proof instead of just executing the program.
    /// Backend is selected by the SP1_PROVER env var (cpu, cuda, network).
    #[arg(long)]
    prove: bool,
}

#[tokio::main]
async fn main() {
    // Pick up SP1's tracing logs. Default filter is "off" unless RUST_LOG is
    // set, so set `RUST_LOG=info` (or higher) to see proving progress.
    sp1_sdk::utils::setup_logger();

    let cli = Cli::parse();

    // Load the four wire files.
    let read = |name: &str| {
        let p = cli.fixture_dir.join(name);
        fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
    };
    let vk_json = read("vk.serde.json");
    let proof_json = read("proof.serde.json");
    let skeleton_json = read("public_input_skeleton.json");
    let app_stmt_json = read("app_statement.json");

    // Parse + assemble the VerifiableProof host-side.
    let wrap_vk = parse_wrap_vk(&vk_json).expect("parse vk.serde.json");
    let wrap_proof = parse_wrap_proof(&proof_json).expect("parse proof.serde.json");
    let ocaml = OcamlProof::parse(&skeleton_json).expect("parse public_input_skeleton.json");
    let app_stmt = parse_app_statement(&app_stmt_json).expect("parse app_statement.json");

    let verifiable = ocaml
        .into_verifiable(wrap_proof, &wrap_vk, &[app_stmt])
        .expect("OcamlProof::into_verifiable");

    let mut stdin = SP1Stdin::new();
    stdin.write(&verifiable);

    // Backend selected by SP1_PROVER env var (mock, cpu, cuda, network).
    // Defaults to cpu. Network mode goes through an explicit NetworkProver
    // so we can apply auction/limits/whitelist knobs from env vars.
    let prover_kind = std::env::var("SP1_PROVER").unwrap_or_else(|_| "cpu".to_string());

    if cli.prove {
        let proof = if prover_kind == "network" {
            prove_via_network(stdin).await
        } else {
            prove_via_env(stdin).await
        };

        let mut public_values = proof.public_values.clone();
        let valid: bool = public_values.read();
        assert!(valid, "Pickles proof verification failed inside SP1 zkVM");

        println!("Pickles proof verified successfully inside SP1 zkVM!");
        println!("SP1 proof generated and verified.");
    } else {
        let client = ProverClient::from_env().await;
        let (mut public_values, report) =
            client.execute(ELF, stdin).await.expect("execution failed");

        let valid: bool = public_values.read();
        assert!(valid, "Pickles proof verification failed inside SP1 zkVM");

        println!("Pickles proof verified successfully inside SP1 zkVM!");
        println!("Execution used {} cycles", report.total_instruction_count());

        let mut entries: Vec<(&String, &u64)> = report.cycle_tracker.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, cycles) in entries {
            println!("  {name}: {cycles} cycles");
        }
    }
}

/// CPU / CUDA / Mock path. No request-level knobs beyond SP1_PROOF_TYPE.
async fn prove_via_env(stdin: SP1Stdin) -> SP1ProofWithPublicValues {
    let client = ProverClient::from_env().await;
    let pk = client.setup(ELF).await.expect("setup failed");
    let request = client.prove(&pk, stdin);
    let proof = match std::env::var("SP1_PROOF_TYPE").as_deref() {
        Ok("plonk") => request.plonk().await,
        Ok("groth16") => request.groth16().await,
        Ok("compressed") => request.compressed().await,
        Ok("core") | Err(_) => request.await,
        Ok(other) => panic!(
            "unknown SP1_PROOF_TYPE={other:?} (expected core|compressed|plonk|groth16)"
        ),
    }
    .expect("prove failed");
    client
        .verify(&proof, pk.verifying_key(), None)
        .expect("proof verification failed");
    proof
}

/// Succinct prover network. Reads all knobs from env vars (see .env.example).
async fn prove_via_network(stdin: SP1Stdin) -> SP1ProofWithPublicValues {
    // Builder. NETWORK_PRIVATE_KEY + NETWORK_RPC_URL are read by .build()
    // unless we override here. SP1_NETWORK_MODE selects mainnet/reserved.
    let network_mode = match std::env::var("SP1_NETWORK_MODE").ok().as_deref() {
        Some(s) => s.parse::<NetworkMode>().unwrap_or_else(|_| {
            panic!("unknown SP1_NETWORK_MODE={s:?} (expected mainnet|reserved)")
        }),
        None => NetworkMode::Mainnet,
    };
    let prover = ProverClient::builder().network_for(network_mode).build().await;
    let pk = prover.setup(ELF).await.expect("setup failed");

    let mut req = prover.prove(&pk, stdin);

    // SP1_FULFILLMENT_STRATEGY: auction (default) | hosted | reserved.
    if let Ok(s) = std::env::var("SP1_FULFILLMENT_STRATEGY") {
        let strat = match s.to_ascii_lowercase().as_str() {
            "auction" => FulfillmentStrategy::Auction,
            "hosted" => FulfillmentStrategy::Hosted,
            "reserved" => FulfillmentStrategy::Reserved,
            other => panic!(
                "unknown SP1_FULFILLMENT_STRATEGY={other:?} (expected auction|hosted|reserved)"
            ),
        };
        req = req.strategy(strat);
    }

    if let Some(v) = parse_u64_env("SP1_MAX_PRICE_PER_PGU") {
        req = req.max_price_per_pgu(v);
    }
    if let Some(v) = parse_u64_env("SP1_TIMEOUT_SECS") {
        req = req.timeout(Duration::from_secs(v));
    }

    // Limits — only meaningful when skip_simulation is true (otherwise the
    // SDK computes them from the simulation run).
    if let Some(b) = parse_bool_env("SP1_SKIP_SIMULATION") {
        req = req.skip_simulation(b);
    }
    if let Some(v) = parse_u64_env("SP1_CYCLE_LIMIT") {
        req = req.cycle_limit(v);
    }
    if let Some(v) = parse_u64_env("SP1_GAS_LIMIT") {
        req = req.gas_limit(v);
    }

    // SP1_WHITELIST: comma-separated 0x… prover addresses. Empty/unset leaves
    // the SDK to fall back to its default reliable-prover pool.
    if let Ok(raw) = std::env::var("SP1_WHITELIST") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let addrs: Vec<Address> = trimmed
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap_or_else(|e| panic!("SP1_WHITELIST: invalid address {s:?}: {e}")))
                .collect();
            req = req.whitelist(Some(addrs));
        }
    }

    // Proof type (same env var as the env path).
    let proof = match std::env::var("SP1_PROOF_TYPE").as_deref() {
        Ok("plonk") => req.plonk().await,
        Ok("groth16") => req.groth16().await,
        Ok("compressed") => req.compressed().await,
        Ok("core") | Err(_) => req.await,
        Ok(other) => panic!(
            "unknown SP1_PROOF_TYPE={other:?} (expected core|compressed|plonk|groth16)"
        ),
    }
    .expect("prove failed");

    prover
        .verify(&proof, pk.verifying_key(), None)
        .expect("proof verification failed");
    proof
}

fn parse_u64_env(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .map(|v| v.parse().unwrap_or_else(|e| panic!("{name}={v:?} is not a u64: {e}")))
}

fn parse_bool_env(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" => false,
        other => panic!("{name}={other:?} is not a bool"),
    })
}
