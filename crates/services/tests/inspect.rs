//! The inspector's wire forms (plan 13): entity pages as bounded text that
//! round-trips any value, pages cut to the wire bound, system times in
//! microseconds, and the log lag from what the writer has made durable.

use mantis_core::ecs::{EntityId, InspectPage, InspectedEntity};
use mantis_core::schedule::{Phase, Timing};
use mantis_services::inspect::{
    InspectorState, PAGE_TEXT, VALUE_TEXT, cut, entity_page, page_rows, phase_name, system_times,
};

fn entity(i: u32, values: &[(&'static str, &str)]) -> InspectedEntity {
    InspectedEntity {
        id: EntityId::new(i, 2),
        components: values.iter().map(|(n, v)| (*n, (*v).to_owned())).collect(),
    }
}

#[test]
fn entity_pages_round_trip_any_value_and_fit_the_wire() {
    let tricky = "Name(\"a\\tb\\nc\\\\d=e\")";
    let page = InspectPage {
        total: 7,
        entities: vec![
            entity(1, &[("test.name", tricky), ("test.hp", "Hp(5)")]),
            entity(4, &[("test.hp", "Hp(9)")]),
        ],
    };
    let wire = entity_page(3, 40, "test.hp", &page);
    assert_eq!((wire.cell.0, wire.tick, wire.total, wire.count), (3, 40, 7, 2));
    assert_eq!(wire.component.as_str(), "test.hp");
    let rows = page_rows(wire.text.as_str());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "1:2");
    assert_eq!(
        rows[0].1,
        vec![
            ("test.name".to_owned(), tricky.to_owned()),
            ("test.hp".to_owned(), "Hp(5)".to_owned())
        ]
    );
    assert_eq!(
        rows[1],
        ("4:2".to_owned(), vec![("test.hp".to_owned(), "Hp(9)".to_owned())])
    );

    // Long values are cut on a character boundary; a page stops at the
    // wire bound and says how many entities it holds.
    let long = "é".repeat(300);
    assert!(cut(&long, VALUE_TEXT).len() <= VALUE_TEXT);
    let big = InspectPage {
        total: 100,
        entities: (0..100)
            .map(|i| {
                entity(
                    i,
                    &[
                        ("a.one", &long),
                        ("a.two", &long),
                        ("a.three", &long),
                        ("a.four", &long),
                    ],
                )
            })
            .collect(),
    };
    let wire = entity_page(1, 1, "a.one", &big);
    assert!(wire.count > 0 && wire.count < 100, "{}", wire.count);
    assert!(wire.text.as_str().len() <= PAGE_TEXT);
    assert_eq!(page_rows(wire.text.as_str()).len(), usize::from(wire.count));
}

#[test]
fn system_times_are_in_microseconds_and_the_log_lag_follows_the_writer() {
    let mut t = Timing::default();
    t.record(2_500);
    t.record(1_000);
    let mut encode = Timing::default();
    encode.record(12_001);
    let times = system_times(2, 90, [("m.one", Phase::Movement, &t)], 4, &encode);
    assert_eq!(times.systems.len(), 1);
    let s = times.systems.iter().next().unwrap();
    assert_eq!(
        (s.name.as_str(), s.micros_last, s.micros_p99, s.runs),
        ("m.one", 1, 3, 2)
    );
    assert_eq!(phase_name(s.phase), "Movement");
    assert_eq!((times.inbox_depth, times.encode_micros_p99), (4, 13));

    let (tx, _rx) = std::sync::mpsc::channel();
    let state = InspectorState::new(tx);
    assert_eq!(state.log_lag(2, 90), 0, "nothing pending");
    state.pushed(2, 80);
    state.pushed(2, 85);
    assert_eq!(state.log_lag(2, 90), 10);
    state.durable(2, 80);
    assert_eq!(state.log_lag(2, 90), 5);
    state.durable(2, 85);
    assert_eq!(state.log_lag(2, 90), 0);
}
