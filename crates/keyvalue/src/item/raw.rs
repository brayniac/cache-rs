//! Field accessors over an item stored as a packed byte buffer.
//!
//! The [`RawItem`] provides direct byte-level access to item data stored as
//! a packed buffer of `[ItemHeader][optional][key][value]`.
//!
//! Numeric items (`Value::U64`) use an extended, 8-aligned value slot:
//! `[ItemHeader][optional][key][pad][value: u64][version: u64]`, where the
//! derived pad brings the value to an 8-byte boundary. Both words are
//! accessed atomically, and in-place updates run under a seqlock on the
//! version word so that the value and the item CRC stay consistent for
//! concurrent readers. The version also feeds CAS-token construction: every
//! in-place update bumps it, so tokens observe increments (matching
//! memcached, where incr/decr assign a fresh cas unique).

use crate::item::*;
use crate::NotNumericError;
use crate::Value;
use core::sync::atomic::{fence, AtomicU64, Ordering};

/// A cursor over an item's packed bytes, addressed through a raw pointer.
///
/// `RawItem` does not own or validate the bytes it points at; the caller
/// must guarantee the pointer targets a properly aligned, live item buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawItem {
    data: *mut u8,
}

impl RawItem {
    /// Wrap a raw pointer as a `RawItem`.
    ///
    /// # Safety
    ///
    /// The pointer must point to a valid item buffer with a properly
    /// initialized [`ItemHeader`]. Undefined behavior results from
    /// passing an invalid or misaligned pointer. Numeric items
    /// additionally require the buffer to start 8-byte aligned (which
    /// segment placement guarantees) so that the value and version
    /// words are atomically addressable.
    pub fn from_ptr(ptr: *mut u8) -> RawItem {
        Self { data: ptr }
    }

    /// View the item's header without copying it.
    pub fn header(&self) -> &ItemHeader {
        unsafe { &*(self.data as *const ItemHeader) }
    }

    /// Raw pointer to the item's header, for in-place field mutation.
    fn header_mut(&mut self) -> *mut ItemHeader {
        self.data as *mut ItemHeader
    }

    /// Length of the stored key, in bytes.
    #[inline]
    pub fn klen(&self) -> u8 {
        self.header().klen()
    }

    /// Slice view over the stored key bytes.
    pub fn key(&self) -> &[u8] {
        unsafe {
            let ptr = self.data.add(self.key_offset());
            let len = self.klen() as usize;
            std::slice::from_raw_parts(ptr, len)
        }
    }

    /// Number of value bytes recorded in the header.
    #[inline]
    fn vlen(&self) -> u32 {
        self.header().vlen()
    }

    /// Atomic view of the numeric value word.
    ///
    /// # Safety
    ///
    /// Caller must ensure the item is numeric: the value slot is then
    /// 8-aligned by construction (aligned item start + derived pad).
    #[inline]
    unsafe fn value_word(&self) -> &AtomicU64 {
        &*(self.data.add(self.value_offset()) as *const AtomicU64)
    }

    /// Atomic view of the numeric version word (seqlock).
    ///
    /// # Safety
    ///
    /// Caller must ensure the item is numeric.
    #[inline]
    unsafe fn version_word(&self) -> &AtomicU64 {
        &*(self.data.add(self.value_offset() + 8) as *const AtomicU64)
    }

