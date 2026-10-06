//! Sounds: mixer graphs (`*.mixer.toml` to MMIX, phase 0) and sound banks
//! (`*.soundbank.toml` to MSBK, phase 10) built from WAV inputs.
//!
//! # Mixer graphs
//!
//! One `[bus.<name>]` per bus (1 to 256, kept in file order); exactly one has no
//! `parent` (the master). Effects run in order: `[effect.<bus>.<n>]`, `n` from 0, at
//! most 4 per bus.
//!
//! ```toml
//! [bus.master]
//! id = 0                  # stable id sounds name; unique, not 4294967295
//! gain = 1.0              # linear, 0 to 4 (default 1)
//!
//! [bus.sfx]
//! id = 1
//! parent = "master"       # a bus name in this file
//! gain = 0.8
//!
//! [effect.master.0]
//! kind = "limiter"        # gain (db) | low_pass, high_pass (cutoff_hz) | limiter (threshold, release_ms)
//! threshold = 0.95        # linear, (0, 1]
//! release_ms = 80.0       # (0, 10000]
//!
//! [effect.sfx.0]
//! kind = "low_pass"
//! cutoff_hz = 8000.0      # (0, 96000]
//! ```
//!
//! Output: `<stem>.mix`, kind `MixerGraph`, presentation domain.
//!
//! # Sound banks
//!
//! A bank names its mixer graph by source path (cooked in phase 0) and its sample rate;
//! each `[sound.<name>]` names its clips by WAV path. WAV files are **inputs**: they are
//! read while cooking a bank, never cooked alone. Every clip must be 16-bit PCM or 32-bit
//! float, mono or stereo, at the bank's sample rate (nothing is resampled). A WAV named by
//! several sounds is stored once; clips are stored in order of first use.
//!
//! ```toml
//! sample_rate = 48000                 # 8000 to 192000
//! mixer = "audio/main.mixer.toml"
//!
//! [sound.step]
//! id = 1                              # stable id the game posts; unique in the bank
//! bus = 1                             # a bus id of the mixer graph
//! clips = ["audio/step_1.wav", "audio/step_2.wav"]   # 1 to 16
//! selection = "random"                # round_robin | random (default round_robin)
//! volume = 0.9                        # 0 to 4 (default 1)
//! pitch_min = 0.95                    # 0.25 to 4 (default 1)
//! pitch_max = 1.05                    # pitch_min to 4 (default pitch_min)
//! looping = false                     # default false
//! spatial = true                      # default false
//! priority = 10                       # 0 to 255, higher wins (default 0)
//! max_instances = 4                   # >= 1 (default 1)
//! steal = "oldest"                    # oldest | quietest | refuse (default oldest)
//! attenuation = "inverse"             # linear | inverse | exponential (default inverse)
//! min_distance = 1.0                  # > 0 (default 1)
//! max_distance = 40.0                 # > min_distance (default 50)
//! rolloff = 1.0                       # > 0 (default 1)
//! ```
//!
//! Output: `<stem>.sbk`, kind `SoundBank`, presentation domain.

use std::collections::BTreeMap;

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mixer_graph::{
    Bus, Effect, MAX_BUSES, MAX_CUTOFF_HZ, MAX_EFFECTS, MAX_GAIN, MAX_GAIN_DB, MAX_RELEASE_MS, MIN_GAIN_DB,
    MixerGraph, NO_PARENT,
};
use mantis_formats::sound_bank::{
    Attenuation, AttenuationModel, Clip, ClipSelection, MAX_CLIPS, MAX_CLIPS_PER_SOUND, MAX_PITCH,
    MAX_SAMPLE_RATE, MAX_SOUNDS, MAX_VOLUME, MIN_PITCH, MIN_SAMPLE_RATE, Sound, SoundBank, StealPolicy,
};

use super::wav;
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::source::{Doc, Fields, output_name, within};

const MIXER_SUFFIX: &str = ".mixer.toml";
const BANK_SUFFIX: &str = ".soundbank.toml";

