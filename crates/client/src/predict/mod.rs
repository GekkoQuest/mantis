//! Prediction and reconciliation (plan 8.2).
//!
//! The client integrates its own avatar with the same deterministic `step` the server
//! runs, buffering every input with its sequence number and the state it produced. When
//! an authoritative state arrives acknowledging sequence `n`:
//! - if it is bit-identical to what was predicted for `n`, nothing changes (the common
//!   case, and the cheap one);
//! - otherwise the avatar is rewound to the authoritative state and every input after
//!   `n` is replayed with the modifiers it was originally integrated with.
//!
//! The simulation always holds the corrected state; the visible jump is smoothed in the
//! render world ([`crate::render_world::Correction`]), never here. Prediction depends on
//! inputs alone: no random rolls, ever (plan 8.2). Rolled outcomes arrive as
//! authoritative state.

pub mod buffer;
pub mod metrics;

use buffer::{BufferError, InputBuffer, InputRecord};

use crate::core_api::{AvatarKinematics, InputSeq, MotionModifiers, MotionStep, MoveInput, Vec3};

/// Counters describing prediction health.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PredictionStats {
    /// Inputs predicted.
    pub predicted: u64,
    /// Acknowledgements whose state matched the prediction bit for bit.
    pub matched: u64,
    /// Acknowledgements that required rewind-and-replay.
    pub corrected: u64,
    /// Inputs replayed in total.
    pub replayed: u64,
    /// Acknowledgements ignored as stale or duplicate.
    pub stale: u64,
    /// Acknowledgements rejected as invalid (acking an input never sent).
    pub invalid: u64,
    /// Resumed connections ([`Predictor::resume`]).
    pub resumed: u64,
}

/// Outcome of one reconciliation.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reconciliation {
    /// The authoritative state matched the prediction; nothing changed.
    Matched,
    /// The avatar was rewound and replayed.
    Corrected {
        /// Inputs replayed after the acknowledged one.
        replayed: u32,
        /// Current predicted position before minus after: the visual jump to smooth.
        correction: Vec3,
        /// True when inputs after the acknowledged one had been evicted, so the replay
        /// could not be complete.
        gap: bool,
    },
    /// The acknowledgement was not newer than the last one; ignored.
    Stale,
    /// The acknowledgement names an input that was never sent; ignored (fail closed).
    Invalid,
}

impl Reconciliation {
    /// Magnitude of the visual correction (zero unless corrected).
    pub fn magnitude(&self) -> f32 {
        match self {
            Reconciliation::Corrected { correction, .. } => correction.length(),
            Reconciliation::Matched | Reconciliation::Stale | Reconciliation::Invalid => 0.0,
        }
    }
}

/// Errors from [`Predictor::predict`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PredictError {
    /// The input's sequence is not [`Predictor::next_seq`].
    WrongSequence {
        /// Expected sequence.
        expected: InputSeq,
        /// Sequence given.
        got: InputSeq,
    },
}

impl core::fmt::Display for PredictError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PredictError::WrongSequence { expected, got } => {
                write!(f, "input sequence {} given, {} expected", got.0, expected.0)
            }
        }
    }
}

impl std::error::Error for PredictError {}

impl From<BufferError> for PredictError {
    fn from(e: BufferError) -> Self {
        match e {
            BufferError::NotContiguous { expected, got } => PredictError::WrongSequence { expected, got },
        }
    }
}

/// The local avatar predictor. Allocation-free after construction.
#[derive(Debug)]
pub struct Predictor<M: MotionStep> {
    motion: M,
    dt: f32,
    buffer: InputBuffer<M::State>,
    state: M::State,
    next_seq: InputSeq,
    last_ack: Option<InputSeq>,
    stats: PredictionStats,
}

impl<M: MotionStep> Predictor<M> {
    /// A predictor starting from `state`, assigning sequences from `first_seq`, holding up
    /// to `capacity` unacknowledged inputs, integrating with timestep `dt`.
    pub fn new(motion: M, state: M::State, first_seq: InputSeq, capacity: usize, dt: f32) -> Self {
        Self {
            motion,
            dt,
            buffer: InputBuffer::with_capacity(capacity),
            state,
            next_seq: first_seq,
            last_ack: None,
            stats: PredictionStats::default(),
        }
    }

    /// The sequence the next input must carry.
    pub fn next_seq(&self) -> InputSeq {
        self.next_seq
    }

    /// Current predicted state.
    pub fn state(&self) -> &M::State {
        &self.state
    }

