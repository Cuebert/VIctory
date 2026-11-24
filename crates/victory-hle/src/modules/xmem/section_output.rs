//! Destination for an exact, fully mapped ResetCounterValues output. The
//! command retains section identity at recording time across remaps and replay.
//!
//! The destination starts on a DWORD, anywhere in a 16-byte record (most of
//! the title's bases end in hex 4), and is a whole number of records long. A
//! store writes those bytes alone: the alignment bytes before a base that is
//! not record-aligned are the title's, like the bytes after the end.

use super::{PAGE, Section, Slice, View, physical};
use std::sync::Arc;
use victory_gpu::resource::RetainedSectionOutput;

/// A counter record; an output is a whole number of them.
const STRIDE: usize = 16;
/// What the destination's base must be aligned to: a DWORD, not a record.
const ALIGN: usize = 4;
const MAX_BYTES: usize = 65_535 * 32 * STRIDE;

struct Output {
    section: Arc<Section>,
    offset: usize,
    len: usize,
}

/// The physical bytes shown at `[address, address + len)`, if `address` is
/// DWORD-aligned, `len` is a nonzero whole number of records within the cap,
/// and every byte is mapped from consecutive bytes of one section. Only the
/// view registry is consulted: no title address is read.
pub(crate) fn retain(address: u64, len: usize) -> Option<Arc<dyn RetainedSectionOutput>> {
    let start = usize::try_from(address).ok()?;
    let end = start.checked_add(len)?;
    if start == 0 || start % ALIGN != 0 || len == 0 || len % STRIDE != 0 || len > MAX_BYTES {
        return None;
    }
    let state = physical();
    let (&first, (_, slice)) = state.views.range(..=start).next_back()?;
    let offset = slice.index.checked_mul(PAGE)?.checked_add(start - first)?;
    let mut at = start;
    for (&base, (size, next)) in state.views.range(first..end) {
        let limit = base.checked_add(*size)?.min(end);
        if base > at || limit <= at || !Arc::ptr_eq(&slice.section, &next.section) {
            return None;
        }
        let actual = next.index.checked_mul(PAGE)?.checked_add(at - base)?;
        if actual != offset.checked_add(at - start)? {
            return None;
        }
        at = limit;
        if at == end {
            offset.checked_add(len)?;
            return Some(Arc::new(Output { section: slice.section.clone(), offset, len }));
        }
    }
    None
}

impl Output {
    /// Current virtual aliases intersecting the retained physical bytes.
    /// These intervals only invalidate cache metadata.
    fn aliases(&self) -> Vec<(usize, usize)> {
        let end = self.offset + self.len;
        physical().views.iter().filter_map(|(&base, (size, view))| {
            if !Arc::ptr_eq(&view.section, &self.section) {
                return None;
            }
            let view_offset = view.index.checked_mul(PAGE)?;
            let first = self.offset.max(view_offset);
            let last = end.min(view_offset.checked_add(*size)?);
            if first >= last {
                return None;
            }
            Some((base.checked_add(first - view_offset)?, base.checked_add(last - view_offset)?))
        }).collect()
    }
}

impl RetainedSectionOutput for Output {
    #[cfg(feature="diagnostics")]
    fn physical_identity(&self)->Option<(u64,u64)> {
        (self.section.1!=0).then_some((self.section.1,self.offset as u64))
    }
    fn store(&self, bytes: &[u8]) -> bool {
        if bytes.len() != self.len {
            return false;
        }
        let within = self.offset % PAGE;
        let slice = Slice { section: self.section.clone(), index: self.offset / PAGE };
        let end = within + self.len;
        let Some(mut view) = View::map(&slice, end.div_ceil(PAGE)) else {
            return false;
        };
        view.bytes()[within..end].copy_from_slice(bytes);
        victory_gpu::section_output_changed(&self.aliases());
        true
    }
}
