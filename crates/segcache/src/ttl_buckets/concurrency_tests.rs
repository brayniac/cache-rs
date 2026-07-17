//! Multi-threaded tests for the concurrent reserve path.
//!
//! The public `Segcache` API is still `&mut self`; these tests exercise
//! the internal `&self` reserve path directly, which is what item 7
//! will expose.

use crate::segments::SegmentsBuilder;
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