    /// Timestep.
    pub fn dt(&self) -> f32 {
        self.dt
    }

    /// Counters.
    pub fn stats(&self) -> PredictionStats {
        self.stats
    }

    /// The input history.
    pub fn buffer(&self) -> &InputBuffer<M::State> {
        &self.buffer
    }

    /// Last acknowledged sequence.
    pub fn last_ack(&self) -> Option<InputSeq> {
        self.last_ack
    }

    /// Predicts one input. `input.seq` must equal [`Predictor::next_seq`].
    ///
    /// # Errors
    /// [`PredictError::WrongSequence`]; the predictor is unchanged.
    pub fn predict(
        &mut self,
        ground: &M::Ground,
        mods: &MotionModifiers,
        input: MoveInput,
    ) -> Result<&M::State, PredictError> {
        if input.seq != self.next_seq {
            return Err(PredictError::WrongSequence {
                expected: self.next_seq,
                got: input.seq,
            });
        }
        let next = self.motion.step(ground, &self.state, &input, mods, self.dt);
        self.buffer.push(InputRecord {
            input,
            mods: *mods,
            predicted: next,
        })?;
        self.state = next;
        self.next_seq = self.next_seq.next();
        self.stats.predicted = self.stats.predicted.saturating_add(1);
        Ok(&self.state)
    }

    /// Reconciles against an authoritative state that has applied every input up to and
    /// including `ack`.
    pub fn reconcile(
        &mut self,
        ground: &M::Ground,
        ack: InputSeq,
        authoritative: &M::State,
    ) -> Reconciliation {
        if let Some(last) = self.last_ack
            && !ack.is_newer_than(last)
        {
            self.stats.stale = self.stats.stale.saturating_add(1);
            return Reconciliation::Stale;
        }
        // The newest sequence ever issued is next_seq - 1; acking past it is invalid.
        let newest_issued = InputSeq(self.next_seq.0.wrapping_sub(1));
        if ack.is_newer_than(newest_issued) {
            self.stats.invalid = self.stats.invalid.saturating_add(1);
            return Reconciliation::Invalid;
        }
        self.last_ack = Some(ack);

        if let Some(rec) = self.buffer.get(ack)
            && rec.predicted.bits_eq(authoritative)
        {
            self.buffer.ack(ack);
            self.stats.matched = self.stats.matched.saturating_add(1);
            return Reconciliation::Matched;
        }

        // Rewind and replay.
        let gap = match self.buffer.oldest_seq() {
            Some(oldest) => oldest.is_newer_than(ack.next()),
            None => false,
        };
        self.buffer.ack(ack);
        let before = self.state.position();
        let mut s = *authoritative;
        let mut replayed = 0u32;
        for rec in self.buffer.after_mut(ack) {
            s = self.motion.step(ground, &s, &rec.input, &rec.mods, self.dt);
            rec.predicted = s;
            replayed = replayed.saturating_add(1);
        }
        self.state = s;
        self.stats.corrected = self.stats.corrected.saturating_add(1);
        self.stats.replayed = self.stats.replayed.saturating_add(u64::from(replayed));
        Reconciliation::Corrected {
            replayed,
            correction: before - s.position(),
            gap,
        }
    }

    /// A resumed connection: the server restored `authoritative` (its state before the
    /// disconnect) and applies inputs again from `keep_from`, the first sent on the new
    /// connection; every earlier unacknowledged input was lost with the old one. Drops
    /// those and replays the rest from `authoritative`. `ack` is what that state carries
    /// (the last input applied before the disconnect, if any): later states with the same
    /// acknowledgement are stale, the next newer one is reconciled as usual. Not a
    /// correction: the server will agree with every prediction from here on.
    pub fn resume(
        &mut self,
        ground: &M::Ground,
        authoritative: &M::State,
        ack: Option<InputSeq>,
        keep_from: InputSeq,
    ) {
        let before_kept = InputSeq(keep_from.0.wrapping_sub(1));
        self.buffer.ack(before_kept);
        let mut s = *authoritative;
        for rec in self.buffer.after_mut(before_kept) {
            s = self.motion.step(ground, &s, &rec.input, &rec.mods, self.dt);
            rec.predicted = s;
        }
        self.state = s;
        self.last_ack = ack.filter(|a| keep_from.is_newer_than(*a));
        self.stats.resumed = self.stats.resumed.saturating_add(1);
    }

