//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size. Only in builds with diagnostics.

use std::path::PathBuf;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

#[path = "capture_buffer_sample.rs"]
pub(crate) mod buffer_sample;
#[path = "capture_rtv_sample.rs"]
pub(crate) mod rtv_sample;
#[path = "capture_draw_stats.rs"]
pub(crate) mod draw_stats;
#[path = "capture_light_tiles.rs"]
pub(crate) mod light_tiles;

/// The folder frames go to, if capturing.
pub(crate) fn folder() -> Option<&'static PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| std::env::var_os("VICTORY_CAPTURE").map(PathBuf::from))
        .as_ref()
}

/// Frames listed in `VICTORY_CAPTURE_FRAMES` (comma-separated): saved
/// too, and their draws traced at debug level.
pub(crate) fn listed(n: u64) -> bool {
    if n > 0 && (n == REQUESTED_FRAME.load(std::sync::atomic::Ordering::Relaxed)
        || n == REQUESTED_FOLLOWING.load(std::sync::atomic::Ordering::Relaxed)
        || n == FIRST_PREVIEW_FRAME.load(std::sync::atomic::Ordering::Relaxed)) {
        return true;
    }
    static LIST: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let list = std::env::var("VICTORY_CAPTURE_FRAMES").unwrap_or_default();
        list.split(',')
            .filter_map(|f| f.trim().parse().ok())
            .collect()
    })
    .contains(&n)
}

static REQUESTED_FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REQUESTED_FOLLOWING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FIRST_PREVIEW_FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// Command lists can be recorded several presents ahead of their submission.
const TRACE_LEAD: u64 = 3;
pub(crate) fn trace_window(record_frame: u64) -> bool {
    (record_frame..=record_frame.saturating_add(TRACE_LEAD)).any(listed)
}

