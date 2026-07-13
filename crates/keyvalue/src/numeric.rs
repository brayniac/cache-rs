//! Detection of "simple numeric" byte values.
//!
//! Ported from crucible's `cache/core/src/numeric.rs`: a value is a
//! simple numeric if it is the canonical ASCII-decimal rendering of a
//! `u64` — only digits, no leading zeros (except `"0"` itself), no
//! whitespace or sign, and within `u64` range. Used to decide whether a
//! bytes value can be converted to a numeric item.

/// Parse a byte slice as a simple numeric value.
///
/// Returns `Some(value)` iff the bytes are the canonical ASCII-decimal
/// rendering of a `u64` (round-trippable), `None` otherwise.
pub fn parse_simple_numeric(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }

    // no leading zeros (except "0" itself)
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }

    let mut value: u64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }

    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_decimals() {
        assert_eq!(parse_simple_numeric(b"0"), Some(0));
        assert_eq!(parse_simple_numeric(b"1"), Some(1));
        assert_eq!(parse_simple_numeric(b"123"), Some(123));
        assert_eq!(
            parse_simple_numeric(u64::MAX.to_string().as_bytes()),
            Some(u64::MAX)
        );
    }

    #[test]
    fn rejects_non_canonical() {
        assert_eq!(parse_simple_numeric(b""), None);
        assert_eq!(parse_simple_numeric(b"01"), None); // leading zero
        assert_eq!(parse_simple_numeric(b"00"), None);
        assert_eq!(parse_simple_numeric(b" 5"), None); // whitespace
        assert_eq!(parse_simple_numeric(b"5 "), None);
        assert_eq!(parse_simple_numeric(b"+5"), None); // sign
        assert_eq!(parse_simple_numeric(b"-5"), None);
        assert_eq!(parse_simple_numeric(b"1a"), None);
        assert_eq!(parse_simple_numeric(b"hello"), None);
    }

    #[test]
    fn rejects_overflow() {
        // u64::MAX + 1
        assert_eq!(parse_simple_numeric(b"18446744073709551616"), None);
        assert_eq!(parse_simple_numeric(b"99999999999999999999"), None);
    }
}