/// The mixer graph importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mixers;

/// The sound bank importer (WAV files are its inputs).
#[derive(Clone, Copy, Debug, Default)]
pub struct SoundBanks;

fn effect(f: &Fields<'_>) -> Result<Effect, CookError> {
    let kinds = [("gain", 0u8), ("low_pass", 1), ("high_pass", 2), ("limiter", 3)];
    let (kind, _) = f.choice("kind", &kinds, None)?;
    let range = |key: &str, lo: f32, hi: f32, open_low: bool| -> Result<f32, CookError> {
        let (v, line) = f.f32(key)?;
        let ok = if open_low {
            v > lo && v <= hi
        } else {
            within(v, lo, hi)
        };
        if ok {
            Ok(v)
        } else {
            let low = if open_low { "(" } else { "[" };
            Err(f.err(line, &format!("`{key}` = {v} is outside {low}{lo}, {hi}]")))
        }
    };
    Ok(match kind {
        0 => {
            f.only(&["kind", "db"])?;
            Effect::Gain {
                db: range("db", MIN_GAIN_DB, MAX_GAIN_DB, false)?,
            }
        }
        1 | 2 => {
            f.only(&["kind", "cutoff_hz"])?;
            let cutoff_hz = range("cutoff_hz", 0.0, MAX_CUTOFF_HZ, true)?;
            if kind == 1 {
                Effect::LowPass { cutoff_hz }
            } else {
                Effect::HighPass { cutoff_hz }
            }
        }
        _ => {
            f.only(&["kind", "threshold", "release_ms"])?;
            Effect::Limiter {
                threshold: range("threshold", 0.0, 1.0, true)?,
                release_ms: range("release_ms", 0.0, MAX_RELEASE_MS, true)?,
            }
        }
    })
}

/// Reads `[effect.<bus>.<n>]` tables into each bus's chain.
fn effects(doc: &Doc<'_>, buses: &BTreeMap<&str, usize>) -> Result<Vec<Vec<Effect>>, CookError> {
    let mut chains: Vec<BTreeMap<u32, (usize, Effect)>> = vec![BTreeMap::new(); buses.len()];
    for (rest, f) in doc.items("effect") {
        let (bus, n) = rest.rsplit_once('.').ok_or_else(|| {
            f.err(
                f.line(),
                &format!("`[effect.{rest}]`: expected `[effect.<bus>.<n>]`"),
            )
        })?;
        let n = n.parse::<u32>().map_err(|_| {
            f.err(
                f.line(),
                &format!("`[effect.{rest}]`: `{n}` is not an effect number"),
            )
        })?;
        let chain = buses
            .get(bus)
            .and_then(|i| chains.get_mut(*i))
            .ok_or_else(|| f.err(f.line(), &format!("`[effect.{rest}]` names unknown bus `{bus}`")))?;
        chain.insert(n, (f.line(), effect(&f)?));
    }
    let mut out = Vec::with_capacity(chains.len());
    for chain in chains {
        let mut list = Vec::with_capacity(chain.len());
        for (expected, (n, (line, e))) in (0u32..).zip(&chain) {
            if *n != expected {
                return Err(doc.err(
                    *line,
                    &format!(
                        "effect numbers must run 0, 1, ... without gaps (expected {expected}, found {n})"
                    ),
                ));
            }
            if list.len() == MAX_EFFECTS {
                return Err(doc.err(*line, &format!("a bus has at most {MAX_EFFECTS} effects")));
            }
            list.push(*e);
        }
        out.push(list);
    }
    Ok(out)
}

