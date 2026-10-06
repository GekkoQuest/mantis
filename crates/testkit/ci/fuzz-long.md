# CI job: fuzz-long, every wire decoder

Budget row (plan 15): panics or round-trip failures in the wire decoders
over a long run: **0**. This file describes the job for the lead to place in
the gates workflow.

## What it proves

Every decoder that reads bytes from the network survives hostile input:

| Suite | Decoders |
|---|---|
| `crates/testkit/tests/fuzz_decoders.rs` | the adapter contract's generated decoders: every inbound message (with validators) and every outbound message |
| `crates/testkit/tests/fuzz_adapters.rs` | the native adapter's client frames; native server frames, including full and delta snapshots against a baseline; the toy legacy TCP opcode protocol, client packets through the adapter and server packets through its client half; random garbage into all four |

Each seed is a valid encoding of a structurally valid value. It is mutated
by bit flips, byte overwrites, truncation, extension, inflated length
fields, and random garbage. Every input must be refused with an error, or
accepted and then hold the canonical round trip: decode, encode, decode,
encode gives the same bytes. A panic fails the run.

The per-commit gate runs these suites at 300 iterations per message type.
This job runs them long, with a new seed each night.

## Job

Runner: any Linux x86_64. Nightly, and on demand.

```sh
SEED=$(date +%Y%m%d)
MANTIS_FUZZ_SEED=$SEED MANTIS_FUZZ_ITERS=200000 \
  cargo test --release -p mantis-testkit --test fuzz_decoders --test fuzz_adapters -- --nocapture
```

Each suite prints the seed on its first line:

```
fuzz: seed <n> (reproduce with MANTIS_FUZZ_SEED=<n>)
```

It also prints a summary per decoder, for example:

```
fuzz: native server frames: <n> mutations accepted, all round-trip
```

The job fails on a non-zero exit. On failure, keep the log: the seed and
the failing input (printed as hex in the error) reproduce it locally with
the same command and the same `MANTIS_FUZZ_SEED`.

## Notes

- The suites are deterministic for a given seed and iteration count, so
  one seed's run is the same on every machine.
- At 10,000 iterations both suites finish in under 10 s in a debug build.
  At 200,000 in release, expect a few minutes.
- The legacy protocol cannot carry the all-ones entity id: an object id is
  the entity's bits plus one, so that id would wrap to "none". The legacy
  encoder refuses any message or snapshot naming it
  (`AdapterError::Unrepresentable`, nothing written; the host and cell
  count it), and the fuzzer checks that a refused message writes no bytes.
- After a release run, clean the release profile
  (`rm -rf <target-dir>/release`) to keep the runner's disk use bounded.
