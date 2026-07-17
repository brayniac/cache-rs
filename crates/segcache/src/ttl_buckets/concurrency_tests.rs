//! Multi-threaded tests for the concurrent reserve path.
//!
//! The public `Segcache` API is still `&mut self`; these tests exercise
//! the internal `&self` reserve path directly, which is what item 7
//! will expose.

use crate::segments::SegmentsBuilder;
use crate::sync::Ordering;
use crate::*;

#[test]
fn concurrent_reserve_smoke() {
    let segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("build segments");
    let buckets = TtlBuckets::new();
    let bucket = buckets.get_bucket(Duration::from_secs(300));

    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                for _ in 0..100 {
                    bucket.reserve(64, &segments).expect("reserve must succeed");
                }
            });
        }
    });
}

/// N threads hammer one bucket with varied-size reservations. Small
/// segments force constant chain-extension elections. Afterward, every
/// invariant the election must preserve is checked from single-threaded
/// code. Eviction never runs here, so `nseg == chain length` holds
/// (drains never decrement nseg).
#[test]
fn concurrent_reserve_stress() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 2_000;
    const SEG_SIZE: i32 = 4096;
    const SEG_COUNT: usize = 4096;

    let segments = SegmentsBuilder::default()
        .segment_size(SEG_SIZE)
        .heap_size(SEG_SIZE as usize * SEG_COUNT)
        .build()
        .expect("build segments");
    let buckets = TtlBuckets::new();
    let bucket = buckets.get_bucket(Duration::from_secs(300));

    // (segment id, offset, size) per successful grant
    let mut grants: Vec<(u32, usize, usize)> = Vec::with_capacity(THREADS * PER_THREAD);

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let segments = &segments;
                s.spawn(move || {
                    let mut local = Vec::with_capacity(PER_THREAD);
                    for i in 0..PER_THREAD {
                        // 40..432 bytes, 8-aligned, varied per thread
                        let size = 40 + 8 * ((t * 31 + i * 17) % 50);
                        let r = bucket
                            .reserve(size, segments)
                            .expect("reserve must succeed");
                        local.push((r.seg().get(), r.offset(), size));
                    }
                    local
                })
            })
            .collect();
        for h in handles {
            grants.extend(h.join().unwrap());
        }
    });

    assert_eq!(grants.len(), THREADS * PER_THREAD);

    // Walk the chain from head via the header links.
    let mut chain: Vec<u32> = Vec::new();
    let mut cursor = bucket.head();
    let mut prev: Option<core::num::NonZeroU32> = None;
    while let Some(id) = cursor {
        chain.push(id.get());
        let header = segments.header(id);
        let meta = header.metadata(Ordering::Acquire);
        // prev/next symmetric
        assert_eq!(meta.prev, prev, "prev link broken at segment {id}");
        // interior segments Sealed, the tail Live
        if meta.next.is_some() {
            assert_eq!(
                meta.state,
                State::Sealed,
                "interior segment {id} not Sealed"
            );
        } else {
            assert_eq!(meta.state, State::Live, "tail segment {id} not Live");
            assert_eq!(bucket.tail(), Some(id), "bucket tail out of sync");
        }
        // the bounded CAS never overshoots
        assert!(header.write_offset() <= SEG_SIZE);
        prev = cursor;
        cursor = meta.next;
    }

    // every segment ever linked is still in the chain (no eviction ran)
    assert_eq!(chain.len() as u32, bucket.nseg());

    // no segment leaked: chain + free queue account for all segments
    assert_eq!(chain.len() + segments.free(), SEG_COUNT);

    // every grant lies in a chain segment; grants within a segment are
    // disjoint and within capacity
    let chain_set: std::collections::HashSet<u32> = chain.iter().copied().collect();
    let mut by_seg: std::collections::HashMap<u32, Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    for (seg, offset, size) in &grants {
        assert!(
            chain_set.contains(seg),
            "grant in segment {seg} outside chain"
        );
        by_seg.entry(*seg).or_default().push((*offset, *size));
    }
    for (seg, mut seg_grants) in by_seg {
        seg_grants.sort_unstable();
        let mut prev_end = 0usize;
        for (offset, size) in seg_grants {
            assert!(offset >= prev_end, "overlapping grants in segment {seg}");
            prev_end = offset + size;
        }
        assert!(
            prev_end <= SEG_SIZE as usize,
            "grants overflow segment {seg}"
        );
    }
}
