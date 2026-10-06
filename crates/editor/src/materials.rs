//! Material graph editing with recompilation: a `*.material.toml` source is edited in
//! place, compiled exactly as the cook compiles it (the material importer, which runs
//! `mantis_shadergen` and validates every permutation), and the live renderer material is
//! replaced in place, so instances already drawn with it change on the next frame.
//!
//! A compile failure reports every error at its file and line and leaves the live
//! material untouched. [`MaterialEditor::save`] writes the source; the next recook gives
//! the material its new content hash.

use std::path::{Path, PathBuf};

use mantis_client::world_stream::Gpu;
use mantis_cook::importer::CookError;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, importers};
use mantis_core::module::toml::Value;
use mantis_formats::bundle::AssetKind;
use mantis_formats::material::MaterialAsset;
use mantis_formats::texture::TextureAsset;
use mantis_render::renderer::Renderer;
use mantis_render::scene::MaterialId;

use crate::source::{EditError, SourceDoc, floats};

/// A compiled material and its textures, ready for the renderer.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// The material.
    pub asset: MaterialAsset,
    /// Its texture slots, in order.
    pub textures: Vec<TextureAsset>,
}

/// One material source being edited.
#[derive(Clone, Debug)]
pub struct MaterialEditor {
    content: PathBuf,
    path: String,
    doc: SourceDoc,
    dirty: bool,
}

/// Errors of material editing.
#[derive(Debug)]
pub enum MaterialEditError {
    /// The source could not be read or written.
    Io(PathBuf, std::io::Error),
    /// An edit was refused.
    Edit(EditError),
    /// The edited material does not compile: every error at its file and line.
    Compile(Vec<CookError>),
    /// The renderer refused the replacement (the old material stays live).
    Render(String),
}

impl core::fmt::Display for MaterialEditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Edit(e) => write!(f, "{e}"),
            Self::Compile(errors) => {
                let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
                write!(f, "{}", lines.join("\n"))
            }
            Self::Render(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MaterialEditError {}

fn read(path: &Path) -> Result<Vec<u8>, MaterialEditError> {
    std::fs::read(path).map_err(|e| MaterialEditError::Io(path.to_path_buf(), e))
}

impl MaterialEditor {
    /// Opens `path` (relative to `content`, `/`-separated: `materials/ground.material.toml`).
    ///
    /// # Errors
    /// [`MaterialEditError::Io`] or [`MaterialEditError::Edit`] for a source that does not
    /// parse.
    pub fn open(content: &Path, path: &str) -> Result<Self, MaterialEditError> {
        let bytes = read(&content.join(path))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(Self {
            content: content.to_path_buf(),
            path: path.to_owned(),
            doc: SourceDoc::parse(&text).map_err(MaterialEditError::Edit)?,
            dirty: false,
        })
    }

    /// The source path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The source text as edited.
    pub fn text(&self) -> String {
        self.doc.text()
    }

    /// Whether there are unsaved edits.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Sets `key` of `table` (`""` for the root, `lighting`, `node.<name>`, ...).
    ///
    /// # Errors
    /// [`MaterialEditError::Edit`].
    pub fn set(&mut self, table: &str, key: &str, value: &Value) -> Result<(), MaterialEditError> {
        self.doc.set(table, key, value).map_err(MaterialEditError::Edit)?;
        self.dirty = true;
        Ok(())
    }

    /// Sets the default of the color parameter `name`.
    ///
    /// # Errors
    /// [`MaterialEditError::Edit`] (no such `[param.<name>]`).
    pub fn set_color(&mut self, name: &str, rgba: [f32; 4]) -> Result<(), MaterialEditError> {
        self.set(&format!("param.{name}"), "default", &floats(&rgba))
    }

    /// Compiles the edited source as the cook would: the material importer with the
    /// textures it names (read from the content directory), every permutation generated
    /// and validated.
    ///
    /// # Errors
    /// [`MaterialEditError::Compile`] with every error located in the source.
    pub fn compile(&self) -> Result<Compiled, MaterialEditError> {
        let mut tree = ContentTree::new();
        tree.insert(&self.path, self.doc.text());
        let textures: Vec<String> = self
            .doc
            .get("", "textures")
            .and_then(Value::as_strings)
            .unwrap_or_default();
        for t in &textures {
            tree.insert(t, read(&self.content.join(t))?);
            let sidecar = format!("{t}.texture.toml");
            let path = self.content.join(&sidecar);
            if path.is_file() {
                tree.insert(&sidecar, read(&path)?);
            }
        }
        let out = Cook::new(importers::builtin())
            .map_err(|e| MaterialEditError::Compile(vec![e]))?
            .run(&tree)
            .map_err(MaterialEditError::Compile)?;
        let stem = self.path.strip_suffix(".material.toml").unwrap_or(&self.path);
        let located = |what: &str| MaterialEditError::Compile(vec![CookError::at(&self.path, 0, what)]);
        let material = out
            .get(&format!("{stem}.mat"))
            .filter(|a| a.kind == AssetKind::Material)
            .ok_or_else(|| located("the material produced no output"))?;
        let asset = MaterialAsset::parse(&material.bytes)
            .map_err(|e| located(&format!("the cooked material does not load: {e}")))?;
        let mut cooked_textures = Vec::with_capacity(textures.len());
        for t in &textures {
            let cooked = out
                .assets
                .values()
                .find(|a| a.kind == AssetKind::Texture && a.source == *t)
                .ok_or_else(|| located(&format!("`{t}` produced no texture")))?;
            cooked_textures.push(
                TextureAsset::parse(&cooked.bytes)
                    .map_err(|e| located(&format!("`{t}` does not load: {e}")))?,
            );
        }
        Ok(Compiled {
            asset,
            textures: cooked_textures,
        })
    }

    /// Compiles and replaces the live renderer material `material` in place.
    ///
    /// # Errors
    /// [`MaterialEditError::Compile`] (nothing changes), or
    /// [`MaterialEditError::Render`] when the renderer refuses the replacement (the old
    /// material stays live).
    pub fn apply(
        &self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        material: MaterialId,
    ) -> Result<(), MaterialEditError> {
        let compiled = self.compile()?;
        let mut uploaded = Vec::with_capacity(compiled.textures.len());
        for t in &compiled.textures {
            let (_, view) = mantis_render::assets::upload_texture(gpu.device, gpu.queue, gpu.capabilities, t)
                .map_err(|e| MaterialEditError::Render(e.to_string()))?;
            uploaded.push(view);
        }
        let mut slots: [Option<&wgpu::TextureView>; 4] = [None; 4];
        for (slot, view) in slots.iter_mut().zip(&uploaded) {
            *slot = Some(view);
        }
        renderer
            .replace_material(gpu.device, gpu.queue, material, compiled.asset, slots)
            .map_err(|e| MaterialEditError::Render(e.to_string()))
    }

    /// Writes the source.
    ///
    /// # Errors
    /// [`MaterialEditError::Io`].
    pub fn save(&mut self) -> Result<(), MaterialEditError> {
        let path = self.content.join(&self.path);
        std::fs::write(&path, self.doc.text()).map_err(|e| MaterialEditError::Io(path, e))?;
        self.dirty = false;
        Ok(())
    }
}
