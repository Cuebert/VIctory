//! The address map: how a translated shader finds title memory at an
//! address it computes ([`crate::resource::GLOBAL`]), for the host to fill.
//!
//! A raw buffer bound as a root SRV, `t0` in
//! [`super::ADDRESS_MAP_SPACE`], when [`super::HostShader::address_map`]
//! is set:
//!
//! * a header of [`HEADER`] bytes: dword 0 is the number of ranges `n`, the
//!   rest zero;
//! * then `n` entries of [`ENTRY`] bytes, sorted by start address and not
//!   overlapping: the range's first title address (dwords 0 and 1, low
//!   first), its length in bytes (dword 2), the descriptor heap index of a
//!   raw UAV view (`R32_TYPELESS`, raw flag) holding its bytes (dword 3),
//!   and the byte offset of the range's first byte in that view (dword 4);
//!   dword 5 is the READ/WRITE permission mask, and dwords 6 and 7 are zero.
//!
//! The shader finds the last range starting at or below an address with a
//! binary search and reads through `ResourceDescriptorHeap[view]` (a
//! non-uniform index when lanes differ), so the views must be in the heap
//! bound with the map. An address no range covers reads zero. Root
//! descriptors are not bounds-checked: the buffer must hold all `n`
//! entries.

/// Bytes before the first entry.
pub const HEADER: u32 = 16;
/// Bytes per entry.
pub const ENTRY: u32 = 32;
pub const READ: u32 = 1;
pub const WRITE: u32 = 2;
/// The descriptor-words root starts with the capacity in bytes and three
/// reserved dwords. Only user-visible descriptors follow this immutable header.
pub const WORDS_HEADER: u32 = 16;

/// One range of title memory and the view that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    pub len: u32,
    pub view: u32,
    pub view_offset: u32,
    pub access: u32,
}

/// The map's dwords for `ranges`, which must be sorted by start and not
/// overlap.
pub fn encode(ranges: &[Range]) -> Vec<u32> {
    debug_assert!(
        ranges
            .windows(2)
            .all(|w| w[0].start + u64::from(w[0].len) <= w[1].start)
    );
    let mut out = vec![0; (HEADER / 4) as usize];
    out[0] = ranges.len() as u32;
    for r in ranges {
        out.extend([
            r.start as u32,
            (r.start >> 32) as u32,
            r.len,
            r.view,
            r.view_offset,
            r.access,
            0,
            0,
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_registered_ranges_keep_their_exact_extent_and_access() {
        let r = |start, len, view, view_offset, access| Range {
            start,
            len,
            view,
            view_offset,
            access,
        };
        let words = encode(&[r(0x1000, 16, 7, 0, READ), r(0x1010, 4, 8, 32, READ | WRITE)]);
        assert_eq!(words[0], 2);
        assert_eq!(&words[4..10], &[0x1000, 0, 16, 7, 0, READ]);
        assert_eq!(&words[12..18], &[0x1010, 0, 4, 8, 32, READ | WRITE]);
    }
}
