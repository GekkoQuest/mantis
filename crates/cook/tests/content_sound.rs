//! Mixer graph and sound bank importers: WAV inputs (written by hand here), banks that
//! parse with the runtime parser, buses checked against the named mixer graph, every
//! rule failing at its file and line, and determinism.

#![expect(clippy::cast_possible_truncation)]

use std::fmt::Write as _;

use mantis_cook::importer::CookError;
use mantis_cook::importers::content::wav;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mixer_graph::{Effect, MixerGraph};
use mantis_formats::sound_bank::{AttenuationModel, ClipSamples, ClipSelection, SoundBank, StealPolicy};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PCM: u16 = 1;
const FLOAT: u16 = 3;

/// A RIFF WAVE file: a `fmt ` chunk (plain or extensible), an odd-sized `LIST` chunk
/// (padded, and skipped by readers), then `data`.
fn riff(tag: u16, extensible: bool, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
    let block = channels * bits / 8;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&(if extensible { 0xFFFE } else { tag }).to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    fmt.extend_from_slice(&block.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt.extend_from_slice(&3u32.to_le_bytes());
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
    }
    let mut body = b"WAVE".to_vec();
    for (id, chunk) in [
        (b"fmt ", fmt.as_slice()),
        (b"LIST", b"abc".as_slice()),
        (b"data", data),
    ] {
        body.extend_from_slice(id);
        body.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        body.extend_from_slice(chunk);
        if chunk.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn pcm16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    riff(PCM, false, channels, rate, 16, &data)
}

fn float32(channels: u16, rate: u32, samples: &[f32]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    riff(FLOAT, false, channels, rate, 32, &data)
}

const MIXER_PATH: &str = "audio/main.mixer.toml";
const BANK_PATH: &str = "audio/sfx.soundbank.toml";

const MIXER: &str = r#"# the bus tree
[bus.master]
id = 0

[bus.sfx]
id = 1
parent = "master"
gain = 0.8

[bus.music]
id = 7
parent = "master"
gain = 0.5

[effect.master.0]
kind = "limiter"
threshold = 0.95
release_ms = 80

[effect.sfx.0]
kind = "low_pass"
cutoff_hz = 8000

[effect.sfx.1]
kind = "gain"
db = -6

[effect.music.0]
kind = "high_pass"
cutoff_hz = 40
"#;

const BANK: &str = r#"# effects and music
sample_rate = 48000
mixer = "audio/main.mixer.toml"

[sound.step]
id = 1
bus = 1
clips = ["audio/step_1.wav", "audio/step_2.wav"]
selection = "random"
volume = 0.9
pitch_min = 0.95
pitch_max = 1.05
spatial = true
priority = 10
max_instances = 4
steal = "quietest"
attenuation = "linear"
min_distance = 2
max_distance = 40
rolloff = 0.5

[sound.theme]
id = 2
bus = 7
clips = ["audio/theme.wav"]
looping = true
steal = "refuse"

[sound.scuff]
id = 3
bus = 1
clips = ["audio/step_2.wav"]
"#;

fn tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert(MIXER_PATH, MIXER);
    t.insert(BANK_PATH, BANK);
    t.insert("audio/step_1.wav", pcm16(1, 48_000, &[0, 1000, -1000, 32767]));
    t.insert("audio/step_2.wav", riff(PCM, true, 1, 48_000, 16, &[1, 0, 2, 0]));
    t.insert(
        "audio/theme.wav",
        float32(2, 48_000, &[0.5, -0.5, 1.0, -1.0, 0.0, 0.25]),
    );
    t
}

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    Cook::new(importers::builtin()).map_err(|e| vec![e])?.run(tree)
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn single_error(t: &ContentTree) -> Result<CookError, Box<dyn std::error::Error>> {
    let errors = cook(t).err().ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    Ok(e)
}

fn assert_at(t: &ContentTree, path: &str, text: &str, needle: &str, contains: &str) -> TestResult {
    let e = single_error(t)?;
    assert_eq!(e.file, path, "{e}");
    assert_eq!(e.line, line_of(text, needle), "{e} (expected at `{needle}`)");
    assert!(e.message.contains(contains), "{e} (expected `{contains}`)");
    Ok(())
}

fn bank_at(bank: &str, needle: &str, contains: &str) -> TestResult {
    let mut t = tree();
    t.insert(BANK_PATH, bank);
    assert_at(&t, BANK_PATH, bank, needle, contains)
}

