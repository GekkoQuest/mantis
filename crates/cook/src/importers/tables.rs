//! Text tables: `tables/<module key>.<table>` files, copied verbatim into the gameplay
//! bundle after a structural check (UTF-8, `#` comments, whitespace-separated rows with
//! the same column count). Each module validates its own table semantics when it loads
//! the table; schema-driven validation joins here when table schemas are declared.

use mantis_formats::bundle::{AssetKind, Domain};

use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

/// The table importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tables;

impl Importer for Tables {
    fn name(&self) -> &'static str {
        "table.text"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        path.starts_with("tables/")
            && path != super::content::gameplay::ABILITIES
            && !crate::pipeline::is_note(path)
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let text = source.text()?;
        let mut columns = None;
        for (n, line) in text.lines().enumerate() {
            let row = line.split('#').next().unwrap_or("").trim();
            if row.is_empty() {
                continue;
            }
            let count = row.split_whitespace().count();
            match columns {
                None => columns = Some(count),
                Some(c) if c != count => {
                    return Err(CookError::at(
                        source.path,
                        n + 1,
                        &format!("{count} columns; earlier rows have {c}"),
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(vec![Cooked {
            name: source.path.to_owned(),
            kind: AssetKind::Table,
            domain: Domain::Gameplay,
            bytes: source.bytes.to_vec(),
        }])
    }
}
