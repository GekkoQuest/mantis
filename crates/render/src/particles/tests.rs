//! Particle unit tests: WGSL layouts, the range allocator, emission scheduling, and the
//! reference math.

use super::*;
use wgpu::naga;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn wgsl_sizes(source: &str) -> Result<Vec<(String, u32)>, Box<dyn std::error::Error>> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
    let mut layouter = naga::proc::Layouter::default();
    layouter.update(module.to_ctx()).map_err(|e| format!("{e:?}"))?;
    Ok(module
        .types
        .iter()
        .filter_map(|(handle, ty)| Some((ty.name.clone()?, layouter[handle].size)))
        .collect())
}

#[test]
fn mirrors_match_wgsl_layouts() -> TestResult {
    for source in [SIM_WGSL, DRAW_WGSL] {
        let sizes = wgsl_sizes(source)?;
        let size = |name: &str| sizes.iter().find(|(n, _)| n == name).map(|(_, s)| *s as usize);
        assert_eq!(size("Particle"), Some(core::mem::size_of::<GpuParticle>()));
        assert_eq!(size("Emitter"), Some(core::mem::size_of::<GpuEmitter>()));
        assert_eq!(size("EmitterDef"), Some(core::mem::size_of::<GpuEmitterDef>()));
    }
    let sizes = wgsl_sizes(DRAW_WGSL)?;
    assert!(
        sizes
            .iter()
            .any(|(n, s)| n == "ParticleCamera" && *s as usize == core::mem::size_of::<GpuParticleCamera>())
    );
    assert!(core::mem::size_of::<GpuEmitter>() as u64 <= EMITTER_STRIDE);
    Ok(())
}

#[test]
fn allocator_first_fit_and_coalescing() -> TestResult {
    let mut a = RangeAllocator::new(100, 8);
    let r0 = a.allocate(30).ok_or("r0")?;
    let r1 = a.allocate(30).ok_or("r1")?;
    let r2 = a.allocate(30).ok_or("r2")?;
    assert_eq!((r0.clone(), r1.clone(), r2.clone()), (0..30, 30..60, 60..90));
    assert_eq!(a.allocate(11), None, "only 10 left");
    assert_eq!(a.allocate(0), None);
    assert!(a.release(r1.clone()));
    assert!(!a.release(r1.clone()), "double release is refused");
    assert!(!a.release(95..120), "outside the space");
    // First fit: the hole in the middle is used before the tail.
    assert_eq!(a.allocate(10), Some(30..40));
    assert!(a.release(30..40));
    assert!(a.release(r0));
    assert_eq!(a.free_ranges().collect::<Vec<_>>(), vec![0..60, 90..100]);
    assert!(a.release(r2));
    assert_eq!(
        a.free_ranges().collect::<Vec<_>>(),
        vec![0..100],
        "fully coalesced"
    );
    assert_eq!(a.free_slots(), 100);
    assert_eq!(a.allocate(100), Some(0..100));
    Ok(())
}

#[test]
fn allocator_survives_churn() {
    // A deterministic churn: the free space always equals total minus live lengths, and
    // releasing everything coalesces to one range.
    let mut a = RangeAllocator::new(1000, 16);
    let mut live: Vec<core::ops::Range<u32>> = Vec::new();
    let mut state = 12_345u32;
    for _ in 0..2000 {
        state = pcg_hash(state);
        if live.len() < 16 && !state.is_multiple_of(3) {
            if let Some(r) = a.allocate(1 + state % 120) {
                live.push(r);
            }
        } else if !live.is_empty() {
            let r = live.swap_remove(state as usize % live.len());
            assert!(a.release(r));
        }
        let used: u32 = live.iter().map(|r| r.end - r.start).sum();
        assert_eq!(a.free_slots() + used, 1000);
    }
    for r in live.drain(..) {
        assert!(a.release(r));
    }
    assert_eq!(a.free_ranges().collect::<Vec<_>>(), vec![0..1000]);
}

fn schedule(rate: f32, bursts: &[Burst], looping: bool) -> Schedule {
    let mut b = [Burst { time: 0.0, count: 0 }; MAX_BURSTS];
    for (o, s) in b.iter_mut().zip(bursts) {
        *o = *s;
    }
    Schedule {
        capacity: 100,
        duration: 1.0,
        looping,
        rate,
        bursts: b,
        burst_count: bursts.len(),
        lifetime_max: 0.5,
        acceleration: Vec3::ZERO,
        local: false,
        alpha: false,
    }
}

#[test]
fn rate_carries_fractions() -> TestResult {
    let s = schedule(10.0, &[], true);
    let mut e = Emission::started();
    let mut total = 0u32;
    for _ in 0..40 {
        total += e.advance(&s, 0.025).ok_or("live")?.count;
    }
    // One second at 10 per second, give or take float rounding of the carry.
    assert!((9..=10).contains(&total), "{total}");
    Ok(())
}

