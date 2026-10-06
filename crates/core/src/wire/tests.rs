use super::*;
use crate::rng::{Rng, Salt, Seed};

fn round_trip<T: Wire + PartialEq + core::fmt::Debug>(v: &T) {
    let mut buf = Vec::new();
    encode_into(v, &mut buf);
    assert_eq!(decode_exact::<T>(&buf).as_ref(), Ok(v));
}

#[test]
fn primitives_round_trip_little_endian() {
    let mut buf = Vec::new();
    encode_into(&0x0102_0304u32, &mut buf);
    assert_eq!(buf, [4, 3, 2, 1]);
    round_trip(&u8::MAX);
    round_trip(&i16::MIN);
    round_trip(&u64::MAX);
    round_trip(&-7i64);
    round_trip(&true);
    round_trip(&-0.0f32);
    round_trip(&Some(5u16));
    round_trip(&None::<u16>);
    round_trip(&EntityId::new(3, 9));
    round_trip(&Tick(77));
    round_trip(&Vec3::new(1.0, -2.0, 3.5));
    round_trip(&ContentHash::of(b"x"));
    round_trip(&MoveInput {
        seq: InputSeq(9),
        tick: Tick(4),
        buttons: MoveButtons::FORWARD.with(MoveButtons::JUMP),
        yaw: Angle16(123),
        aim: AimAngles {
            yaw: Angle16(1),
            pitch: Angle16(2),
        },
    });
}

#[test]
fn boundary_values_are_refused() {
    assert_eq!(decode_exact::<bool>(&[2]), Err(DecodeError::Invalid("bool")));
    assert_eq!(
        decode_exact::<f32>(&f32::NAN.to_bits().to_le_bytes()),
        Err(DecodeError::Invalid("non-finite float"))
    );
    assert_eq!(
        decode_exact::<f32>(&f32::INFINITY.to_bits().to_le_bytes()),
        Err(DecodeError::Invalid("non-finite float"))
    );
    assert_eq!(
        decode_exact::<Option<u8>>(&[2, 0]),
        Err(DecodeError::Invalid("option tag"))
    );
    assert_eq!(
        decode_exact::<MoveButtons>(&0x4000u16.to_le_bytes()),
        Err(DecodeError::Invalid("move buttons"))
    );
    assert_eq!(decode_exact::<u32>(&[1, 2, 3]), Err(DecodeError::UnexpectedEnd));
    assert_eq!(decode_exact::<u8>(&[1, 2]), Err(DecodeError::TrailingBytes));
}

#[test]
fn bounded_array_refuses_hostile_lengths_before_reading() {
    type List = BoundedArray<u32, 4>;
    let ok = List::from_slice(&[1, 2, 3]).unwrap();
    round_trip(&ok);
    assert_eq!(ok.len(), 3);
    assert_eq!(ok.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(ok.get(2), Some(&3));
    assert_eq!(ok.get(3), None);
    // A length of 65535 with no element bytes: refused on the length alone.
    assert_eq!(
        decode_exact::<List>(&u16::MAX.to_le_bytes()),
        Err(DecodeError::Invalid("length over bound"))
    );
    assert_eq!(
        decode_exact::<List>(&[5, 0]),
        Err(DecodeError::Invalid("length over bound"))
    );
    assert!(List::from_slice(&[0; 5]).is_none());
    let mut full = List::from_slice(&[0; 4]).unwrap();
    assert_eq!(full.push(9), Err(crate::mem::CapacityError(9)));
    full.clear();
    assert!(full.is_empty());
    assert_eq!(full.capacity(), 4);
    // Equality ignores dead slots.
    let mut a = List::new();
    a.push(1).unwrap();
    let b = List::from_slice(&[1]).unwrap();
    assert_eq!(a, b);
    assert_eq!(format!("{a:?}"), "[1]");
}

#[test]
fn strings_are_bounded_utf8() {
    type Name = WireString<8>;
    let n = Name::new("héllo").unwrap();
    assert_eq!(n.as_str(), "héllo");
    round_trip(&n);
    assert!(Name::new("much too long").is_none());
    assert_eq!(
        decode_exact::<Name>(&[2, 0, 0xFF, 0xFE]),
        Err(DecodeError::Invalid("utf-8"))
    );
    assert_eq!(
        decode_exact::<Name>(&[9, 0]),
        Err(DecodeError::Invalid("length over bound"))
    );
    assert_eq!(
        decode_exact::<Name>(&[3, 0, b'a']),
        Err(DecodeError::UnexpectedEnd)
    );
    assert_eq!(format!("{n:?}"), "\"héllo\"");
}

/// Every fuzz sample is valid by construction: it round-trips exactly.
#[test]
fn fuzz_samples_round_trip() {
    let mut rng = Rng::for_cell(Seed(1), Tick(0), Salt::named("test.wire.fuzz"));
    for _ in 0..2000 {
        round_trip(&u64::fuzz_sample(&mut rng));
        round_trip(&i8::fuzz_sample(&mut rng));
        round_trip(&f32::fuzz_sample(&mut rng));
        round_trip(&Option::<Vec3>::fuzz_sample(&mut rng));
        round_trip(&MoveInput::fuzz_sample(&mut rng));
        round_trip(&BoundedArray::<EntityId, 16>::fuzz_sample(&mut rng));
        round_trip(&WireString::<20>::fuzz_sample(&mut rng));
        round_trip(&ContentHash::fuzz_sample(&mut rng));
    }
}

#[test]
fn errors_display() {
    assert_eq!(
        WireError::UnknownMessage(MessageId(7)).to_string(),
        "unknown message id 7"
    );
    assert_eq!(
        WireError::Rejected {
            message: "Move",
            reason: ValidationError("speed")
        }
        .to_string(),
        "Move: validation failed: speed"
    );
    assert_eq!(
        WireError::Decode {
            message: "Move",
            error: DecodeError::TrailingBytes
        }
        .to_string(),
        "Move: trailing bytes"
    );
}
