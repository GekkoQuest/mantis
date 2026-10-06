//! UI layouts and themes: `ui/**/*.layout` and `ui/**/*.theme`, in the `mantis_ui`
//! markup syntax (see `mantis_ui::markup`), phase 0.
//!
//! A layout is validated with `mantis_ui::markup::parse_layout` (theme blocks and exactly
//! one root element, unique ids, templates without formulas), a theme with
//! `parse_theme`. A markup error's line becomes the cook error's line; its column is
//! kept in the message. The cooked payload is the source text, unchanged, under the same
//! name (`ui/hud.layout`), kind `Ui`, presentation domain.

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_ui::markup::{MarkupError, parse_layout, parse_theme};

use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

/// The UI markup importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct UiMarkup;

fn located(path: &str, e: &MarkupError) -> CookError {
    let line = usize::try_from(e.line).unwrap_or(0);
    CookError::at(path, line, &format!("column {}: {}", e.column, e.message))
}

impl Importer for UiMarkup {
    fn name(&self) -> &'static str {
        "ui.markup"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        path.starts_with("ui/")
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e == "layout" || e == "theme")
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let text = source.text()?;
        if source.extension() == Some("layout") {
            parse_layout(text).map_err(|e| located(source.path, &e))?;
        } else {
            parse_theme(text).map_err(|e| located(source.path, &e))?;
        }
        Ok(vec![Cooked {
            name: source.path.to_owned(),
            kind: AssetKind::Ui,
            domain: Domain::Presentation,
            bytes: source.bytes.to_vec(),
        }])
    }
}
