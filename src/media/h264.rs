//! What the server and its listeners need to know about H.264 bytes, and no
//! more.
//!
//! Annex-B (ISO 14496-10 Annex B) is a byte stream: NAL units separated by the
//! start code `00 00 01`, each optionally preceded by extra zero bytes. Two
//! questions are asked of it here — where the units are, and whether an access
//! unit carries an IDR picture, the one a decoder can start from. A viewer who
//! joins mid-stream must wait for one: a P-picture without its reference
//! decodes to garbage rather than to an error, so the decoder cannot be left to
//! notice. The web frame's flag byte is where that answer travels, so the
//! player need not parse what this side already parsed.
//!
//! Pure on purpose: no decoding, no parameter sets, nothing of a slice header
//! beyond its type byte.

/// The NAL unit type lives in the low five bits of the first byte after the
/// start code.
pub const NAL_TYPE_MASK: u8 = 0x1F;
/// A slice of a non-IDR picture.
pub const NAL_SLICE: u8 = 1;
/// A slice of an IDR picture — decodable with no earlier picture in hand.
pub const NAL_IDR: u8 = 5;
/// Sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// Picture parameter set.
pub const NAL_PPS: u8 = 8;
/// Access unit delimiter.
pub const NAL_AUD: u8 = 9;

/// The NAL units of an Annex-B byte string, without their start codes, in
/// order.
///
/// Zero bytes that sit between a unit and the next start code belong to
/// nobody — Annex B names them `trailing_zero_8bits` — and are not part of the
/// unit before them.
pub fn nal_units(annex_b: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    // Index of the first byte after the most recent start code.
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i + 2 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            if let Some(from) = open {
                push_unit(&mut units, &annex_b[from..i]);
            }
            i += 3;
            open = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(from) = open {
        push_unit(&mut units, &annex_b[from..]);
    }
    units
}

/// Keeps a unit if anything is left of it once the zeros that lead into the
/// next start code are dropped. A four-byte start code is `00` + `00 00 01`,
/// so the scanner above always leaves its first zero on the unit before it.
fn push_unit<'a>(units: &mut Vec<&'a [u8]>, bytes: &'a [u8]) {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    if end > 0 {
        units.push(&bytes[..end]);
    }
}

/// The type of one NAL unit, or `None` for an empty one.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|byte| byte & NAL_TYPE_MASK)
}

/// Whether an access unit carries an IDR slice — a picture a decoder can start
/// from.
pub fn access_unit_has_idr(access_unit: &[u8]) -> bool {
    nal_units(access_unit)
        .into_iter()
        .any(|unit| nal_type(unit) == Some(NAL_IDR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(code: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = code.to_vec();
        out.extend_from_slice(body);
        out
    }

    const LONG: &[u8] = &[0, 0, 0, 1];
    const SHORT: &[u8] = &[0, 0, 1];

    /// TP-VIDEO-AU-02 · an IDR slice makes the access unit a key picture.
    ///
    /// The shape ffmpeg actually emits for a key picture: delimiter,
    /// parameter sets, then the IDR slice. The answer must be found *behind*
    /// the non-slice units, not in the first one.
    #[test]
    fn an_access_unit_with_an_idr_slice_is_a_key_picture() {
        let mut au = unit(LONG, &[0x09, 0xF0]);
        au.extend(unit(LONG, &[0x67, 0x42, 0xC0, 0x1E]));
        au.extend(unit(LONG, &[0x68, 0xCE, 0x38, 0x80]));
        au.extend(unit(LONG, &[0x65, 0x88, 0x84, 0x00]));
        assert!(access_unit_has_idr(&au));
    }

    /// TP-VIDEO-AU-02 · a P picture is not one, however it is dressed.
    ///
    /// A delimiter and an SEI in front of a P slice must not read as a key
    /// picture: a viewer that started decoding here would render garbage
    /// until the next real IDR, and nothing would say why.
    #[test]
    fn an_access_unit_of_p_slices_is_not_a_key_picture() {
        let mut au = unit(LONG, &[0x09, 0x30]);
        au.extend(unit(LONG, &[0x06, 0x05, 0x01]));
        au.extend(unit(LONG, &[0x41, 0x9A, 0x02]));
        au.extend(unit(LONG, &[0x41, 0x9A, 0x03]));
        assert!(!access_unit_has_idr(&au));
    }

    /// TP-VIDEO-AU-02 · both start code lengths are start codes.
    ///
    /// Encoders use the three-byte form inside an access unit; a scanner that
    /// only knew the four-byte one would miss every unit but the first.
    #[test]
    fn three_byte_start_codes_are_start_codes_too() {
        let mut au = unit(LONG, &[0x09, 0xF0]);
        au.extend(unit(SHORT, &[0x65, 0x88]));
        assert!(access_unit_has_idr(&au));
        assert_eq!(nal_units(&au).len(), 2);
    }

    /// TP-VIDEO-AU-02 · units are split at start codes and trimmed of the
    /// zero bytes that precede the next one.
    #[test]
    fn units_are_split_at_start_codes_and_trailing_zeros_belong_to_nobody() {
        let mut au = unit(LONG, &[0x09, 0xF0]);
        au.extend(unit(SHORT, &[0x67, 0xAA]));
        // A unit followed by trailing_zero_8bits and then a long start code.
        au.extend([0x00]);
        au.extend(unit(LONG, &[0x65, 0xBB]));
        let units = nal_units(&au);
        assert_eq!(
            units,
            vec![&[0x09, 0xF0][..], &[0x67, 0xAA][..], &[0x65, 0xBB][..]],
            "three units, none carrying a start code or a stray zero"
        );
        assert_eq!(
            units.iter().map(|u| nal_type(u)).collect::<Vec<_>>(),
            vec![Some(NAL_AUD), Some(NAL_SPS), Some(NAL_IDR)]
        );
    }

    /// TP-VIDEO-AU-02 · nothing is not a key picture, and a bare start code
    /// is not a unit.
    #[test]
    fn empty_input_has_no_units_and_no_idr() {
        assert!(nal_units(&[]).is_empty());
        assert!(!access_unit_has_idr(&[]));
        assert!(
            nal_units(LONG).is_empty(),
            "a start code with nothing after it"
        );
        assert!(!access_unit_has_idr(LONG));
        assert_eq!(nal_type(&[]), None);
    }

    /// TP-VIDEO-AU-02 · the slice types are the numbers the standard gives them.
    ///
    /// Pinned because the flag on the wire switches on exactly these, and a
    /// renumbering would silently flag every P picture as a key.
    #[test]
    fn the_unit_types_are_the_standards_numbers() {
        assert_eq!(
            (NAL_SLICE, NAL_IDR, NAL_SPS, NAL_PPS, NAL_AUD),
            (1, 5, 7, 8, 9)
        );
        assert_eq!(
            nal_type(&[0x65]),
            Some(NAL_IDR),
            "nal_ref_idc bits are masked off"
        );
        assert_eq!(nal_type(&[0x41]), Some(NAL_SLICE));
    }
}
