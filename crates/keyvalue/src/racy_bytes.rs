//! Relaxed-atomic, word-granular byte access for memory that unpinned
//! readers race.
//!
//! # Why this exists
//!
//! Segcache's read paths verify a key by comparing bytes at a location
//! taken from a hashtable slot, holding no pin. Between the slot load and
//! the comparison, the segment can be drained, recycled, and re-reserved,
//! so the bytes being compared can be **concurrently rewritten** by a
//! writer defining a new item into that space. The engine's protocol is
//! built for that: an unpinned verify is advisory (a wrong answer is
//! caught by the pinned revalidation or the slot re-read), so a torn or
//! garbage read is *handled* — but under the language's memory model a
//! plain read racing a plain write is undefined behavior regardless of
//! whether the value is discarded, and ThreadSanitizer rightly reports it.
//!
//! The fix is to make the race *defined* rather than pretend it cannot
//! happen: every write into memory that an unpinned reader may be
//! examining (item definition into reserved space, relocation copies) and
//! every read on the racy verify path goes through these relaxed atomic
//! helpers. Values seen mid-write are still garbage — by design — but the
//! accesses are atomic, so TSan is silent and the behavior is defined.
//! This is the seqlock reader pattern: read optimistically with atomics,
//! validate before trusting.
//!
//! # Word granularity, and the one residual
//!
//! Overlapping atomic accesses of **different sizes** are not blessed by
//! the language's memory model (std::sync::atomic, "Memory model for
//! atomic accesses": conflicting mixed-size accesses are unsupported),
//! even though they are TSan-silent and byte-coherent on every supported
//! target. To stay inside the model, every helper here operates on
//! aligned `AtomicU64` words only — partial coverage at range edges is
//! handled with byte masks (reads) or read-merge-write (writes), never
//! with byte-sized atomics. That is sound because of two segment-layout
//! invariants the callers uphold (documented per function): item starts
//! are 8-aligned and item extents are whole words, so the containing
//! words of any intra-item range are in-bounds and no OTHER writer shares
//! them — a merge-write cannot clobber a neighbor's concurrent store.
//!
//! One mixed-size pair remains by necessity: delete's tombstone
//! (`ItemHeader::set_deleted`, an `AtomicU8` RMW on the flags byte of a
//! *published* item) can race a verify's `AtomicU64` load of the header
//! word. It cannot be unified: verify races both `set_deleted` (byte)
//! and `define`'s header store (word) — `define` and `set_deleted` never
//! run concurrently on one item (delete requires a published, pinned,
//! tag-valid item), but verify can race either, so no single access size
//! matches both. The pair is byte-coherent on every supported
//! architecture and TSan-silent; it is documented here and at
//! `set_deleted` rather than hidden.
//!
//! # The racy extent is the header + key prefix, not the whole item
//!
//! The unpinned verify reads ONLY the header word(s) and the key bytes —
//! never optional or value bytes (those are read exclusively under a pin
//! after revalidation, on immutable published items). Writers therefore
//! route only the item's RACY PREFIX — `[0, round8(key_end))`, see
//! `RawItem::racy_prefix_len` — through these helpers, and write the
//! remainder with plain (vectorizable) copies: verify's word loads never
//! extend past the rounded prefix boundary, so plain bytes beyond it
//! share no word with any racy load.
//!
//! `Relaxed` is correct throughout: these bytes carry no synchronization
//! of their own. Publication ordering is provided entirely by the
//! hashtable slot CAS (`Release`) against the reader's slot load
//! (`Acquire`), and the drain protocol's pins order reclamation.

use core::sync::atomic::{AtomicU64, Ordering};

/// Round down to the containing word boundary.
#[inline]
fn word_floor(addr: usize) -> usize {
    addr & !7
}

/// Copy `len` bytes from a private, stable `src` into `dst`: the
/// whole-word fast path for relocation copies, where item starts are
/// 8-aligned and item sizes are rounded to 8.
///
/// # Safety
///
/// - `src` must be valid for `len` plain reads and not concurrently
///   written.
/// - `dst` must be 8-aligned, valid for `len` (a multiple of 8) bytes of
///   atomic stores; concurrent *atomic* reads are fine — that is the
///   point — but no concurrent non-atomic access and no other writer.
/// - The ranges must not overlap.
#[inline]
pub unsafe fn racy_copy_words(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(
        (dst as usize).is_multiple_of(8) && len.is_multiple_of(8),
        "racy_copy_words requires an 8-aligned destination and a word-multiple length"
    );
    let mut i = 0;
    while i < len {
        let word = (src.add(i) as *const u64).read_unaligned();
        (*(dst.add(i) as *const AtomicU64)).store(word, Ordering::Relaxed);
        i += 8;
    }
}

