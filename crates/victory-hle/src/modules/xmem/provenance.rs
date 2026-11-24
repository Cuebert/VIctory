//! Diagnostic history of successful XMem allocations. Windows calls made
//! directly by the title can release or reuse a reservation, so these records
//! are observations only. They are never used to authorize a memory access.

use std::collections::BTreeMap;
use std::sync::Mutex;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Origin { Reservation, Placeholder, Callback, Default }
static SPANS: Mutex<BTreeMap<(usize, Origin), (usize,u64)>> = Mutex::new(BTreeMap::new());

pub(super) fn add(start: usize, size: usize, origin: Origin) {
    let Some(end) = start.checked_add(size).filter(|_| start != 0 && size != 0) else { return };
    let mut spans = SPANS.lock().unwrap_or_else(|e| e.into_inner());
    if spans.len() < 65_536 || spans.contains_key(&(start, origin)) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64=AtomicU64::new(1);
        let generation=NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed,|n|n.checked_add(1)).unwrap_or(0);
        spans.insert((start, origin), (end,generation));
    }
}

pub(super) fn remove(start: usize, origin: Origin) {
    SPANS.lock().unwrap_or_else(|e| e.into_inner()).remove(&(start, origin));
}

/// A same-reservation observation only: a title can bypass our free/remap callbacks.
pub(super) fn reservation_id(at:usize,end:usize)->Option<(u8,u64,u64)> {
    let spans=SPANS.lock().unwrap_or_else(|e|e.into_inner());
    // Reservation records can overlap under address reuse. Refuse ambiguity
    // among the bounded neighbours. This classification never provides a lease.
    let mut found=None;
    for (&(lo,origin),&(hi,id)) in spans.range(..=(at,Origin::Default)).rev().take(12) {
        if origin != Origin::Reservation || hi<end { continue; }
        if found.is_some() || id==0 {return None;}
        found=Some((3,id,(at-lo) as u64));
    }
    found
}

pub(crate) fn describe(address: u64, width: u64) -> String {
    let (Ok(start), Ok(size)) = (usize::try_from(address), usize::try_from(width)) else { return "invalid span".into() };
    let Some(end) = start.checked_add(size).filter(|_| start != 0 && size != 0) else { return "invalid span".into() };
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: this asks the OS for mapping metadata. It never reads the
    // caller-supplied address and writes only to our local struct.
    let queried = unsafe { VirtualQuery(start as _, &mut info, size_of_val(&info)) } != 0;
    let (state, kind) = if queried {
        (match info.State { MEM_COMMIT => "committed", MEM_RESERVE => "reserved", MEM_FREE => "free", _ => "other" },
         match info.Type { MEM_PRIVATE => "private", MEM_MAPPED => "mapped", MEM_IMAGE => "image", _ => "none" })
    } else { ("unavailable", "none") };
    let region_covers = queried && (info.BaseAddress as usize).checked_add(info.RegionSize).is_some_and(|hi| hi >= end);
    let spans = SPANS.lock().unwrap_or_else(|e| e.into_inner());
    let (mut reservation, mut placeholder, mut callback, mut default) = (false, false, false, false);
    let mut allocation_base_matches = false;
    for (&(lo, origin), &(hi,_)) in spans.range(..=(start, Origin::Default)).rev() {
        if hi < end { continue; }
        match origin {
            Origin::Reservation => reservation = true,
            Origin::Placeholder => placeholder = true,
            Origin::Callback => callback = true,
            Origin::Default => default = true,
        }
        if matches!(origin, Origin::Reservation | Origin::Placeholder) && queried && info.AllocationBase as usize == lo {
            allocation_base_matches = true;
        }
    }
    format!("reservation_observed={reservation}, placeholder_observed={placeholder}, allocator_callback_observed={callback}, allocator_default_observed={default}, native_state={state}, native_type={kind}, one_region={region_covers}, native_base_matches_xmem={allocation_base_matches}")
}