pub(crate) fn first_preview(frame: u64) {
    let _ = FIRST_PREVIEW_FRAME.compare_exchange(0, frame, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
}

/// Touching capture-next.txt in the capture directory requests one full frame
/// once the game enters the desired scene, independently of startup time.
/// Writing the single word `pair` also captures its next present. Poll
/// outside the draw/dispatch path and bound the small control-file read.
pub(crate) fn poll_request(frame: u64) {
    use std::time::{Duration, Instant, SystemTime};
    if !dump_on() {
        return;
    }
    let Some(folder) = folder() else { return };
    static POLL: std::sync::Mutex<Option<(Instant, Option<SystemTime>)>> = std::sync::Mutex::new(None);
    let mut poll = POLL.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if poll.is_some_and(|(last, _)| now.duration_since(last) < Duration::from_millis(250)) {
        return;
    }
    if present_1440p_on() {
        poll_preview_target(folder);
    }
    poll_watched_shaders(folder);
    crate::shadow::poll_pending_refresh(folder);
    buffer_sample::poll(folder);
    rtv_sample::poll(folder);
    draw_stats::poll(folder);
    crate::draw::materials::poll(folder);
    let stamp = std::fs::metadata(folder.join("capture-next.txt")).and_then(|m| m.modified()).ok();
    if stamp.is_some() && stamp != poll.and_then(|(_, previous)| previous) {
        use std::io::Read;
        let mut control = [0u8; 9];
        let pair = std::fs::File::open(folder.join("capture-next.txt")).ok()
            .and_then(|f| f.take(control.len() as u64).read(&mut control).ok())
            .is_some_and(|n| n < control.len() && control[..n].trim_ascii() == b"pair");
        let next = frame + 1 + TRACE_LEAD;
        REQUESTED_FOLLOWING.store(if pair { next + 1 } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        REQUESTED_FRAME.store(next, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(frame = next, "requested render target and presentation capture");
    }
    *poll = Some((now, stamp));
}

static PREVIEW_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputBinding { Rtv(usize), Uav(u8, u32) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewPass { pub(crate) hash: u64, pub(crate) output: OutputBinding }
static PASS_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PASS: std::sync::Mutex<Option<PreviewPass>> = std::sync::Mutex::new(None);

pub(crate) fn preview_pass() -> Option<PreviewPass> {
    if !PASS_ACTIVE.load(std::sync::atomic::Ordering::Acquire) { return None }
    *PASS.lock().unwrap_or_else(|e| e.into_inner())
}

// For reusable command lists, retain complete recorded input bindings of
// specifically requested hashes even before any capture frame is armed.
static WATCH: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());
static WATCH_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn watches(stages: Option<&str>) -> bool {
    if !WATCH_ON.load(std::sync::atomic::Ordering::Relaxed) { return false; }
    stages.is_some_and(|s| WATCH.read().unwrap_or_else(|e| e.into_inner()).iter().any(|hash| s.contains(hash)))
}

fn poll_watched_shaders(folder: &std::path::Path) {
    use std::io::Read;
    static STAMP: std::sync::Mutex<Option<std::time::SystemTime>> = std::sync::Mutex::new(None);
    let path = folder.join("trace-shaders.txt");
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut last = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *last == stamp { return; }
    *last = stamp;
    let mut text = String::new();
    if stamp.is_some() {
        let Ok(n) = std::fs::File::open(&path).and_then(|f| f.take(1025).read_to_string(&mut text)) else { return; };
        if n > 1024 { return; }
    }
    let hashes: Vec<_> = text.trim_start_matches('\u{feff}').split(|c: char| c.is_ascii_whitespace() || c==',')
        .filter(|s| !s.is_empty()).map(str::to_ascii_lowercase).collect();
    if hashes.len()>32 || hashes.iter().any(|h| h.len()!=16 || !h.bytes().all(|c| c.is_ascii_hexdigit())) {
        tracing::warn!("trace-shaders.txt expects up to 32 shader hashes of 16 hex digits"); return;
    }
    let count = hashes.len();
    *WATCH.write().unwrap_or_else(|e| e.into_inner()) = hashes;
    WATCH_ON.store(count!=0, std::sync::atomic::Ordering::Release);
    tracing::info!(count,"updated shaders with persistent input tracing");
}

fn parse_preview(text: &str) -> Option<(usize, Option<PreviewPass>)> {
    let words: Vec<_> = text.split_whitespace().collect();
    let parse_hash = |h: &str| (h.len() == 16).then(|| u64::from_str_radix(h, 16).ok()).flatten();
    match words.as_slice() {
        ["auto"] => Some((0, None)),
        [h] => Some((usize::from_str_radix(h.strip_prefix("0x")?, 16).ok().filter(|n| *n != 0)?, None)),
        ["pass", h, "rtv", n] => Some((0, Some(PreviewPass { hash: parse_hash(h)?, output: OutputBinding::Rtv(n.parse().ok()?) }))),
        ["pass", h, "uav", slot, offset] => Some((0, Some(PreviewPass { hash: parse_hash(h)?, output: OutputBinding::Uav(slot.parse().ok()?, offset.parse().ok()?) }))),
        _ => None,
    }
}

/// A matching ID must be observed through note_target during the current frame.
/// Input is an identifier from a target PNG; it is never used as an address.
fn poll_preview_target(folder: &std::path::Path) {
    use std::io::Read;
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::Mutex<Option<std::time::SystemTime>> = std::sync::Mutex::new(None);
    let path = folder.join("preview-target.txt");
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == stamp { return; }
    *last = stamp;
    let mut text = String::new();
    if stamp.is_none() { text.push_str("auto"); } else {
        let Ok(n) = std::fs::File::open(path).and_then(|f| f.take(65).read_to_string(&mut text)) else { return };
        if n > 64 { return }
    }
    let raw = text.trim().trim_start_matches('\u{feff}');
    if let Some((id, pass)) = parse_preview(raw) {
        *PASS.lock().unwrap_or_else(|e| e.into_inner()) = pass;
        PASS_ACTIVE.store(pass.is_some(), std::sync::atomic::Ordering::Release);
        PREVIEW_ID.store(id, Relaxed);
        let _ = LAST_1440P.lock().unwrap_or_else(|e| e.into_inner()).take();
        tracing::info!(resource = format_args!("{id:#x}"), ?pass, "requested live 1440p preview target");
    } else {
        tracing::warn!("preview-target.txt expects auto, a captured 0x ID, pass HASH rtv INDEX, or pass HASH uav SLOT OFFSET_DWORDS");
    }
}

/// A submitted producer of the selected shader and output attachment. Limit this
/// diagnostic to color surfaces supported by the original 1440p preview.
pub(crate) fn note_pass_target(pass: PreviewPass, id: usize, copy: &std::sync::Arc<crate::resource::HostTexture>) {
    if preview_pass() != Some(pass) { return; }
    let d = unsafe { copy.texture.GetDesc() };
    if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && d.SampleDesc.Count == 1
        && d.Width == 2560 && d.Height == 1440 && matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        *LAST_1440P.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, copy.clone()));
    }
}

