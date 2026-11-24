//! Diagnostics: where the submission copies of title memory go (which
//! ranges, how often, of what memory) and what limits their rate, logged
//! with the frame-rate line. `VICTORY_COPY_MODE=plain` copies without the
//! mapped-page guard, `none` skips the copies (both for timing only).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Counters since the last line.
pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    /// Write-watch askings and their time.
    Watches,
    WatchUs,
    /// Bytes of watched ranges checked, and of those found unchanged.
    Checked,
    Unchanged,
    /// Copies that met an unmapped page.
    Fallbacks,
    /// Writes taken by the guard on view memory.
    GuardFaults,
    /// Texture bytes detiled after a watched write; hashed; detiled after a
    /// hash change.
    TexWatched,
    TexHashed,
    TexChanged,
    HostFallbacks,
    HostFallbackUs,
    UnprobedHoleBytes,
    KernelWatchHits,
    KernelWatchHitNs,
    KernelWatchMisses,
    KernelWatchMissNs,
    GuardCalls,
    GuardNs,
    GuardTypeNs,
    PartialRefreshes,
    PartialBytes,
    PendingChecks,
    PendingReads,
    PendingUpdates,
    PendingUs,
}
const COUNTERS: usize = C::PendingUs as usize + 1;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];
// Counts and total nanoseconds for successful private queries, grouped by
// the actual rounded kernel interval. Boundaries are in 64 KiB guest pages.
const WATCH_LIMITS: [usize; 5] = [1, 4, 16, 128, 1024];
static PRIVATE_BUCKETS: [[AtomicU64; 2]; 6] =
    [const { [const { AtomicU64::new(0) }; 2] }; 6];

thread_local! {
    static WATCH_COVERAGE: std::cell::RefCell<Option<Vec<(usize, usize)>>> = const {
        std::cell::RefCell::new(None)
    };
}
static WATCH_COVERED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Observes overlap only. Every query and its writes still reach the kernel.
pub(crate) struct WatchScope;
impl WatchScope {
    pub(crate) fn begin() -> Self {
        WATCH_COVERAGE.with_borrow_mut(|scope| *scope = Some(Vec::new()));
        Self
    }
    pub(crate) fn write_barrier() {
        WATCH_COVERAGE.with_borrow_mut(|s| { if let Some(v) = s { v.clear(); } });
    }
}
impl Drop for WatchScope {
    fn drop(&mut self) {
        WATCH_COVERAGE.with_borrow_mut(|s| *s = None);
    }
}

pub(crate) fn private_watch(first: usize, last: usize, ns: u64) {
    let pages = last - first;
    let bucket = WATCH_LIMITS.partition_point(|&limit| pages > limit);
    PRIVATE_BUCKETS[bucket][0].fetch_add(1, Relaxed);
    PRIVATE_BUCKETS[bucket][1].fetch_add(ns, Relaxed);
    WATCH_COVERAGE.with_borrow_mut(|scope| {
        let Some(ranges) = scope else { return };
        let lo = ranges.partition_point(|&(_, end)| end < first);
        if ranges.get(lo).is_some_and(|&(a, b)| a <= first && last <= b) {
            WATCH_COVERED[0].fetch_add(1, Relaxed);
            WATCH_COVERED[1].fetch_add(ns, Relaxed);
            return;
        }
        let hi = lo + ranges[lo..].partition_point(|&(start, _)| start <= last);
        let a = ranges.get(lo).map_or(first, |&(a, _)| a.min(first));
        let b = if hi > lo { ranges[hi - 1].1.max(last) } else { last };
        ranges.splice(lo..hi, [(a, b)]);
    });
}

pub(crate) fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

/// How copies run: 0 guarded (as always), 1 plain, 2 not at all.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

/// Per source range (start, length): copies, bytes, microseconds, memory type.
type Ranges = HashMap<(usize, usize), (u64, u64, u64, u32)>;
static RANGES: Mutex<Option<Ranges>> = Mutex::new(None);

/// A copy of `len` bytes from `src` (memory type `kind`) took `us`.
pub(crate) fn range(src: usize, len: usize, us: u64, kind: u32) {
    let mut map = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, kind));
    e.0 += 1;
    e.1 += len as u64;
    e.2 += us;
}

fn page_faults() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    u64::from(c.PageFaultCount)
}

/// Logs the counters per frame over `frames` frames, and the ranges that
/// took most bytes.
pub(crate) fn log(frames: f64) {
    crate::texture::log_streaming(frames);
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    if c[C::PendingChecks as usize] > 0.0 {
        tracing::info!("late title constants per frame: {:.1} intervals, {:.3} MB compared, {:.3} MB copied, {:.2} ms",
            c[C::PendingChecks as usize], mb(c[C::PendingReads as usize]),
            mb(c[C::PendingUpdates as usize]), ms(c[C::PendingUs as usize]));
    }
    tracing::info!(
        "uncached partial-page refreshes per frame: {:.1} required copies, {:.2} MB",
        c[C::PartialRefreshes as usize], mb(c[C::PartialBytes as usize]),
    );
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
    );
    let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let buckets = PRIVATE_BUCKETS.each_ref().map(|b| {
        (b[0].swap(0, Relaxed) as f64 / frames, b[1].swap(0, Relaxed) as f64 / frames / 1e6)
    });
    tracing::info!(?buckets, "private watch (calls/frame, ms/frame) for <=64KiB, 256KiB, 1MiB, 8MiB, 64MiB, larger");
    let same = WATCH_COVERED[0].swap(0, Relaxed) as f64 / frames;
    let time = WATCH_COVERED[1].swap(0, Relaxed) as f64 / frames / 1e6;
    tracing::info!("private watches fully covered by an earlier query in the same submission since its last title write: {same:.1} calls/frame {time:.2} ms/frame");
    tracing::info!(
        "watch phases per frame over {frames} frames: GetWriteWatch success {:.1} calls {:.3} ms, failure {:.1} calls {:.3} ms; fallback guard {:.1} calls {:.3} ms including {:.3} ms checking allocation type",
        c[C::KernelWatchHits as usize], c[C::KernelWatchHitNs as usize] / 1e6,
        c[C::KernelWatchMisses as usize], c[C::KernelWatchMissNs as usize] / 1e6,
        c[C::GuardCalls as usize], c[C::GuardNs as usize] / 1e6,
        c[C::GuardTypeNs as usize] / 1e6,
    );
    tracing::info!(
        "sparse host uploads per frame over {frames} frames: {:.1} fallbacks {:.2} ms, {:.2} MB of confirmed holes skipped",
        c[C::HostFallbacks as usize],
        ms(c[C::HostFallbackUs as usize]),
        mb(c[C::UnprobedHoleBytes as usize]),
    );
    let mut ranges: Vec<_> = ranges.into_iter().flatten().collect();
    ranges.sort_by_key(|&(_, (_, bytes, _, _))| std::cmp::Reverse(bytes));
    for ((src, len), (n, bytes, us, _)) in ranges.into_iter().take(10) {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        tracing::info!(
            "  copied from {src:#x}+{len:#x} (memory {kind:#x}): {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
}
