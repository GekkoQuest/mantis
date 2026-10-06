use super::*;
use crate::hash::StableHasher;
use crate::kinematics::{MoveButtons, MoveInput};

struct Schema;
impl LogSchema for Schema {
    type Intent = MoveInput;
    type Command = u64;
    type Outcome = u64;
}

type Entry = SchemaEntry<Schema>;

const BUILD: BuildId = BuildId([7; 32]);

fn content() -> ContentHash {
    ContentHash::of(b"test content")
}

fn header() -> LogHeader {
    LogHeader {
        build: BUILD,
        content: content(),
        cell: CellId(3),
        seed: Seed(99),
        start_tick: Tick(10),
    }
}

/// Counts syncs, to observe the fsync policy.
#[derive(Default)]
struct CountingSink {
    bytes: Vec<u8>,
    syncs: u32,
}

impl LogSink for CountingSink {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.syncs += 1;
        Ok(())
    }
}

fn mv(seq: u32) -> MoveInput {
    MoveInput {
        seq: crate::kinematics::InputSeq(seq),
        tick: Tick(u64::from(seq)),
        buttons: MoveButtons::FORWARD,
        ..MoveInput::default()
    }
}

/// Two ticks of every record kind.
fn sample_log() -> (Vec<u8>, Vec<Entry>) {
    let mut w = LogWriter::<Schema, Vec<u8>>::create(Vec::new(), &header(), 4096).unwrap();
    let expected = vec![
        LogEntry::Intent {
            tick: Tick(10),
            session: SessionId(5),
            intent: mv(1),
        },
        LogEntry::Seed {
            tick: Tick(10),
            seed: 0xABCD,
        },
        LogEntry::TickEnd {
            tick: Tick(10),
            state_hash: 111,
        },
        LogEntry::Command {
            tick: Tick(11),
            command: 42,
        },
        LogEntry::Outcome {
            tick: Tick(11),
            outcome: 43,
        },
        LogEntry::Intent {
            tick: Tick(11),
            session: SessionId(6),
            intent: mv(2),
        },
        LogEntry::TickEnd {
            tick: Tick(11),
            state_hash: 222,
        },
    ];
    for e in &expected {
        match e {
            LogEntry::Intent {
                tick,
                session,
                intent,
            } => w.append_intent(*tick, *session, intent).unwrap(),
            LogEntry::Seed { tick, seed } => w.append_seed(*tick, *seed).unwrap(),
            LogEntry::Command { tick, command } => w.append_command(*tick, command).unwrap(),
            LogEntry::Outcome { tick, outcome } => w.append_outcome(*tick, outcome).unwrap(),
            LogEntry::TickEnd { tick, state_hash } => w.end_tick(*tick, *state_hash).unwrap(),
        }
    }
    w.flush_segment().unwrap();
    (w.into_sink(), expected)
}

fn read_all(bytes: &[u8]) -> Result<Vec<Entry>, LogError> {
    let mut r = LogReader::<Schema>::open(bytes, BUILD, content())?;
    let mut out = Vec::new();
    while let Some(e) = r.next_entry()? {
        out.push(e);
    }
    Ok(out)
}

#[test]
fn round_trip() {
    let (bytes, expected) = sample_log();
    let r = LogReader::<Schema>::open(&bytes, BUILD, content()).unwrap();
    assert_eq!(*r.header(), header());
    assert_eq!(read_all(&bytes).unwrap(), expected);
}

#[test]
fn header_layout_is_pinned() {
    let (bytes, _) = sample_log();
    assert_eq!(bytes.get(..8), Some(&MAGIC[..]));
    assert_eq!(bytes.get(8..10), Some(&[1u8, 0][..]), "version 1, little-endian");
    assert_eq!(bytes.get(10..42), Some(&[7u8; 32][..]), "build id");
    assert_eq!(bytes.get(42..74), Some(&content().as_bytes()[..]));
    assert_eq!(bytes.get(74..82), Some(&3u64.to_le_bytes()[..]), "cell");
    assert_eq!(bytes.get(82..90), Some(&99u64.to_le_bytes()[..]), "seed");
    assert_eq!(bytes.get(90..98), Some(&10u64.to_le_bytes()[..]), "start tick");
    let check = StableHasher::hash_bytes(bytes.get(..98).unwrap());
    assert_eq!(bytes.get(98..106), Some(&check.to_le_bytes()[..]));
}