/// Store `len` bytes from a private `src` at an arbitrary offset `dst`,
/// word-granular: fully covered words are stored whole; partially covered
/// edge words are read, merged, and stored whole.
///
/// # Safety
///
/// - `src` must be valid for `len` plain reads and not concurrently
///   written; ranges must not overlap.
/// - The CONTAINING WORDS of `[dst, dst + len)` must be in-bounds of one
///   allocation, and no other writer may concurrently store to those
///   words — the read-merge-write would lose its update. Both hold for
///   `define` writing inside one reserved item: the item's words are
///   in-bounds by layout (8-aligned start, word-multiple size) and a
///   reserved item has exactly one writer.
#[inline]
pub unsafe fn racy_store_bytes(src: *const u8, dst: *mut u8, len: usize) {
    if len == 0 {
        return;
    }
    let start = dst as usize;
    let end = start + len;
    let mut w = word_floor(start);
    while w < end {
        let word_ptr = w as *const AtomicU64;
        let cover_from = start.max(w);
        let cover_to = end.min(w + 8);
        if cover_to - cover_from == 8 {
            let word = (src.add(cover_from - start) as *const u64).read_unaligned();
            (*word_ptr).store(word, Ordering::Relaxed);
        } else {
            let mut bytes = (*word_ptr).load(Ordering::Relaxed).to_ne_bytes();
            for (j, b) in bytes.iter_mut().enumerate() {
                let addr = w + j;
                if addr >= cover_from && addr < cover_to {
                    *b = *src.add(addr - start);
                }
            }
            (*word_ptr).store(u64::from_ne_bytes(bytes), Ordering::Relaxed);
        }
        w += 8;
    }
}

/// Compare `expected` against the racy bytes at `a`, loading `a` with
/// aligned relaxed `AtomicU64` words. A concurrent writer makes the
/// result unreliable — the caller's protocol must treat the answer as
/// advisory, exactly as segcache's slot re-read and pinned revalidation
/// already do.
///
/// Little-endian builds take a shift-based fast path (one aligned atomic
/// load and one unaligned plain load per 8 bytes, no per-byte work);
/// other targets use a generic masked per-word compare.
///
/// # Safety
///
/// The CONTAINING WORDS of `[a, a + expected.len())` must be in-bounds
/// of one allocation. In segcache that follows from layout: item starts
/// are 8-aligned and the heap length is a multiple of 8, so any in-heap
/// byte range's containing words are in-heap. Concurrent atomic writes
/// are fine; no concurrent non-atomic write may occur.
#[inline]
pub unsafe fn racy_eq(a: *const u8, expected: &[u8]) -> bool {
    let len = expected.len();
    if len == 0 {
        return true;
    }
    let start = a as usize;

    #[cfg(target_endian = "little")]
    {
        let lo = start & 7;
        let load = |addr: usize| (*(addr as *const AtomicU64)).load(Ordering::Relaxed);
        if len >= 8 {
            // Head: the first word's bytes [lo, 8) against expected[..8-lo].
            let n0 = 8 - lo;
            let w0 = load(word_floor(start));
            let want0 = (expected.as_ptr() as *const u64).read_unaligned();
            if n0 == 8 {
                if w0 != want0 {
                    return false;
                }
            } else {
                let mask = (1u64 << (8 * n0)) - 1;
                if ((w0 >> (8 * lo)) ^ want0) & mask != 0 {
                    return false;
                }
            }
            // Aligned middle: full words, four at a time with an XOR
            // accumulator (no per-word branch — the dependent
            // compare-and-branch per 8 bytes measurably drags on long
            // keys), then the remaining words singly.
            let mut pos = n0;
            while pos + 32 <= len {
                let e = expected.as_ptr().add(pos) as *const u64;
                let acc = (load(start + pos) ^ e.read_unaligned())
                    | (load(start + pos + 8) ^ e.add(1).read_unaligned())
                    | (load(start + pos + 16) ^ e.add(2).read_unaligned())
                    | (load(start + pos + 24) ^ e.add(3).read_unaligned());
                if acc != 0 {
                    return false;
                }
                pos += 32;
            }
            while pos + 8 <= len {
                if load(start + pos) != (expected.as_ptr().add(pos) as *const u64).read_unaligned()
                {
                    return false;
                }
                pos += 8;
            }
            // Tail: bytes [pos, len) against the high bytes of expected's
            // final 8 (in-bounds: len >= 8).
            if pos < len {
                let n = len - pos;
                let got = load(start + pos) & ((1u64 << (8 * n)) - 1);
                let want = (expected.as_ptr().add(len - 8) as *const u64).read_unaligned()
                    >> (8 * (8 - n));
                if got != want {
                    return false;
                }
            }
            true
        } else {
            // len < 8: the range spans at most two words. Assemble
            // expected into one u64 (byte loop, not copy_from_slice — a
            // runtime-length memcpy libcall costs more than the <8
            // iterations on the verify hot path) and the memory bytes
            // likewise.
            let mut want = 0u64;
            for (i, &b) in expected.iter().enumerate() {
                want |= (b as u64) << (8 * i);
            }
            let n0 = 8 - lo;
            let mut got = load(word_floor(start)) >> (8 * lo);
            if len > n0 {
                got |= load(word_floor(start) + 8) << (8 * n0);
            }
            let mask = (1u64 << (8 * len)) - 1;
            (got ^ want) & mask == 0
        }
    }

    #[cfg(not(target_endian = "little"))]
    {
        // Generic per-word masked compare.
        let end = start + len;
        let mut w = word_floor(start);
        while w < end {
            let loaded = (*(w as *const AtomicU64)).load(Ordering::Relaxed);
            let cover_from = start.max(w);
            let cover_to = end.min(w + 8);
            let mut mask = [0u8; 8];
            let mut want = [0u8; 8];
            for j in (cover_from - w)..(cover_to - w) {
                mask[j] = 0xFF;
                want[j] = expected[w + j - start];
            }
            if loaded & u64::from_ne_bytes(mask) != u64::from_ne_bytes(want) {
                return false;
            }
            w += 8;
        }
        true
    }
}

