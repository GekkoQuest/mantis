//! Steady-state frames allocate nothing; a reshape is measured.

mod common;

use common::{TestResult, VIEWPORT, center, sample_ui};
use mantis_testkit::alloc::{assert_no_alloc, count_allocs};
use mantis_ui::{ListItem, UiEvent, Value};

#[global_allocator]
static ALLOC: mantis_testkit::alloc::CountingAllocator = mantis_testkit::alloc::CountingAllocator;

#[test]
fn steady_frames_allocate_nothing_and_reshape_is_measured() -> TestResult {
    let mut ui = sample_ui()?;
    {
        let props = ui.properties_mut();
        let items = props.intern("inventory.items");
        let list = (0..4)
            .map(|i| ListItem::new().with("name", Value::Text(format!("item {i}"))))
            .collect();
        props.set(items, Value::List(list));
    }
    let scale = 1.5;
    let viewport = [VIEWPORT[0] * scale, VIEWPORT[1] * scale];
    ui.frame(viewport, scale);
    let ok = ui.rect_of("ok").ok_or("ok")?;
    let cancel = ui.rect_of("cancel").ok_or("cancel")?;
    let (ox, oy) = center(ok, scale);
    let (cx, cy) = center(cancel, scale);
    let outside = UiEvent::PointerMove {
        x: 790.0 * scale,
        y: 590.0 * scale,
    };
    let over_ok = UiEvent::PointerMove { x: ox, y: oy };
    let over_cancel = UiEvent::PointerMove { x: cx, y: cy };
    // Warm up: every hover state once, so the draw list reaches capacity.
    for ev in [&over_ok, &over_cancel, &outside] {
        ui.handle(ev);
        ui.frame(viewport, scale);
    }
    let quads = ui.draw_list().len();
    assert!(quads > 20);

    assert_no_alloc("steady ui frames", || {
        for _ in 0..3 {
            ui.handle(&over_ok);
            ui.frame(viewport, scale);
            ui.handle(&over_cancel);
            ui.frame(viewport, scale);
            ui.handle(&outside);
            ui.frame(viewport, scale);
        }
    });
    assert_eq!(ui.draw_list().len(), quads);

    // Toggling an enabled flag is paint-only: after one warm-up toggle,
    // disabling and re-enabling a button allocates nothing and reshapes nothing.
    let can_confirm = ui.properties_mut().intern("dialog.can_confirm");
    let ok_shapes = ui.reshape_count("ok");
    for on in [false, true] {
        ui.properties_mut().set_bool(can_confirm, on);
        ui.frame(viewport, scale);
    }
    let enabled_quads = ui.draw_list().quads.clone();
    assert_no_alloc("toggle enabled flag", || {
        for on in [false, true, false, true] {
            ui.properties_mut().set_bool(can_confirm, on);
            ui.frame(viewport, scale);
        }
    });
    assert_eq!(ui.reshape_count("ok"), ok_shapes);
    assert_eq!(ui.draw_list().quads, enabled_quads);
    ui.properties_mut().set_bool(can_confirm, false);
    ui.frame(viewport, scale);
    assert_ne!(ui.draw_list().quads, enabled_quads, "disabled colors differ");
    ui.properties_mut().set_bool(can_confirm, true);
    ui.frame(viewport, scale);

    // Changing an Int bound into a template reshapes that node only.
    let name_shapes = ui.reshape_count("name");
    let hp_shapes = ui.reshape_count("hp").ok_or("hp")?;
    let hp = ui.properties_mut().intern("hp");
    ui.properties_mut().set_int(hp, 91);
    let ((), stats) = count_allocs(|| {
        ui.frame(viewport, scale);
    });
    println!("MANTIS-METRIC ui_reshape_allocs n={}", stats.allocs);
    assert_eq!(ui.text_of("hp"), Some("HP 91 / 100"));
    assert_eq!(ui.reshape_count("hp"), Some(hp_shapes + 1));
    assert_eq!(
        ui.reshape_count("name"),
        name_shapes,
        "unrelated text is not reshaped"
    );

    // And the frame after that is steady again.
    assert_no_alloc("frame after reshape", || {
        ui.frame(viewport, scale);
    });
    Ok(())
}