#[test]
fn refuses_other_build_content_and_garbage() {
    let (bytes, _) = sample_log();
    assert_eq!(
        LogReader::<Schema>::open(&bytes, BuildId([8; 32]), content()).err(),
        Some(LogError::BuildMismatch {
            expected: BuildId([8; 32]),
            found: BUILD
        })
    );
    let other = ContentHash::of(b"other");
    assert_eq!(
        LogReader::<Schema>::open(&bytes, BUILD, other).err(),
        Some(LogError::ContentMismatch {
            expected: other,
            found: content()
        })
    );
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(
        LogReader::<Schema>::open(&bad, BUILD, content()).err(),
        Some(LogError::BadMagic)
    );
    let mut bad = bytes.clone();
    bad[8] = 2;
    assert_eq!(
        LogReader::<Schema>::open(&bad, BUILD, content()).err(),
        Some(LogError::UnsupportedVersion(2))
    );
    let mut bad = bytes.clone();
    bad[80] ^= 1;
    assert_eq!(
        LogReader::<Schema>::open(&bad, BUILD, content()).err(),
        Some(LogError::HeaderCorrupt)
    );
    assert_eq!(
        LogReader::<Schema>::open(&bytes[..50], BUILD, content()).err(),
        Some(LogError::TruncatedTail { offset: 0 })
    );
}

#[test]
fn every_flipped_record_byte_is_detected() {
    let (bytes, _) = sample_log();
    for i in HEADER_LEN..bytes.len() {
        let mut bad = bytes.clone();
        bad[i] ^= 0x40;
        let r = read_all(&bad);
        assert!(
            matches!(r, Err(LogError::Corrupt { .. } | LogError::TruncatedTail { .. })),
            "flip at {i} gave {r:?}"
        );
    }
}

#[test]
fn every_truncation_is_a_clean_prefix() {
    let (bytes, expected) = sample_log();
    for cut in HEADER_LEN..=bytes.len() {
        let mut r = LogReader::<Schema>::open(&bytes[..cut], BUILD, content()).unwrap();
        let mut got = Vec::new();
        let end = loop {
            match r.next_entry() {
                Ok(Some(e)) => got.push(e),
                other => break other,
            }
        };
        assert_eq!(got, expected[..got.len()], "cut {cut}: records read are a prefix");
        if cut == bytes.len() || got.len() == expected.len() {
            assert_eq!(end, Ok(None));
        } else {
            match end {
                Ok(None) => assert_eq!(r.position(), cut, "clean end only on a record boundary"),
                Err(LogError::TruncatedTail { offset }) => assert_eq!(offset, r.position()),
                other => panic!("cut {cut}: {other:?}"),
            }
        }
    }
}

#[test]
fn appends_are_tick_ordered() {
    let mut w = LogWriter::<Schema, Vec<u8>>::create(Vec::new(), &header(), 256).unwrap();
    assert_eq!(
        w.append_seed(Tick(9), 1),
        Err(LogError::OutOfOrder {
            expected_at_least: Tick(10),
            got: Tick(9)
        })
    );
    w.append_seed(Tick(10), 1).unwrap();
    w.append_seed(Tick(12), 1).unwrap();
    assert_eq!(
        w.append_seed(Tick(11), 1),
        Err(LogError::OutOfOrder {
            expected_at_least: Tick(12),
            got: Tick(11)
        })
    );
    w.end_tick(Tick(12), 0).unwrap();
    assert_eq!(
        w.append_command(Tick(12), &1),
        Err(LogError::OutOfOrder {
            expected_at_least: Tick(13),
            got: Tick(12)
        })
    );
}

