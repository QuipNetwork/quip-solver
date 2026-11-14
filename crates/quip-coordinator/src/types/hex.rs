//! Hex encoding for hashes, keys, and RPC parameters.

/// Hex-encode bytes with a `0x` prefix (for RPC params).
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode a `0x`-optional hex string.
///
/// # Errors
/// Returns an error when the hex string has odd length or a non-hex nibble.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex ({} digits)", s.len()));
    }
    let decode_nibble = |byte| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("invalid hex digit".to_string()),
    };
    let (pairs, _) = s.as_bytes().as_chunks::<2>();
    let mut bytes = Vec::with_capacity(pairs.len());
    for &[high, low] in pairs {
        bytes.push((decode_nibble(high)? << 4) | decode_nibble(low)?);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decode_rejects_non_hex_input() {
        for (label, input) in [("even-length Unicode", "00€0"), ("non-hex ASCII", "0g")] {
            assert!(hex_decode(input).is_err(), "{label}");
        }
    }
}