fn mixer_at(mixer: &str, needle: &str, contains: &str) -> TestResult {
    let mut t = ContentTree::new();
    t.insert(MIXER_PATH, mixer);
    assert_at(&t, MIXER_PATH, mixer, needle, contains)
}

fn wav_at(wav_bytes: Vec<u8>, contains: &str) -> TestResult {
    let mut t = tree();
    t.insert("audio/step_1.wav", wav_bytes);
    assert_at(&t, BANK_PATH, BANK, "clips = [\"audio/step_1.wav\"", contains)
}

#[test]
fn a_mixer_graph_cooks_with_its_tree_and_effects() -> TestResult {
    let out = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    let mix = out.get("audio/main.mix").ok_or("mixer output")?;
    assert_eq!(mix.kind, AssetKind::MixerGraph);
    assert_eq!(mix.domain, Domain::Presentation);
    let g = MixerGraph::parse(&mix.bytes)?;
    let shape: Vec<(u32, Option<u32>, f32)> = g.buses.iter().map(|b| (b.id, b.parent, b.gain)).collect();
    assert_eq!(shape, [(0, None, 1.0), (1, Some(0), 0.8), (7, Some(0), 0.5)]);
    assert_eq!(
        g.bus(1).map(|b| b.effects.clone()),
        Some(vec![
            Effect::LowPass { cutoff_hz: 8000.0 },
            Effect::Gain { db: -6.0 }
        ])
    );
    assert_eq!(
        g.bus(0).map(|b| b.effects.clone()),
        Some(vec![Effect::Limiter {
            threshold: 0.95,
            release_ms: 80.0
        }])
    );
    assert_eq!(
        g.bus(7).map(|b| b.effects.clone()),
        Some(vec![Effect::HighPass { cutoff_hz: 40.0 }])
    );
    Ok(())
}

#[test]
fn a_sound_bank_cooks_from_wav_inputs() -> TestResult {
    let out = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    // WAV files are inputs: never cooked alone, never reported as unhandled.
    let wav_source = |a: &mantis_cook::pipeline::CookedAsset| {
        std::path::Path::new(&a.source)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    };
    assert!(!out.assets.values().any(wav_source));
    let asset = out.get("audio/sfx.sbk").ok_or("bank output")?;
    assert_eq!(asset.kind, AssetKind::SoundBank);
    assert_eq!(asset.domain, Domain::Presentation);
    let bank = SoundBank::parse(&asset.bytes)?;
    assert_eq!(bank.sample_rate, 48_000);
    // Clips in order of first use, each stored once.
    let clips: Vec<(u8, ClipSamples)> = bank
        .clips
        .iter()
        .map(|c| (c.channels, c.samples.clone()))
        .collect();
    assert_eq!(
        clips,
        [
            (1, ClipSamples::I16(vec![0, 1000, -1000, 32767])),
            (1, ClipSamples::I16(vec![1, 2])),
            (2, ClipSamples::F32(vec![0.5, -0.5, 1.0, -1.0, 0.0, 0.25])),
        ]
    );
    let step = bank.sound(1).ok_or("step")?;
    assert_eq!(step.clips, [0, 1]);
    assert_eq!(step.selection, ClipSelection::Random);
    assert_eq!((step.volume, step.pitch_min, step.pitch_max), (0.9, 0.95, 1.05));
    assert!(step.spatial && !step.looping);
    assert_eq!((step.bus, step.priority, step.max_instances), (1, 10, 4));
    assert_eq!(step.steal, StealPolicy::Quietest);
    assert_eq!(step.attenuation.model, AttenuationModel::Linear);
    assert_eq!(
        (
            step.attenuation.min_distance,
            step.attenuation.max_distance,
            step.attenuation.rolloff
        ),
        (2.0, 40.0, 0.5)
    );
    let theme = bank.sound(2).ok_or("theme")?;
    assert_eq!(theme.clips, [2]);
    assert!(theme.looping && !theme.spatial);
    assert_eq!(theme.steal, StealPolicy::Refuse);
    // Defaults.
    let scuff = bank.sound(3).ok_or("scuff")?;
    assert_eq!(scuff.clips, [1]);
    assert_eq!(scuff.selection, ClipSelection::RoundRobin);
    assert_eq!((scuff.volume, scuff.pitch_min, scuff.pitch_max), (1.0, 1.0, 1.0));
    assert_eq!((scuff.priority, scuff.max_instances), (0, 1));
    assert_eq!(scuff.steal, StealPolicy::Oldest);
    assert_eq!(scuff.attenuation.model, AttenuationModel::Inverse);
    assert_eq!(
        (
            scuff.attenuation.min_distance,
            scuff.attenuation.max_distance,
            scuff.attenuation.rolloff
        ),
        (1.0, 50.0, 1.0)
    );
    Ok(())
}