    /// Borrow the value, returning either bytes or a decoded u64.
    ///
    /// Numeric values are read with a seqlock so a concurrent in-place
    /// update can never be observed torn. The retry loop is degenerate
    /// today (all mutation is serialized by `&mut` at the cache level)
    /// and becomes load-bearing when readers go concurrent. Note that
    /// loom cannot verify seqlock orderings (no SC total order in its
    /// model) — the protocol shape is pinned by unit tests instead.
    pub fn value(&self) -> Value<'_> {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked; slot aligned by construction.
            let (value_word, version_word) = unsafe { (self.value_word(), self.version_word()) };
            loop {
                let v1 = version_word.load(Ordering::Acquire);
                if v1 & 1 == 1 {
                    // write in progress
                    std::hint::spin_loop();
                    continue;
                }
                let value = value_word.load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                let v2 = version_word.load(Ordering::Relaxed);
                if v1 == v2 {
                    return Value::U64(value);
                }
            }
        } else {
            let bytes = unsafe {
                let ptr = self.data.add(self.value_offset());
                let len = self.vlen() as usize;
                std::slice::from_raw_parts(ptr, len)
            };
            Value::Bytes(bytes)
        }
    }

    /// Current seqlock version of a numeric item, for CAS-token
    /// construction. Every in-place update bumps this by two, so tokens
    /// built from it observe increments. A racing read (odd or stale
    /// version) only produces a token that is already stale — a spurious
    /// CAS failure, the safe direction.
    #[inline]
    pub fn numeric_version(&self) -> Option<u64> {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked.
            Some(unsafe { self.version_word() }.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    /// Length of the item's optional data segment, in bytes.
    #[inline]
    pub fn olen(&self) -> u8 {
        self.header().olen()
    }

    /// Slice view over the optional data, or `None` when there is none.
    pub fn optional(&self) -> Option<&[u8]> {
        let olen = self.olen() as usize;
        if olen > 0 {
            unsafe {
                let ptr = self.data.add(self.optional_offset());
                Some(std::slice::from_raw_parts(ptr, olen))
            }
        } else {
            None
        }
    }

    /// Assert the header's magic sentinel is intact.
    #[inline]
    pub fn check_magic(&self) {
        self.header().check_magic()
    }

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.header().is_deleted()
    }

    pub fn set_deleted(&mut self, deleted: bool) {
        unsafe { (*self.header_mut()).set_deleted(deleted) }
    }

    /// Populate the item's header and copy in the key, value, and optional
    /// bytes.
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        unsafe {
            (*self.header_mut()).init();
            (*self.header_mut()).set_olen(optional.len() as u8);
            (*self.header_mut()).set_klen(key.len() as u8);

            // Copy optional data
            std::ptr::copy_nonoverlapping(
                optional.as_ptr(),
                self.data.add(self.optional_offset()),
                optional.len(),
            );

            // Copy key
            std::ptr::copy_nonoverlapping(
                key.as_ptr(),
                self.data.add(self.key_offset()),
                key.len(),
            );

            // Copy value
            match value {
                Value::Bytes(v) => {
                    (*self.header_mut()).set_numeric(false);
                    (*self.header_mut()).set_vlen(v.len() as u32);
                    std::ptr::copy_nonoverlapping(
                        v.as_ptr(),
                        self.data.add(self.value_offset()),
                        v.len(),
                    );
                }
                Value::U64(v) => {
                    (*self.header_mut()).set_numeric(true);
                    (*self.header_mut()).set_vlen(8);

                    // Zero the derived alignment pad between the key and
                    // the value slot (deterministic bytes for the CRC).
                    let pad = numeric_value_pad(key.len(), optional.len());
                    if pad > 0 {
                        std::ptr::write_bytes(self.data.add(self.key_offset() + key.len()), 0, pad);
                    }

                    // The item is not yet published, so plain-vs-atomic
                    // ordering is moot; use atomic stores for uniformity
                    // with the seqlock protocol. Native-endian.
                    self.value_word().store(v, Ordering::Relaxed);
                    self.version_word().store(0, Ordering::Relaxed);
                }
            }

            // Compute and store the CRC32.
            #[cfg(feature = "integrity")]
            {
                let crc = self.compute_crc();
                (*self.header_mut()).set_crc32(crc);
            }
        }
    }

    /// Wrapping in-place addition on a numeric value, returning the new
    /// value. The write runs under the item's seqlock: the version goes
    /// odd (staling any outstanding CAS token — fail-safe ordering,
    /// before the value moves), the value and CRC are updated, and the
    /// version lands even. The CRC therefore covers the value at all
    /// times, and concurrent seqlock readers can never observe a torn
    /// value/CRC pair.
    pub fn fetch_wrapping_add(&self, rhs: u64) -> Result<u64, NotNumericError> {
        self.seqlocked_update(|v| v.wrapping_add(rhs))
    }

    /// Saturating in-place subtraction on a numeric value, returning the
    /// new value. See [`Self::fetch_wrapping_add`] for the protocol.
    pub fn fetch_saturating_sub(&self, rhs: u64) -> Result<u64, NotNumericError> {
        self.seqlocked_update(|v| v.saturating_sub(rhs))
    }

    fn seqlocked_update(&self, op: impl Fn(u64) -> u64) -> Result<u64, NotNumericError> {
        if !self.header().is_numeric() {
            return Err(NotNumericError);
        }

        // SAFETY: is_numeric checked; slot aligned by construction.
        let (value_word, version_word) = unsafe { (self.value_word(), self.version_word()) };

        // Seqlock write. Writers are serialized externally (today by
        // `&mut` at the cache level; later by the segment reader-pin
        // protocol, which also excludes eviction byte-copies while the
        // pin is held).
        version_word.fetch_add(1, Ordering::AcqRel); // odd: write in progress
        let new = op(value_word.load(Ordering::Relaxed));
        value_word.store(new, Ordering::Relaxed);
        #[cfg(feature = "integrity")]
        {
            let crc = self.compute_crc_numeric(new);
            self.crc_word().store(crc, Ordering::Relaxed);
        }
        version_word.fetch_add(1, Ordering::Release); // even: stable
        Ok(new)
    }

    /// Atomic view of the header CRC field.
    ///
    /// The header is `repr(C, packed)`, so this goes through pointer
    /// arithmetic (the CRC is the trailing 4 bytes of the header, at
    /// item offset 8 — 4-aligned given 8-aligned item starts), never a
    /// field reference.
    #[cfg(feature = "integrity")]
    #[inline]
    fn crc_word(&self) -> &core::sync::atomic::AtomicU32 {
        unsafe { &*(self.data.add(ITEM_HDR_SIZE - 4) as *const core::sync::atomic::AtomicU32) }
    }

    /// Verify the item's CRC32 integrity.
    ///
    /// Returns `true` if the stored CRC matches a freshly computed one.
    /// Numeric items are checked under the seqlock so a concurrent
    /// in-place update is never misreported as corruption.
    #[cfg(feature = "integrity")]
    pub fn check_integrity(&self) -> bool {
        if self.header().is_numeric() {
            // SAFETY: is_numeric checked.
            let version_word = unsafe { self.version_word() };
            loop {
                let v1 = version_word.load(Ordering::Acquire);
                if v1 & 1 == 1 {
                    std::hint::spin_loop();
                    continue;
                }
                let value = unsafe { self.value_word() }.load(Ordering::Relaxed);
                let stored = self.crc_word().load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                let v2 = version_word.load(Ordering::Relaxed);
                if v1 == v2 {
                    return stored == self.compute_crc_numeric(value);
                }
            }
        } else {
            self.header().crc32() == self.compute_crc()
        }
    }

    /// Compute CRC32 over the item with the CRC field zeroed.
    ///
    /// For numeric items this covers the header, optional, key, pad, and
    /// the value word — but NOT the version word, which is seqlock
    /// protocol state (corrupting it can only cause spurious CAS-token
    /// mismatches, never silent data corruption).
    #[cfg(feature = "integrity")]
    fn compute_crc(&self) -> u32 {
        if self.header().is_numeric() {
            let value = unsafe { self.value_word() }.load(Ordering::Relaxed);
            self.compute_crc_numeric(value)
        } else {
            self.compute_crc_span(self.value_offset() + self.vlen() as usize)
        }
    }

    /// Numeric CRC: hash up to the value slot from the buffer, then the
    /// value from a caller-supplied snapshot (an atomic load), so the
    /// computation never does a plain read of the concurrently-updated
    /// word.
    #[cfg(feature = "integrity")]
    fn compute_crc_numeric(&self, value: u64) -> u32 {
        let crc_field_size = std::mem::size_of::<u32>();
        let crc_field_offset = ITEM_HDR_SIZE - crc_field_size;

        let mut hasher = crc32fast::Hasher::new();
        unsafe {
            // header before the CRC field
            hasher.update(std::slice::from_raw_parts(self.data, crc_field_offset));
            // CRC field treated as zeros
            hasher.update(&[0u8; 4]);
            // optional + key + pad (immutable after define)
            let after_offset = crc_field_offset + crc_field_size;
            let value_offset = self.value_offset();
            if value_offset > after_offset {
                hasher.update(std::slice::from_raw_parts(
                    self.data.add(after_offset),
                    value_offset - after_offset,
                ));
            }
        }
        // the value word, from the snapshot (native-endian bytes)
        hasher.update(&value.to_ne_bytes());
        hasher.finalize()
    }

    /// Bytes-item CRC over `[0, end)` with the CRC field zeroed.
    #[cfg(feature = "integrity")]
    fn compute_crc_span(&self, end: usize) -> u32 {
        let crc_field_size = std::mem::size_of::<u32>();
        let crc_field_offset = ITEM_HDR_SIZE - crc_field_size;

        let mut hasher = crc32fast::Hasher::new();
        unsafe {
            let before = std::slice::from_raw_parts(self.data, crc_field_offset);
            hasher.update(before);
            hasher.update(&[0u8; 4]);
            let after_offset = crc_field_offset + crc_field_size;
            if end > after_offset {
                let after =
                    std::slice::from_raw_parts(self.data.add(after_offset), end - after_offset);
                hasher.update(after);
            }
        }
        hasher.finalize()
    }

    // -- Offset calculations --

    #[inline]
    fn optional_offset(&self) -> usize {
        ITEM_HDR_SIZE
    }

    #[inline]
    fn key_offset(&self) -> usize {
        self.optional_offset() + self.olen() as usize
    }

    #[inline]
    fn value_offset(&self) -> usize {
        let unpadded = self.key_offset() + self.klen() as usize;
        if self.header().is_numeric() {
            unpadded + numeric_value_pad(self.klen() as usize, self.olen() as usize)
        } else {
            unpadded
        }
    }

    /// Returns item size, rounded up to 8-byte alignment. Numeric items
    /// include the alignment pad and the seqlock version word.
    pub fn size(&self) -> usize {
        let klen = self.klen() as usize;
        let olen = self.olen() as usize;
        let extra = if self.header().is_numeric() {
            numeric_value_pad(klen, olen) + 8
        } else {
            0
        };
        let raw = ITEM_HDR_SIZE + olen + klen + extra + self.vlen() as usize;
        ((raw >> 3) + 1) << 3
    }
}

