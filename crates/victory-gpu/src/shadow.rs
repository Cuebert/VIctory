//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

static REFRESH_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switch the local comparison at a presentation boundary, outside command
/// recording. Missing control files leave the original behavior active.
#[cfg(feature = "diagnostics")]
pub(crate) fn poll_pending_refresh(folder: &std::path::Path) {
    use std::io::Read;
    static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);
    let path = folder.join("pending-title-writes.txt");
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut previous = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *previous == modified { return; }
    let enabled = if modified.is_some() {
        let mut bytes = Vec::new();
        let Ok(_) = std::fs::File::open(path).and_then(|f| f.take(9).read_to_end(&mut bytes)) else { return };
        match bytes.trim_ascii() {
            b"apply" if bytes.len() < 9 => true,
            b"off" if bytes.len() < 9 => false,
            _ => return,
        }
    } else { false };
    *previous = modified;
    REFRESH_PENDING.store(enabled, Ordering::Relaxed);
    tracing::info!(enabled, "late title constant refresh");
}

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// When the address map publishes this shadow, its deferred refresh
    /// holds this allocation as long as it holds the shadow.
    _map_owner: Option<Arc<crate::resource::OwnedAllocation>>,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// Exact Reset outputs pinned to a physical section for this submission.
    section_outputs: Mutex<Vec<(usize, usize)>>,
    section_active: std::sync::atomic::AtomicBool,
    /// The copy's bytes before the submission ran, from an offset on,
    /// covering the ranges its shaders write: what the GPU wrote differs.
    before: Mutex<Option<(usize, Vec<u8>)>>,
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool (at most 8 kept, the largest).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    if pool.len() > 8 {
        pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
        pool.truncate(8);
    }
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// Copies only from our mapped D3D allocation. The caller owns the
    /// submission lock and has verified completion of the last GPU fence.
    #[cfg(feature="diagnostics")]
    pub(crate) fn material_record(&self,address:u64)->Option<[u32;7]> {
        let offset=usize::try_from(address.checked_sub(self.start)?).ok()?;
        if (offset as u64).checked_add(28)? > self.len {return None}
        let mut bytes=[0u8;28];
        unsafe {std::ptr::copy_nonoverlapping(self.data.add(offset),bytes.as_mut_ptr(),bytes.len())};
        Some(std::array::from_fn(|i|u32::from_le_bytes(bytes[4*i..4*i+4].try_into().unwrap())))
    }
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start
    }

    /// The title memory the copy holds: its first address and length.
    pub(crate) fn span(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(
        buffer: ID3D12Resource,
        data: *mut u8,
        gpu: u64,
        start: u64,
        len: u64,
        owner: Option<Arc<crate::resource::OwnedAllocation>>,
    ) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            _map_owner: owner,
            copied: Mutex::new(vec![0; pages as usize]),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            section_outputs: Mutex::default(),
            section_active: std::sync::atomic::AtomicBool::new(false),
            before: Mutex::default(),
        }
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// one snapshot per copy and submission, grown to cover every range the
    /// submission declares (the world declares overlapping views of its
    /// 174 MB heap dozens of times), in reused memory (fresh snapshots cost
    /// ~2.3 million page faults per world frame).
    pub(crate) fn writing(&self, address: usize, len: usize) {
        let (from, to) = self.inside(address, len);
        // The parts no earlier range of the submission declared: their
        // refresh was free to bring the title's bytes into the copy (a
        // refresh leaves declared ranges alone), and may just have.
        let fresh = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let declared = pending.iter().map(|&(a, e)| self.inside(a, e - a));
            let fresh = outside(from, to, declared.collect());
            pending.push((address, address + len));
            fresh
        };
        // SAFETY: inside the mapping.
        let host = |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let old = match &*before {
            Some((at, bytes)) => (*at, at + bytes.len()),
            None => (from, from),
        };
        let (start, end) = (old.0.min(from), old.1.max(to));
        if (start, end) != old {
            // Parts newly covered from the copy, the rest as it was taken.
            let mut grown = pooled(end - start);
            match before.take() {
                Some((at, bytes)) => {
                    grown.extend_from_slice(host(start, at));
                    grown.extend_from_slice(&bytes);
                    grown.extend_from_slice(host(at + bytes.len(), end));
                    give_back(bytes);
                }
                None => grown.extend_from_slice(host(start, end)),
            }
            *before = Some((start, grown));
        }
        // What the snapshot already held of the fresh parts (it spans the
        // gaps between the ranges declared so far) was taken before their
        // refresh: taken again, or the refresh would pass for a shader's
        // write and go back over what the title writes meanwhile.
        if let Some((at, bytes)) = &mut *before {
            for (a, e) in fresh {
                let (a, e) = (a.max(old.0), e.min(old.1));
                if a < e {
                    bytes[a - *at..e - *at].copy_from_slice(host(a, e));
                }
            }
        }
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let sections = self
            .section_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = &mut *snapshot
            && *taken <= from
            && to <= *taken + before.len()
            && from < to
        {
            let before = &mut before[from - *taken..to - *taken];
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            let mut title = vec![0u8; PIECE];
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                if let Some(end) = sections
                    .iter()
                    .filter(|&&(a, b)| a <= start + at && start + at < b)
                    .map(|r| r.1)
                    .max()
                {
                    at = (end - start).min(to);
                    continue;
                }
                let next = sections
                    .iter()
                    .map(|r| r.0)
                    .filter(|&s| s > start + at)
                    .min()
                    .unwrap_or(start + to);
                let n = (PIECE - (start + at) % PIECE)
                    .min(to - at)
                    .min(next - start - at);
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &mut before[at - from..at - from + n];
                let title = &mut title[..n];
                if host != old && crate::host::read_into((start + at) as *const u8, title) {
                    // Title memory with the GPU's bytes put in; the copy
                    // takes the title's bytes in turn, and so does the
                    // snapshot: to a later range that overlaps this one,
                    // they are not a shader's write to put back.
                    let mut changed = false;
                    for ((t, h), o) in title.iter_mut().zip(host.iter_mut()).zip(old) {
                        if *h != *o && *t != *h {
                            *t = *h;
                            changed = true;
                        }
                        *h = *t;
                        *o = *t;
                    }
                    if changed {
                        crate::watch::writing(start + at, n);
                        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                        written += n;
                    }
                    synced.push((start + at) / PAGE);
                }
                at += n;
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if first < last
                && !synced.is_empty()
                && self.watched.load(Ordering::Relaxed) != NOT_WATCHED
                && let Some((now, _)) = crate::watch::writes(first * PAGE, last * PAGE, true)
            {
                let base = start / PAGE;
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                for page in synced.into_iter().filter(|p| (first..last).contains(p)) {
                    copied[page - base] = now;
                }
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|&r| r != (address, address + len));
        if pending.is_empty()
            && let Some((_, bytes)) = snapshot.take()
        {
            give_back(bytes);
        }
        written
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        if !self.section_active.load(Ordering::Acquire) {
            return self.update_unprotected(address, len);
        }
        let sections = self
            .section_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if sections.is_empty() {
            return self.update_unprotected(address, len);
        }
        let end = address + len;
        outside(
            address,
            end,
            sections
                .iter()
                .copied()
                .filter(|&(a, b)| a < end && address < b)
                .collect(),
        )
        .into_iter()
        .map(|(a, b)| self.update_unprotected(a, b - a))
        .sum()
    }

    pub(crate) fn protect_section(&self, address: usize, len: usize, begin: bool) {
        let range = (address, address + len);
        let mut sections = self
            .section_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if begin {
            sections.push(range);
        } else if let Some(i) = sections.iter().position(|r| *r == range) {
            sections.swap_remove(i);
        }
        self.section_active
            .store(!sections.is_empty(), Ordering::Release);
    }

    fn update_unprotected(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
            // A UAV can cover the whole heap. The title can still change
            // constants in it before this submission reaches the GPU.
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                if cfg!(test) || REFRESH_PENDING.load(Ordering::Relaxed) {
                    bytes += self.update_pending(at.max(a), to.min(e));
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    /// The submission has declared a UAV, but has not executed it. Refresh
    /// later title writes before submitting and advance the saved baseline
    /// for exactly those bytes. Retain independently staged host changes.
    fn update_pending(&self, from: usize, to: usize) -> usize {
        // The delayed observation must retain the exact allocation. A loose
        // shadow or a declaration of an address may have outlived its bytes.
        if from >= to || !self._map_owner.as_ref().is_some_and(|owner| owner.contains(from, to - from)) {
            return 0;
        }
        let start = self.start as usize;
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let Some((offset, before)) = &mut *snapshot else { return 0 };
        let from = from.max(start + *offset);
        let to = to.min(start + *offset + before.len());
        if from >= to { return 0; }
        #[cfg(feature = "diagnostics")]
        let timer = std::time::Instant::now();
        let watch = if self.watched.load(Ordering::Relaxed) == NOT_WATCHED {
            None
        } else {
            crate::watch::writes(from, to, true)
        };
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut source = Vec::new();
        let mut changed = 0;
        #[cfg(feature = "diagnostics")]
        let mut inspected = 0usize;
        let mut at = from;
        while at < to {
            let page = at / PAGE;
            let n = (PAGE - at % PAGE).min(to - at);
            let last_copy = copied[page - start / PAGE];
            let current = last_copy != 0 && watch.as_ref().is_some_and(|(_, pages)| {
                pages.binary_search_by_key(&page, |&(p, _)| p)
                    .map_or(true, |index| pages[index].1 <= last_copy)
            });
            if !current {
                source.resize(n, 0);
                if crate::host::read_into(at as *const u8, &mut source) {
                    #[cfg(feature = "diagnostics")]
                    { inspected += n; }
                    let offset_in_shadow = at - start;
                    let base = &mut before[offset_in_shadow - *offset..][..n];
                    // SAFETY: this part of the persistent mapping is inside
                    // the retained buffer and submissions are serialized.
                    let dest = unsafe { std::slice::from_raw_parts_mut(self.data.add(offset_in_shadow), n) };
                    if source[..] != *base {
                        if *dest == *base {
                            dest.copy_from_slice(&source);
                            base.copy_from_slice(&source);
                            changed += n;
                        } else {
                            for ((host, saved), &title) in dest.iter_mut().zip(base.iter_mut()).zip(&source) {
                                if *host == *saved && title != *saved {
                                    *host = title;
                                    *saved = title;
                                    changed += 1;
                                }
                            }
                        }
                    }
                    let resource_end = start + self.len as usize;
                    if let Some((now, _)) = &watch
                        && at == (page * PAGE).max(start)
                        && at + n == ((page + 1) * PAGE).min(resource_end)
                        && *dest == source[..]
                    {
                        copied[page - start / PAGE] = *now;
                    }
                }
            }
            at += n;
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{add, C};
            add(C::PendingChecks, 1);
            add(C::PendingReads, inspected as u64);
            add(C::PendingUpdates, changed as u64);
            add(C::PendingUs, timer.elapsed().as_micros() as u64);
        }
        changed
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
    fn update_free(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            // A strict part of one never-synchronized page is always copied
            // in full by copy_stale and cannot set its stamp. Leave the
            // kernel's dirty bits and the shared journal to their next reader.
            let page = from / PAGE;
            let start = self.start as usize;
            let end = start + self.len as usize;
            if (to - 1) / PAGE == page
                && (from > (page * PAGE).max(start) || to < ((page + 1) * PAGE).min(end))
                && self.copied.lock().unwrap_or_else(|e| e.into_inner())[page - start / PAGE] == 0
            {
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::PartialRefreshes, 1);
                    add(C::PartialBytes, (to - from) as u64);
                }
                return self.copy(from, to);
            }
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
    }
}

