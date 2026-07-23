//! Item header with byte-aligned field layout.
//!
//! Each item in a segment begins with this header, followed by optional data,
//! key bytes, and value bytes.
//!
//! ```text
//! Base layout — 6 bytes, `integrity` feature off:
//!   offset : field : width : notes
//!        0 : klen   :   1   : key length (u8)
//!        1 : flags  :   1   : bit-packed, see below
//!        2 : vlen   :   4   : value length (u32)
//!
//! Integrity layout — 12 bytes, `integrity` feature on:
//!   offset : field  : width : notes
//!        0 : magic  :   2   : sentinel bytes 0x4B, 0x56 ("KV")
//!        2 : klen   :   1   : key length (u8)
//!        3 : flags  :   1   : bit-packed, see below
//!        4 : vlen   :   4   : value length (u32)
//!        8 : crc32  :   4   : checksum, see coverage note below
//!
//! flags byte, bit 7 down to bit 0:
//!   [ numeric:1 | deleted:1 | olen:6 ]
//!   - numeric (bit 7): value is a packed integer rather than raw bytes
//!   - deleted (bit 6): tombstone marker
//!   - olen (bits 5-0): length of the optional data segment, 0-63
//!
//! crc32 coverage: computed over the entire item — magic, klen, flags,
//! vlen, optional data, key, and value — with the crc32 field itself
//! held at zero while the checksum is calculated.
//! ```

/// Byte width of `ItemHeader`, derived from its (possibly
/// integrity-extended) field layout.
pub const ITEM_HDR_SIZE: usize = std::mem::size_of::<ItemHeader>();

/// Magic sentinel bytes for integrity checking: the ASCII bytes "KV".
#[cfg(feature = "integrity")]
pub const ITEM_MAGIC: [u8; 2] = [0x4B, 0x56];

/// Size of the integrity fields (magic + CRC32) when the feature is enabled.
#[cfg(feature = "integrity")]
pub const ITEM_INTEGRITY_SIZE: usize = 2 + 4; // magic(2) + crc32(4)

#[cfg(not(feature = "integrity"))]
#[allow(dead_code)]
pub const ITEM_INTEGRITY_SIZE: usize = 0;

// Flag masks within the `flags` byte.
const NUMERIC_MASK: u8 = 0b1000_0000;
const DELETE_MASK: u8 = 0b0100_0000;
const OLEN_MASK: u8 = 0b0011_1111;

/// Packed item header stored at the start of each item.
///
/// Base layout: `[klen:1][flags:1][vlen:4]` = 6 bytes.
/// With `integrity`: `[magic:2][klen:1][flags:1][vlen:4][crc32:4]` = 12 bytes.
///
/// All fields are directly byte-addressable — no cross-word bit manipulation.
#[repr(C, packed)]
pub struct ItemHeader {
    #[cfg(feature = "integrity")]
    magic: [u8; 2],
    klen: u8,
    flags: u8,
    vlen: u32,
    #[cfg(feature = "integrity")]
    crc32: u32,
}

// Verify expected sizes at compile time.
#[cfg(not(feature = "integrity"))]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 6);
#[cfg(feature = "integrity")]
const _: () = assert!(std::mem::size_of::<ItemHeader>() == 12);

impl ItemHeader {
    /// Initialize header fields to zero (and set magic if enabled).
    pub fn init(&mut self) {
        self.klen = 0;
        self.flags = 0;
        self.vlen = 0;
        #[cfg(feature = "integrity")]
        {
            self.magic = ITEM_MAGIC;
            self.crc32 = 0;
        }
    }

    /// Assert that this header's magic bytes still match the sentinel
    /// value written by `init`.
    ///
    /// # Panics
    /// A mismatch means the header has been corrupted, so this asserts rather
    /// than returning a `Result`.
    pub fn check_magic(&self) {
        #[cfg(feature = "integrity")]
        {
            let magic = self.magic;
            assert_eq!(
                magic, ITEM_MAGIC,
                "item magic mismatch: expected {:02X?}, got {:02X?}",
                ITEM_MAGIC, magic,
            );
        }
    }

    /// Store the CRC32 value in the header.
    #[cfg(feature = "integrity")]
    pub fn set_crc32(&mut self, crc: u32) {
        self.crc32 = crc;
    }

    /// Get the stored CRC32 value.
    #[cfg(feature = "integrity")]
    pub fn crc32(&self) -> u32 {
        self.crc32
    }

    // -- Key length --

    #[inline]
    pub fn klen(&self) -> u8 {
        self.klen
    }

    #[inline]
    pub fn set_klen(&mut self, klen: u8) {
        self.klen = klen;
    }

    // -- Value length --

    #[inline]
    pub fn vlen(&self) -> u32 {
        self.vlen
    }

    #[inline]
    pub fn set_vlen(&mut self, vlen: u32) {
        self.vlen = vlen;
    }

    // -- Optional data length (6 bits, max 63) --

    #[inline]
    pub fn olen(&self) -> u8 {
        self.flags & OLEN_MASK
    }

    #[inline]
    pub fn set_olen(&mut self, olen: u8) {
        debug_assert!(olen <= OLEN_MASK, "olen exceeds 6-bit max (63)");
        self.flags = (self.flags & !OLEN_MASK) | (olen & OLEN_MASK);
    }

    // -- Numeric flag --

    #[inline]
    pub fn is_numeric(&self) -> bool {
        self.flags & NUMERIC_MASK != 0
    }

    #[inline]
    pub fn set_numeric(&mut self, numeric: bool) {
        if numeric {
            self.flags |= NUMERIC_MASK;
        } else {
            self.flags &= !NUMERIC_MASK;
        }
    }

    // -- Deleted flag --

    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & DELETE_MASK != 0
    }

    #[inline]
    pub fn set_deleted(&mut self, deleted: bool) {
        if deleted {
            self.flags |= DELETE_MASK;
        } else {
            self.flags &= !DELETE_MASK;
        }
    }
}

impl std::fmt::Debug for ItemHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItemHeader")
            .field("klen", &self.klen())
            .field("vlen", &self.vlen())
            .field("olen", &self.olen())
            .field("is_numeric", &self.is_numeric())
            .field("is_deleted", &self.is_deleted())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed() -> ItemHeader {
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn is_deleted_roundtrip() {
        let mut h = zeroed();
        assert!(!h.is_deleted());
        h.set_deleted(true);
        assert!(h.is_deleted());
        h.set_deleted(false);
        assert!(!h.is_deleted());
    }

    #[test]
    fn is_deleted_independent_of_other_flags() {
        let mut h = zeroed();
        h.set_deleted(true);
        h.set_numeric(true);
        h.set_olen(5);
        assert!(
            h.is_deleted(),
            "is_deleted should survive set_numeric and set_olen"
        );
        assert!(h.is_numeric());
        assert_eq!(h.olen(), 5);
    }

    #[test]
    fn set_numeric_does_not_clear_deleted() {
        let mut h = zeroed();
        h.set_deleted(true);
        h.set_numeric(false);
        assert!(h.is_deleted());
    }
}
