# CI job: soak-10m, x86_64 vs aarch64

Budget row (plan 17): replay state-hash divergences over the soak, same
build, x86_64 vs aarch64: **0**. Scenario: `crates/testkit/scenarios/soak-10m.toml`.
Decision 0013 (cross-architecture determinism) is what this job enforces.

This file describes the job for the lead to place in the CI configuration.

## What it proves

The same source, built natively for each architecture, simulates the same
ten minutes of play to the same state on every tick of every cell, and each
architecture replays the other's logs without divergence. Bots, network
conditions, and seeds are all deterministic (simulated time, seeded loss and
jitter), so the runs are comparable tick for tick.

## Jobs

Two producer jobs run in parallel, then one comparison job.

### 1. `soak-x86_64` (runner: x86_64 Linux) and `soak-aarch64` (runner: aarch64 Linux)

Identical steps on each runner:

```sh
cargo build --release -p toy-server
CARGO_BIN=target/release/toy-server
$CARGO_BIN soak --out soak-out --ticks 18000 --bots 64 --seed 1
# Self-check: replay this architecture's own logs.
$CARGO_BIN replay soak-out/cell-1.log soak-out/cell-2.log
```

Upload `soak-out/` (two cell logs and `hashes.txt`) as an artifact named
`soak-<arch>`. Upload the release binary as `toy-server-<arch>`.

### 2. `soak-compare` (needs both producer jobs)

Runs on each architecture (a matrix of two), after downloading both
artifacts:

```sh
# 1. The per-tick state-hash traces are identical.
diff soak-x86_64/hashes.txt soak-aarch64/hashes.txt

# 2. This architecture replays the other architecture's logs. Every
#    tick's state hash is verified against the hash recorded by the
#    producer; any mismatch exits non-zero naming the tick.
toy-server replay soak-<other-arch>/cell-1.log soak-<other-arch>/cell-2.log
```

The job fails on any `diff` output or any non-zero exit.

## Notes

- Every toy subcommand runs with the same content: the cooked package
  checked in at `packages/toy/cooked`, verified (`--cooked DIR` and
  `--key FILE` choose another; the jobs use the defaults). The log header
  records its content hash, and `replay` refuses a log written with other
  content naming both hashes. The default cook path is compiled into the
  binary from its source checkout, so the compare job runs the binary from
  a checkout of the same commit (or passes `--cooked` pointing at one).
  Covered locally by `packages/toy/server/tests/soak_replay.rs`, which runs
  these exact invocations with fewer ticks.
- Logs carry a build id derived from the package name and version, not
  the target, so a log written on one architecture is accepted by the
  other architecture's binary of the same source. A version bump between
  producer and replayer is refused with a build mismatch, by design.
- `hashes.txt` lines are `tick cell state_hash` (hash in hex), one per cell
  per tick: 36,000 lines for this scenario.
- On a divergence, the first differing line of `hashes.txt` names the tick
  and cell; replaying that cell's log on both architectures stops at the
  same tick with the expected and actual hashes, which is where to start.
- Run time is dominated by the soak itself on each runner; no step needs a
  display or network access beyond fetching crates.
- Suggested trigger: nightly on the default branch, and on any change under
  `crates/core`, `crates/server`, or `packages/toy`.