#[derive(Default)]
struct PassTrace { lines: String, issued: usize, skipped: usize, omitted: usize, record_us: u64 }

#[derive(Clone)]
struct Writer {
    record_frame: u64,
    op: &'static str,
    stages: Option<std::sync::Arc<str>>,
    output: Option<OutputBinding>,
}
type OutputHistory = std::collections::BTreeMap<u64, std::collections::BTreeMap<usize, Writer>>;
static HISTORY: std::sync::LazyLock<std::sync::Mutex<OutputHistory>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(OutputHistory::new()));

/// Last submitted command capable of writing each concrete RTV/UAV or copy
/// destination. Keep four presents including the current one. No pixel-coverage
/// claim is made for an indirect or conditionally-executed shader.
pub(crate) fn remember_writes(frame: u64, record_frame: u64, op: &'static str,
    stages: Option<&std::sync::Arc<str>>, outputs: impl Iterator<Item=(usize, Option<OutputBinding>)>) {
    let mut outputs = outputs.peekable();
    if outputs.peek().is_none() { return; }
    let mut frames = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    while frames.first_key_value().is_some_and(|(old,_)| old.saturating_add(3)<frame) {
        frames.pop_first();
    }
    let writes = frames.entry(frame).or_default();
    for (resource, output) in outputs {
        if writes.len() < 8192 || writes.contains_key(&resource) {
            writes.insert(resource, Writer { record_frame, op, stages: stages.cloned(), output });
        }
    }
}

pub(crate) fn save_history(frame: u64) {
    use std::io::Write;
    let Some(folder) = folder() else { return; };
    let history = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let saved = (|| -> std::io::Result<()> {
        let file = std::fs::File::create(folder.join(format!("frame-{frame}-history.tsv")))?;
        let mut file = std::io::BufWriter::new(file);
        writeln!(file, "submit_frame\trecord_frame\tresource\tcommand\toutput\tstages")?;
        for (submitted, writes) in history.range(frame.saturating_sub(3)..=frame) {
            for (resource, w) in writes {
                let slot = match w.output {
                    Some(OutputBinding::Rtv(n)) => format!("rtv {n}"),
                    Some(OutputBinding::Uav(s,o)) => format!("uav {s} {o}"),
                    None => "destination".to_owned(),
                };
                writeln!(file, "{submitted}\t{}\t{resource:#x}\t{}\t{slot}\t{}", w.record_frame, w.op, w.stages.as_deref().unwrap_or(""))?;
            }
        }
        file.flush()
    })();
    if let Err(error) = saved { tracing::warn!(frame, %error, "failed to save recent submitted writers"); }
}
static PASSES: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<u64, PassTrace>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

pub(crate) fn transfer(list: &crate::host::HostList, op: &'static str, src: usize, dst: usize, issued: bool) {
    let frame = crate::queue::presented() + 1;
    if !dump_on() { return; }
    list.defer_host(move || {
        if issued {
            remember_writes(crate::queue::presented() + 1, frame, op, None, std::iter::once((dst, None)));
        }
        if listed(crate::queue::presented() + 1) {
            let bindings = format!("src={src:#x} dst={dst:#x}");
            submitted_pass(frame, op, issued, 0, "", &bindings, 0);
        }
    });
}

pub(crate) fn submitted_pass(record_frame: u64, op: &str, issued: bool, pipeline: usize, rtvs: &str, bindings: &str, us: u64) {
    use std::fmt::Write;
    let frame = crate::queue::presented() + 1;
    if !listed(frame) { return }
    let mut all = PASSES.lock().unwrap_or_else(|e| e.into_inner());
    let trace = all.entry(frame).or_default();
    if issued { trace.issued += 1; } else { trace.skipped += 1; }
    trace.record_us += us;
    let ordinal = trace.issued + trace.skipped;
    // Reserve tail space for explicitly watched passes, which include
    // composition and upscaling after the long geometry and lighting phases.
    let size = trace.lines.len() + rtvs.len() + bindings.len() + 160;
    if size > 12 * 1024 * 1024 || (size > 8 * 1024 * 1024 && !watches(Some(bindings))) {
        trace.omitted += 1;
        return;
    }
    let _ = writeln!(trace.lines, "{ordinal}\t{record_frame}\t{op}\t{issued}\t{pipeline:#x}\t{rtvs}\t{bindings}");
}

