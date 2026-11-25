//! One shared formatter for interface IDs, used by every QueryInterface diagnostic.
//!
//! The console asks for interfaces by GUID, and an `E_NOINTERFACE` refusal is only
//! actionable when the requested ID is logged in the canonical D3D spelling. Keeping a
//! single implementation here means every call site and the tests agree on that spelling.

/// Format an interface ID as `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
///
/// `slot` names the vtable slot the request came through when one is known; it is
/// appended as ` slot=N` so one log line identifies both the interface and the entry.
pub fn guid_str(d1: u32, d2: u16, d3: u16, d4: [u8; 8], slot: Option<usize>) -> String {
    let mut s = format!(
        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    );
    if let Some(n) = slot {
        s.push_str(&format!(" slot={n}"));
    }
    s
}

/// Format an interface ID from its 16 raw bytes, in the order the GUID stores them.
pub fn guid_bytes(b: [u8; 16], slot: Option<usize>) -> String {
    guid_str(
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        slot,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_guid_formats_canonically() {
        // IID_IUnknown: 00000000-0000-0000-C000-000000000046
        assert_eq!(
            guid_str(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46], None),
            "{00000000-0000-0000-C000-000000000046}"
        );
        // E_NOINTERFACE's usual suspect: a console-only IID with a non-zero every field.
        assert_eq!(
            guid_str(0x9D5E_2271, 0x9D52, 0x4B7A, [0x9E, 0x57, 0x42, 0x9A, 0x0B, 0x32, 0x1F, 0x8D], None),
            "{9D5E2271-9D52-4B7A-9E57-429A0B321F8D}"
        );
    }

    #[test]
    fn slot_is_appended_when_known() {
        assert_eq!(
            guid_str(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46], Some(0)),
            "{00000000-0000-0000-C000-000000000046} slot=0"
        );
        assert_eq!(
            guid_str(0x0000_0000, 0x0000, 0x0000, [0xC0, 0, 0, 0, 0, 0, 0, 0x46], None),
            "{00000000-0000-0000-C000-000000000046}"
        );
    }

    #[test]
    fn raw_bytes_match_the_field_form() {
        let b = [0x71, 0x22, 0x5E, 0x9D, 0x52, 0x9D, 0x7A, 0x4B, 0x9E, 0x57, 0x42, 0x9A, 0x0B, 0x32, 0x1F, 0x8D];
        assert_eq!(guid_bytes(b, Some(12)), "{9D5E2271-9D52-4B7A-9E57-429A0B321F8D} slot=12");
    }
}