/// Load the 8-aligned word at `p` with one relaxed atomic load — for
/// callers that decode multiple header fields out of a single racy word
/// (`RawItem::verify_key_racy`).
///
/// # Safety
///
/// `p` must be 8-aligned and valid for 8 bytes of atomic load; no
/// concurrent non-atomic write may occur.
#[inline]
pub unsafe fn racy_load_word(p: *const u8) -> u64 {
    debug_assert!((p as usize).is_multiple_of(8));
    (*(p as *const AtomicU64)).load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8-aligned scratch buffer so word-granular edge accesses stay
    /// in-bounds, as segment placement guarantees in production.
    fn aligned(len_words: usize, fill: u8) -> Vec<u64> {
        vec![u64::from_ne_bytes([fill; 8]); len_words]
    }

    fn as_bytes(buf: &[u64]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }
    }

    #[test]
    fn store_and_eq_roundtrip_all_alignments() {
        // Sweep lengths and offsets so full-word, edge-merge, and
        // single-partial-word shapes are all exercised.
        let src: Vec<u8> = (0..64u8).collect();
        for offset in 0..8 {
            for len in 0..48 {
                let mut buf = aligned(10, 0xEE);
                let base = buf.as_mut_ptr() as *mut u8;
                unsafe {
                    racy_store_bytes(src.as_ptr(), base.add(offset), len);
                    assert!(
                        racy_eq(base.add(offset), &src[..len]),
                        "offset={offset} len={len}"
                    );
                }
                let bytes = as_bytes(&buf);
                assert!(bytes[..offset].iter().all(|&b| b == 0xEE), "head clobbered");
                assert_eq!(&bytes[offset..offset + len], &src[..len]);
                assert!(
                    bytes[offset + len..].iter().all(|&b| b == 0xEE),
                    "tail clobbered (offset={offset} len={len})"
                );
            }
        }
    }

    #[test]
    fn copy_words_is_exact() {
        let src: Vec<u8> = (100..164).collect();
        for len in [0usize, 8, 24, 64] {
            let mut buf = aligned(9, 0xEE);
            let base = buf.as_mut_ptr() as *mut u8;
            unsafe { racy_copy_words(src.as_ptr(), base, len) };
            let bytes = as_bytes(&buf);
            assert_eq!(&bytes[..len], &src[..len]);
            assert!(bytes[len..].iter().all(|&b| b == 0xEE));
        }
    }

    #[test]
    fn eq_detects_each_byte_position() {
        let base_data: Vec<u8> = (0..24u8).collect();
        for offset in 0..8 {
            let mut buf = aligned(6, 0);
            let base = buf.as_mut_ptr() as *mut u8;
            unsafe { racy_store_bytes(base_data.as_ptr(), base.add(offset), base_data.len()) };
            for corrupt in 0..base_data.len() {
                unsafe {
                    *base.add(offset + corrupt) ^= 0x80;
                    assert!(
                        !racy_eq(base.add(offset), &base_data),
                        "corruption at byte {corrupt} (offset {offset}) undetected"
                    );
                    *base.add(offset + corrupt) ^= 0x80;
                }
            }
            assert!(unsafe { racy_eq(base.add(offset), &base_data) });
        }
    }

    #[test]
    fn load_word_reads_memory_order() {
        let src: Vec<u8> = (10..26u8).collect();
        let mut buf = aligned(2, 0);
        let base = buf.as_mut_ptr() as *mut u8;
        unsafe { racy_store_bytes(src.as_ptr(), base, src.len()) };
        let w = unsafe { racy_load_word(base) };
        assert_eq!(
            &w.to_ne_bytes(),
            &src[..8],
            "to_ne_bytes must be memory order"
        );
    }
}
