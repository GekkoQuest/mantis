//! Built-in importers: content. Presentation-domain assets authored as text, each
//! validated by the runtime parser of its format before it is returned.
//!
//! | importer | sources | output | kind | phase |
//! |---|---|---|---|---|
//! | [`grading::Gradings`] `grading.toml` | `*.grading.toml` | `<stem>.grd` (MGRD) | `ColorGrading` | 0 |
//! | [`ui::UiMarkup`] `ui.markup` | `ui/**/*.layout`, `ui/**/*.theme` | the same path (text) | `Ui` | 0 |
//! | [`sound::Mixers`] `mixer.toml` | `*.mixer.toml` | `<stem>.mix` (MMIX) | `MixerGraph` | 0 |
//! | [`particles::Particles`] `particles.toml` | `*.particles.toml` | `<stem>.pfx` (MPFX) | `ParticleEffect` | 5 |
//! | [`gameplay::Graphs`] `graph.toml` | `*.graph.toml` | `<stem>.graph` (MGPH, gameplay domain; decision 0021) | `GameplayGraph` | 5 |
//! | [`gameplay::Abilities`] `table.abilities` | `tables/abilities` (rows naming cooked graphs) | the same path (text, gameplay domain) | `Table` | 6 |
//! | [`material::Materials`] `material.toml` | `*.material.toml` | `<stem>.mat` (MMAT version 2: graph, texture hashes and flags, named parameter defaults) | `Material` | 10 |
//! | [`presentation::Presentations`] `presentation.toml` | `*.presentation.toml` | `<stem>.prs` (MPRS) | `Presentation` | 10 |
//! | [`sound::SoundBanks`] `soundbank.toml` | `*.soundbank.toml`, reading `*.wav` inputs | `<stem>.sbk` (MSBK) | `SoundBank` | 10 |
//!
//! Particle effects and gameplay graphs run at phase 5 so phase-10 presentation graphs
//! can name effects by source path and bind only to marker nodes of cooked graphs. Each submodule documents its source syntax with an example. Every
//! structured source uses the strict TOML subset of `mantis_core::module::toml`, one
//! named table per item, and unknown tables and keys are errors at their line.
//!
//! Every output is in the presentation domain; versions start at 1 (the material
//! importer is at 2, for MMAT version 2).

use std::sync::Arc;

use crate::importer::Importer;

mod fields;
pub mod gameplay;
pub mod grading;
pub mod material;
pub mod particles;
pub mod presentation;
pub mod sound;
pub mod ui;
pub mod wav;

/// This module's importers.
pub fn importers() -> Vec<Arc<dyn Importer>> {
    vec![
        Arc::new(grading::Gradings),
        Arc::new(ui::UiMarkup),
        Arc::new(sound::Mixers),
        Arc::new(particles::Particles),
        Arc::new(gameplay::Graphs::default()),
        Arc::new(gameplay::Abilities),
        Arc::new(material::Materials),
        Arc::new(presentation::Presentations),
        Arc::new(sound::SoundBanks),
    ]
}