pub(crate) fn save_passes(frame: u64) {
    use std::io::Write;
    let Some(folder) = folder() else { return };
    let Some(trace) = PASSES.lock().unwrap_or_else(|e| e.into_inner()).remove(&frame) else { return };
    let saved = std::fs::File::create(folder.join(format!("frame-{frame}-passes.tsv"))).and_then(|mut f| {
        writeln!(f,"# submitted lists; issued={} dropped={} omitted={} instrumentation_record_cpu_us={}",trace.issued,trace.skipped,trace.omitted,trace.record_us)?;
        writeln!(f,"ordinal\trecord_frame\tcommand\tissued_on_host\tpipeline\tRTV_slots\tshader_declared_resources")?;
        f.write_all(trace.lines.as_bytes())
    });
    if let Err(error) = saved { tracing::warn!(frame, %error, "failed to save submitted pass trace"); }
}

/// Whether frame `n` is saved.
pub(crate) fn wanted(n: u64) -> bool {
    folder().is_some() && (n.is_power_of_two() || listed(n))
}

/// Saves 8-bit RGBA or BGRA rows as `<name>.png`, at half size.
fn save_as(name: &str, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    let Some(folder) = folder() else { return };
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    // Filter byte 0 per row, then RGB of every other pixel of every other row.
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &header);
    chunk(
        &mut png,
        b"IDAT",
        &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 3),
    );
    chunk(&mut png, b"IEND", &[]);
    let path = folder.join(format!("{name}.png"));
    match std::fs::write(&path, png) {
        Ok(()) => tracing::info!(path = %path.display(), "frame saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame not saved"),
    }
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

/// Saves a presentation texture. When comparing the copy, hash every pixel,
/// including alpha, at full precision and resolution, omitting row padding.
pub(crate) fn save_read_back(
    name: &str,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    compare: bool,
) -> Option<blake3::Hash> {
    let f = footprint.Footprint;
    let pixel_bytes = match f.Format.0 {
        9..=14 => 8,
        23..=32 | 87 | 90 | 91 => 4,
        format => {
            tracing::warn!(format, "unknown present readback format");
            return None;
        }
    };
    let pitch = f.RowPitch as usize;
    let row_bytes = f.Width as usize * pixel_bytes;
    let height = f.Height as usize;
    let length = (height.checked_sub(1)?).checked_mul(pitch)?.checked_add(row_bytes)?;
    // SAFETY: this dedicated readback buffer contains one completed copy.
    // GetCopyableFootprints need not reserve padding after its last row.
    unsafe {
        if row_bytes > pitch || footprint.Offset != 0 || length as u64 > buffer.GetDesc().Width {
            return None;
        }
        let mut data = std::ptr::null_mut();
        buffer.Map(0, None, Some(&mut data)).ok()?;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), length);
        let digest = compare.then(|| {
            let mut h = blake3::Hasher::new();
            for row in rows.chunks(pitch) {
                h.update(&row[..row_bytes]);
            }
            h.finalize()
        });
        match f.Format.0 {
            27..=32 | 87 | 90 | 91 => save_as(name, f.Width, f.Height, pitch, matches!(f.Format.0, 87 | 90 | 91), rows),
            format => {
                if let Some(rgba) = to_rgba(format as u32, f.Width as usize, height, pitch, rows) {
                    save_as(name, f.Width, f.Height, f.Width as usize * 4, false, &rgba);
                }
            }
        }
        buffer.Unmap(0, None);
        digest
    }
}

/// With `VICTORY_DUMP_TARGETS` set: title textures the frames listed in
/// `VICTORY_CAPTURE_FRAMES` render to or write, in first-use order, saved
/// at that frame's present as `frame-<n>-<i>-<resource>-<w>x<h>-f<format>.png`
/// (what each holds at the end of the frame; formats normalized to show
/// structure: floats scaled by their largest value, depth by its range).
/// The host copies are held: the title may release a target before the
/// frame is presented.
static TARGETS: std::sync::Mutex<Vec<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(Vec::new());

