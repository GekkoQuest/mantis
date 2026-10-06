//! std.titles end to end: grants from services and from other modules'
//! events, showing a title, the query, refusals, and replay.

#![expect(clippy::unwrap_used)]

use std::sync::Arc;

use mantis_core::ecs::World;
use mantis_core::module::{Events, ask};
use mantis_core::schedule::{Phase, SystemDesc, SystemError, TickContext};
use mantis_core::wire::{Message, decode_message};
use mantis_server::harness::{Harness, bytes_of};
use mantis_server::modules::{ExtensionKind, ExtensionRefusal, Registrar, RegistryError, ServerModule};
use std_titles_contract::{ActiveTitle, AwardTitle, GrantTitle, SetActiveTitle, TITLES_TABLE, Titles};

const TITLES: &str = include_str!("../../manifest.toml");
const LIST: &[u8] = b"# id name\n1 the Swift\n2 the Patient\n";
const AWARDER: &str = "[module]\nkey = \"test.awarder\"\nversion = \"0.1.0\"\n";

/// Another module: awards title 2 to character 12 on tick 3, through the
/// titles contract's event.
struct Awarder;

impl ServerModule for Awarder {
    fn key(&self) -> &'static str {
        "test.awarder"
    }
    fn register(&self, r: &mut Registrar<'_>) -> Result<(), RegistryError> {
        r.event::<AwardTitle>(8)?;
        let access = r.access().write_resource::<Events<AwardTitle>>().build()?;
        r.system(
            SystemDesc {
                name: "test.awarder.award",
                phase: Phase::Timers,
                priority: 0,
                access,
            },
            |w: &mut World, ctx: &TickContext| -> Result<(), SystemError> {
                if ctx.tick.0 == 3
                    && let Some(q) = w.resource_mut::<Events<AwardTitle>>()
                {
                    q.send(AwardTitle {
                        character: 12,
                        title: 2,
                    });
                }
                Ok(())
            },
        )
    }
}

fn bed() -> Harness {
    let mut h = Harness::new(
        &[Arc::new(std_titles_server::Module), Arc::new(Awarder)],
        &[TITLES, AWARDER],
        &[],
        &[(TITLES_TABLE, LIST)],
    )
    .unwrap();
    h.join(1, 11, 0.0, 0.0);
    h.join(2, 12, 0.0, 0.0);
    h.tick().unwrap();
    h
}

#[test]
fn titles_are_granted_shown_and_asked_about() {
    let mut h = bed();
    h.service(
        GrantTitle::ID.0,
        &bytes_of(&GrantTitle {
            character: 11,
            title: 1,
        }),
    );
    h.tick().unwrap();
    h.send(1, SetActiveTitle::ID.0, &bytes_of(&SetActiveTitle { title: 1 }));
    h.tick().unwrap();
    assert_eq!(ask(h.world(), &ActiveTitle(11)).unwrap(), Some(1));
    let got = h.take(1);
    let last = decode_message::<Titles>(&got.messages.last().unwrap().1).unwrap();
    assert_eq!(
        (last.owned.iter().copied().collect::<Vec<_>>(), last.active),
        (vec![1], 1)
    );
    // The other module's award arrives the tick after it was sent, and is
    // recorded as a system outcome for the persistence writer.
    h.ticks(2).unwrap();
    assert!(h.outcomes.iter().any(|o| {
        o.kind == ExtensionKind(GrantTitle::ID.0)
            && o.session.is_none()
            && decode_message::<GrantTitle>(o.payload.as_slice())
                .is_ok_and(|g| g.character == 12 && g.title == 2)
    }));
    let got = h.take(2);
    assert!(
        got.messages
            .iter()
            .any(|(k, _)| *k == ExtensionKind(Titles::ID.0))
    );
    h.send(2, SetActiveTitle::ID.0, &bytes_of(&SetActiveTitle { title: 2 }));
    h.tick().unwrap();
    assert_eq!(ask(h.world(), &ActiveTitle(12)).unwrap(), Some(2));
    assert!(h.replay().unwrap() > 0);
}

#[test]
fn bad_titles_are_refused() {
    let mut h = bed();
    h.send(1, SetActiveTitle::ID.0, &bytes_of(&SetActiveTitle { title: 1 })); // not held
    h.send(
        1,
        GrantTitle::ID.0,
        &bytes_of(&GrantTitle {
            character: 11,
            title: 1,
        }),
    ); // clients cannot grant
    h.service(
        GrantTitle::ID.0,
        &bytes_of(&GrantTitle {
            character: 11,
            title: 9,
        }),
    ); // unknown title
    h.tick().unwrap();
    let reasons: Vec<ExtensionRefusal> = h.take(1).refusals.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        [ExtensionRefusal::NotAllowed, ExtensionRefusal::NotAllowed]
    );
    assert_eq!(
        h.outcomes.last().unwrap().result,
        Err(ExtensionRefusal::NotAllowed)
    );
    assert_eq!(ask(h.world(), &ActiveTitle(11)).unwrap(), None);
}
