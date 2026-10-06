//! mantis-render: the renderer (plan 8.3, decisions 0003 and 0012).

#![forbid(unsafe_code)]

pub mod assets;
pub mod crowd;
pub mod culling;
pub mod deform;
pub mod gpu;
pub mod gpu_test;
pub mod gpu_timer;
pub mod gpu_types;
pub mod graph;
pub mod layouts;
pub mod lighting;
pub mod materials;
pub mod math;
pub mod mesh;
pub mod occlusion;
pub mod particles;
pub mod post;
pub mod renderer;
pub mod scene;
pub mod streaming;
pub mod textures;
pub mod ui_pass;
pub mod world_light;