/// The parts of `[from, to)` outside every range in `ranges`, in order.
fn outside(from: usize, to: usize, mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let (mut at, mut parts) = (from, Vec::new());
    for (a, e) in ranges {
        if at < to && a > at {
            parts.push((at, a.min(to)));
        }
        at = at.max(e);
    }
    if at < to {
        parts.push((at, to));
    }
    parts
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BufferOrigin { Runtime, Allocator, Heap, TitleAddress }

struct Entry {
    #[cfg(feature="diagnostics")]
    registration: u64,
    end: u64,
    shadow: Option<Arc<Shadow>>,
    owner: Option<Arc<crate::resource::OwnedAllocation>>,
    flags: u32,
    origin: BufferOrigin,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());
static MAP_GENERATION: AtomicU64 = AtomicU64::new(1);

#[cfg(feature="diagnostics")]
pub(crate) fn owned_identity(start:u64, bytes:u64) -> Option<(u8,u64,u64)> {
    let end = start.checked_add(bytes)?;
    let map = buffers();
    // The nearest enclosing registration is sufficient only if unambiguous.
    // Limit work on ordinary copies; an uncertain case simply has no ID.
    let mut found = None;
    for (i,(&(base,_),entry)) in map.range(..=(start,usize::MAX)).rev().enumerate() {
        if i == 12 { return None; }
        if entry.end < end { continue; }
        if entry.owner.is_none() || found.is_some() || entry.registration == 0 { return None; }
        found=Some((2,entry.registration,start-base));
    }
    found
}

pub(crate) fn map_generation() -> u64 {
    MAP_GENERATION.load(Ordering::Acquire)
}

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(
    resource: usize,
    start: u64,
    len: u64,
    owner: Option<Arc<crate::resource::OwnedAllocation>>,
    flags: u32,
    _origin: BufferOrigin,
) {
    let Some(end) = start.checked_add(len) else {
        return;
    };
    let mut map = buffers();
    if let Some(old) = map.insert(
        (start, resource),
        Entry {
            #[cfg(feature="diagnostics")]
            registration: {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                NEXT.fetch_update(Ordering::Relaxed,Ordering::Relaxed,|n|n.checked_add(1)).unwrap_or(0)
            },
            end,
            shadow: None,
            owner,
            flags,
            origin: _origin,
        },
    ) && old.owner.is_some()
        && old.shadow.is_some()
    {
        MAP_GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    let mut map = buffers();
    if let Some(entry) = map.remove(&(start, resource))
        && entry.owner.is_some()
        && entry.shadow.is_some()
    {
        MAP_GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    make_shadow(key, map.get_mut(&key)?)
}

/// Resolves a root UAV only inside a unique registered writable, retained allocation.
/// Stop the view before the next overlapping registration.
pub(crate) fn owned_uav(address: u64, cap: u64) -> Option<(Arc<Shadow>, u64)> {
    owned_root_view(address, cap, true)
}

/// Read-only guest roots can share the native resource with a writable
/// root. Resolve their registered owner and check shader-read permission.
pub(crate) fn owned_root_view(address: u64, cap: u64, write: bool) -> Option<(Arc<Shadow>, u64)> {
    let mut map = buffers();
    let mut possible = map
        .range(..=(address, usize::MAX))
        .rev()
        .filter(|(_, e)| e.end > address);
    let (&key, entry) = possible.next()?;
    let allowed = if write {
        entry.flags & 4 != 0
    } else {
        entry.flags & 8 == 0
    };
    if possible.next().is_some() || entry.owner.is_none() || !allowed {
        return None;
    }
    let next = map
        .range((
            std::ops::Bound::Excluded((address, usize::MAX)),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .map(|(&(start, _), _)| start)
        .unwrap_or(entry.end);
    let len = (entry.end.min(next) - address).min(cap) & !3;
    if len == 0 || !(address - key.0).is_multiple_of(16) {
        return None;
    }
    Some((make_shadow(key, map.get_mut(&key)?)?, len))
}

/// A fully owned subrange with the permissions required by the mapped shader.
pub(crate) fn owned_compute_span(
    address: u64,
    len: u64,
    read: bool,
    write: bool,
) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len)?;
    if len == 0 || !address.is_multiple_of(4) {
        return None;
    }
    let mut map = buffers();
    let mut overlaps = map.range(..(end, 0)).filter(|(_, e)| e.end > address);
    let (&key, e) = overlaps.next()?;
    if overlaps.next().is_some()
        || key.0 > address
        || e.end < end
        || (read && e.flags & 8 != 0)
        || (write && e.flags & 4 == 0)
        || !e
            .owner
            .as_ref()?
            .contains(usize::try_from(address).ok()?, usize::try_from(len).ok()?)
    {
        return None;
    }
    make_shadow(key, map.get_mut(&key)?)
}

/// The exact writable, external title resource the section pins. A normal
/// binding to this registration therefore resolves to the same host resource.
pub(crate) fn section_output(
    address: u64,
    len: usize,
) -> Option<(Arc<Shadow>, Arc<dyn crate::resource::RetainedSectionOutput>)> {
    let end = address.checked_add(u64::try_from(len).ok()?)?;
    if len == 0 || !address.is_multiple_of(4) || !len.is_multiple_of(16) {
        return None;
    }
    let mut map = buffers();
    let mut spans = map.range(..(end, 0)).filter(|(_, e)| e.end > address);
    let (&key, entry) = spans.next()?;
    if spans.next().is_some()
        || key.0 > address
        || end > entry.end
        || !(address - key.0).is_multiple_of(4)
        || entry.flags & 4 == 0
        || entry.owner.is_some()
        || !matches!(entry.origin, BufferOrigin::TitleAddress)
    {
        return None;
    }
    let lease = crate::resource::retain_section_output(address, len)?;
    Some((make_shadow(key, map.get_mut(&key)?)?, lease))
}

fn make_shadow(key: (u64, usize), entry: &mut Entry) -> Option<Arc<Shadow>> {
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Arc::new(Shadow::new(
            buffer,
            data,
            gpu,
            key.0,
            len,
            entry.owner.clone(),
        )));
        if entry.owner.is_some() {
            MAP_GENERATION.fetch_add(1, Ordering::Release);
        }
    }
    entry.shadow.clone()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(buffer, data, gpu, start, len, None));
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// Only registered, runtime-owned buffer regions with both a live native
/// copy and an allocation that can be held until this list is reset.
pub(crate) fn map_sources() -> Option<(u64, Vec<(Arc<Shadow>, u32)>)> {
    const MAX_BUFFERS: usize = 8192;
    let map = buffers();
    let mut sources = Vec::new();
    for e in map.values() {
        if let Some(copy) = &e.shadow
            && e.owner.is_some()
        {
            if sources.len() == MAX_BUFFERS {
                return None;
            }
            sources.push((copy.clone(), e.flags));
        }
    }
    Some((map_generation(), sources))
}

/// Counts registrations and native copies at submission. The physical
/// registry is queried after releasing the buffer lock. These numbers
/// observe the existing map and never create a shadow or retain a new range.
#[cfg(feature = "diagnostics")]
pub(crate) fn report_map_backing(frame: u64) {
    const LIMIT: usize = 65_536;
    let (total, entries) = {
        let map = buffers();
        let entries: Vec<_> = map.iter().take(LIMIT).map(|(&(start, _), e)| {
            ((start, e.end - start), e.origin, e.shadow.is_some(), e.owner.is_some(), e.flags)
        }).collect();
        (map.len(), entries)
    };
    let spans: Vec<_> = entries.iter().map(|&(span, ..)| span).collect();
    let physical = crate::resource::section_coverage(&spans);
    let mut origins = [[0u32; 2]; 4];
    let (mut owned, mut full, mut partial, mut readable, mut writable) = ([0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2], [0u32; 2]);
    let (mut preceding_end, mut overlaps) = (0, 0usize);
    for (i, &((start, len), origin, native, has_owner, flags)) in entries.iter().enumerate() {
        let col = usize::from(native);
        origins[origin as usize][col] += 1;
        owned[col] += u32::from(has_owner);
        readable[col] += u32::from(flags & 8 == 0);
        writable[col] += u32::from(flags & 4 != 0);
        if len != 0 && let Some(&bytes) = physical.as_ref().and_then(|s| s.get(i)) {
            full[col] += u32::from(bytes == len);
            partial[col] += u32::from(bytes > 0 && bytes < len);
        }
        overlaps += usize::from(len != 0 && start < preceding_end);
        preceding_end = preceding_end.max(start + len);
    }
    let [runtime, allocator, heap, title_address] = origins;
    tracing::info!(frame, total, sampled=entries.len(), ?runtime, ?allocator, ?heap, ?title_address,
        ?owned, ?readable, ?writable, section_registry=physical.is_some(), section_full=?full,
        section_partial=?partial, overlaps, "laser buffers by backing [without native copy, with native copy]");
}

/// An acceleration-structure descriptor can point inside several registered
/// buffers. Report only full-span containment, current backing and rights.
/// This treats the supplied value as a key and never follows it.
#[cfg(feature = "diagnostics")]
pub(crate) fn range_evidence(start: u64, len: u64) -> String {
    let Some(end) = start.checked_add(len).filter(|_| start != 0 && len != 0) else {
        return String::from("empty or overflowing range");
    };
    let (mut registered, mut native, mut owned_read, mut owned_write) = (0usize, 0usize, 0usize, 0usize);
    let mut smallest: Option<(u64, BufferOrigin)> = None;
    {
        let map = buffers();
        for (&(lo, _), entry) in map.range(..=(start, usize::MAX)) {
            if entry.end < end { continue; }
            registered += 1;
            native += usize::from(entry.shadow.is_some());
            if entry.owner.is_some() && entry.shadow.is_some() {
                owned_read += usize::from(entry.flags & 8 == 0);
                owned_write += usize::from(entry.flags & 4 != 0);
            }
            let bytes = entry.end - lo;
            if smallest.is_none_or(|(size, _)| size > bytes) { smallest = Some((bytes, entry.origin)); }
        }
    }
    let section_bytes = crate::resource::section_coverage(&[(start, len)]).and_then(|v| v.first().copied());
    let xmem = crate::resource::allocation_history(start, len);
    format!("width={len}, registrations={registered}, native={native}, owned_read={owned_read}, owned_write={owned_write}, smallest={smallest:?}, section_bytes={section_bytes:?}, xmem={xmem:?}")
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three pages of title memory on page bounds (plain heap memory, so
    /// not write-watched: refreshes copy whole ranges) and their host
    /// copy; `None` without a host GPU. Returns the memory's owner, the
    /// address of the first page and the copy.
    fn title() -> Option<(Vec<u8>, usize, Shadow)> {
        let memory = vec![0u8; 4 * PAGE];
        let base = (memory.as_ptr() as usize).next_multiple_of(PAGE);
        let (buffer, data) = host()?.shared_buffer(3 * PAGE as u64)?;
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let shadow = Shadow::new(buffer, data, gpu, base as u64, 3 * PAGE as u64, None);
        Some((memory, base, shadow))
    }

    #[cfg(feature="diagnostics")]
    #[test]
    fn material_record_reads_the_completed_host_copy_with_checked_bounds() {
        let (_owner,base,shadow)=title().expect("this fixture requires D3D12");
        let wanted=[9u32,7,5,3,1,25,31];
        let bytes:Vec<u8>=wanted.iter().flat_map(|w|w.to_le_bytes()).collect();
        // The fixture writes only its private host mapping. The title bytes still contain zero.
        unsafe{std::ptr::copy_nonoverlapping(bytes.as_ptr(),shadow.data.add(32),bytes.len())};
        let host=host().unwrap();
        let _guard=host.idle_submission().expect("fixture completed");
        assert_eq!(shadow.material_record(base as u64+32),Some(wanted));
        assert!(shadow.material_record(base as u64-1).is_none());
        assert!(shadow.material_record(base as u64+shadow.len-27).is_none());
    }

    /// A write of one byte: the title's into its memory, or a shader's
    /// into the copy.
    fn poke(at: usize, value: u8) {
        // SAFETY: the tests pass addresses inside the memory or the copy.
        unsafe { (at as *mut u8).write_volatile(value) }
    }

    fn peek(at: usize) -> u8 {
        // SAFETY: as in `poke`.
        unsafe { (at as *const u8).read_volatile() }
    }

    #[test]
    fn pending_shader_output_observes_later_title_constants() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self){ REFRESH_PENDING.store(self.0, Ordering::Release); }
        }
        let _restore=Restore(REFRESH_PENDING.swap(true, Ordering::AcqRel));
        let size = 3 * PAGE as u64;
        let memory = crate::resource::Memory::zeroed(size, PAGE as u64).unwrap();
        let base = memory.address() as usize;
        let (buffer, data) = super::host().unwrap().shared_buffer(size).unwrap();
        // SAFETY: the D3D buffer handle is retained by the shadow.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let s = Shadow::new(buffer, data, gpu, base as u64, size, memory.map_owner(0, size));
        let host = s.data as usize;
        s.update(base, 3 * PAGE);
        s.writing(base, 3 * PAGE);
        let dimensions = [1024u32, 512, 2560, 1440];
        for (i, value) in dimensions.iter().flat_map(|v| v.to_le_bytes()).enumerate() {
            poke(base + PAGE + i, value);
        }
        // Preserve independently staged host bytes in the same request.
        poke(host + PAGE + 24, 0xa5);
        s.update(base + PAGE, 32);
        let actual: Vec<_> = (0..16).map(|i| peek(host + PAGE + i)).collect();
        assert_eq!(actual, dimensions.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        assert_eq!(peek(host + PAGE + 24), 0xa5);
        // A title write made after preparation and an actual shader write
        // must both survive the submission's eventual copy-back.
        poke(base + PAGE + 1, 0xe7);
        poke(host + 8, 0x33);
        s.copy_back(base, 3 * PAGE);
        assert_eq!((peek(base + PAGE + 1), peek(base + PAGE + 24), peek(base + 8)), (0xe7, 0xa5, 0x33));
        let (_storage, unowned_base, unowned) = title().unwrap();
        unowned.update(unowned_base, PAGE);
        unowned.writing(unowned_base, PAGE);
        poke(unowned_base + 5, 91);
        assert_eq!(unowned.update_pending(unowned_base, unowned_base + 16), 0);
        assert_eq!(peek(unowned.data as usize + 5), 0);
    }

    #[test]
    fn partial_refresh_observes_new_writes_preserves_aliases_and_skips_gpu_bytes() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let Some(device) = host() else { return };
        // SAFETY: this test owns the reservation, and only accesses committed bytes.
        let addr = unsafe {
            VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
        };
        assert!(!addr.is_null());
        let base = addr as usize;
        // Materialize every OS page before measuring whether a second
        // watch on stable resident bytes is empty.
        unsafe { addr.cast::<u8>().write_bytes(0, PAGE) };
        let shadow = || {
            let (buffer, data) = device.shared_buffer(PAGE as u64).unwrap();
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Shadow::new(buffer, data, gpu, base as u64, PAGE as u64,None)
        };
        let (part, whole) = (shadow(), shadow());
        let part_byte = |i| peek(part.data as usize + i);
        let whole_byte = |i| peek(whole.data as usize + i);
        assert_eq!(whole.update(base, PAGE), PAGE);
        assert_eq!(whole.update(base, PAGE), 0);
        poke(base + 137, 21);
        assert_eq!(part.update(base + 90, 128), 128);
        assert_eq!(part_byte(137), 21);
        poke(base + 137, 39);
        assert_eq!(part.update(base + 135, 20), 20);
        assert_eq!(part_byte(137), 39);
        assert_eq!(whole.update(base, PAGE), PAGE);
        assert_eq!(whole_byte(137), 39);
        assert_eq!(whole.update(base, PAGE), 0);
        part.writing(base + 130, 16);
        poke(part.data as usize + 137, 60);
        poke(base + 110, 47);
        assert_eq!(part.update(base + 100, 64), 48);
        assert_eq!((part_byte(110), part_byte(137)), (47, 60));
        part.copy_back(base + 130, 16);
        assert_eq!(peek(base + 137), 60);
        assert_eq!(part.copied.lock().unwrap()[0], 0);
        // Reuse this same address with different backing. With a partial
        // page refresh there is no previous clean answer to replay.
        unsafe { assert_ne!(VirtualFree(addr, PAGE, MEM_DECOMMIT), 0) };
        crate::watch::title_memory_remapped(base, PAGE);
        assert_eq!(part.update(base + 105, 64), 64);
        assert_eq!((part_byte(110), part_byte(137)), (0, 0));
        assert_eq!(unsafe { VirtualAlloc(addr, PAGE, MEM_COMMIT, PAGE_READWRITE) }, addr);
        unsafe { addr.cast::<u8>().write_bytes(0, PAGE) };
        crate::watch::title_memory_remapped(base, PAGE);
        poke(base + 137, 72);
        assert_eq!(part.update(base + 105, 64), 64);
        assert_eq!(part_byte(137), 72);
        assert_eq!(part.update(base, PAGE), PAGE);
        assert_eq!(part.update(base + 105, 64), 0);
        unsafe { assert_ne!(VirtualFree(addr, 0, MEM_RELEASE), 0) };
    }

    /// A range declared after ranges on both sides of it, once the snapshot
    /// already spans it: what its refresh brought into the copy is not a
    /// shader's write, and does not go back over what the title wrote since.
    #[test]
    fn a_range_between_earlier_ones_keeps_the_titles_newer_bytes() {
        let Some((_memory, base, s)) = title() else {
            return; // no host GPU here
        };
        let (a, b, c) = (base, base + PAGE, base + 2 * PAGE);
        let copy = |at: usize| s.data as usize + (at - base);
        // An earlier submission read the middle page; the title has written
        // it since.
        s.update(b, PAGE);
        poke(b + 5, 1);
        poke(b + 6, 3);
        // This submission's shaders write the outer pages, then the middle
        // one: each range refreshed, then declared.
        for at in [a, c, b] {
            s.update(at, PAGE);
            s.writing(at, PAGE);
        }
        assert_eq!(peek(copy(b + 5)), 1, "the refresh");
        // While the GPU runs: a shader writes two bytes, the title one.
        poke(copy(a + 7), 9);
        poke(copy(b + 9), 7);
        poke(b + 5, 2);
        for at in [a, c, b] {
            s.copy_back(at, PAGE);
        }
        assert_eq!(peek(b + 5), 2, "the title's newer byte");
        assert_eq!(peek(b + 6), 3, "the title's byte from before");
        // What the shader wrote does go back, in either kind of range.
        assert_eq!((peek(a + 7), peek(b + 9)), (9, 7));
        assert!(s.before.lock().unwrap().is_none(), "the snapshot is done");
    }

    /// Two declared ranges that overlap: the first one's copy-back gives
    /// the copy the title's bytes, which the second must not take for a
    /// shader's and put back over what the title wrote in between.
    #[test]
    fn overlapping_ranges_keep_the_titles_newer_bytes() {
        let Some((_memory, base, s)) = title() else {
            return; // no host GPU here
        };
        let copy = |at: usize| s.data as usize + (at - base);
        s.update(base, 2 * PAGE);
        s.writing(base, PAGE);
        s.writing(base, 2 * PAGE);
        // While the GPU runs: a shader's byte and the title's, in one page.
        poke(copy(base + 3), 9);
        poke(base + 8, 1);
        s.copy_back(base, PAGE);
        assert_eq!((peek(base + 3), peek(base + 8)), (9, 1));
        // The title writes its byte again before the second range's turn.
        poke(base + 8, 2);
        s.copy_back(base, 2 * PAGE);
        assert_eq!(peek(base + 8), 2, "the title's newer byte");
        assert_eq!(peek(base + 3), 9, "the shader's byte");
    }
}
