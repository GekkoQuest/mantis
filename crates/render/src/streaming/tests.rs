//! Sector streaming tests against a recording loader.

use super::*;

#[derive(Default)]
struct Recorder {
    next: u64,
    requests: Vec<(RequestId, ContentHash, f32)>,
    cancels: Vec<RequestId>,
    unloads: Vec<ContentHash>,
}

impl SectorLoader for Recorder {
    fn request(&mut self, hash: ContentHash, priority: f32) -> RequestId {
        self.next += 1;
        let id = RequestId(self.next);
        self.requests.push((id, hash, priority));
        id
    }
    fn reprioritize(&mut self, _id: RequestId, _priority: f32) {}
    fn cancel(&mut self, id: RequestId) {
        self.cancels.push(id);
    }
    fn unload(&mut self, hash: ContentHash) {
        self.unloads.push(hash);
    }
}

fn hash(x: i32, z: i32, lod: u8) -> ContentHash {
    let mut b = [0u8; 32];
    let parts = x.to_le_bytes().into_iter().chain(z.to_le_bytes()).chain([lod, 1]);
    for (d, v) in b.iter_mut().zip(parts) {
        *d = v;
    }
    ContentHash::from_bytes(b)
}

/// The grid coordinate and LOD a test hash names.
fn decode(h: &ContentHash) -> ((i32, i32), u8) {
    let b = h.as_bytes();
    let word = |at: usize| {
        let mut w = [0u8; 4];
        for (d, v) in w.iter_mut().zip(b.iter().skip(at)) {
            *d = *v;
        }
        i32::from_le_bytes(w)
    };
    ((word(0), word(4)), b.get(8).copied().unwrap_or(0))
}

/// A 9 x 9 grid of 100-unit sectors centered on the origin.
fn world() -> Vec<SectorEntry> {
    let mut out = Vec::new();
    for x in -4i8..=4 {
        for z in -4i8..=4 {
            let (xi, zi) = (i32::from(x), i32::from(z));
            out.push(SectorEntry {
                coord: (xi, zi),
                lods: [hash(xi, zi, 0), hash(xi, zi, 1), hash(xi, zi, 2), hash(xi, zi, 3)],
                lod_count: 4,
                center: Vec3::new(f32::from(x) * 100.0, 0.0, f32::from(z) * 100.0),
                priority_bias: 0.0,
                lod_distance_scale: 1.0,
            });
        }
    }
    out
}

fn config() -> StreamingConfig {
    StreamingConfig {
        load_radius: 150.0,
        unload_radius: 200.0,
        lookahead: 2.0,
        lod_distances: [50.0, 120.0, 400.0, f32::MAX],
        resident_budget: 64,
        max_in_flight: 64,
        direction_bonus: 0.5,
    }
}

/// Completes every outstanding request successfully.
fn complete_all(s: &mut SectorStreamer, r: &Recorder) {
    for (id, _, _) in &r.requests {
        let _ = s.complete(*id, true);
    }
}

fn loaded(s: &SectorStreamer, coord: (i32, i32)) -> Option<u8> {
    match s.find(coord).and_then(|i| s.state(i)) {
        Some(SectorState::Loaded { lod, .. }) => Some(lod),
        _ => None,
    }
}

#[test]
fn nearest_first_with_lod_by_distance() {
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    let stats = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    // Within 150: the center, 4 neighbors at 100, 4 diagonals at 141.
    assert_eq!(stats.requested, 9);
    let first = r.requests.first().map(|(_, h, _)| *h);
    assert_eq!(
        first,
        Some(hash(0, 0, 0)),
        "the camera's own sector, finest LOD, first"
    );
    assert!(
        r.requests.windows(2).all(|w| matches!(w, [a, b] if a.2 >= b.2)),
        "issued in priority order"
    );
    complete_all(&mut s, &r);
    assert_eq!(loaded(&s, (0, 0)), Some(0));
    assert_eq!(loaded(&s, (1, 0)), Some(1), "100 units away: second LOD");
    assert_eq!(loaded(&s, (1, 1)), Some(2), "141 units away: third LOD");
    assert_eq!(loaded(&s, (2, 0)), None, "200 units away: not wanted");
}

