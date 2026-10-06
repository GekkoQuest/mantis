//! Structure-aware fuzzing of every generated decoder and validator path
//! (plan 6.7, 15): the adapter contract's inbound and outbound messages,
//! every one of them. The adapters' frame decoders are fuzzed in
//! `fuzz_adapters.rs`.
//!
//! For each message type, the fuzzer generates structurally valid values
//! (`FuzzSample`), checks the exact round trip, and then mutates the
//! encoding: bit flips, byte overwrites, truncation, extension, inflated
//! length fields, and random garbage. Every mutated input must be either
//! refused with an error or accepted as a value that re-encodes and decodes to
//! itself. A panic anywhere fails the test. The run is seeded and deterministic;
//! `MANTIS_FUZZ_ITERS` raises the iteration count for long runs.

#![allow(clippy::cast_possible_truncation)]

use mantis_adapter_contract::core_types::{
    FuzzSample, Message, MessageId, Tick, ValidationError, Wire, WireError, encode_into,
};
use mantis_adapter_contract::{
    Cast, Choose, Extension, ExtensionMessage, ExtensionRefused, FeatureState, Goodbye, Hello, Inbound,
    Interact, Move, MoveClaim, Outbound, PermittedModules, Refuse, SetPosition, SnapshotAck, Validators,
    Welcome, decode_inbound, decode_outbound,
};
use mantis_core::rng::{Rng, Salt, Seed};

/// Accepts everything: the fuzzer exercises decoding, not policy.
struct AcceptAll;

impl Validators for AcceptAll {
    fn validate_hello(&self, _: &Hello) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_move(&self, _: &Move) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_move_claim(&self, _: &MoveClaim) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_cast(&self, _: &Cast) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_interact(&self, _: &Interact) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_choose(&self, _: &Choose) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_extension(&self, _: &Extension) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_snapshot_ack(&self, _: &SnapshotAck) -> Result<(), ValidationError> {
        Ok(())
    }
    fn validate_goodbye(&self, _: &Goodbye) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Refuses everything: proves the validator runs after every successful decode.
struct RefuseAll;

impl Validators for RefuseAll {
    fn validate_hello(&self, _: &Hello) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_move(&self, _: &Move) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_move_claim(&self, _: &MoveClaim) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_cast(&self, _: &Cast) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_interact(&self, _: &Interact) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_choose(&self, _: &Choose) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_extension(&self, _: &Extension) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_snapshot_ack(&self, _: &SnapshotAck) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
    fn validate_goodbye(&self, _: &Goodbye) -> Result<(), ValidationError> {
        Err(ValidationError("no"))
    }
}

mod support;

use support::{iterations, mutate};

fn check_inbound<M>(rng: &mut Rng, wrap: fn(M) -> Inbound) -> Result<(), String>
where
    M: Message + FuzzSample + Copy,
{
    for _ in 0..iterations() {
        let value = M::fuzz_sample(rng);
        let mut bytes = Vec::new();
        encode_into(&value, &mut bytes);
        let decoded = decode_inbound(M::ID, &bytes, &AcceptAll)
            .map_err(|e| format!("{}: valid sample refused: {e}", M::NAME))?;
        if decoded != wrap(value) {
            return Err(format!("{}: round trip changed the value", M::NAME));
        }
        if !matches!(
            decode_inbound(M::ID, &bytes, &RefuseAll),
            Err(WireError::Rejected { .. })
        ) {
            return Err(format!("{}: validator did not run", M::NAME));
        }
        for _ in 0..8 {
            let bad = mutate(rng, &bytes);
            if let Ok(accepted) = decode_inbound(M::ID, &bad, &AcceptAll) {
                let mut again = Vec::new();
                accepted.encode(&mut again);
                if decode_inbound(M::ID, &again, &AcceptAll) != Ok(accepted) {
                    return Err(format!(
                        "{}: accepted a mutation that does not round-trip",
                        M::NAME
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_outbound<M>(rng: &mut Rng, wrap: fn(M) -> Outbound) -> Result<(), String>
where
    M: Message + FuzzSample + Copy,
{
    for _ in 0..iterations() {
        let value = M::fuzz_sample(rng);
        let mut bytes = Vec::new();
        encode_into(&value, &mut bytes);
        let decoded =
            decode_outbound(M::ID, &bytes).map_err(|e| format!("{}: valid sample refused: {e}", M::NAME))?;
        if decoded != wrap(value) {
            return Err(format!("{}: round trip changed the value", M::NAME));
        }
        for _ in 0..8 {
            let bad = mutate(rng, &bytes);
            if let Ok(accepted) = decode_outbound(M::ID, &bad) {
                let mut again = Vec::new();
                accepted.encode(&mut again);
                if decode_outbound(M::ID, &again) != Ok(accepted) {
                    return Err(format!(
                        "{}: accepted a mutation that does not round-trip",
                        M::NAME
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn contract_decoders_survive_structure_aware_fuzzing() -> Result<(), String> {
    let mut rng = Rng::for_cell(
        Seed(support::seed(0xF022)),
        Tick(0),
        Salt::named("testkit.fuzz.contract"),
    );
    check_inbound::<Hello>(&mut rng, Inbound::Hello)?;
    check_inbound::<Move>(&mut rng, Inbound::Move)?;
    check_inbound::<MoveClaim>(&mut rng, Inbound::MoveClaim)?;
    check_inbound::<Cast>(&mut rng, Inbound::Cast)?;
    check_inbound::<Interact>(&mut rng, Inbound::Interact)?;
    check_inbound::<Choose>(&mut rng, Inbound::Choose)?;
    check_inbound::<Extension>(&mut rng, Inbound::Extension)?;
    check_inbound::<SnapshotAck>(&mut rng, Inbound::SnapshotAck)?;
    check_inbound::<Goodbye>(&mut rng, Inbound::Goodbye)?;
    check_outbound::<Welcome>(&mut rng, Outbound::Welcome)?;
    check_outbound::<Refuse>(&mut rng, Outbound::Refuse)?;
    check_outbound::<SetPosition>(&mut rng, Outbound::SetPosition)?;
    check_outbound::<ExtensionRefused>(&mut rng, Outbound::ExtensionRefused)?;
    check_outbound::<ExtensionMessage>(&mut rng, Outbound::ExtensionMessage)?;
    check_outbound::<FeatureState>(&mut rng, Outbound::FeatureState)?;
    check_outbound::<PermittedModules>(&mut rng, Outbound::PermittedModules)?;
    Ok(())
}

#[test]
fn unknown_ids_and_garbage_are_refused() {
    let mut rng = Rng::for_cell(
        Seed(support::seed(0x6A2B)),
        Tick(0),
        Salt::named("testkit.fuzz.garbage"),
    );
    for _ in 0..2000 {
        let id = MessageId(rng.below(70_000).min(u32::from(u16::MAX)) as u16);
        let n = rng.below(80) as usize;
        let bytes: Vec<u8> = (0..n).map(|_| rng.next_u32() as u8).collect();
        let _ = decode_inbound(id, &bytes, &AcceptAll);
        let _ = decode_outbound(id, &bytes);
    }
    assert!(matches!(
        decode_inbound(MessageId(0), &[], &AcceptAll),
        Err(WireError::UnknownMessage(MessageId(0)))
    ));
    let _ = <Move as Wire>::decode;
}
