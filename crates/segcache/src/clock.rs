//! The cache's notion of the current time.
//!
//! Every expiry decision reads this. In production it is the coarse
//! monotonic clock and this module compiles away to exactly that call.
//!
//! A trace replay needs it to follow the trace instead. A replay consumes
//! hours of recorded time in seconds of wall clock, so a TTL shorter than
//! the run never fires and expiry is measured as though it did not exist.
//! Measured on one trace: 925 seconds of recorded time replayed in 28, so a
//! TTL of a minute fired a fraction of the times it would in production.
//!
//! This exists so a comparison against another engine can be fair. Driving
//! only one side's clock is as wrong as driving neither: the engine that
//! honours expiry loses the hits it correctly discards, while the engine
//! still on wall time keeps serving them.
//!
//! # Why the override is per thread
//!
//! The cache does its work on the caller's thread, so the thread driving a
//! replay is the thread whose expiry decisions matter, and scoping the
//! override to it is the tighter blast radius. A process-global version
//! reaches unrelated threads -- in a sibling codebase it made four tests
//! across two modules flaky, because test harnesses run tests in parallel
//! and one test setting the clock changed what the others saw.
//!
//! Gated behind `virtual-clock`: with the feature off there is no lookup,
//! no branch, and no difference from calling the clock directly.
//!
//! Also compiled into this crate's own unit tests, feature or not, so a test
//! in `src/` can move time instead of sleeping through it and a plain
//! `cargo test` runs it. That reaches unit tests only: an integration test
//! under `tests/` links the library built without `cfg(test)`, and still
//! needs the feature.

use clocksource::coarse::Instant;

#[cfg(any(feature = "virtual-clock", test))]
use clocksource::coarse::Duration;
#[cfg(any(feature = "virtual-clock", test))]
use core::cell::Cell;

#[cfg(any(feature = "virtual-clock", test))]
thread_local! {
    /// The instant to report on this thread instead of the monotonic clock.
    ///
    /// `Option` rather than a sentinel value: every `Instant` is a legitimate
    /// reading, so there is no value left over to mean "not set".
    static VIRTUAL_NOW: Cell<Option<Instant>> = const { Cell::new(None) };

    /// Where trace seconds were pinned onto the monotonic clock: the real
    /// instant at the first `set_virtual_now`, and the trace second it
    /// carried.
    ///
    /// `coarse::Instant` is deliberately opaque -- it has no constructor
    /// from a seconds count in this version -- so a trace second becomes an
    /// instant by offset from an anchor rather than by conversion.
    static ANCHOR: Cell<Option<(Instant, u32)>> = const { Cell::new(None) };
}

/// The current instant, as every expiry decision sees it.
#[inline]
pub fn now() -> Instant {
    #[cfg(any(feature = "virtual-clock", test))]
    {
        if let Some(instant) = VIRTUAL_NOW.with(Cell::get) {
            return instant;
        }
    }
    Instant::now()
}

/// Drive this thread's expiry from a caller-supplied seconds count.
///
/// The value need not share an origin with the system clock: the cache only
/// compares it against expiries derived from it, so any monotonic seconds
/// count works, which is what a trace's timestamps are.
/// Traces are not perfectly ordered, and `Instant` is documented as
/// monotonically nondecreasing, so a second earlier than the anchor holds
/// the clock still rather than moving it back. Letting it move back would
/// un-expire items that an earlier record had already aged out.
#[cfg(any(feature = "virtual-clock", test))]
pub fn set_virtual_now(secs: u32) {
    let (base, base_secs) = ANCHOR.with(|anchor| match anchor.get() {
        Some(pinned) => pinned,
        None => {
            let pinned = (Instant::now(), secs);
            anchor.set(Some(pinned));
            pinned
        }
    });
    let elapsed = secs.saturating_sub(base_secs);
    VIRTUAL_NOW.with(|now| now.set(Some(base + Duration::from_secs(elapsed))));
}

/// Restore the system clock on this thread.
#[cfg(any(feature = "virtual-clock", test))]
pub fn clear_virtual_now() {
    VIRTUAL_NOW.with(|now| now.set(None));
    ANCHOR.with(|anchor| anchor.set(None));
}

/// The virtual time in force on this thread, or `None` for the real clock.
#[cfg(any(feature = "virtual-clock", test))]
pub fn virtual_now() -> Option<Instant> {
    VIRTUAL_NOW.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The override must be reachable from a unit test without the feature,
    /// or the `cfg(test)` half of the gate is doing nothing.
    #[test]
    fn a_unit_test_drives_the_clock_without_the_feature() {
        set_virtual_now(1_000);
        let start = now();
        set_virtual_now(1_060);
        assert_eq!(now(), start + Duration::from_secs(60));

        clear_virtual_now();
        assert!(
            virtual_now().is_none(),
            "clearing must restore the real clock"
        );
    }
}
