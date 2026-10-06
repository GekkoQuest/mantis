//! The skeleton importer: `*.skeleton.toml` to MSKL.

use mantis_anim::Skeleton;
use mantis_anim::skeleton::inverse_bind_matrices;
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::skeleton::{BoneDef, MAX_BONES, SkeletonAsset, UNIT_TOLERANCE, bone_name_hash};

use super::fields::{Doc, Fields, has_suffix, output_name};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

/// `*.skeleton.toml` to a skeleton.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkeletonImporter;

/// A unit quaternion from `key` (default identity), normalized when within tolerance.
pub(crate) fn rotation(f: &Fields<'_>, key: &str) -> Result<[f32; 4], CookError> {
    let q = f.opt_vec::<4>(key)?.unwrap_or([0.0, 0.0, 0.0, 1.0]);
    unit_quaternion(q).ok_or_else(|| {
        f.err(
            f.line_of(key),
            &format!("`{key}` is not a unit quaternion (x, y, z, w)"),
        )
    })
}

/// `q` normalized when its length is within the format's tolerance of 1.
pub(crate) fn unit_quaternion(q: [f32; 4]) -> Option<[f32; 4]> {
    let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
    ((len - 1.0).abs() <= UNIT_TOLERANCE).then(|| q.map(|c| c / len))
}

impl Importer for SkeletonImporter {
    fn name(&self) -> &'static str {
        "skeleton.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".skeleton.toml")
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&["bone"], &[])?;
        doc.root().only(&[])?;
        let mut names: Vec<&str> = Vec::new();
        let mut bones: Vec<BoneDef> = Vec::new();
        for (name, f) in doc.tables_under("bone") {
            f.only(&["parent", "translation", "rotation", "scale"])?;
            let parent = match f.opt_str("parent")? {
                None => None,
                Some(p) => {
                    let index = names.iter().position(|n| *n == p).ok_or_else(|| {
                        f.err(
                            f.line_of("parent"),
                            &format!("unknown bone `{p}` (a parent is declared above its children)"),
                        )
                    })?;
                    Some(u16::try_from(index).map_err(|_| f.err(f.line(), "too many bones"))?)
                }
            };
            let scale = f.opt_vec::<3>("scale")?.unwrap_or([1.0; 3]);
            if scale.iter().any(|s| *s <= 0.0) {
                return Err(f.err(f.line_of("scale"), "every `scale` component must be positive"));
            }
            let hash = bone_name_hash(name);
            if let Some(other) = names.iter().find(|n| bone_name_hash(n) == hash) {
                return Err(f.err(
                    f.line(),
                    &format!("bone `{name}` has the same name hash as `{other}`; rename one"),
                ));
            }
            if bones.len() >= MAX_BONES as usize {
                return Err(f.err(f.line(), &format!("more than {MAX_BONES} bones")));
            }
            names.push(name);
            bones.push(BoneDef {
                parent,
                name_hash: hash,
                translation: f.opt_vec::<3>("translation")?.unwrap_or([0.0; 3]),
                rotation: rotation(&f, "rotation")?,
                scale,
                inverse_bind: [0.0; 16],
            });
        }
        if bones.is_empty() {
            return Err(CookError::at(source.path, 0, "no `[bone.<name>]` tables"));
        }
        let inverses = inverse_bind_matrices(&bones);
        for (bone, inverse) in bones.iter_mut().zip(inverses) {
            bone.inverse_bind = inverse;
        }
        let asset = SkeletonAsset { bones };
        let bytes = asset.encode();
        let parsed = SkeletonAsset::parse(&bytes)
            .map_err(|e| CookError::at(source.path, 0, &format!("the cooked skeleton does not load: {e}")))?;
        Skeleton::new(&parsed)
            .map_err(|e| CookError::at(source.path, 0, &format!("the skeleton does not bind: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, ".skeleton.toml", ".skeleton"),
            kind: AssetKind::Skeleton,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

/// Resolves the skeleton named by `key` of `f` (an earlier phase's output).
pub(crate) fn resolve_skeleton(
    ctx: &ImportContext<'_>,
    f: &Fields<'_>,
    from: &str,
    key: &str,
) -> Result<SkeletonAsset, CookError> {
    let path = f.str(key)?;
    let line = f.line_of(key);
    let (_, bytes) = ctx.resolve_bytes(path, AssetKind::Skeleton, from, line)?;
    SkeletonAsset::parse(bytes).map_err(|e| f.err(line, &format!("`{path}`: {e}")))
}

/// The index of bone `name` in `skeleton`.
pub(crate) fn bone_index(skeleton: &SkeletonAsset, name: &str) -> Option<u16> {
    skeleton
        .find(bone_name_hash(name))
        .and_then(|i| u16::try_from(i).ok())
}
