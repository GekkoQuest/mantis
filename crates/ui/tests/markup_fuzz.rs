//! Parsing never panics: every truncation of a valid file and random byte
//! flips either parse or return a positioned error.

mod common;

use common::{LAYOUT, THEME, TestResult};
use mantis_ui::Ui;
use mantis_ui::markup::{parse_layout, parse_template, parse_theme};

/// Deterministic xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

fn check(src: &str) {
    for result in [parse_layout(src).err(), parse_theme(src).err()]
        .into_iter()
        .flatten()
    {
        assert!(result.line >= 1 && result.column >= 1, "{result}");
        assert!(!result.message.is_empty());
    }
    let _ = parse_template(src);
}

#[test]
fn every_truncation_parses_or_errors() {
    let full = format!("{THEME}\n{LAYOUT}");
    for (i, _) in full.char_indices() {
        check(full.get(..i).unwrap_or(""));
    }
    check(&full);
}

#[test]
fn random_byte_flips_never_panic() {
    let full = format!("{THEME}\n{LAYOUT}");
    let bytes = full.as_bytes();
    let mut rng = Rng(0x6d61_6e74_6973_5549);
    let specials = b"{}=:\"#\\/-.0123456789 \n";
    for _ in 0..3000 {
        let mut b = bytes.to_vec();
        for _ in 0..=rng.below(4) {
            let i = rng.below(b.len());
            let v = if rng.below(2) == 0 {
                specials.get(rng.below(specials.len())).copied().unwrap_or(b'x')
            } else {
                u8::try_from(rng.below(256)).unwrap_or(0)
            };
            if let Some(slot) = b.get_mut(i) {
                *slot = v;
            }
        }
        check(&String::from_utf8_lossy(&b));
    }
}

#[test]
fn garbage_reload_keeps_the_old_tree() -> TestResult {
    let mut ui = common::sample_ui()?;
    ui.frame(common::VIEWPORT, 1.0);
    let before = ui.rect_of("ok");
    let full = LAYOUT.as_bytes();
    let mut rng = Rng(42);
    let mut errors = 0;
    for _ in 0..300 {
        let mut b = full.to_vec();
        let i = rng.below(b.len());
        if let Some(slot) = b.get_mut(i) {
            *slot = b"{}=\"#".get(rng.below(5)).copied().unwrap_or(b'{');
        }
        let src = String::from_utf8_lossy(&b).into_owned();
        match ui.reload(&src, None) {
            Ok(_) => {
                // Restore the original for the next round.
                ui.reload(LAYOUT, None)?;
            }
            Err(_) => errors += 1,
        }
        ui.frame(common::VIEWPORT, 1.0);
        assert!(ui.rect_of("root").is_some(), "never blank");
    }
    assert!(errors > 0);
    assert_eq!(ui.rect_of("ok"), before);
    // A source with an unknown font stack or style is rejected too.
    assert!(ui.reload(r#"panel { text text="a" font="nope" }"#, None).is_err());
    assert!(ui.reload(r"panel style=nope { }", None).is_err());
    assert!(Ui::new(common::fonts()?, "", None).is_err());
    Ok(())
}