#[test]
fn bursts_fire_once_per_loop() -> TestResult {
    let s = schedule(
        0.0,
        &[Burst { time: 0.0, count: 5 }, Burst { time: 0.5, count: 7 }],
        true,
    );
    let mut e = Emission::started();
    let mut counts = Vec::new();
    for _ in 0..12 {
        counts.push(e.advance(&s, 0.25).ok_or("live")?.count);
    }
    assert_eq!(counts, vec![5, 0, 7, 0, 5, 0, 7, 0, 5, 0, 7, 0]);
    // The ring start follows the emitted total.
    let w = e.advance(&s, 0.25).ok_or("live")?;
    assert_eq!((w.start, w.base, w.count), (36, 36, 5));
    // A frame longer than a loop emits one loop's bursts.
    let mut long = Emission::started();
    assert_eq!(long.advance(&s, 5.0).ok_or("live")?.count, 12);
    Ok(())
}

#[test]
fn one_shot_finishes_and_frees() -> TestResult {
    let s = schedule(
        0.0,
        &[Burst {
            time: 0.0,
            count: 200,
        }],
        false,
    );
    let mut e = Emission::started();
    let first = e.advance(&s, 0.1).ok_or("live")?;
    assert_eq!(first.count, 100, "capped by capacity");
    // Emission ends at the duration; the instance lives until the youngest particle is
    // older than the longest lifetime (0.5 s after the last spawn).
    let mut frames = 1;
    while e.advance(&s, 0.1).is_some() {
        frames += 1;
        assert!(frames < 100, "never finished");
    }
    // Emission runs the full duration (ten frames); by then the frame-1 spawn is older
    // than the longest lifetime, so the next frame frees the instance.
    assert_eq!(frames, 10);
    // A stopped looping emitter also finishes.
    let looping = schedule(50.0, &[], true);
    let mut l = Emission::started();
    let _ = l.advance(&looping, 0.1).ok_or("live")?;
    l.emitting = false;
    let mut more = 0;
    while l.advance(&looping, 0.1).is_some() {
        more += 1;
    }
    assert_eq!(more, 4);
    Ok(())
}

#[test]
fn sincos_matches_std() {
    for i in 0..=4096u16 {
        let turns = f32::from(i) / 4096.0;
        let (s, c) = sincos_turns(turns);
        let angle = turns * core::f32::consts::TAU;
        assert!((s - angle.sin()).abs() < 2e-6, "sin {turns}");
        assert!((c - angle.cos()).abs() < 2e-6, "cos {turns}");
    }
}

#[test]
fn reference_spawns_and_integrates() {
    let def = GpuEmitterDef {
        drag: 0.0,
        shape: 2,
        shape_a: 0.5f32.cos(),
        shape_b: 0.0,
        speed_min: 2.0,
        speed_max: 2.0,
        lifetime_min: 1.0,
        lifetime_max: 1.0,
        ..GpuEmitterDef::default()
    };
    let emitter = GpuEmitter {
        position: [1.0, 2.0, 3.0],
        dt: 0.1,
        rotation: [0.0, 0.0, 0.0, 1.0],
        capacity: 8,
        spawn_count: 8,
        seed: 7,
        acceleration: [0.0, -10.0, 0.0],
        ..GpuEmitter::default()
    };
    let mut pool = vec![GpuParticle::default(); 8];
    simulate_reference(&mut pool, &emitter, &def);
    for p in &pool {
        assert_eq!(p.position, [1.0, 2.0, 3.0]);
        assert!(p.is_alive());
        let [vx, vy, vz] = p.velocity;
        let speed = (vx * vx + vy * vy + vz * vz).sqrt();
        assert!((speed - 2.0).abs() < 1e-5);
        assert!(vy / speed >= 0.5f32.cos() - 1e-5, "inside the cone");
    }
    let before = pool.clone();
    let integrate = GpuEmitter {
        spawn_count: 0,
        ..emitter
    };
    simulate_reference(&mut pool, &integrate, &def);
    for (p, b) in pool.iter().zip(&before) {
        assert!((p.velocity[1] - (b.velocity[1] - 1.0)).abs() < 1e-5);
        assert!((p.position[1] - (b.position[1] + p.velocity[1] * 0.1)).abs() < 1e-5);
        assert!((p.age - 0.1).abs() < 1e-7);
    }
    // Distinct emission indices give distinct particles.
    let a = spawn_reference(&emitter, &def, 0);
    let b = spawn_reference(&emitter, &def, 1);
    assert_ne!(a.velocity, b.velocity);
    // A reset frame clears the slots it does not spawn into.
    let reset = GpuEmitter {
        spawn_count: 2,
        flags: FLAG_RESET,
        ..emitter
    };
    simulate_reference(&mut pool, &reset, &def);
    assert_eq!(pool.iter().filter(|p| p.is_alive()).count(), 2);
    // A scaled instance spawns the same particle, scaled about the emitter.
    let doubled = GpuEmitter {
        scale: 2.0,
        ..emitter
    };
    let (one, two) = (
        spawn_reference(&emitter, &def, 3),
        spawn_reference(&doubled, &def, 3),
    );
    for ((v2, v1), ((p2, p1), o)) in two
        .velocity
        .iter()
        .zip(one.velocity)
        .zip(two.position.iter().zip(one.position).zip(emitter.position))
    {
        assert!((v2 - 2.0 * v1).abs() < 1e-5);
        assert!((p2 - o - 2.0 * (p1 - o)).abs() < 1e-5);
    }
}