impl std::fmt::Debug for RawItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("RawItem")
            .field("size", &self.size())
            .field("header", self.header())
            .field(
                "raw",
                &format!("{:02X?}", unsafe {
                    &std::slice::from_raw_parts(self.data, self.size())
                }),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8-aligned scratch buffer, as segment placement guarantees.
    fn aligned_buf(len_words: usize) -> Vec<u64> {
        vec![0u64; len_words]
    }

    fn define_numeric(buf: &mut [u64], key: &[u8], value: u64, optional: &[u8]) -> RawItem {
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(key, Value::U64(value), optional);
        raw
    }

    #[test]
    fn numeric_slot_alignment_sweep() {
        let mut buf = aligned_buf(128);
        let base = buf.as_ptr() as usize;
        let key = [0xAAu8; 255];
        let opt = [0xBBu8; 63];

        for klen in 1..=255usize {
            for olen in [0usize, 1, 7, 63] {
                let raw = define_numeric(&mut buf, &key[..klen], 42, &opt[..olen]);
                let value_addr = base + raw.value_offset();
                assert_eq!(
                    value_addr % 8,
                    0,
                    "value misaligned for klen={klen} olen={olen}"
                );
                assert_eq!((value_addr + 8) % 8, 0);
                assert_eq!(raw.value(), Value::U64(42));
                // size helper and instance size agree
                assert_eq!(raw.size(), item_size(klen, &Value::U64(42), olen));
            }
        }
    }

    #[test]
    fn bytes_items_unpadded() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"value"), b"");
        // bytes layout is exactly header + key + value
        assert_eq!(raw.size(), item_size(3, &Value::Bytes(b"value"), 0));
        assert_eq!(raw.size(), (((ITEM_HDR_SIZE + 3 + 5) >> 3) + 1) << 3);
        assert_eq!(raw.value(), Value::Bytes(b"value"));
    }

    #[test]
    fn seqlocked_ops() {
        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 5, b"");

        assert_eq!(raw.numeric_version(), Some(0));

        // each op bumps the version by exactly two (odd transient state)
        assert_eq!(raw.fetch_wrapping_add(1), Ok(6));
        assert_eq!(raw.numeric_version(), Some(2));
        assert_eq!(raw.value(), Value::U64(6));

        assert_eq!(raw.fetch_saturating_sub(2), Ok(4));
        assert_eq!(raw.numeric_version(), Some(4));

        // wrap at the 64-bit mark (memcached incr semantics)
        assert_eq!(raw.fetch_wrapping_add(u64::MAX - 3), Ok(0));

        // saturate at zero (memcached decr semantics)
        assert_eq!(raw.fetch_saturating_sub(100), Ok(0));
    }

    #[test]
    fn non_numeric_ops_error() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"text"), b"");
        assert_eq!(raw.fetch_wrapping_add(1), Err(NotNumericError));
        assert_eq!(raw.fetch_saturating_sub(1), Err(NotNumericError));
        assert_eq!(raw.numeric_version(), None);
    }

    #[test]
    fn pad_bytes_zeroed() {
        let mut buf = aligned_buf(64);
        // pollute the buffer first
        for w in buf.iter_mut() {
            *w = u64::MAX;
        }
        let raw = define_numeric(&mut buf, b"k", 1, b"");
        let pad = numeric_value_pad(1, 0);
        if pad > 0 {
            let start = raw.key_offset() + 1;
            let bytes = unsafe { std::slice::from_raw_parts(raw.data.add(start), pad) };
            assert!(bytes.iter().all(|&b| b == 0), "pad not zeroed: {bytes:?}");
        }
    }

    #[cfg(feature = "integrity")]
    #[test]
    fn crc_covers_numeric_value_across_increments() {
        let mut buf = aligned_buf(64);
        let raw = define_numeric(&mut buf, b"counter", 5, b"opt");
        assert!(raw.check_integrity());

        // the CRC is updated under the seqlock on every increment
        raw.fetch_wrapping_add(1).unwrap();
        assert!(raw.check_integrity());
        raw.fetch_saturating_sub(2).unwrap();
        assert!(raw.check_integrity());

        // corrupting the VALUE is detected (full coverage — the
        // requirement that forced the seqlock design)
        let value_off = raw.value_offset();
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(!raw.check_integrity());
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(raw.check_integrity());

        // corrupting key or optional is detected
        let key_off = raw.key_offset();
        unsafe { *raw.data.add(key_off) ^= 0xFF };
        assert!(!raw.check_integrity());
        unsafe { *raw.data.add(key_off) ^= 0xFF };
        assert!(raw.check_integrity());

        // the version word is protocol state, excluded from coverage:
        // corrupting it can only cause spurious CAS-token mismatches
        let version_off = raw.value_offset() + 8;
        unsafe { *raw.data.add(version_off) ^= 0x02 };
        assert!(raw.check_integrity());
    }

    #[cfg(feature = "integrity")]
    #[test]
    fn crc_covers_bytes_value() {
        let mut buf = aligned_buf(64);
        let mut raw = RawItem::from_ptr(buf.as_mut_ptr() as *mut u8);
        raw.define(b"key", Value::Bytes(b"value"), b"");
        assert!(raw.check_integrity());
        let value_off = raw.value_offset();
        unsafe { *raw.data.add(value_off) ^= 0xFF };
        assert!(!raw.check_integrity());
    }
}