#[test]
fn wav_inputs_are_checked() -> TestResult {
    wav_at(riff(PCM, false, 1, 48_000, 8, &[1, 2]), "8 bits")?;
    wav_at(
        riff(PCM, false, 1, 48_000, 24, &[1, 2, 3]),
        "only 16-bit PCM and 32-bit IEEE float",
    )?;
    wav_at(riff(FLOAT, false, 1, 48_000, 64, &[0; 8]), "IEEE float")?;
    wav_at(riff(2, false, 1, 48_000, 16, &[0; 2]), "format tag 2")?;
    wav_at(riff(PCM, true, 1, 48_000, 24, &[0; 3]), "24 bits")?;
    wav_at(pcm16(3, 48_000, &[0, 0, 0]), "3 channels")?;
    wav_at(pcm16(1, 44_100, &[0, 0]), "44100 Hz")?;
    wav_at(float32(1, 48_000, &[0.5, 1.5]), "within -1 to 1")?;
    wav_at(float32(1, 48_000, &[f32::NAN]), "finite")?;
    wav_at(pcm16(1, 48_000, &[]), "positive whole number")?;
    wav_at(riff(PCM, false, 2, 48_000, 16, &[0, 0, 0, 0, 0, 0]), "6 bytes")?;
    wav_at(b"RIFX\0\0\0\0WAVE".to_vec(), "not a RIFF WAVE")?;
    let mut truncated = pcm16(1, 48_000, &[1, 2, 3]);
    truncated.truncate(truncated.len() - 2);
    wav_at(truncated, "past the end")?;
    // The decoder on its own.
    let w = wav::decode(&float32(2, 22_050, &[0.25, -0.25]))?;
    assert_eq!((w.sample_rate, w.channels), (22_050, 2));
    assert!(wav::decode(&riff(PCM, false, 1, 8000, 16, &[])).is_err());
    Ok(())
}

#[test]
fn bank_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| BANK.replacen(from, to, 1);
    // Buses are checked against the named mixer graph.
    bank_at(
        &r("bus = 7", "bus = 9"),
        "bus = 9",
        "bus 9 is not a bus of the mixer graph",
    )?;
    bank_at(
        &r("id = 3", "id = 1 # again"),
        "id = 1 # again",
        "already used by sound `step`",
    )?;
    bank_at(&r("volume = 0.9", "volume = 5"), "volume = 5", "outside 0 to 4")?;
    bank_at(
        &r("pitch_max = 1.05", "pitch_max = 0.9"),
        "pitch_max",
        "outside `pitch_min`",
    )?;
    bank_at(
        &r("pitch_min = 0.95", "pitch_min = 0.1"),
        "pitch_min",
        "outside 0.25 to 4",
    )?;
    bank_at(
        &r("max_distance = 40", "max_distance = 2"),
        "max_distance",
        "greater than `min_distance`",
    )?;
    bank_at(
        &r("min_distance = 2", "min_distance = 0"),
        "min_distance",
        "must be > 0",
    )?;
    bank_at(&r("rolloff = 0.5", "rolloff = 0"), "rolloff", "must be > 0")?;
    bank_at(
        &r("max_instances = 4", "max_instances = 0"),
        "max_instances",
        "at least 1",
    )?;
    bank_at(
        &r("priority = 10", "priority = 300"),
        "priority",
        "does not fit in u8",
    )?;
    bank_at(
        &r("steal = \"quietest\"", "steal = \"newest\""),
        "steal",
        "is not one of",
    )?;
    bank_at(
        &r("selection = \"random\"", "selection = \"shuffle\""),
        "selection",
        "is not one of",
    )?;
    bank_at(
        &r("attenuation = \"linear\"", "attenuation = \"cubic\""),
        "attenuation",
        "is not one of",
    )?;
    bank_at(
        &r("clips = [\"audio/theme.wav\"]", "clips = []"),
        "clips = []",
        "a sound has 1 to 16",
    )?;
    let seventeen = format!("clips = [{}]", vec!["\"audio/theme.wav\""; 17].join(", "));
    bank_at(
        &r("clips = [\"audio/theme.wav\"]", &seventeen),
        "clips = [\"audio/theme.wav\", ",
        "17 clips",
    )?;
    bank_at(
        &r("audio/theme.wav", "audio/missing.wav"),
        "audio/missing.wav",
        "not a file of the content tree",
    )?;
    bank_at(
        &r("sample_rate = 48000", "sample_rate = 4000"),
        "sample_rate",
        "outside 8000 to 192000",
    )?;
    bank_at(
        &r("looping = true", "looping = true\nstreamed = true"),
        "streamed",
        "unknown key `streamed`",
    )?;
    bank_at(
        &r("[sound.scuff]\nid = 3\n", "[sound.scuff]\n"),
        "[sound.scuff]",
        "missing `id`",
    )?;
    // The mixer reference resolves by source path.
    bank_at(
        &r("audio/main.mixer.toml", "audio/other.mixer.toml"),
        "mixer =",
        "audio/other.mixer.toml",
    )?;
    // A missing root key is about the whole file (line 0).
    let mut t = tree();
    t.insert(BANK_PATH, r("mixer = \"audio/main.mixer.toml\"\n", ""));
    let e = single_error(&t)?;
    assert_eq!((e.file.as_str(), e.line), (BANK_PATH, 0), "{e}");
    assert!(e.message.contains("missing `mixer`"), "{e}");
    Ok(())
}

