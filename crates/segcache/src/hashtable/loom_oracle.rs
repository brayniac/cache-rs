//! Shared loom fixture: a stateful location -> key oracle.
//!
//! # Why this exists
//!
//! [`KeyVerifier`] is the seam between the hashtable's slot protocol and raw
//! storage: the hashtable NEVER touches segment bytes itself, it asks the
//! verifier "does `location` hold `key`?". That makes the verifier the only
//! thing a loom model needs in order to reproduce the hazard the slot
//! protocol actually defends against — **a location whose bytes stopped
//! being this entry's while a thread was looking at them**.
//!
//! The loom models in `table.rs` historically stubbed the verifier with
//! `AlwaysVerifier`, which answers `true` for everything. That is fine for
//! CAS-uniqueness and election-shaped invariants, but it makes every model
//! using it structurally blind to key identity: a location can be relocated,
//! recycled, refilled with somebody else's key, or freed outright and the
//! stub keeps saying "yes, your key is there". Every read path's
//! `verify`-failure branch — the entire STALE-LOCATION guard in
//! `MultiChoiceHashtable::verify_slot` — is dead code under `AlwaysVerifier`.
//!
//! [`KeyOracle`] replaces that stub with model atomics representing
//! "which key currently lives at this location". Raw mmap'd bytes stay
//! entirely outside the model; there is no production hook and nothing to
//! compile out of release builds.
//!
//! # What it can model
//!
//! - **relocation** — the key moves to a new location, the slot is relinked
//!   in place ([`KeyOracle::drain_relocate`]);
//! - **recycle + refill** — the source segment is finalized, recycled, and
//!   rewritten by another writer, so the old location now holds an unrelated
//!   key ([`OTHER`]);
//! - **removal** — the item is freed and the location holds nothing at all
//!   ([`KeyOracle::vacate`]).
//!
//! # Faithfulness rules
//!
//! Models must sequence oracle mutations in the order the real system
//! performs them, or they manufacture states production cannot reach and
//! the resulting "bug" is a fiction. [`KeyOracle::drain_relocate`] encodes
//! the one ordering that matters (copy -> publish -> recycle) so callers
//! cannot get it wrong.

use crate::hashtable::location::Location;
use crate::hashtable::table::MultiChoiceHashtable;
use crate::hashtable::traits::{Hashtable, KeyVerifier};
use crate::sync::{AtomicU64, Ordering};

/// The key every oracle-backed model tracks. Deliberately the same literal
/// the pre-existing `AlwaysVerifier` models use, so models can be converted
/// without re-tuning bucket/stripe assignments.
pub(crate) const KEY: &[u8] = b"key";

/// A key that only ever appears as the OCCUPANT of a recycled location: it
/// is never inserted into the hashtable. Placing it at a cell is how a model
/// says "this segment was finalized, recycled, and rewritten by an unrelated
/// writer".
pub(crate) const OTHER: &[u8] = b"other";

/// Non-zero so a vacant cell (`0`) is distinguishable from an occupied one.
const KEY_ID: u64 = 1;
const OTHER_ID: u64 = 2;

/// Where the subject key starts out.
pub(crate) const SRC: usize = 0;
/// An intermediate location, for models that need two successive drains.
pub(crate) const MID: usize = 1;
/// Where a relocation moves the key to.
pub(crate) const DST: usize = 2;
/// Where a racing writer publishes a replacement copy of the key.
pub(crate) const NEW: usize = 3;

/// Number of distinct storage locations the oracle models. Kept small on
/// purpose: every cell is a loom-tracked atomic.
pub(crate) const NUM_CELLS: usize = 4;

fn key_id(key: &[u8]) -> u64 {
    if key == KEY {
        KEY_ID
    } else if key == OTHER {
        OTHER_ID
    } else {
        // Unknown keys never match an occupied cell (ids start at 1).
        0
    }
}

/// A location -> key map backed by loom-tracked atomics.
///
/// One cell per modeled storage location. `0` means the location holds
/// nothing this model knows about (freed, or recycled and not yet rewritten);
/// otherwise the cell holds the id of the key whose bytes currently live
/// there.
pub(crate) struct KeyOracle {
    cells: [AtomicU64; NUM_CELLS],
}

