//! Mixed-script shaping through font fallback: Latin, Hangul, a Han
//! ideograph, and a right-to-left Hebrew run on one line.

use mantis_ui::font::{FontId, FontLibrary};
use mantis_ui::test_font;
use mantis_ui::text::{ShapedText, Shaper, TextLayout, TextStyle};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn mixed_script_line_uses_fallback_and_visual_order() -> TestResult {
    let mut lib = FontLibrary::new();
    let latin = lib.add_font(test_font::latin())?;
    let cjk = lib.add_font(test_font::cjk())?;
    let rtl = lib.add_font(test_font::rtl())?;
    let stack = lib.define_stack("ui", &[latin, cjk, rtl])?;

    // "player " + Hangul + Han + " " + Hebrew word + " end"
    let text = "player 한글中 שלום end";
    let mut engine = Shaper::new();
    let mut shaped = ShapedText::default();
    engine.shape(&lib, text, TextStyle { stack, size: 16.0 }, &mut shaped);
    let mut layout = TextLayout::default();
    shaped.layout(None, &mut layout);
    assert_eq!(layout.lines.len(), 1);

    // Every glyph is real and comes from a font that covers its cluster.
    let expected_font = |c: char| -> FontId {
        if c.is_ascii() {
            latin
        } else if ('\u{05D0}'..='\u{05EA}').contains(&c) {
            rtl
        } else {
            cjk
        }
    };
    let visible: Vec<_> = layout.glyphs.iter().collect();
    let non_space = text.chars().filter(|c| !c.is_whitespace()).count();
    // Spaces map to empty but real glyphs, so the glyph count is the char count.
    assert_eq!(visible.len(), text.chars().count());
    assert!(non_space > 0);
    for g in &visible {
        assert_ne!(g.glyph, 0, "a .notdef glyph at cluster {}", g.cluster);
        let c = text
            .get(g.cluster as usize..)
            .and_then(|s| s.chars().next())
            .ok_or("cluster out of range")?;
        assert!(lib.covers(g.font, c), "font {:?} does not cover {c:?}", g.font);
        assert_eq!(g.font, expected_font(c), "wrong fallback for {c:?}");
    }

    // The Hebrew run: glyphs in visual order have decreasing cluster indices.
    let heb_start = text.find('ש').ok_or("hebrew start")?;
    let heb_end = text.find('ם').ok_or("hebrew end")? + 'ם'.len_utf8();
    let heb: Vec<_> = visible
        .iter()
        .filter(|g| (heb_start..heb_end).contains(&(g.cluster as usize)))
        .collect();
    assert_eq!(heb.len(), 4);
    for pair in heb.windows(2) {
        let [a, b] = pair else { continue };
        assert!(a.x < b.x, "visual order is left to right in the list");
        assert!(a.cluster > b.cluster, "RTL run: clusters decrease left to right");
        assert!(a.rtl && b.rtl);
    }

    // Runs are ordered correctly on the line: Latin, then Hangul and Han, then
    // the Hebrew word, then the trailing Latin word, all left to right because
    // the paragraph is left to right.
    let x_of = |byte: usize| {
        visible
            .iter()
            .find(|g| g.cluster as usize == byte)
            .map(|g| g.x)
            .ok_or("missing glyph")
    };
    let p = x_of(0)?;
    let hangul = x_of(text.find('한').ok_or("hangul")?)?;
    let han = x_of(text.find('中').ok_or("han")?)?;
    let shin = x_of(heb_start)?;
    let mem = x_of(text.find('ם').ok_or("mem")?)?;
    let end = x_of(text.find("end").ok_or("end")?)?;
    assert!(p < hangul && hangul < han, "Latin, then Hangul, then Han");
    assert!(
        han < mem && mem < shin,
        "Hebrew after Han; its last letter leftmost"
    );
    assert!(shin < end, "trailing Latin after the Hebrew run");

    // Glyph x positions never overlap and advance strictly.
    for pair in visible.windows(2) {
        let [a, b] = pair else { continue };
        assert!(a.x + a.advance <= b.x + 1e-3);
    }
    Ok(())
}

#[test]
fn rtl_paragraph_reverses_run_order() -> TestResult {
    let mut lib = FontLibrary::new();
    let latin = lib.add_font(test_font::latin())?;
    let rtl = lib.add_font(test_font::rtl())?;
    let stack = lib.define_stack("ui", &[latin, rtl])?;
    // A right-to-left paragraph with an embedded Latin word.
    let text = "שלום abc דג";
    let mut engine = Shaper::new();
    let mut shaped = ShapedText::default();
    engine.shape(&lib, text, TextStyle { stack, size: 16.0 }, &mut shaped);
    let mut layout = TextLayout::default();
    shaped.layout(None, &mut layout);
    let line = layout.lines.first().ok_or("line")?;
    assert!(line.rtl);
    let x_of = |byte: usize| {
        layout
            .glyphs
            .iter()
            .find(|g| g.cluster as usize == byte)
            .map(|g| g.x)
            .ok_or("missing glyph")
    };
    let first_word = x_of(0)?;
    let latin_a = x_of(text.find('a').ok_or("a")?)?;
    let latin_c = x_of(text.find('c').ok_or("c")?)?;
    let last_word = x_of(text.find('ד').ok_or("dalet")?)?;
    assert!(last_word < latin_a, "last logical word is leftmost");
    assert!(latin_a < latin_c, "embedded LTR word keeps its order");
    assert!(latin_c < first_word, "first logical word is rightmost");
    Ok(())
}
