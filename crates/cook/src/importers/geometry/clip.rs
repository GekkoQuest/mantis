//! The clip importer: `*.clip.toml` to MCLP.

use mantis_anim::{Clip, Skeleton};
use mantis_formats::anim_clip::{Channel, ClipAsset, Interpolation, MAX_KEYS, TrackDef};
use mantis_formats::bundle::{AssetKind, Domain};

use super::skeleton::{bone_index, resolve_skeleton, unit_quaternion};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::source::{Doc, Fields, has_suffix, output_name};

/// `*.clip.toml` to a clip (phase 10: it resolves its skeleton).
#[derive(Clone, Copy, Debug, Default)]
pub struct ClipImporter;

impl Importer for ClipImporter {
    fn name(&self) -> &'static str {
        "clip.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        10
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".clip.toml")
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::from_source(source)?;
        doc.only_tables(&[], &["track"])?;
        let root = doc.root();
        root.only(&["skeleton", "duration", "sample_rate", "looping", "root_motion"])?;
        let skeleton = resolve_skeleton(ctx, &root, source.path, "skeleton")?;
        let duration = root.f32("duration")?.0;
        if duration <= 0.0 {
            return Err(root.err(root.line_of("duration"), "`duration` must be positive"));
        }
        let sample_rate = root.f32_or("sample_rate", 30.0)?.0;
        if sample_rate <= 0.0 {
            return Err(root.err(root.line_of("sample_rate"), "`sample_rate` must be positive"));
        }
        let mut tracks = Vec::new();
        for (rest, f) in doc.items("track") {
            let (bone_name, channel_name) = rest
                .rsplit_once('.')
                .ok_or_else(|| f.err(f.line(), "a track table is `[track.<bone>.<channel>]`"))?;
            let bone = bone_index(&skeleton, bone_name)
                .ok_or_else(|| f.err(f.line(), &format!("unknown bone `{bone_name}`")))?;
            let channel = match channel_name {
                "translation" => Channel::Translation,
                "rotation" => Channel::Rotation,
                "scale" => Channel::Scale,
                other => {
                    return Err(f.err(
                        f.line(),
                        &format!("unknown channel `{other}` (translation, rotation, or scale)"),
                    ));
                }
            };
            tracks.push(track(&f, bone, channel, duration)?);
        }
        tracks.sort_by_key(|t| (t.bone, t.channel));
        let asset = ClipAsset {
            duration,
            sample_rate,
            looping: root.bool_or("looping", false)?.0,
            root_motion: root.bool_or("root_motion", false)?.0,
            bone_count: u32::try_from(skeleton.bones.len()).unwrap_or(u32::MAX),
            tracks,
        };
        if asset.root_motion
            && !asset
                .tracks
                .iter()
                .any(|t| t.bone == 0 && t.channel != Channel::Scale)
        {
            return Err(root.err(
                root.line_of("root_motion"),
                "root motion needs a translation or rotation track on the root bone",
            ));
        }
        let bytes = asset.encode();
        let parsed = ClipAsset::parse(&bytes)
            .map_err(|e| CookError::at(source.path, 0, &format!("the cooked clip does not load: {e}")))?;
        let skeleton = Skeleton::new(&skeleton).map_err(|e| {
            root.err(
                root.line_of("skeleton"),
                &format!("the skeleton does not bind: {e}"),
            )
        })?;
        let clip = Clip::new(&parsed)
            .map_err(|e| CookError::at(source.path, 0, &format!("the clip does not bind: {e}")))?;
        if clip.bone_count() != skeleton.bone_count() {
            return Err(root.err(root.line_of("skeleton"), "the clip does not match its skeleton"));
        }
        Ok(vec![Cooked {
            name: output_name(source.path, ".clip.toml", ".clip"),
            kind: AssetKind::AnimClip,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

fn track(f: &Fields<'_>, bone: u16, channel: Channel, duration: f32) -> Result<TrackDef, CookError> {
    f.only(&["times", "values", "interpolation"])?;
    let interpolation = match f.opt_str("interpolation")?.map_or("linear", |(s, _)| s) {
        "linear" => Interpolation::Linear,
        "step" => Interpolation::Step,
        other => {
            return Err(f.err(
                f.line_of("interpolation"),
                &format!("unknown interpolation `{other}` (linear or step)"),
            ));
        }
    };
    let times = f.f32s("times")?.0;
    let times_line = f.line_of("times");
    if times.is_empty() || times.len() > MAX_KEYS as usize {
        return Err(f.err(times_line, &format!("a track has 1 to {MAX_KEYS} keys")));
    }
    if let Some(i) = times.windows(2).position(|w| matches!(w, [a, b] if a >= b)) {
        return Err(f.err(
            times_line,
            &format!(
                "key times must increase strictly (key {} is not after key {})",
                i + 2,
                i + 1
            ),
        ));
    }
    if let Some(t) = times.iter().find(|t| !(0.0..=duration).contains(*t)) {
        return Err(f.err(times_line, &format!("key time {t} is outside 0 to {duration}")));
    }
    let mut values = f.f32s("values")?.0;
    let values_line = f.line_of("values");
    let width = channel.width();
    if values.len() != times.len() * width {
        return Err(f.err(
            values_line,
            &format!(
                "{} values for {} keys of width {width} (expected {})",
                values.len(),
                times.len(),
                times.len() * width
            ),
        ));
    }
    if channel == Channel::Rotation {
        for (k, q) in values.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *q = unit_quaternion(*q)
                .ok_or_else(|| f.err(values_line, &format!("key {} is not a unit quaternion", k + 1)))?;
        }
    }
    Ok(TrackDef {
        bone,
        channel,
        interpolation,
        times,
        values,
    })
}