impl KeyOracle {
    /// All locations start vacant. Seed with [`KeyOracle::place`].
    pub(crate) fn new() -> Self {
        Self {
            cells: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The [`Location`] naming `cell`.
    ///
    /// Offset by one so no cell maps to `Location::new(0)`, and far below
    /// `Location::GHOST` so a real location is never mistaken for the ghost
    /// sentinel.
    pub(crate) fn location(cell: usize) -> Location {
        debug_assert!(cell < NUM_CELLS);
        Location::new(cell as u64 + 1)
    }

    /// Write `key`'s bytes at `cell` — a segment write (reserve + define, or
    /// a merge's `copy_into` destination write).
    ///
    /// `Release`, because in production the bytes must be visible to any
    /// thread that later observes a slot published with this location.
    pub(crate) fn place(&self, cell: usize, key: &[u8]) {
        let id = key_id(key);
        debug_assert!(id != 0, "place() called with a key the oracle cannot name");
        self.cells[cell].store(id, Ordering::Release);
    }

    /// The item at `cell` was freed and the space released: the location now
    /// holds nothing. Models removal (`remove` + segment decrement).
    pub(crate) fn vacate(&self, cell: usize) {
        self.cells[cell].store(0, Ordering::Release);
    }

    /// One merge-drain relocation of [`KEY`], in the order production
    /// performs it (`Segment::copy_into`):
    ///
    /// 1. copy the item into `dst` — its bytes are valid there BEFORE
    ///    anything points at them;
    /// 2. relink the slot with the `Release` CAS, publishing `dst`;
    /// 3. the source segment is finalized, recycled, and rewritten by
    ///    another writer, so `src` now holds an unrelated key.
    ///
    /// Step 3 runs whether or not the relink landed: a lost relink means the
    /// item at `src` was superseded by a racing writer, and the source
    /// segment is recycled all the same.
    ///
    /// Returns whether the relink CAS landed. Models that race the drain
    /// against a mutator must tolerate `false`; models where nothing else
    /// touches the entry should assert `true`.
    pub(crate) fn drain_relocate(&self, ht: &MultiChoiceHashtable, src: usize, dst: usize) -> bool {
        self.place(dst, KEY);
        let relinked = ht.cas_location(KEY, Self::location(src), Self::location(dst), true);
        self.place(src, OTHER);
        relinked
    }

    /// Count the live hashtable entries for [`KEY`] across every modeled
    /// location — the duplicate detector for insert-path models.
    ///
    /// DESTRUCTIVE, and deliberately so: it counts by unlinking. `remove`
    /// matches on tag AND location, so each call removes at most one slot,
    /// and the inner loop catches the pathological case of two slots
    /// published with the same location. Call it once, after every thread
    /// has joined.
    ///
    /// Counting this way keeps the fixture out of `table.rs`'s private
    /// internals — the alternative is a hand-rolled bucket scan, which is
    /// what the `AlwaysVerifier` models copy-paste today.
    pub(crate) fn drain_live_entries(ht: &MultiChoiceHashtable) -> usize {
        let mut live = 0;
        for cell in 0..NUM_CELLS {
            while ht.remove(KEY, Self::location(cell)) {
                live += 1;
            }
        }
        live
    }
}

impl KeyVerifier for KeyOracle {
    /// Answer from the CURRENT occupant of `location`, exactly as
    /// `SegmentsVerifier` answers from the bytes currently at that offset.
    ///
    /// A `false` here therefore carries the same ambiguity as production's:
    /// it may mean "different key", or it may mean "this location stopped
    /// being your entry's while you were asking". Resolving that ambiguity
    /// is what the slot protocol's STALE-LOCATION guard is for, and what
    /// these models exercise.
    fn verify(&self, key: &[u8], location: Location, _allow_deleted: bool) -> bool {
        let raw = location.as_raw();
        if raw == 0 || raw > NUM_CELLS as u64 {
            return false;
        }
        let cell = (raw - 1) as usize;
        let occupant = self.cells[cell].load(Ordering::Acquire);
        occupant != 0 && occupant == key_id(key)
    }
}
