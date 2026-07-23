//! Tests for TTL bucket index mapping.

use crate::*;

#[test]
fn bucket_index() {
    let buckets = TtlBuckets::new();
    let index_of = |secs: u32| buckets.get_bucket_index(Duration::from_secs(secs));

    // A non-positive TTL and any TTL past the top of the last tier both
    // saturate at the final bucket (index 1023).
    assert_eq!(index_of(0), 1023);
    assert_eq!(index_of(8_388_608), 1023); // one second past tier-4 top
    assert_eq!(
        buckets.get_bucket_index(Duration::from_secs(u32::MAX)),
        1023
    );

    // Spot-check one representative TTL inside each tier. The expected
    // indices are derived directly from the shift/offset formula the code
    // uses: tier 1 = secs>>3, tier 2 = (secs>>7)+256, tier 3 =
    // (secs>>11)+512, tier 4 = (secs>>15)+768.
    let cases: &[(u32, usize)] = &[
        // tier 1 (each bucket 8s wide, indices 0..255)
        (5, 0),
        (24, 3),
        (999, 124),
        // tier 2 (each bucket 128s wide, indices 256..511)
        (2048, 272),
        (5000, 295),
        // tier 3 (each bucket 2048s wide, indices 512..767)
        (32_768, 528),
        (200_000, 609),
        // tier 4 (each bucket 32768s wide, indices 768..1023)
        (524_288, 784),
        (2_000_000, 829),
    ];
    for &(secs, expected) in cases {
        assert_eq!(index_of(secs), expected, "ttl {secs}s");
    }

    // Every TTL that falls within one bucket's width must resolve to the
    // same index. Walk both edges of a chosen bucket in each tier and
    // confirm they collapse together, then confirm the next second rolls
    // over to the following bucket.
    let width_check = |low: u32, high: u32, next: u32| {
        let idx = index_of(low);
        assert_eq!(index_of(high), idx, "top of bucket ({high}s)");
        assert_eq!(index_of(next), idx + 1, "rollover into next bucket");
    };
    // tier 1 bucket 40 spans [320, 327]; 328 opens bucket 41
    width_check(320, 327, 328);
    // tier 2 bucket at index 300 spans [5632, 5759]; 5760 opens the next
    width_check(5632, 5759, 5760);
    // tier 3 bucket at index 600 spans [180224, 182271]; 182272 opens next
    width_check(180_224, 182_271, 182_272);

    // Exercise the exact boundaries where one tier hands off to the next.
    // The last TTL of a tier and the first TTL of the following tier must
    // land in adjacent-but-distinct index ranges.
    assert_eq!(index_of(2_047), 255); // last tier-1 bucket
    assert_eq!(index_of(2_048), 272); // first populated tier-2 bucket
    assert_eq!(index_of(32_767), 511); // last tier-2 bucket
    assert_eq!(index_of(32_768), 528); // first populated tier-3 bucket
    assert_eq!(index_of(524_287), 767); // last tier-3 bucket
    assert_eq!(index_of(524_288), 784); // first populated tier-4 bucket
    assert_eq!(index_of(8_388_607), 1023); // top of tier 4
}
