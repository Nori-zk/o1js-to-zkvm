#!/usr/bin/env bash
set -euo pipefail

# Generate a real SP1 proof of pickles verification end-to-end. Backend
# selected by SP1_PROVER:
#   cpu     - host CPU (rayon-parallel; tune RAYON_NUM_THREADS)
#   cuda    - local NVIDIA GPU via sp1-gpu-server
#   network - Succinct prover network (requires creds)
# Defaults to cpu so machines without a GPU still work.
#
# Inputs come from $FIXTURE_DIR (defaults to fixtures/mainnet-blockchain-snark).
# The fixture's vk.serde.json is also baked into the guest ELF at build time
# — the VK is fixed per ELF.

# Load .env from the repo root if present. Existing shell env vars take
# precedence (matches the behavior of `dotenvy::dotenv()`), so e.g. the
# `SP1_PROVER=cuda` set by `make prove-cuda` is not overridden by an
# `SP1_PROVER=...` line in .env.
ENV_FILE="$(dirname "$0")/../.env"
if [ -f "$ENV_FILE" ]; then
  echo "==> Loading env from $(realpath "$ENV_FILE")"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|\#*) continue ;; esac
    line="${line#export }"
    key="${line%%=*}"
    val="${line#*=}"
    # Strip inline comment (whitespace + # + rest of line) and trailing
    # whitespace. Without this, `KEY=val  # note` would load `val  # note`.
    val="$(printf '%s' "$val" | sed -E 's/[[:space:]]+#.*$//; s/[[:space:]]+$//')"
    # Strip surrounding quotes from the value (single or double).
    case "$val" in
      \"*\") val="${val#\"}"; val="${val%\"}" ;;
      \'*\') val="${val#\'}"; val="${val%\'}" ;;
    esac
    if [ -z "${!key+x}" ]; then
      export "$key=$val"
    fi
  done < "$ENV_FILE"
fi

export SP1_PROVER=${SP1_PROVER:-cpu}
export RUST_LOG=${RUST_LOG:-info}
FIXTURE_DIR=${FIXTURE_DIR:-$(pwd)/fixtures/mainnet-blockchain-snark}
export CUDA_VISIBLE_DEVICES=${CUDA_VISIBLE_DEVICES:-${CUDA_DEVICE:-0}}

if [ ! -d "$FIXTURE_DIR" ]; then
  echo "error: FIXTURE_DIR=$FIXTURE_DIR does not exist" >&2
  exit 1
fi

if [ "$SP1_PROVER" = "network" ] && [ -z "${NETWORK_PRIVATE_KEY:-}" ]; then
  echo "error: SP1_PROVER=network but NETWORK_PRIVATE_KEY is not set." >&2
  echo "       export NETWORK_PRIVATE_KEY=0x... (see .env.example)" >&2
  echo "       optional: NETWORK_RPC_URL (defaults to Succinct mainnet)" >&2
  echo "       optional: SP1_PROOF_TYPE=core|compressed|plonk|groth16 (default: core)" >&2
  exit 1
fi

echo "==> Fixture: $FIXTURE_DIR"
echo "==> SP1_PROVER=$SP1_PROVER"
if [ -n "${SP1_PROOF_TYPE:-}" ]; then
  echo "==> SP1_PROOF_TYPE=$SP1_PROOF_TYPE"
fi

# Build host + guest with VK baked from this fixture.
echo "==> Building o1zkvm..."
VK_JSON="$FIXTURE_DIR/vk.serde.json" make build-rust

# For SP1_PROVER=cuda the SDK auto-downloads sp1-gpu-server to ~/.sp1/bin/
# on first use (see sp1-cuda/src/server.rs::maybe_download_server) and spawns
# it as a child of the host process — no pre-start needed.
echo "==> Generating real SP1 proof ($SP1_PROVER)..."
target/release/o1zkvm --fixture-dir "$FIXTURE_DIR" --prove

echo "==> SP1 proof generation succeeded."
