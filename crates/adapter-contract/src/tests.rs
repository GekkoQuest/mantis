use super::*;
use crate::core_types::*;

/// A test validator: refuses `Choose` options above 3 and empty tokens.
struct Strict;

impl Validators for Strict {
    fn validate_hello(&self, msg: &Hello) -> Result<(), ValidationError> {
        if msg.token.is_empty() {
            Err(ValidationError("empty token"))
        } else {
            Ok(())
        }
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
    fn validate_choose(&self, msg: &Choose) -> Result<(), ValidationError> {
        if msg.option > 3 {
            Err(ValidationError("option out of range"))
        } else {
            Ok(())
        }
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

fn bytes<T: Wire>(m: &T) -> Vec<u8> {
    let mut v = Vec::new();
    encode_into(m, &mut v);
    v
}

#[test]
fn registered_ids_are_pinned() {
    // These ids are the protocol; the registry and lock guard them too.
    assert_eq!(<Hello as Message>::ID, MessageId(1));
    assert_eq!(<Move as Message>::ID, MessageId(2));
    assert_eq!(<SnapshotAck as Message>::ID, MessageId(8));
    assert_eq!(<Welcome as Message>::ID, MessageId(10));
    assert_eq!(<SetPosition as Message>::ID, MessageId(12));
}

#[test]
fn inbound_decode_validates_and_fails_closed() {
    let choose = Choose {
        prompt: PromptId(7),
        option: 2,
    };
    let got = decode_inbound(MessageId(6), &bytes(&choose), &Strict).unwrap();
    assert_eq!(got, Inbound::Choose(choose));
    assert_eq!(got.id(), MessageId(6));

    let bad = Choose {
        prompt: PromptId(7),
        option: 9,
    };
    assert_eq!(
        decode_inbound(MessageId(6), &bytes(&bad), &Strict),
        Err(WireError::Rejected {
            message: "Choose",
            reason: ValidationError("option out of range")
        })
    );
    assert_eq!(
        Inbound::Choose(bad).validate(&Strict),
        decode_inbound(MessageId(6), &bytes(&bad), &Strict).map(|_| ())
    );
    assert_eq!(
        decode_inbound(MessageId(99), &[], &Strict),
        Err(WireError::UnknownMessage(MessageId(99)))
    );
    // An outbound id is not an inbound message.
    assert_eq!(
        decode_inbound(MessageId(10), &[], &Strict),
        Err(WireError::UnknownMessage(MessageId(10)))
    );
    let mut trailing = bytes(&choose);
    trailing.push(0);
    assert_eq!(
        decode_inbound(MessageId(6), &trailing, &Strict),
        Err(WireError::Decode {
            message: "Choose",
            error: DecodeError::TrailingBytes
        })
    );
}

#[test]
fn intents_round_trip() {
    let mv = Move {
        input: MoveInput {
            seq: InputSeq(5),
            tick: Tick(9),
            buttons: MoveButtons::FORWARD,
            yaw: Angle16(1000),
            aim: AimAngles::default(),
        },
    };
    let mut out = Vec::new();
    Inbound::Move(mv).encode(&mut out);
    assert_eq!(decode_inbound(MessageId(2), &out, &Strict), Ok(Inbound::Move(mv)));
    let cast = Cast {
        ability: AbilityId(3),
        target: Some(EntityId::new(4, 1)),
        view_tick: Tick(100),
        view_frac: 32_768,
    };
    assert_eq!(
        decode_inbound(MessageId(4), &bytes(&cast), &Strict),
        Ok(Inbound::Cast(cast))
    );
    let mut hello = Hello {
        protocol: PROTOCOL_VERSION,
        capabilities: 0,
        content: ContentHash::of(b"c"),
        modules: BoundedArray::new(),
        token: BoundedArray::new(),
    };
    assert!(
        decode_inbound(MessageId(1), &bytes(&hello), &Strict).is_err(),
        "empty token refused"
    );
    hello.token.push(1).unwrap();
    hello
        .modules
        .push(ModuleEntry {
            name: WireString::new("std.party").unwrap(),
            hash: ContentHash::of(b"m"),
        })
        .unwrap();
    assert_eq!(
        decode_inbound(MessageId(1), &bytes(&hello), &Strict),
        Ok(Inbound::Hello(hello))
    );
}

#[test]
fn outbound_round_trip_and_movement_mode_wire() {
    let welcome = Welcome {
        protocol: 1,
        capabilities: 0b101,
        session: 77,
        tick: Tick(5),
        tick_rate: 30,
        mode: MovementMode::Validated,
        avatar: Some(EntityId::new(1, 0)),
        character: 42,
    };
    let mut out = Vec::new();
    Outbound::Welcome(welcome).encode(&mut out);
    assert_eq!(
        decode_outbound(MessageId(10), &out),
        Ok(Outbound::Welcome(welcome))
    );
    assert_eq!(bytes(&MovementMode::Predictive), [0]);
    assert_eq!(bytes(&MovementMode::Validated), [1]);
    assert_eq!(
        decode_exact::<MovementMode>(&[2]),
        Err(DecodeError::Invalid("MovementMode"))
    );
    let nan = SetPosition {
        entity: EntityId::new(1, 0),
        position: Vec3::new(f32::NAN, 0.0, 0.0),
        yaw: Angle16(0),
        tick: Tick(1),
    };
    assert!(
        decode_outbound(MessageId(12), &bytes(&nan)).is_err(),
        "non-finite refused"
    );
}

struct Order(Vec<&'static str>);
impl SnapshotVisitor for Order {
    fn header(&mut self, _: &SnapshotHeader) {
        self.0.push("header");
    }
    fn entered(&mut self, _: EntityId, _: AppearanceId) {
        self.0.push("entered");
    }
    fn remote(&mut self, _: &RemoteSample) {
        self.0.push("remote");
    }
    fn removed(&mut self, _: EntityId) {
        self.0.push("removed");
    }
    fn marker(&mut self, _: &TimelineMarker) {
        self.0.push("marker");
    }
}

#[test]
fn snapshot_frame_collects_in_contract_order_and_fails_closed() {
    let mut f = SnapshotFrame::with_capacity(1, 2, 1, 0);
    f.header(&SnapshotHeader {
        server_tick: Tick(9),
        ..SnapshotHeader::default()
    });
    f.entered(EntityId::new(1, 0), AppearanceId(4));
    let sample = RemoteSample {
        id: EntityId::new(1, 0),
        tick: Tick(9),
        position: Vec3::X,
        velocity: Vec3::ZERO,
        yaw: Angle16(0),
    };
    f.remote(&sample);
    f.remote(&sample);
    f.remote(&sample); // over capacity
    f.removed(EntityId::new(2, 0));
    let marker = TimelineMarker {
        id: MarkerId {
            graph: GraphId(1),
            node: NodeKey(1),
        },
        kind: MarkerKind::CastStart,
        at: Tick(9),
        offset: 0,
        source: EntityId::new(1, 0),
        target: None,
        instance: GraphInstanceId(1),
    };
    f.marker(&marker); // capacity 0
    assert_eq!(f.overflowed, 2);
    assert_eq!(f.find_remote(EntityId::new(1, 0)), Some(&sample));
    let mut order = Order(Vec::new());
    f.visit(&mut order);
    assert_eq!(order.0, ["header", "entered", "remote", "remote", "removed"]);
    f.clear();
    assert_eq!(
        (f.remotes.len(), f.overflowed, f.header.server_tick),
        (0, 0, Tick::ZERO)
    );
}