#[test]
fn movement_prefetches_ahead_and_view_breaks_ties() {
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    // Moving +x at 50 units/s: the predicted position is 100 units ahead.
    let _ = s.update(Vec3::ZERO, Vec3::new(50.0, 0.0, 0.0), Vec3::X, &mut r);
    let find = |c: (i32, i32)| {
        r.requests
            .iter()
            .find(|(_, h, _)| decode(h).0 == c)
            .map(|(_, h, p)| (decode(h).1, *p))
    };
    assert!(
        find((2, 0)).is_some(),
        "a sector 200 ahead is within 150 of the predicted position"
    );
    assert!(find((-2, 0)).is_none(), "nothing extra behind");
    assert_eq!(
        find((2, 0)).map(|x| x.0),
        Some(1),
        "LOD from the nearer of now and ahead"
    );
    // Neighbors at the same distance from the camera: ahead outranks behind and beside.
    let prio = |c: (i32, i32)| find(c).map_or(0.0, |x| x.1);
    assert!(prio((1, 0)) > prio((-1, 0)));
    assert!(prio((1, 0)) > prio((0, 1)) && prio((0, 1)) > 0.0);
}

#[test]
fn hysteresis_keeps_edge_sectors_and_leaving_unloads() {
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    let _ = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    complete_all(&mut s, &r);
    // Step 40 units: (−1, 0) is now 140 away — still wanted; (−1, −1) is ~170 away, beyond
    // the load radius but inside the unload radius, so it stays.
    let _ = s.update(Vec3::new(40.0, 0.0, 0.0), Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert!(loaded(&s, (-1, -1)).is_some());
    assert!(r.unloads.is_empty());
    // Far away: everything around the origin unloads.
    let _ = s.update(Vec3::new(1000.0, 0.0, 0.0), Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!(loaded(&s, (0, 0)), None);
    assert!(r.unloads.contains(&hash(0, 0, 0)));
}

#[test]
fn pending_loads_are_cancelled_when_no_longer_wanted() {
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    let _ = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    let issued = r.requests.len();
    let stats = s.update(Vec3::new(1000.0, 0.0, 1000.0), Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!(
        stats.cancelled as usize, issued,
        "every pending request cancelled"
    );
    assert_eq!(r.cancels.len(), issued);
    // Completions for cancelled requests are ignored.
    let (first, _, _) = r
        .requests
        .first()
        .copied()
        .unwrap_or((RequestId(0), ContentHash::ZERO, 0.0));
    assert!(!s.complete(first, true));
}

#[test]
fn approaching_refines_lod_and_failures_retry() {
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    let _ = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    complete_all(&mut s, &r);
    assert_eq!(loaded(&s, (1, 0)), Some(1));
    r.requests.clear();
    let _ = s.update(Vec3::new(90.0, 0.0, 0.0), Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert!(
        r.requests.iter().any(|(_, h, _)| *h == hash(1, 0, 0)),
        "refined LOD requested"
    );
    assert_eq!(
        loaded(&s, (1, 0)),
        Some(1),
        "the coarse LOD stays resident until the fine one arrives"
    );
    complete_all(&mut s, &r);
    assert_eq!(loaded(&s, (1, 0)), Some(0));
    // A failed load returns the sector to unloaded and the next update retries it.
    let mut s = SectorStreamer::new(config(), world());
    let mut r = Recorder::default();
    let _ = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    let (center, _, _) = r
        .requests
        .first()
        .copied()
        .unwrap_or((RequestId(0), ContentHash::ZERO, 0.0));
    assert!(s.complete(center, false));
    r.requests.clear();
    let _ = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!(r.requests.first().map(|(_, h, _)| *h), Some(hash(0, 0, 0)));
}

#[test]
fn budgets_cap_residents_and_requests() {
    let c = StreamingConfig {
        resident_budget: 5,
        max_in_flight: 3,
        ..config()
    };
    let mut s = SectorStreamer::new(c, world());
    let mut r = Recorder::default();
    let stats = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!((stats.requested, stats.in_flight), (3, 3));
    complete_all(&mut s, &r);
    let stats = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!(stats.requested, 2, "fills to the resident budget");
    complete_all(&mut s, &r);
    let stats = s.update(Vec3::ZERO, Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert_eq!((stats.requested, stats.resident), (0, 5));
    // Moving to a corner: the sector under the camera outranks residents left behind and
    // takes a slot from the lowest-priority one.
    let stats = s.update(Vec3::new(100.0, 0.0, 100.0), Vec3::ZERO, Vec3::NEG_Z, &mut r);
    assert!(stats.resident <= 5 && stats.unloaded >= 1, "{stats:?}");
    let under = s.find((1, 1)).and_then(|i| s.state(i));
    assert!(
        matches!(
            under,
            Some(SectorState::Loading { .. } | SectorState::Loaded { .. })
        ),
        "{under:?}"
    );
}