#[test]
fn fsync_policy() {
    let mut w = LogWriter::<Schema, CountingSink>::create(CountingSink::default(), &header(), 256).unwrap();
    assert_eq!(w.sink().syncs, 1, "header is synced");
    w.append_intent(Tick(10), SessionId(1), &mv(1)).unwrap();
    w.end_tick(Tick(10), 0).unwrap();
    let r = w.flush_segment().unwrap();
    assert!(!r.synced, "intents only: no fsync");
    assert!(r.bytes > 0);
    w.append_command(Tick(11), &5).unwrap();
    w.end_tick(Tick(11), 0).unwrap();
    assert!(w.flush_segment().unwrap().synced, "economy command: fsync");
    w.append_outcome(Tick(12), &6).unwrap();
    let r = w.commit().unwrap();
    assert!(r.synced, "outcome committed before the ack");
    assert_eq!(w.pending_bytes(), 0);
    assert!(w.commit().unwrap().synced, "commit always syncs");
    assert_eq!(
        w.flush_segment().unwrap(),
        FlushReport {
            bytes: 0,
            synced: false
        }
    );
    assert_eq!(w.sink().syncs, 4);
}

#[test]
fn malformed_payloads_fail_closed() {
    // A record with a valid checksum but an unknown kind, then one whose
    // payload has trailing bytes, then undefined move buttons.
    let frame = |body: &[u8]| {
        let mut v = Vec::new();
        v.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        v.extend_from_slice(body);
        v.extend_from_slice(&StableHasher::hash_bytes(body).to_le_bytes());
        v
    };
    let base = LogWriter::<Schema, Vec<u8>>::create(Vec::new(), &header(), 0)
        .unwrap()
        .into_sink();
    let mut body = vec![9u8];
    body.extend_from_slice(&10u64.to_le_bytes());
    let mut bytes = base.clone();
    bytes.extend(frame(&body));
    assert_eq!(read_all(&bytes), Err(LogError::Corrupt { offset: HEADER_LEN }));

    let mut body = vec![KIND_SEED];
    body.extend_from_slice(&10u64.to_le_bytes());
    body.extend_from_slice(&1u64.to_le_bytes());
    body.push(0);
    let mut bytes = base.clone();
    bytes.extend(frame(&body));
    assert_eq!(
        read_all(&bytes),
        Err(LogError::Decode {
            offset: HEADER_LEN,
            error: DecodeError::TrailingBytes
        })
    );

    let mut body = vec![KIND_INTENT];
    body.extend_from_slice(&10u64.to_le_bytes());
    body.extend_from_slice(&1u64.to_le_bytes()); // session
    body.extend_from_slice(&1u32.to_le_bytes()); // seq
    body.extend_from_slice(&10u64.to_le_bytes()); // tick
    body.extend_from_slice(&0x8000u16.to_le_bytes()); // undefined button bit
    body.extend_from_slice(&[0; 6]);
    let mut bytes = base;
    bytes.extend(frame(&body));
    assert_eq!(
        read_all(&bytes),
        Err(LogError::Decode {
            offset: HEADER_LEN,
            error: DecodeError::Invalid("move buttons")
        })
    );
}

#[test]
fn file_sink_round_trip() {
    let path = std::env::temp_dir().join(format!("mantis-log-test-{}.mlog", std::process::id()));
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut w = LogWriter::<Schema, std::fs::File>::create(file, &header(), 1024).unwrap();
        w.append_command(Tick(10), &77).unwrap();
        w.end_tick(Tick(10), 5).unwrap();
        assert!(w.flush_segment().unwrap().synced);
    }
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        read_all(&bytes).unwrap(),
        vec![
            LogEntry::Command {
                tick: Tick(10),
                command: 77
            },
            LogEntry::TickEnd {
                tick: Tick(10),
                state_hash: 5
            },
        ]
    );
}
