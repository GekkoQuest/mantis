//! Built-in importers: geometry and animation (see [`importers`](super)).
//!
//! | importer | phase | sources | inputs | output | kind |
//! |---|---|---|---|---|---|
//! | `mesh.obj` | 0 | `*.obj` (not `*.skin.obj`) | `<file>.obj.toml` | `<stem>.mesh` (MMSH) | `Mesh` |
//! | `mesh.skinned` | 0 | `*.skinmesh.toml` | the OBJ it names (`*.skin.obj` by convention) | `<stem>.skinmesh` (MMSH, skinned) | `Mesh` |
//! | `skeleton.toml` | 0 | `*.skeleton.toml` | | `<stem>.skeleton` (MSKL) | `Skeleton` |
//! | `clip.toml` | 10 | `*.clip.toml` | | `<stem>.clip` (MCLP) | `AnimClip` |
//! | `animgraph.toml` | 15 | `*.animgraph.toml` | | `<stem>.animgraph` (MAGR) | `AnimGraph` |
//! | `vat.bake` | 15 | `*.vat.toml` | | `<stem>.vat` ([`vat`], MVAT) | `VertexAnimation` |
//!
//! Every output is in the presentation domain and is parsed with its runtime parser (and
//! skeletons, clips, and graphs bound with `mantis_anim`) before the cook accepts it.
//! Paths inside sources are content-root-relative source paths.
//!
//! # Meshes: Wavefront OBJ
//!
//! Statements: `v x y z` (extra components ignored), `vt u [v]`, `vn x y z`, `f` with
//! three or more corners in any of the forms `v`, `v/vt`, `v//vn`, `v/vt/vn`, with
//! positive (1-based) or negative (relative) indices; polygons are fan-triangulated.
//! `vt2 u v` is an engine extension: a second UV set parallel to `vt` (the `k`-th `vt2`
//! pairs with the `k`-th `vt`), used for lightmap UVs. `o`, `g`, `s`, `usemtl`, and
//! `mtllib` are ignored (one mesh per file); any other statement is an error.
//!
//! Corners without `vn` get smooth normals: the area-weighted sum of the face normals
//! around their position. Identical vertices (position, normal, UVs, and skinning) are
//! welded; triangles with a repeated position are dropped.
//!
//! Handedness (decision 0019): OBJ is right-handed (+Z toward the viewer) and wound
//! counter-clockwise from outside. The cook negates **Z** of positions and normals and
//! swaps the second and third corner of every triangle, so `(b - a) x (c - a)` points
//! outward in the left-handed world frame. Texture V is flipped (`1 - v`): OBJ's V origin
//! is the bottom of the image, the runtime's the top.
//!
//! Meshlets hold at most `MESHLET_MAX_TRIANGLES` triangles and `MESHLET_MAX_VERTICES`
//! vertices, grown greedily by adjacency (fewest new vertices first, then nearest);
//! bounding spheres contain every vertex; normal cones are conservative (see
//! `meshlets::normal_cone`), or open when the normals spread 90 degrees or more.
//!
//! Optional settings sidecar `crate.obj.toml` next to `crate.obj`:
//!
//! ```toml
//! scale = 0.01         # uniform, positive (default 1)
//! lightmap_uv = true   # uv1 from the `vt2` set when present, else zero (default false)
//! ```
//!
//! # Skinned meshes: `*.skinmesh.toml`
//!
//! ```toml
//! mesh = "meshes/arm.skin.obj"   # OBJ geometry (any `*.obj`; `*.skin.obj` is not cooked alone)
//! scale = 1.0                    # optional, as in the OBJ sidecar
//! lightmap_uv = false            # optional
//!
//! [weights.1]                    # OBJ vertex number (1-based, the `v` line order)
//! joints = [0]                   # 1 to 4 bone indices, distinct
//! weights = [1.0]                # non-negative, summing to 1 (within 1e-3)
//!
//! [weights.2]
//! joints = [0, 1]
//! weights = [0.5, 0.5]
//! ```
//!
//! Every vertex a face uses needs a table. Weights are quantized to bytes summing to
//! exactly 255 (largest remainder).
//!
//! # Skeletons: `*.skeleton.toml`
//!
//! ```toml
//! [bone.root]
//! translation = [0.0, 0.0, 0.0]    # optional, default zero
//! rotation = [0.0, 0.0, 0.0, 1.0]  # optional quaternion x, y, z, w (unit within 1e-3)
//! scale = [1.0, 1.0, 1.0]          # optional, positive
//!
//! [bone.hip]
//! parent = "root"                  # a bone declared above; absent for a root
//! translation = [0.0, 1.0, 0.0]
//! ```
//!
//! Bones keep file order; inverse bind matrices come from
//! `mantis_anim::skeleton::inverse_bind_matrices`. Skeleton, clip, and graph sources are
//! authored in the world frame of decision 0019 (no conversion).
//!
//! # Clips: `*.clip.toml`
//!
//! ```toml
//! skeleton = "skeletons/arm.skeleton.toml"
//! duration = 1.0
//! sample_rate = 30.0     # optional, default 30
//! looping = true         # optional, default false
//! root_motion = false    # optional, default false
//!
//! [track.hip.rotation]   # [track.<bone>.<translation|rotation|scale>]
//! interpolation = "linear"            # optional: linear (default) or step
//! times = [0.0, 0.5, 1.0]             # strictly increasing, within 0 to duration
//! values = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.3826834, 0.9238795, 0.0, 0.0, 0.0, 1.0]
//! ```
//!
//! # Animation graphs: `*.animgraph.toml`
//!
//! ```toml
//! skeleton = "skeletons/body.skeleton.toml"
//!
//! [clip.walk]
//! path = "clips/walk.clip.toml"
//! [clip.run]
//! path = "clips/run.clip.toml"
//!
//! [parameter.speed]
//! kind = "float"        # float (default), bool, or trigger
//! default = 0.0         # float: a number; bool: true or false; trigger: none
//! [parameter.jump]
//! kind = "trigger"
//!
//! [node.main]
//! kind = "state_machine"
//! states = ["idle", "move"]   # node names; state indices follow this order
//! entry = "idle"              # optional, default the first state
//! [node.idle]
//! kind = "clip"
//! clip = "walk"
//! speed = 0.0                 # optional, default 1
//! [node.move]
//! kind = "blend1d"
//! parameter = "speed"         # a float parameter
//! children = ["slow", "fast"]
//! thresholds = [0.0, 1.0]     # strictly increasing
//! [node.slow]
//! kind = "clip"
//! clip = "walk"
//! [node.fast]
//! kind = "clip"
//! clip = "run"
//!
//! [transition.main.start]     # [transition.<machine>.<name>], checked in file order
//! from = "idle"               # a state, or "any"
//! to = "move"
//! crossfade = 0.2             # optional seconds, default 0
//! exit_time = 0.5             # optional normalized time
//! conditions = ["speed > 0.1"]   # `<p> <op> <value>` (> >= < <= == !=) or `<p> triggered`
//!
//! [layer.base]                # bottom first, file order
//! node = "main"
//! weight = 1.0                # optional, 0 to 1
//! mode = "override"           # optional: override (default) or additive
//! # reference = "walk"        # additive only: the reference clip (default the bind pose)
//! # mask = ["spine", "head"]  # optional bone names (default every bone)
//!
//! [foot.left]                 # two-bone IK chains
//! root = "hip"
//! mid = "knee"
//! tip = "ankle"
//! pole = [0.0, 0.0, 1.0]
//!
//! [look_at]
//! head = "head"
//! axis = [0.0, 0.0, 1.0]      # normalized
//! max_angle = 1.0             # radians, (0, pi]
//! ```
//!
//! Every node is used by exactly one layer or parent node. Clips resolve to content
//! hashes (identical clips share one entry), and the graph is bound with
//! `mantis_anim::AnimGraph::new` on the resolved skeleton and clips.
//!
//! # Vertex animation bakes: `*.vat.toml`
//!
//! ```toml
//! skeleton = "skeletons/arm.skeleton.toml"
//! clip = "clips/bend.clip.toml"
//! mesh = "meshes/arm.skinmesh.toml"    # a skinned mesh source
//! fps = 30.0
//! ```
//!
//! Baked with `mantis_anim::bake_vat` on the cooked mesh (byte weights divided by 255);
//! the payload layout is in [`vat`].

use std::sync::Arc;

use crate::importer::Importer;

mod clip;
mod graph;
mod mesh;
mod meshes;
mod meshlets;
mod obj;
mod skeleton;
pub mod vat;

pub use clip::ClipImporter;
pub use graph::GraphImporter;
pub use meshes::{ObjImporter, SkinnedMeshImporter};
pub use skeleton::SkeletonImporter;
pub use vat::VatImporter;

/// This module's importers.
pub fn importers() -> Vec<Arc<dyn Importer>> {
    vec![
        Arc::new(ObjImporter),
        Arc::new(SkinnedMeshImporter),
        Arc::new(SkeletonImporter),
        Arc::new(ClipImporter),
        Arc::new(GraphImporter),
        Arc::new(VatImporter),
    ]
}
