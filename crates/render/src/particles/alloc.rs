//! A first-fit allocator of contiguous slot ranges with coalescing, allocation-free after
//! construction.

use core::ops::Range;

/// Contiguous ranges of a fixed slot space. Free ranges are kept sorted by start and
/// merged with their neighbors on release, so a fully released space is one range again.
#[derive(Clone, Debug)]
pub struct RangeAllocator {
    total: u32,
    /// Free ranges as (start, length), sorted by start, never adjacent, never empty.
    free: Vec<(u32, u32)>,
    max_ranges: usize,
}

impl RangeAllocator {
    /// A space of `total` slots that holds at most `max_ranges` live allocations. The
    /// free list is reserved up front: allocating and releasing never touch the heap.
    pub fn new(total: u32, max_ranges: usize) -> Self {
        // Live ranges split the free space into at most one more piece than they number.
        let mut free = Vec::with_capacity(max_ranges.saturating_add(2));
        if total > 0 {
            free.push((0, total));
        }
        Self {
            total,
            free,
            max_ranges,
        }
    }

    /// Slots in the space.
    pub fn total(&self) -> u32 {
        self.total
    }

    /// Slots currently free.
    pub fn free_slots(&self) -> u32 {
        self.free.iter().map(|&(_, len)| len).sum()
    }

    /// Free ranges (for diagnostics and tests).
    pub fn free_ranges(&self) -> impl Iterator<Item = Range<u32>> + '_ {
        self.free.iter().map(|&(start, len)| start..start + len)
    }

    /// The lowest-addressed free range of at least `len` slots, or `None` when no free
    /// range is large enough (or `len` is zero).
    pub fn allocate(&mut self, len: u32) -> Option<Range<u32>> {
        if len == 0 {
            return None;
        }
        let at = self.free.iter().position(|&(_, l)| l >= len)?;
        let entry = self.free.get_mut(at)?;
        let start = entry.0;
        if entry.1 == len {
            let _ = self.free.remove(at);
        } else {
            entry.0 += len;
            entry.1 -= len;
        }
        Some(start..start + len)
    }

    /// Returns a range from [`RangeAllocator::allocate`]. Ranges outside the space, empty
    /// ranges, and ranges overlapping free space are ignored (returns `false`).
    pub fn release(&mut self, range: Range<u32>) -> bool {
        let (start, end) = (range.start, range.end);
        if start >= end || end > self.total {
            return false;
        }
        let at = self.free.partition_point(|&(s, _)| s < start);
        let overlaps_prev = at
            .checked_sub(1)
            .and_then(|p| self.free.get(p))
            .is_some_and(|&(s, l)| s + l > start);
        let overlaps_next = self.free.get(at).is_some_and(|&(s, _)| s < end);
        if overlaps_prev || overlaps_next {
            return false;
        }
        let merge_prev = at
            .checked_sub(1)
            .and_then(|p| self.free.get(p))
            .is_some_and(|&(s, l)| s + l == start);
        let merge_next = self.free.get(at).is_some_and(|&(s, _)| s == end);
        match (merge_prev, merge_next) {
            (true, true) => {
                let next_len = self.free.get(at).map_or(0, |&(_, l)| l);
                let _ = self.free.remove(at);
                if let Some(prev) = at.checked_sub(1).and_then(|p| self.free.get_mut(p)) {
                    prev.1 += (end - start) + next_len;
                }
            }
            (true, false) => {
                if let Some(prev) = at.checked_sub(1).and_then(|p| self.free.get_mut(p)) {
                    prev.1 += end - start;
                }
            }
            (false, true) => {
                if let Some(next) = self.free.get_mut(at) {
                    next.0 = start;
                    next.1 += end - start;
                }
            }
            (false, false) => {
                if self.free.len() >= self.max_ranges.saturating_add(2) {
                    // More pieces than live ranges allow: the caller released something it
                    // never allocated. Refuse rather than grow.
                    return false;
                }
                self.free.insert(at, (start, end - start));
            }
        }
        true
    }
}