/// With `VICTORY_PRESENT_1440P` set: the latest `2560x1440` color render target
/// (`f28`/`f29`/`f87`/`f91`) written during the current frame, substituted as
/// plane 0 at present time when the title's `3840x2160` upscale pass outputs zero.
static LAST_1440P: std::sync::Mutex<Option<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(None);

fn present_1440p_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESENT_1440P").is_some())
}

/// Takes the latest `2560x1440` color render target noted this frame, if
/// `VICTORY_PRESENT_1440P` is enabled.
pub(crate) fn take_1440p_target() -> Option<(usize, std::sync::Arc<crate::resource::HostTexture>)> {
    if !present_1440p_on() {
        return None;
    }
    let id = PREVIEW_ID.load(std::sync::atomic::Ordering::Relaxed);
    LAST_1440P.lock().unwrap_or_else(|e| e.into_inner()).take()
        .filter(|(observed, _)| id == 0 || id == *observed)
}

pub(crate) fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && std::env::var_os("VICTORY_DUMP_TARGETS").is_some())
}

/// Notes a title texture the frame being built renders to or writes.
pub(crate) fn note_target(resource: crate::object::This) {
    if resource.is_null() {
        return;
    }
    let id = PREVIEW_ID.load(std::sync::atomic::Ordering::Relaxed);
    if !PASS_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) && present_1440p_on() && (id == 0 || id == resource as usize)
        && let Some(copy) = crate::resource::host_copy(resource)
    {
        // SAFETY: a live host texture.
        let d = unsafe { copy.texture.GetDesc() };
        if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && d.SampleDesc.Count == 1
            && d.Width == 2560
            && d.Height == 1440
            && matches!(d.Format.0, 28 | 29 | 87 | 91)
        {
            *LAST_1440P.lock().unwrap_or_else(|e| e.into_inner()) = Some((resource as usize, copy));
        }
    }
    if !dump_on() || !listed(crate::queue::presented() + 1) {
        return;
    }
    let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !targets.iter().any(|&(r, _)| r == resource as usize)
        && let Some(copy) = crate::resource::host_copy(resource)
    {
        targets.push((resource as usize, copy));
    }
}

/// At the present of frame `frame`: saves the noted targets.
pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(host) = crate::host::host() else { return };
    for (i, (resource, copy)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            continue;
        }
        // Every submission has run; the dump's list moves it there and back.
        let state = copy.state.load(Relaxed);
        dump_one(host, texture, state, frame, i, *resource as crate::object::This, &d);
    }
}

fn dump_one(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
    frame: u64,
    i: usize,
    resource: crate::object::This,
    d: &D3D12_RESOURCE_DESC,
) {
    let Ok(list) = host.new_list() else { return };
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = read_back(&host.device, &list.list, texture);
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        read
    };
    list.close();
    host.execute(&[&list]);
    let Some((buffer, footprint)) = read else { return };
    let f = footprint.Footprint;
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), buffer.GetDesc().Width as usize);
        if let Some(rgba) = to_rgba(f.Format.0 as u32, f.Width as usize, f.Height as usize, pitch, rows) {
            let name = format!(
                "frame-{frame}-{i:02}-{resource:?}-{}x{}-f{}",
                d.Width, d.Height, d.Format.0
            );
            save_as(&name, f.Width, f.Height, f.Width as usize * 4, false, &rgba);
        } else {
            tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        }
        buffer.Unmap(0, None);
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 if man != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float packed in `bits` with a 5-bit exponent and `man` mantissa bits
/// (the 11- and 10-bit floats of R11G11B10).
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 if m != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled.
fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
    // Channel values as floats, then scaled.
    type Decode = Box<dyn Fn(&[u8]) -> [f32; 3]>;
    let (n, decode): (usize, Decode) = match format {
        27..=32 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[0], p[1], p[2], 255]);
                }
            }
            return Some(out);
        }
        87..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [(v & 0x3ff) as f32, (v >> 10 & 0x3ff) as f32, (v >> 20 & 0x3ff) as f32]
        })),
        26 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [small_float(v & 0x7ff, 6), small_float(v >> 11 & 0x7ff, 6), small_float(v >> 22, 5)]
        })),
        9 | 10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i)) / 65535.0))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        15..=16 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)), f32::from_bits(u32_at(b, 1)), 0.0])),
        35..=38 => (4, Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0])),
        48..=52 => (2, Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0])),
        // Single channels (and depth planes): shown as grey.
        39..=43 => (4, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        19..=22 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        44..=47 => (4, Box::new(move |b| [(u32_at(b, 0) & 0xff_ffff) as f32; 3])),
        53..=54 => (2, Box::new(move |b| [half(u16_at(b, 0)); 3])),
        55..=59 => (2, Box::new(move |b| [f32::from(u16_at(b, 0)); 3])),
        60..=65 => (1, Box::new(move |b| [f32::from(b[0]); 3])),
        _ => return None,
    };
    let values: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| decode(px(x, y, n)))
        .collect();
    let grey = matches!(format, 19..=22 | 39..=47 | 53..=65);
    let finite = values.iter().flatten().copied().filter(|v| v.is_finite());
    let (low, high) = if grey {
        finite.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)))
    } else {
        (0.0, finite.fold(0.0f32, f32::max))
    };
    let range = (high - low).max(1e-20);
    if matches!(format, 9..=11 | 26) {
        let mut neg = 0usize;
        let mut zero = 0usize;
        let mut positive = 0usize;
        let mut nan = 0usize;
        let mut infinity = 0usize;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &c in values.iter().flatten() {
            if c.is_nan() { nan += 1; }
            else if c.is_infinite() { infinity += 1; }
            else {
                min = min.min(c); max = max.max(c);
                if c < 0.0 { neg += 1; } else if c > 0.0 { positive += 1; } else { zero += 1; }
            }
        }
        tracing::info!(format, width=w, height=h, neg, zero, positive, nan, infinity, min, max, "raw decoded RGB channel counts");
    }
    tracing::info!(format, low, high, "target values");
    for v in values {
        let c = v.map(|c| (((c - low) / range).clamp(0.0, 1.0) * 255.0) as u8);
        out.extend([c[0], c[1], c[2], 255]);
    }
    Some(out)
}

