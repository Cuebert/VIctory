//! Recover a failed guarded read into host memory. Only a freshly queried,
//! definitely inaccessible source region may skip subsequent guarded reads.

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
};

fn inaccessible_end(address: usize) -> Option<usize> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: the query inspects an address in our own process and fills
    // the supplied structure. It does not dereference the source in Rust.
    if unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) }
        != size_of_val(&info)
    {
        return None;
    }
    let inaccessible = matches!(info.State, MEM_RESERVE | MEM_FREE)
        || (info.State == MEM_COMMIT && info.Protect == PAGE_NOACCESS);
    if inaccessible && info.BaseAddress as usize <= address {
        (info.BaseAddress as usize).checked_add(info.RegionSize)
    } else {
        None
    }
}

pub(crate) fn recover(
    start: usize,
    len: usize,
    mut read: impl FnMut(usize, usize) -> bool,
    mut zero: impl FnMut(usize, usize),
) -> usize {
    const PAGE: usize = 4096;
    const BLOCK: usize = 65536;
    let mut offset = 0;
    let mut skipped = 0;
    while offset < len {
        let at = start + offset;
        let block = (BLOCK - at % BLOCK).min(len - offset);
        if read(at, block) {
            offset += block;
            continue;
        }
        let block_end = offset + block;
        let mut previous_failed = false;
        while offset < block_end {
            let at = start + offset;
            let page = (PAGE - at % PAGE).min(block_end - offset);
            if read(at, page) {
                previous_failed = false;
                offset += page;
                continue;
            }
            // Isolated holes and readable pages require no query. Once two
            // adjacent reads have failed, a long reservation can be zeroed
            // with one write. Readable islands and remaps on a later copy
            // are always probed. Guarded and ambiguous pages keep retrying.
            let count = if previous_failed && len - offset > page {
                inaccessible_end(at).map_or(page, |end| {
                    end.saturating_sub(at).clamp(page, len - offset)
                })
            } else {
                page
            };
            skipped += count - page;
            zero(at, count);
            offset += count;
            previous_failed = true;
        }
    }
    skipped
}
