//! Render graph tests: ordering, culling, validation, lifetimes, and aliasing, all without
//! ra GPU.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RGBA: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

fn tex(w: u32, h: u32, f: wgpu::TextureFormat) -> TextureDesc {
    TextureDesc::d2(w, h, f)
}

#[test]
fn chain_is_ordered_and_dead_passes_are_culled() -> TestResult {
    let mut graph = GraphBuilder::new();
    let ra = graph.create_texture("ra", tex(64, 64, RGBA));
    let rb = graph.create_texture("rb", tex(64, 64, RGBA));
    let unused = graph.create_texture("unused", tex(64, 64, RGBA));
    let out = graph.import_texture("swapchain", tex(64, 64, RGBA));

    let a1 = graph.add_pass("produce_a", PassKind::Render).write_texture(
        ra,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let _ = graph.add_pass("dead", PassKind::Render).write_texture(
        unused,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let b1 = {
        let mut pass = graph.add_pass("a_to_b", PassKind::Render);
        pass.read_texture(a1, TextureAccess::Sampled);
        pass.write_texture(rb, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let out1 = {
        let mut pass = graph.add_pass("present", PassKind::Render);
        pass.read_texture(b1, TextureAccess::Sampled);
        pass.write_texture(out, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(out1);
    let compiled = graph.compile()?;
    assert_eq!(compiled.ordered_names(), vec!["produce_a", "a_to_b", "present"]);
    assert_eq!(compiled.culled().len(), 1);
    assert_eq!(
        compiled.pass_name(compiled.culled().first().copied().ok_or("culled")?),
        "dead"
    );
    assert_eq!(compiled.binding(unused.resource()), ResourceBinding::Unused);
    assert_eq!(compiled.binding(out.resource()), ResourceBinding::Imported);
    Ok(())
}

#[test]
fn independent_passes_keep_declaration_order() -> TestResult {
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let ty = graph.create_texture("ty", tex(8, 8, RGBA));
    let o1 = graph.import_texture("o1", tex(8, 8, RGBA));
    let o2 = graph.import_texture("o2", tex(8, 8, RGBA));
    let x1 = graph.add_pass("A", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let y1 = graph.add_pass("B", PassKind::Render).write_texture(
        ty,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let o2v = {
        let mut pass = graph.add_pass("C", PassKind::Render);
        pass.read_texture(y1, TextureAccess::Sampled);
        pass.write_texture(o2, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let o1v = {
        let mut pass = graph.add_pass("D", PassKind::Render);
        pass.read_texture(x1, TextureAccess::Sampled);
        pass.write_texture(o1, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(o1v);
    graph.output(o2v);
    assert_eq!(graph.compile()?.ordered_names(), vec!["A", "B", "C", "D"]);
    Ok(())
}

#[test]
fn reader_of_an_old_version_is_moved_before_the_next_writer() -> TestResult {
    // "history_read" is declared after "overwrite" but reads the version "overwrite"
    // replaces, so it must run first.
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let ty = graph.create_texture("ty", tex(8, 8, RGBA));
    let x1 = graph.add_pass("produce", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let x2 = graph.add_pass("overwrite", PassKind::Render).write_texture(
        x1,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let y1 = {
        let mut pass = graph.add_pass("history_read", PassKind::Render);
        pass.read_texture(x1, TextureAccess::Sampled);
        pass.write_texture(ty, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(x2);
    graph.output(y1);
    assert_eq!(
        graph.compile()?.ordered_names(),
        vec!["produce", "history_read", "overwrite"]
    );
    Ok(())
}

#[test]
fn impossible_orders_are_reported_as_cycles() -> TestResult {
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let ty = graph.create_texture("ty", tex(8, 8, RGBA));
    let tz = graph.create_texture("tz", tex(8, 8, RGBA));
    let x1 = graph.add_pass("P", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let x2 = graph.add_pass("W", PassKind::Render).write_texture(
        x1,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let y1 = {
        let mut pass = graph.add_pass("Q", PassKind::Render);
        pass.read_texture(x2, TextureAccess::Sampled);
        pass.write_texture(ty, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    // R needs Q's output (so after W) and the version W replaces (so before W).
    let z1 = {
        let mut pass = graph.add_pass("R", PassKind::Render);
        pass.read_texture(x1, TextureAccess::Sampled);
        pass.read_texture(y1, TextureAccess::Sampled);
        pass.write_texture(tz, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(z1);
    let result = graph.compile();
    let Err(GraphError::Cycle { passes }) = &result else {
        return Err(format!("expected a cycle, got {result:?}").into());
    };
    assert!(
        passes.contains(&"W".to_owned()) && passes.contains(&"R".to_owned()),
        "{passes:?}"
    );
    Ok(())
}

#[test]
fn discard_writes_do_not_keep_the_previous_producer_alive() -> TestResult {
    for (mode, expect_kept) in [(WriteMode::Discard, false), (WriteMode::Load, true)] {
        let mut graph = GraphBuilder::new();
        let tx = graph.create_texture("tx", tex(8, 8, RGBA));
        let x1 = graph.add_pass("first", PassKind::Render).write_texture(
            tx,
            TextureAccess::ColorTarget,
            WriteMode::Discard,
        );
        let x2 =
            graph
                .add_pass("second", PassKind::Render)
                .write_texture(x1, TextureAccess::ColorTarget, mode);
        graph.output(x2);
        let compiled = graph.compile()?;
        assert_eq!(
            compiled.ordered_names().contains(&"first"),
            expect_kept,
            "{mode:?}"
        );
    }
    Ok(())
}

#[test]
fn side_effect_passes_are_never_culled() -> TestResult {
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let x1 = graph.add_pass("draw", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    graph
        .add_pass("readback", PassKind::Copy)
        .read_texture(x1, TextureAccess::CopySrc)
        .side_effect();
    assert_eq!(graph.compile()?.ordered_names(), vec!["draw", "readback"]);
    Ok(())
}

#[test]
fn declaration_errors_fail_closed() {
    // Writing ra version that is not the latest.
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let _ = graph.add_pass("p1", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let _ = graph.add_pass("fork", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    assert!(matches!(
        graph.compile(),
        Err(GraphError::StaleWrite {
            version: 0,
            latest: 1,
            ..
        })
    ));

    // Reading ra transient that was never written, and load-writing one.
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    graph
        .add_pass("read", PassKind::Render)
        .read_texture(tx, TextureAccess::Sampled);
    assert!(matches!(graph.compile(), Err(GraphError::ReadBeforeWrite { .. })));
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    let _ = graph.add_pass("load", PassKind::Render).write_texture(
        tx,
        TextureAccess::ColorTarget,
        WriteMode::Load,
    );
    assert!(matches!(graph.compile(), Err(GraphError::ReadBeforeWrite { .. })));

    // Imported resources hold valid contents at version 0.
    let mut graph = GraphBuilder::new();
    let hist = graph.import_texture("history", tex(8, 8, RGBA));
    graph
        .add_pass("read_history", PassKind::Render)
        .read_texture(hist, TextureAccess::Sampled)
        .side_effect();
    assert!(graph.compile().is_ok());

    // Depth access on ra color format, color access on ra depth format, ra write as ra read.
    let mut graph = GraphBuilder::new();
    let compiled = graph.create_texture("compiled", tex(8, 8, RGBA));
    let _ = graph.add_pass("pass", PassKind::Render).write_texture(
        compiled,
        TextureAccess::DepthWrite,
        WriteMode::Discard,
    );
    assert!(matches!(graph.compile(), Err(GraphError::InvalidAccess { .. })));
    let mut graph = GraphBuilder::new();
    let rd = graph.create_texture("rd", tex(8, 8, DEPTH));
    let _ = graph.add_pass("pass", PassKind::Render).write_texture(
        rd,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    assert!(matches!(graph.compile(), Err(GraphError::InvalidAccess { .. })));
    let mut graph = GraphBuilder::new();
    let h = graph.import_texture("h", tex(8, 8, RGBA));
    graph
        .add_pass("pass", PassKind::Render)
        .read_texture(h, TextureAccess::ColorTarget);
    assert!(matches!(graph.compile(), Err(GraphError::InvalidAccess { .. })));

    // One pass writing the same resource twice.
    let mut graph = GraphBuilder::new();
    let tx = graph.create_texture("tx", tex(8, 8, RGBA));
    {
        let mut pass = graph.add_pass("twice", PassKind::Compute);
        let x1 = pass.write_texture(tx, TextureAccess::StorageWrite, WriteMode::Discard);
        let _ = pass.write_texture(x1, TextureAccess::StorageWrite, WriteMode::Load);
    }
    assert!(matches!(graph.compile(), Err(GraphError::DuplicateWrite { .. })));

    // A handle from another graph, and ra texture handle used as the wrong kind.
    let mut other = GraphBuilder::new();
    for i in 0..4 {
        let _ = other.create_texture(&format!("t{i}"), tex(8, 8, RGBA));
    }
    let foreign = other.create_texture("foreign", tex(8, 8, RGBA));
    let mut graph = GraphBuilder::new();
    let _ = graph.add_pass("pass", PassKind::Render).write_texture(
        foreign,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    assert!(matches!(graph.compile(), Err(GraphError::UnknownResource { .. })));
    let mut graph = GraphBuilder::new();
    let buf = graph.create_buffer("rb", BufferDesc { size: 16 });
    let _ = graph.create_texture("t", tex(8, 8, RGBA));
    let as_texture = TextureHandle {
        id: buf.resource(),
        version: 0,
    };
    let _ = graph.add_pass("pass", PassKind::Render).write_texture(
        as_texture,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    assert!(matches!(graph.compile(), Err(GraphError::UnknownResource { .. })));
}

#[test]
fn lifetimes_span_first_to_last_use_in_execution_order() -> TestResult {
    let mut graph = GraphBuilder::new();
    let ra = graph.create_texture("ra", tex(8, 8, RGBA));
    let rb = graph.create_texture("rb", tex(8, 8, RGBA));
    let out = graph.import_texture("out", tex(8, 8, RGBA));
    let a1 = graph.add_pass("0", PassKind::Render).write_texture(
        ra,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let b1 = graph.add_pass("1", PassKind::Render).write_texture(
        rb,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let b2 = {
        let mut pass = graph.add_pass("2", PassKind::Render);
        pass.read_texture(a1, TextureAccess::Sampled);
        pass.write_texture(b1, TextureAccess::ColorTarget, WriteMode::Load)
    };
    let out_v = {
        let mut pass = graph.add_pass("3", PassKind::Render);
        pass.read_texture(b2, TextureAccess::Sampled);
        pass.write_texture(out, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(out_v);
    let compiled = graph.compile()?;
    assert_eq!(compiled.lifetime(ra.resource()), Some((0, 2)));
    assert_eq!(compiled.lifetime(rb.resource()), Some((1, 3)));
    assert_eq!(compiled.lifetime(out.resource()), Some((3, 3)));
    Ok(())
}

#[test]
fn transients_with_disjoint_lifetimes_share_memory() -> TestResult {
    // A ping-pong chain: t0 → t1 → t2 → t3 → out, all the same description. Only two
    // physical textures are ever live at once.
    let mut graph = GraphBuilder::new();
    let ts: Vec<TextureHandle> = (0..4)
        .map(|i| graph.create_texture(&format!("t{i}"), tex(256, 256, HDR)))
        .collect();
    let out = graph.import_texture("out", tex(256, 256, RGBA));
    let mut prev: Option<TextureHandle> = None;
    for (i, t) in ts.iter().enumerate() {
        let mut pass = graph.add_pass(&format!("step{i}"), PassKind::Render);
        if let Some(pv) = prev {
            pass.read_texture(pv, TextureAccess::Sampled);
        }
        prev = Some(pass.write_texture(*t, TextureAccess::ColorTarget, WriteMode::Discard));
    }
    let out_v = {
        let mut pass = graph.add_pass("resolve", PassKind::Render);
        pass.read_texture(prev.ok_or("chain")?, TextureAccess::Sampled);
        pass.write_texture(out, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(out_v);
    let compiled = graph.compile()?;
    assert_eq!(compiled.physical().len(), 2, "{:?}", compiled.physical());
    let phys = |i: usize| ts.get(i).map(|t| compiled.binding(t.resource()));
    assert_eq!(phys(0), phys(2));
    assert_eq!(phys(1), phys(3));
    assert_ne!(phys(0), phys(1));
    assert_eq!(compiled.aliased_bytes() * 2, compiled.unaliased_bytes());
    // The shared texture's usage is the union of its users' usages.
    for pass in compiled.physical() {
        if let PhysicalResource::Texture { usage, .. } = pass {
            assert_eq!(
                *usage,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
            );
        }
    }
    Ok(())
}

#[test]
fn different_descriptions_and_imports_are_never_aliased() -> TestResult {
    let mut graph = GraphBuilder::new();
    let small = graph.create_texture("small", tex(64, 64, HDR));
    let big = graph.create_texture("big", tex(128, 128, HDR));
    let imp = graph.import_texture("imp", tex(64, 64, HDR));
    let s1 = graph.add_pass("s", PassKind::Render).write_texture(
        small,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    let b1 = {
        let mut pass = graph.add_pass("rb", PassKind::Render);
        pass.read_texture(s1, TextureAccess::Sampled);
        pass.write_texture(big, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let i1 = {
        let mut pass = graph.add_pass("i", PassKind::Render);
        pass.read_texture(b1, TextureAccess::Sampled);
        pass.write_texture(imp, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    graph.output(i1);
    let compiled = graph.compile()?;
    assert_eq!(compiled.physical().len(), 2);
    assert_eq!(compiled.binding(imp.resource()), ResourceBinding::Imported);
    let req = compiled
        .imported_usage()
        .iter()
        .find(|(r, _)| *r == imp.resource())
        .map(|(_, u)| *u);
    assert_eq!(
        req,
        Some(RequiredUsage::Texture(wgpu::TextureUsages::RENDER_ATTACHMENT))
    );
    Ok(())
}

#[test]
fn buffers_alias_best_fit_and_grow() -> TestResult {
    let mut graph = GraphBuilder::new();
    let ra = graph.create_buffer("ra", BufferDesc { size: 1024 });
    let rb = graph.create_buffer("rb", BufferDesc { size: 4096 });
    let c_ = graph.create_buffer("c", BufferDesc { size: 512 });
    let rd = graph.create_buffer("rd", BufferDesc { size: 8192 });
    let re = graph.create_buffer("re", BufferDesc { size: 600 });
    let out = graph.import_buffer("out", BufferDesc { size: 16 });
    // ra and rb live through pass 2, where compiled is born, so compiled gets its own buffer. At pass 3, rd
    // (8192) fits no free buffer, so the largest free one (rb, 4096) grows. At pass 4, re
    // (600) takes the smallest free buffer that fits: ra (1024), not compiled (512, too small).
    let a1 = graph.add_pass("pa", PassKind::Compute).write_buffer(
        ra,
        BufferAccess::StorageWrite,
        WriteMode::Discard,
    );
    let b1 = graph.add_pass("pb", PassKind::Compute).write_buffer(
        rb,
        BufferAccess::StorageWrite,
        WriteMode::Discard,
    );
    let c1 = {
        let mut pass = graph.add_pass("pc", PassKind::Compute);
        pass.read_buffer(a1, BufferAccess::StorageRead);
        pass.read_buffer(b1, BufferAccess::StorageRead);
        pass.write_buffer(c_, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    let d1 = {
        let mut pass = graph.add_pass("pd", PassKind::Compute);
        pass.read_buffer(c1, BufferAccess::StorageRead);
        pass.write_buffer(rd, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    let e1 = {
        let mut pass = graph.add_pass("pe", PassKind::Compute);
        pass.read_buffer(d1, BufferAccess::StorageRead);
        pass.write_buffer(re, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    let out_v = {
        let mut pass = graph.add_pass("po", PassKind::Compute);
        pass.read_buffer(e1, BufferAccess::Indirect);
        pass.write_buffer(out, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    graph.output(out_v);
    let compiled = graph.compile()?;
    assert_eq!(compiled.physical().len(), 3);
    assert_ne!(
        compiled.binding(c_.resource()),
        compiled.binding(ra.resource()),
        "c overlaps a in pass pc"
    );
    assert_eq!(compiled.binding(rd.resource()), compiled.binding(rb.resource()));
    assert_eq!(compiled.binding(re.resource()), compiled.binding(ra.resource()));
    let size_usage = |r: ResourceId| match compiled.binding(r) {
        ResourceBinding::Physical(i) => match compiled.physical().get(i) {
            Some(PhysicalResource::Buffer { size, usage }) => Some((*size, *usage)),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(
        size_usage(rb.resource()),
        Some((8192, wgpu::BufferUsages::STORAGE))
    );
    assert_eq!(
        size_usage(ra.resource()),
        Some((1024, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT))
    );
    Ok(())
}

#[test]
#[expect(clippy::too_many_lines)] // One linear frame description reads best as one function.
fn a_representative_frame_compiles_in_dependency_order() -> TestResult {
    let (w, h) = (1920, 1080);
    let mut graph = GraphBuilder::new();
    let swap = graph.import_texture("swapchain", tex(w, h, wgpu::TextureFormat::Bgra8UnormSrgb));
    let history = graph.import_texture("taa_history", tex(w, h, HDR));
    let instances = graph.import_buffer("instances", BufferDesc { size: 1 << 20 });
    let draws = graph.create_buffer("draw_args", BufferDesc { size: 1 << 16 });
    let shadow = graph.create_texture(
        "shadow_cascades",
        TextureDesc {
            depth_or_array_layers: 4,
            ..tex(2048, 2048, DEPTH)
        },
    );
    let clusters = graph.create_buffer("light_clusters", BufferDesc { size: 1 << 20 });
    let depth = graph.create_texture("depth", tex(w, h, DEPTH));
    let ao = graph.create_texture("ssao", tex(w, h, wgpu::TextureFormat::R8Unorm));
    let hdr = graph.create_texture("hdr", tex(w, h, HDR));
    let resolved = graph.create_texture("taa_out", tex(w, h, HDR));
    let bloom_a = graph.create_texture("bloom_down", tex(w / 2, h / 2, HDR));
    let bloom_b = graph.create_texture("bloom_blur_h", tex(w / 2, h / 2, HDR));
    let bloom_c = graph.create_texture("bloom_blur_v", tex(w / 2, h / 2, HDR));
    let ldr = graph.create_texture("ldr", tex(w, h, RGBA));

    let draws1 = {
        let mut pass = graph.add_pass("cull", PassKind::Compute);
        pass.read_buffer(instances, BufferAccess::StorageRead);
        pass.write_buffer(draws, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    let shadow1 = {
        let mut pass = graph.add_pass("shadows", PassKind::Render);
        pass.read_buffer(draws1, BufferAccess::Indirect);
        pass.write_texture(shadow, TextureAccess::DepthWrite, WriteMode::Discard)
    };
    let depth1 = {
        let mut pass = graph.add_pass("depth_prepass", PassKind::Render);
        pass.read_buffer(draws1, BufferAccess::Indirect);
        pass.write_texture(depth, TextureAccess::DepthWrite, WriteMode::Discard)
    };
    let clusters1 = {
        let mut pass = graph.add_pass("cluster_lights", PassKind::Compute);
        pass.read_texture(depth1, TextureAccess::Sampled);
        pass.write_buffer(clusters, BufferAccess::StorageWrite, WriteMode::Discard)
    };
    let ao1 = {
        let mut pass = graph.add_pass("ssao", PassKind::Compute);
        pass.read_texture(depth1, TextureAccess::Sampled);
        pass.write_texture(ao, TextureAccess::StorageWrite, WriteMode::Discard)
    };
    let hdr1 = {
        let mut pass = graph.add_pass("opaque", PassKind::Render);
        pass.read_buffer(draws1, BufferAccess::Indirect);
        pass.read_buffer(clusters1, BufferAccess::StorageRead);
        pass.read_texture(shadow1, TextureAccess::Sampled);
        pass.read_texture(ao1, TextureAccess::Sampled);
        pass.read_texture(depth1, TextureAccess::DepthRead);
        pass.write_texture(hdr, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let hdr2 = {
        let mut pass = graph.add_pass("sky", PassKind::Render);
        pass.read_texture(depth1, TextureAccess::DepthRead);
        pass.write_texture(hdr1, TextureAccess::ColorTarget, WriteMode::Load)
    };
    let (resolved1, history1) = {
        let mut pass = graph.add_pass("taa", PassKind::Render);
        pass.read_texture(hdr2, TextureAccess::Sampled);
        pass.read_texture(history, TextureAccess::Sampled);
        let r = pass.write_texture(resolved, TextureAccess::ColorTarget, WriteMode::Discard);
        (r, history)
    };
    let history2 = {
        let mut pass = graph.add_pass("store_history", PassKind::Copy);
        pass.read_texture(resolved1, TextureAccess::CopySrc);
        pass.write_texture(history1, TextureAccess::CopyDst, WriteMode::Discard)
    };
    let bloom_down_out = {
        let mut pass = graph.add_pass("bloom", PassKind::Render);
        pass.read_texture(resolved1, TextureAccess::Sampled);
        pass.write_texture(bloom_a, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let bloom_h_out = {
        let mut pass = graph.add_pass("bloom_h", PassKind::Render);
        pass.read_texture(bloom_down_out, TextureAccess::Sampled);
        pass.write_texture(bloom_b, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let bloom1 = {
        let mut pass = graph.add_pass("bloom_v", PassKind::Render);
        pass.read_texture(bloom_h_out, TextureAccess::Sampled);
        pass.write_texture(bloom_c, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let ldr1 = {
        let mut pass = graph.add_pass("tonemap", PassKind::Render);
        pass.read_texture(resolved1, TextureAccess::Sampled);
        pass.read_texture(bloom1, TextureAccess::Sampled);
        pass.write_texture(ldr, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    let swap1 = {
        let mut pass = graph.add_pass("ui_composite", PassKind::Render);
        pass.read_texture(ldr1, TextureAccess::Sampled);
        pass.write_texture(swap, TextureAccess::ColorTarget, WriteMode::Discard)
    };
    // A debug pass nobody consumes is culled.
    let debug = graph.create_texture("debug", tex(w, h, RGBA));
    let _ = graph.add_pass("debug_overlay", PassKind::Render).write_texture(
        debug,
        TextureAccess::ColorTarget,
        WriteMode::Discard,
    );
    graph.output(swap1);
    graph.output(history2);
    let compiled = graph.compile()?;
    let names = compiled.ordered_names();
    let pos = |n: &str| names.iter().position(|tx| *tx == n).unwrap_or(usize::MAX);
    assert!(pos("cull") < pos("shadows") && pos("cull") < pos("depth_prepass"));
    assert!(pos("depth_prepass") < pos("cluster_lights") && pos("depth_prepass") < pos("ssao"));
    assert!(
        pos("cluster_lights") < pos("opaque")
            && pos("ssao") < pos("opaque")
            && pos("shadows") < pos("opaque")
    );
    assert!(pos("opaque") < pos("sky") && pos("sky") < pos("taa"));
    assert!(
        pos("taa") < pos("store_history"),
        "history is read before it is overwritten"
    );
    assert!(
        pos("bloom") < pos("bloom_h")
            && pos("bloom_v") < pos("tonemap")
            && pos("tonemap") < pos("ui_composite")
    );
    assert_eq!(names.len(), 14);
    // The blur ping-pong reuses the downsample target.
    assert_eq!(
        compiled.binding(bloom_c.resource()),
        compiled.binding(bloom_a.resource())
    );
    assert_eq!(compiled.culled().len(), 1);
    assert!(
        compiled.aliased_bytes() < compiled.unaliased_bytes(),
        "{} vs {}",
        compiled.aliased_bytes(),
        compiled.unaliased_bytes()
    );
    Ok(())
}
