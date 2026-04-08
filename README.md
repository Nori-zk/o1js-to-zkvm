# o1js-to-zkvm

Take a circuit written in o1js, generate a proof for it, and re-verify
that proof inside the SP1 zkVM. The point is to bridge o1js circuits
into the broader zkVM ecosystem so they can be composed with other
SP1 programs.

## Install

```sh
make install
```

Installs the SP1 toolchain, protoc, and npm dependencies.

## Build

```sh
make build-ts                                       # TypeScript CLI
CIRCUIT_JSON=fixtures/circuit.json make build-rust  # Rust o1zkvm binary
```

## End-to-end test

```sh
make rust-e2e-tests
```

Compiles the circuit, generates a proof, and verifies it inside the
SP1 zkVM (mock mode).