#[cfg(test)]
mod raw_formats {
    use super::{half, small_float, to_rgba};

    #[test]
    fn preserves_finite_zero_infinite_and_nan_channels() {
        assert_eq!(half(0x0001), 2.0f32.powi(-24));
        assert_eq!(half(0xbc00), -1.0);
        assert_eq!(half(0x8000).to_bits(), (-0.0f32).to_bits());
        assert_eq!(half(0x7c00), f32::INFINITY);
        assert_eq!(half(0xfc00), f32::NEG_INFINITY);
        assert!(half(0x7e00).is_nan() && half(0xfc01).is_nan());
        for mantissa in [5,6] {
            assert_eq!(small_float(31 << mantissa, mantissa), f32::INFINITY);
            assert!(small_float((31 << mantissa) + 1, mantissa).is_nan());
        }
        let unorm = [0xff,0xff, 0,0x80, 0,0, 0,0];
        assert_eq!(to_rgba(11, 1, 1, 8, &unorm).unwrap(), [255,127,0,255]);
    }
}

/// Pixels per side of the grid [`probe`] reads.
const GRID: u32 = 4;
/// Bytes between the grid's pixels in the readback buffer (placement
/// alignment).
const SLOT: u64 = 512;

/// Records copies of a grid of pixels of `texture` (8-bit color, in the
/// copy-source state) into a readback buffer, to tell black frames.
pub(crate) fn probe(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<ID3D12Resource> {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if !matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: SLOT * u64::from(GRID * GRID),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        for i in 0..GRID * GRID {
            let (x, y) = (
                (2 * (i % GRID) + 1) * d.Width as u32 / (2 * GRID),
                (2 * (i / GRID) + 1) * d.Height / (2 * GRID),
            );
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: SLOT * u64::from(i),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: d.Format,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let pixel = D3D12_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
        }
        Some(buffer)
    }
}

/// Whether every pixel [`probe`] read is (near) black.
pub(crate) fn black(buffer: &ID3D12Resource) -> bool {
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished (every submission is waited for).
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return false;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (SLOT * u64::from(GRID * GRID)) as usize);
        let level = bytes
            .chunks(SLOT as usize)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .max()
            .unwrap_or(0);
        buffer.Unmap(0, None);
        let mut levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
        if levels.len() == 12 {
            levels.pop_front();
        }
        levels.push_back(level);
        level < 8
    }
}

/// The brightest probed channel of the last frames, oldest first: a fade
/// darkens over several, a glitch drops at once.
static LEVELS: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(crate) fn recent_levels() -> Vec<u8> {
    let levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    levels.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }
}