    /// Hard reset (spawn, teleport, zone change): adopts `state`, drops history, and
    /// continues sequences from `next_seq`.
    pub fn reset(&mut self, state: M::State, next_seq: InputSeq) {
        self.state = state;
        self.buffer.clear();
        self.next_seq = next_seq;
        self.last_ack = None;
    }
}

#[cfg(test)]
mod tests {
    use super::metrics::CorrectionHistogram;
    use super::*;
    use crate::core_api::{
        AimAngles, Angle16, CoreMotion, FlatGround, Motion, MotionParams, MotionState, MoveButtons, Tick,
    };

    type Model = CoreMotion<FlatGround>;

    fn model() -> Model {
        CoreMotion::new(Motion::new(MotionParams::DEFAULT).expect("default motion parameters are valid"))
    }
    use crate::testing::TestResult;

    const DT: f32 = 1.0 / 30.0;

    fn input(seq: u32, buttons: MoveButtons) -> MoveInput {
        MoveInput {
            seq: InputSeq(seq),
            tick: Tick(u64::from(seq)),
            buttons,
            yaw: Angle16(0),
            aim: AimAngles::default(),
        }
    }

    fn pattern(i: u32) -> MoveButtons {
        match i % 7 {
            0..=2 => MoveButtons::FORWARD,
            3 => MoveButtons::FORWARD.with(MoveButtons::STRAFE_RIGHT),
            4 => MoveButtons::NONE,
            5 => MoveButtons::STRAFE_LEFT.with(MoveButtons::WALK),
            _ => MoveButtons::BACKWARD,
        }
    }

    /// A stand-in server: integrates the same inputs with the same motion.
    struct Server {
        motion: Model,
        state: MotionState,
        mods: MotionModifiers,
    }

    impl Server {
        fn apply(&mut self, g: FlatGround, i: &MoveInput) {
            self.state = self.motion.step(&g, &self.state, i, &self.mods, DT);
        }
    }

    fn predictor() -> Predictor<Model> {
        Predictor::new(model(), MotionState::default(), InputSeq(0), 64, DT)
    }

    #[test]
    fn agreeing_server_never_corrects() -> TestResult {
        let g = FlatGround(0.0);
        let mut p = predictor();
        let mut srv = Server {
            motion: model(),
            state: MotionState::default(),
            mods: MotionModifiers::default(),
        };
        let mut hist = CorrectionHistogram::new();
        for i in 0..300u32 {
            let inp = input(i, pattern(i));
            let _ = p.predict(&g, &MotionModifiers::default(), inp)?;
            srv.apply(g, &inp);
            // The server acks with a 3-input lag, as over a network.
            if i >= 3 && i % 2 == 1 {
                let r = p.reconcile(&g, InputSeq(i), &srv.state);
                hist.record(r.magnitude());
                assert_eq!(r, Reconciliation::Matched);
            }
        }
        assert_eq!(p.stats().corrected, 0);
        assert!(p.stats().matched > 100);
        assert_eq!(hist.quantile(0.99), Some(metrics::BUCKET_WIDTH));
        Ok(())
    }

    #[test]
    fn divergence_rewinds_and_replays_to_the_server_trajectory() -> TestResult {
        let g = FlatGround(0.0);
        let mut p = predictor();
        // The server applies a speed modifier the client did not predict.
        let mut srv = Server {
            motion: model(),
            state: MotionState::default(),
            mods: MotionModifiers {
                speed_scale: 0.5,
                ..MotionModifiers::default()
            },
        };
        let mut sent = Vec::new();
        for i in 0..20u32 {
            let inp = input(i, MoveButtons::FORWARD);
            let _ = p.predict(&g, &MotionModifiers::default(), inp)?;
            sent.push(inp);
        }
        for inp in sent.iter().take(11) {
            srv.apply(g, inp);
        }
        let r = p.reconcile(&g, InputSeq(10), &srv.state);
        let Reconciliation::Corrected {
            replayed,
            correction,
            gap,
        } = r
        else {
            return Err(format!("expected a correction, got {r:?}").into());
        };
        assert_eq!((replayed, gap), (9, false));
        assert!(
            correction.z > 0.0,
            "client predicted further than the slower server"
        );
        // The replayed state equals rewinding to the server state and re-integrating the
        // remaining inputs with the client's recorded modifiers, bit for bit.
        let mut expect = srv.state;
        for inp in sent.iter().skip(11) {
            expect = model().step(&g, &expect, inp, &MotionModifiers::default(), DT);
        }
        assert!(p.state().bits_eq(&expect));
        // History now holds the corrected predictions, so a consistent later ack matches.
        let mut srv2 = Server {
            motion: model(),
            state: srv.state,
            mods: MotionModifiers::default(),
        };
        for inp in sent.iter().skip(11).take(4) {
            srv2.apply(g, inp);
        }
        assert_eq!(
            p.reconcile(&g, InputSeq(14), &srv2.state),
            Reconciliation::Matched
        );
        Ok(())
    }