/// Checks every bus reaches the master without a cycle; reports at the bus header.
fn check_tree(
    doc: &Doc<'_>,
    specs: &[(&str, Fields<'_>, u32, Option<&str>)],
    index: &BTreeMap<&str, usize>,
) -> Result<(), CookError> {
    for (name, f, _, _) in specs {
        let mut at = *name;
        let mut steps = 0;
        while let Some(parent) = index
            .get(at)
            .and_then(|i| specs.get(*i))
            .and_then(|(_, _, _, p)| *p)
        {
            at = parent;
            steps += 1;
            if steps > specs.len() {
                return Err(doc.err(f.line(), &format!("bus `{name}` is in a parent cycle")));
            }
        }
    }
    Ok(())
}

impl Mixers {
    fn graph(doc: &Doc<'_>) -> Result<MixerGraph, CookError> {
        // (name, fields, id, parent name)
        let mut specs = Vec::new();
        let mut ids: BTreeMap<u32, &str> = BTreeMap::new();
        for (name, f) in doc.items("bus") {
            f.only(&["id", "parent", "gain"])?;
            let (id, line) = f.int::<u32>("id")?;
            if id == NO_PARENT {
                return Err(f.err(line, &format!("bus id {NO_PARENT} is reserved")));
            }
            if let Some(other) = ids.insert(id, name) {
                return Err(f.err(line, &format!("bus id {id} is already used by bus `{other}`")));
            }
            specs.push((name, f, id, f.opt_str("parent")?.map(|(p, _)| p)));
        }
        if specs.is_empty() || specs.len() > MAX_BUSES as usize {
            return Err(doc.err(
                0,
                &format!("{} buses; a mixer graph has 1 to {MAX_BUSES}", specs.len()),
            ));
        }
        let index: BTreeMap<&str, usize> = specs.iter().enumerate().map(|(i, s)| (s.0, i)).collect();
        let mut master: Option<&str> = None;
        for (name, f, _, parent) in &specs {
            match parent {
                None => {
                    if let Some(m) = master {
                        return Err(f.err(
                            f.line(),
                            &format!("bus `{name}` has no parent, but `{m}` is already the master"),
                        ));
                    }
                    master = Some(name);
                }
                Some(p) if !index.contains_key(p) => {
                    return Err(f.err(
                        f.line_of("parent"),
                        &format!("parent `{p}` is not a bus of this graph"),
                    ));
                }
                Some(_) => {}
            }
        }
        if master.is_none() {
            return Err(doc.err(0, "no master bus: exactly one bus has no `parent`"));
        }
        check_tree(doc, &specs, &index)?;
        let chains = effects(doc, &index)?;
        let mut buses = Vec::with_capacity(specs.len());
        for ((_, f, id, parent), effects) in specs.iter().zip(chains) {
            let (gain, line) = f.f32_or("gain", 1.0)?;
            if !within(gain, 0.0, MAX_GAIN) {
                return Err(f.err(line, &format!("`gain` = {gain} is outside 0 to {MAX_GAIN}")));
            }
            let parent = parent
                .and_then(|p| index.get(p))
                .and_then(|i| specs.get(*i))
                .map(|s| s.2);
            buses.push(Bus {
                id: *id,
                parent,
                gain,
                effects,
            });
        }
        Ok(MixerGraph { buses })
    }
}

impl Importer for Mixers {
    fn name(&self) -> &'static str {
        "mixer.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(MIXER_SUFFIX)
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::from_source(source)?;
        doc.only_tables(&[], &["bus", "effect"])?;
        doc.root().only(&[])?;
        let graph = Mixers::graph(&doc)?;
        let bytes = graph.encode();
        MixerGraph::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("cooked mixer graph fails its runtime parser: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, MIXER_SUFFIX, ".mix"),
            kind: AssetKind::MixerGraph,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

const SOUND_KEYS: &[&str] = &[
    "id",
    "bus",
    "clips",
    "selection",
    "volume",
    "pitch_min",
    "pitch_max",
    "looping",
    "spatial",
    "priority",
    "max_instances",
    "steal",
    "attenuation",
    "min_distance",
    "max_distance",
    "rolloff",
];

/// The clip table being built: WAV path to clip index, in order of first use.
struct Clips<'d> {
    index: BTreeMap<&'d str, u32>,
    table: Vec<Clip>,
    sample_rate: u32,
}

impl<'d> Clips<'d> {
    fn add(
        &mut self,
        path: &'d str,
        f: &Fields<'_>,
        line: usize,
        ctx: &ImportContext<'_>,
    ) -> Result<u32, CookError> {
        if let Some(i) = self.index.get(path) {
            return Ok(*i);
        }
        let src = ctx
            .source(path)
            .ok_or_else(|| f.err(line, &format!("clip `{path}` is not a file of the content tree")))?;
        let w = wav::decode(src.bytes).map_err(|e| f.err(line, &format!("clip `{path}`: {e}")))?;
        if w.sample_rate != self.sample_rate {
            return Err(f.err(
                line,
                &format!(
                    "clip `{path}` is {} Hz; the bank is {} Hz (clips are not resampled)",
                    w.sample_rate, self.sample_rate
                ),
            ));
        }
        let i = u32::try_from(self.table.len())
            .ok()
            .filter(|i| *i < MAX_CLIPS)
            .ok_or_else(|| f.err(line, &format!("a bank has at most {MAX_CLIPS} clips")))?;
        self.table.push(Clip {
            channels: w.channels,
            samples: w.samples,
        });
        self.index.insert(path, i);
        Ok(i)
    }
}

fn attenuation(f: &Fields<'_>) -> Result<Attenuation, CookError> {
    let models = [
        ("linear", AttenuationModel::Linear),
        ("inverse", AttenuationModel::Inverse),
        ("exponential", AttenuationModel::Exponential),
    ];
    let (model, _) = f.choice("attenuation", &models, Some(AttenuationModel::Inverse))?;
    let (min_distance, nl) = f.f32_or("min_distance", 1.0)?;
    if min_distance <= 0.0 {
        return Err(f.err(nl, &format!("`min_distance` = {min_distance} must be > 0")));
    }
    let (max_distance, xl) = f.f32_or("max_distance", 50.0)?;
    if max_distance <= min_distance {
        return Err(f.err(
            xl,
            &format!("`max_distance` = {max_distance} must be greater than `min_distance` = {min_distance}"),
        ));
    }
    let (rolloff, rl) = f.f32_or("rolloff", 1.0)?;
    if rolloff <= 0.0 {
        return Err(f.err(rl, &format!("`rolloff` = {rolloff} must be > 0")));
    }
    Ok(Attenuation {
        model,
        min_distance,
        max_distance,
        rolloff,
    })
}

fn sound<'d>(
    f: &Fields<'d>,
    clips: &mut Clips<'d>,
    mixer: &MixerGraph,
    ctx: &ImportContext<'_>,
) -> Result<Sound, CookError> {
    f.only(SOUND_KEYS)?;
    let (id, _) = f.int::<u32>("id")?;
    let (bus, bus_line) = f.int::<u32>("bus")?;
    if mixer.bus(bus).is_none() {
        let mut known: Vec<String> = mixer.buses.iter().map(|b| b.id.to_string()).collect();
        known.sort();
        return Err(f.err(
            bus_line,
            &format!(
                "bus {bus} is not a bus of the mixer graph (buses: {})",
                known.join(", ")
            ),
        ));
    }
    let (paths, clips_line) = f.strs("clips")?;
    if paths.is_empty() || paths.len() > MAX_CLIPS_PER_SOUND {
        return Err(f.err(
            clips_line,
            &format!("{} clips; a sound has 1 to {MAX_CLIPS_PER_SOUND}", paths.len()),
        ));
    }
    let mut indices = Vec::with_capacity(paths.len());
    for p in paths {
        indices.push(clips.add(p, f, clips_line, ctx)?);
    }
    let (selection, _) = f.choice(
        "selection",
        &[
            ("round_robin", ClipSelection::RoundRobin),
            ("random", ClipSelection::Random),
        ],
        Some(ClipSelection::RoundRobin),
    )?;
    let (volume, vl) = f.f32_or("volume", 1.0)?;
    if !within(volume, 0.0, MAX_VOLUME) {
        return Err(f.err(vl, &format!("`volume` = {volume} is outside 0 to {MAX_VOLUME}")));
    }
    let (pitch_min, pl) = f.f32_or("pitch_min", 1.0)?;
    if !within(pitch_min, MIN_PITCH, MAX_PITCH) {
        return Err(f.err(
            pl,
            &format!("`pitch_min` = {pitch_min} is outside {MIN_PITCH} to {MAX_PITCH}"),
        ));
    }
    let (pitch_max, ml) = f.f32_or("pitch_max", pitch_min)?;
    if !within(pitch_max, pitch_min, MAX_PITCH) {
        return Err(f.err(
            ml,
            &format!("`pitch_max` = {pitch_max} is outside `pitch_min` ({pitch_min}) to {MAX_PITCH}"),
        ));
    }
    let (max_instances, il) = f.int_or::<u16>("max_instances", 1)?;
    if max_instances == 0 {
        return Err(f.err(il, "`max_instances` must be at least 1"));
    }
    let steals = [
        ("oldest", StealPolicy::Oldest),
        ("quietest", StealPolicy::Quietest),
        ("refuse", StealPolicy::Refuse),
    ];
    Ok(Sound {
        id,
        clips: indices,
        selection,
        volume,
        pitch_min,
        pitch_max,
        looping: f.bool_or("looping", false)?.0,
        spatial: f.bool_or("spatial", false)?.0,
        bus,
        priority: f.int_or::<u8>("priority", 0)?.0,
        max_instances,
        steal: f.choice("steal", &steals, Some(StealPolicy::Oldest))?.0,
        attenuation: attenuation(f)?,
    })
}

impl Importer for SoundBanks {
    fn name(&self) -> &'static str {
        "soundbank.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        10
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(BANK_SUFFIX)
    }

    fn inputs(&self, path: &str) -> bool {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::from_source(source)?;
        doc.only_tables(&[], &["sound"])?;
        let root = doc.root();
        root.only(&["sample_rate", "mixer"])?;
        let (sample_rate, rate_line) = root.int::<u32>("sample_rate")?;
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(root.err(
                rate_line,
                &format!("`sample_rate` = {sample_rate} is outside {MIN_SAMPLE_RATE} to {MAX_SAMPLE_RATE}"),
            ));
        }
        let (mixer_path, mixer_line) = root.str("mixer")?;
        let (_, mixer_bytes) =
            ctx.resolve_bytes(mixer_path, AssetKind::MixerGraph, source.path, mixer_line)?;
        let mixer = MixerGraph::parse(mixer_bytes).map_err(|e| {
            root.err(
                mixer_line,
                &format!("`{mixer_path}` is not a valid mixer graph: {e}"),
            )
        })?;
        let mut clips = Clips {
            index: BTreeMap::new(),
            table: Vec::new(),
            sample_rate,
        };
        let mut sounds = Vec::new();
        let mut ids: BTreeMap<u32, &str> = BTreeMap::new();
        for (name, f) in doc.items("sound") {
            let s = sound(&f, &mut clips, &mixer, ctx)?;
            if let Some(other) = ids.insert(s.id, name) {
                return Err(f.err(
                    f.line_of("id"),
                    &format!("sound id {} is already used by sound `{other}`", s.id),
                ));
            }
            if sounds.len() == MAX_SOUNDS as usize {
                return Err(f.err(f.line(), &format!("a bank has at most {MAX_SOUNDS} sounds")));
            }
            sounds.push(s);
        }
        let bank = SoundBank {
            sample_rate,
            clips: clips.table,
            sounds,
        };
        let bytes = bank.encode();
        SoundBank::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("cooked sound bank fails its runtime parser: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, BANK_SUFFIX, ".sbk"),
            kind: AssetKind::SoundBank,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}