#[test]
fn mixer_rules_fail_at_their_line() -> TestResult {
    let r = |from: &str, to: &str| MIXER.replacen(from, to, 1);
    mixer_at(
        &r("parent = \"master\"\ngain = 0.8", "gain = 0.8"),
        "[bus.sfx]",
        "already the master",
    )?;
    // With every bus parented there is no master at all (a whole-file error).
    let mut t = ContentTree::new();
    t.insert(
        MIXER_PATH,
        r("[bus.master]\nid = 0", "[bus.master]\nid = 0\nparent = \"music\""),
    );
    let e = single_error(&t)?;
    assert_eq!(e.line, 0, "{e}");
    assert!(e.message.contains("no master"), "{e}");
    mixer_at(
        &r("parent = \"master\"", "parent = \"main\""),
        "parent = \"main\"",
        "not a bus of this graph",
    )?;
    mixer_at(
        &r("id = 7", "id = 1 # again"),
        "id = 1 # again",
        "already used by bus `sfx`",
    )?;
    mixer_at(&r("id = 7", "id = 4294967295"), "id = 4294967295", "reserved")?;
    mixer_at(&r("gain = 0.8", "gain = 5"), "gain = 5", "outside 0 to 4")?;
    mixer_at(
        &r("threshold = 0.95", "threshold = 1.5"),
        "threshold",
        "outside (0, 1]",
    )?;
    mixer_at(
        &r("release_ms = 80", "release_ms = 0"),
        "release_ms",
        "outside (0, 10000]",
    )?;
    mixer_at(
        &r("cutoff_hz = 8000", "cutoff_hz = 100000"),
        "cutoff_hz = 100000",
        "outside (0, 96000]",
    )?;
    mixer_at(&r("db = -6", "db = -100"), "db = -100", "outside [-96, 24]")?;
    mixer_at(
        &r("kind = \"gain\"", "kind = \"reverb\""),
        "kind = \"reverb\"",
        "is not one of",
    )?;
    mixer_at(
        &r("db = -6", "db = -6\ncutoff_hz = 10"),
        "cutoff_hz = 10",
        "unknown key",
    )?;
    mixer_at(
        &r("[effect.sfx.1]", "[effect.sfx.2]"),
        "[effect.sfx.2]",
        "without gaps",
    )?;
    mixer_at(
        &r("[effect.music.0]", "[effect.drums.0]"),
        "[effect.drums.0]",
        "unknown bus `drums`",
    )?;
    let mut five = MIXER.to_owned();
    for n in 1..5 {
        write!(five, "[effect.master.{n}]\nkind = \"gain\"\ndb = 0\n")?;
    }
    mixer_at(&five, "[effect.master.4]", "at most 4 effects")?;
    // A two-bus cycle under a valid master.
    let cycle = format!("{MIXER}[bus.a]\nid = 10\nparent = \"b\"\n[bus.b]\nid = 11\nparent = \"a\"\n");
    mixer_at(&cycle, "[bus.a]", "cycle")?;
    let mut t = ContentTree::new();
    t.insert(MIXER_PATH, "# no buses\n");
    let e = single_error(&t)?;
    assert!(e.message.contains("0 buses"), "{e}");
    Ok(())
}

#[test]
fn sound_content_cooks_deterministically() -> TestResult {
    let a = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    for name in ["audio/sfx.sbk", "audio/main.mix"] {
        assert_eq!(a.get(name).map(|x| x.hash), b.get(name).map(|x| x.hash), "{name}");
    }
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}