    #[test]
    fn stale_duplicate_and_invalid_acks_are_ignored() -> TestResult {
        let g = FlatGround(0.0);
        let mut p = predictor();
        for i in 0..5 {
            let _ = p.predict(&g, &MotionModifiers::default(), input(i, MoveButtons::FORWARD))?;
        }
        let snapshot = *p.state();
        let auth = p
            .buffer()
            .get(InputSeq(3))
            .map(|r| r.predicted)
            .ok_or("record 3")?;
        assert_eq!(p.reconcile(&g, InputSeq(3), &auth), Reconciliation::Matched);
        assert_eq!(p.reconcile(&g, InputSeq(3), &auth), Reconciliation::Stale);
        assert_eq!(
            p.reconcile(&g, InputSeq(2), &MotionState::default()),
            Reconciliation::Stale
        );
        assert_eq!(
            p.reconcile(&g, InputSeq(9), &MotionState::default()),
            Reconciliation::Invalid
        );
        assert!(p.state().bits_eq(&snapshot), "ignored acks change nothing");
        assert_eq!((p.stats().stale, p.stats().invalid), (2, 1));
        Ok(())
    }

    #[test]
    fn wrong_sequence_is_rejected_without_side_effects() {
        let g = FlatGround(0.0);
        let mut p = predictor();
        let r = p.predict(&g, &MotionModifiers::default(), input(5, MoveButtons::FORWARD));
        assert_eq!(
            r.err(),
            Some(PredictError::WrongSequence {
                expected: InputSeq(0),
                got: InputSeq(5)
            })
        );
        assert_eq!((p.next_seq(), p.buffer().len()), (InputSeq(0), 0));
    }

    #[test]
    fn eviction_is_reported_as_a_gap() -> TestResult {
        let g = FlatGround(0.0);
        let mut p = Predictor::new(model(), MotionState::default(), InputSeq(0), 4, DT);
        for i in 0..10 {
            let _ = p.predict(&g, &MotionModifiers::default(), input(i, MoveButtons::FORWARD))?;
        }
        assert_eq!(p.buffer().evicted(), 6);
        // Ack 2: records 3..=5 were evicted, 6..=9 remain.
        let r = p.reconcile(&g, InputSeq(2), &MotionState::default());
        assert!(
            matches!(
                r,
                Reconciliation::Corrected {
                    replayed: 4,
                    gap: true,
                    ..
                }
            ),
            "{r:?}"
        );
        Ok(())
    }

    #[test]
    fn sequences_wrap() -> TestResult {
        let g = FlatGround(0.0);
        let mut p = Predictor::new(model(), MotionState::default(), InputSeq(u32::MAX - 2), 16, DT);
        let mut seq = InputSeq(u32::MAX - 2);
        for _ in 0..6 {
            let _ = p.predict(
                &g,
                &MotionModifiers::default(),
                MoveInput {
                    seq,
                    ..input(0, MoveButtons::FORWARD)
                },
            )?;
            seq = seq.next();
        }
        assert_eq!(p.next_seq(), InputSeq(3));
        let auth = p
            .buffer()
            .get(InputSeq(0))
            .map(|r| r.predicted)
            .ok_or("record 0")?;
        assert_eq!(p.reconcile(&g, InputSeq(0), &auth), Reconciliation::Matched);
        assert_eq!(p.buffer().oldest_seq(), Some(InputSeq(1)));
        assert_eq!(p.reconcile(&g, InputSeq(u32::MAX), &auth), Reconciliation::Stale);
        Ok(())
    }

    #[test]
    fn histogram_quantiles() {
        let mut h = CorrectionHistogram::new();
        assert_eq!(h.quantile(0.99), None);
        for _ in 0..98 {
            h.record(0.0);
        }
        h.record(0.032);
        h.record(5.0);
        assert_eq!(h.quantile(0.5), Some(metrics::BUCKET_WIDTH));
        assert!((h.quantile(0.99).unwrap_or(0.0) - 0.035).abs() < 1e-6);
        assert_eq!(h.quantile(1.0), Some(f32::INFINITY));
        assert_eq!(h.max(), 5.0);
        h.record(f32::NAN);
        assert_eq!(h.total(), 101);
    }
}
