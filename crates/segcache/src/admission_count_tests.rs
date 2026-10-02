//! Under S3-FIFO, `admission_count` counts the segments labelled
//! `SegmentPool::Admission`. When it is at or above `admission_cap`, an
//! admission-routed insert calls `evict()` before reserving.
//!
//! With no insert in flight, the count equals the number of Admission-labelled
//! segments: the insert whose CAS labels a segment increments it, and
//! `recycle` or `condemn` decrements it once.

use std::sync::Arc;
use std::time::Duration;

use crate::segments::SegmentPool;
use crate::{Policy, Segcache};

const THREADS: usize = 8;
const INSERTS_PER_THREAD: usize = 2_000;
const TRIALS: usize = 20;

fn labelled_admission(cache: &Segcache) -> u32 {
    cache
        .segments
        .iter_headers_for_test()
        .filter(|h| h.pool() == SegmentPool::Admission)
        .count() as u32
}

// Several threads append to the same tail segment, and each insert that
// lands in a segment labelled Main labels it Admission. Two inserts that both
// find the segment labelled Main must not both count it.
#[test]
fn concurrent_inserts_count_each_admission_segment_once() {
    for trial in 0..TRIALS {
        // A ratio of 1.0 sets `admission_cap` to all 1024 segments, and the
        // 16,000 inserts fill about 350, so no insert evicts and every
        // labelled segment stays in service.
        let cache = Arc::new(
            Segcache::builder()
                .segment_size(4096)
                .heap_size(4096 * 1024)
                .hash_power(16)
                .eviction(Policy::S3Fifo {
                    admission_ratio: 1.0,
                })
                .build()
                .unwrap(),
        );
        let writers: Vec<_> = (0..THREADS)
            .map(|t| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    for i in 0..INSERTS_PER_THREAD {
                        let key = format!("t{t:02}k{i:06}");
                        cache
                            .insert(key.as_bytes(), &[7u8; 64], None, Duration::from_secs(3600))
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let counted = cache.segments.admission_count_for_test();
        let labelled = labelled_admission(&cache);
        assert!(labelled > 0, "no segment was labelled admission-pool");
        assert_eq!(
            counted, labelled,
            "trial {trial}: admission_count is {counted} but {labelled} segments are labelled"
        );
    }
}
