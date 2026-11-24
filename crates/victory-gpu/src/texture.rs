//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    size: u64,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    external_address: bool,
    /// Original packed 16-bit format (86 or 115) when the host texture was
    /// widened to R8G8B8A8_UNORM, so detiled 2-byte texels expand into the
    /// 4-byte staging buffer.
    widened_from: Option<u32>,
    last: Mutex<Last>,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after,
/// with room to read the memory into.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    read: Vec<u8>,
    empty: bool,
}

// A single submitting thread can sample hundreds of textures. Share the
// read scratch across them and bound its retained capacity.
thread_local! {
    static STREAMED_BYTES: std::cell::RefCell<Vec<u8>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(feature = "diagnostics")]
static STREAMING: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(feature = "diagnostics")]
pub(crate) fn log_streaming(frames: f64) {
    use std::sync::atomic::Ordering::Relaxed;
    let [empty, partial, full, bytes] = STREAMING
        .each_ref()
        .map(|n| n.swap(0, Relaxed) as f64 / frames);
    if empty + partial + full > 0. {
        tracing::info!(
            "registered texture reads per frame: {empty:.1} empty, {partial:.1} partial, {full:.1} full, {:.2} MB resident",
            bytes / 1e6
        );
    }
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. Leaves the texture in the copy
/// destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    upload_from(list, resource, texture, false)
}

/// Material indices recovered from a completed GPU command may only fetch
/// new payload bytes through the retained XMem section reader.
#[cfg(feature = "diagnostics")]
pub(crate) fn upload_material(list: &HostList, resource: This, texture: &ID3D12Resource) {
    if physical_reader().is_some() {
        upload_from(list, resource, texture, true);
    }
}

fn upload_from(list: &HostList, resource: This, texture: &ID3D12Resource, sections_only: bool) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging, sections_only);
}

impl Staging {
    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self, sections_only: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now(sections_only);
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self, sections_only: bool) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let Some(end) = start.checked_add(size).filter(|_| size != 0) else {
            return 0;
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if !sections_only && let Some((now, written)) = crate::watch::writes(start, end, false) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            last.read.resize(size, 0);
            crate::host::copy_mapped(last.read.as_mut_ptr(), self.memory, size, false);
            last.hash = None;
            last.empty = false;
            self.detile(&last.read);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        if (self.external_address || sections_only)
            && let Some(reader) = physical_reader()
        {
            // Unwatched raw addresses carry no ownership. Resolve their
            // bytes through XMem's registered sections, including fully
            // resident resources; never retry gaps against process memory.
            const MAX_STREAMED_BYTES: usize = 128 << 20;
            if size > MAX_STREAMED_BYTES {
                if !last.empty {
                    // SAFETY: size is the actual mapped upload allocation.
                    unsafe { self.data.write_bytes(0, self.size as usize) };
                    last.empty = true;
                }
                return 0;
            }
            last.asked = 0;
            return STREAMED_BYTES.with_borrow_mut(|bytes| {
                if bytes.try_reserve(size.saturating_sub(bytes.len())).is_err() {
                    return 0;
                }
                if bytes.len() < size {
                    bytes.resize(size, 0);
                }
                let bytes = &mut bytes[..size];
                let intervals = reader(start, bytes);
                if intervals.is_empty() {
                    #[cfg(feature = "diagnostics")]
                    if sections_only {
                        self.note_material_bytes(bytes, &intervals, 0, None);
                    }
                    #[cfg(feature = "diagnostics")]
                    STREAMING[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if !last.empty {
                        bytes.fill(0);
                        self.detile(bytes);
                        last.hash = None;
                        last.empty = true;
                    }
                    return 0;
                }
                let (hash, resident) = hash_intervals(bytes, &intervals);
                #[cfg(feature = "diagnostics")]
                if sections_only {
                    self.note_material_bytes(bytes, &intervals, resident, Some(&hash));
                }
                #[cfg(feature = "diagnostics")]
                {
                    STREAMING[if resident == size { 2 } else { 1 }]
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    STREAMING[3].fetch_add(resident as u64, std::sync::atomic::Ordering::Relaxed);
                    crate::copystat::add(crate::copystat::C::TexHashed, resident as u64);
                }
                if last.hash != Some(hash) {
                    zero_gaps(bytes, &intervals);
                    self.detile(bytes);
                    last.hash = Some(hash);
                    last.empty = false;
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::TexChanged, resident as u64);
                }
                resident
            });
        }
        if sections_only || !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return 0;
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// Summarize the data already returned by the registered physical reader.
    /// Does not query a title address, allocate a readback, or sample a gap.
    #[cfg(feature = "diagnostics")]
    fn note_material_bytes(
        &self,
        bytes: &[u8],
        intervals: &[(usize, usize)],
        resident: usize,
        hash: Option<&blake3::Hash>,
    ) {
        use std::collections::HashSet;
        let frame = crate::queue::presented() + 1;
        if !crate::capture::listed(frame) {
            return;
        }
        static OBSERVED: std::sync::LazyLock<Mutex<(u64, HashSet<usize>)>> =
            std::sync::LazyLock::new(|| Mutex::new((0, HashSet::new())));
        let address = self.memory as usize;
        let mut seen = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
        if frame != seen.0 {
            seen.0 = frame;
            seen.1.clear();
        }
        if seen.1.len() >= 24 || !seen.1.insert(address) {
            return;
        }
        drop(seen);
        let mut blocks = 0;
        let mut nonzero = 0;
        let mut distinct = HashSet::new();
        let block_size = self.layout.desc.element_bytes as usize;
        if matches!(block_size, 8 | 16) && resident >= block_size {
            // Evenly spaced offsets in the tiled layout. Only inspect full BC blocks
            // whose ownership intervals include them, with at most 64 samples.
            let total = bytes.len() / block_size;
            let samples = total.min(64);
            for i in 0..samples {
                let off = (i * total / samples) * block_size;
                let ix = intervals.partition_point(|&(_, end)| end <= off);
                if intervals
                    .get(ix)
                    .is_some_and(|&(begin, end)| begin <= off && off + block_size <= end)
                {
                    let block = &bytes[off..off + block_size];
                    blocks += 1;
                    nonzero += usize::from(block.iter().any(|&b| b != 0));
                    distinct.insert(block);
                }
            }
        }
        tracing::info!(frame, address=format_args!("{address:#x}"),width=self.layout.desc.width,height=self.layout.desc.height,
            tiled_bytes=bytes.len(),resident,intervals=intervals.len(),blocks,nonzero,distinct=distinct.len(),digest=?hash,
            "fenced material texture bytes from retained physical sections");
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        let mut packed = Vec::new();
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let lines = rows as usize * f.Depth as usize;
            let len = if self.widened_from.is_some() && lines > 0 {
                // The last subresource allocation stops at the last row's used bytes.
                pitch * (lines - 1) + f.Width as usize * 4
            } else {
                pitch * lines
            };
            let Some(remaining) = self.size.checked_sub(footprint.Offset) else {
                return;
            };
            let len = len.min(remaining as usize);
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if let Err(e) = detile_subresource(
                &self.layout,
                tiled,
                mip,
                slice,
                linear,
                pitch,
                rows as usize,
                f.Depth as usize,
                self.widened_from,
                &mut packed,
            ) {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn detile_subresource(
    layout: &victory_formats::texture::Layout,
    tiled: &[u8],
    mip: u32,
    slice: u32,
    linear: &mut [u8],
    row_pitch: usize,
    rows: usize,
    depth: usize,
    widened_from: Option<u32>,
    packed: &mut Vec<u8>,
) -> Result<(), victory_formats::texture::Error> {
    let Some(packed_format) = widened_from else {
        return layout.detile(tiled, mip, slice, linear, row_pitch, row_pitch * rows);
    };
    let width = layout
        .mips
        .get(mip as usize)
        .map_or(0, |m| m.width as usize);
    let packed_pitch = width * 2;
    let packed_plane = packed_pitch * rows;
    packed.resize(packed_plane * depth, 0);
    layout.detile(tiled, mip, slice, packed, packed_pitch, packed_plane)?;
    crate::resource::expand_packed16_rows(
        packed_format,
        width,
        rows * depth,
        packed,
        packed_pitch,
        linear,
        row_pitch,
    );
    Ok(())
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc, external_address) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let title_format = crate::resource::pc_format(desc.format);
    let widened_from = (d.Format.0 as u32 == crate::resource::RGBA8_UNORM
        && crate::resource::PACKED_16.contains(&title_format))
    .then_some(title_format);
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        buffer,
        size: total,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        memory,
        external_address,
        widened_from,
        last: Mutex::default(),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

    #[test]
    fn detile_widened_packed16_expands_into_rgba8_staging() {
        for format in crate::resource::PACKED_16 {
            let desc = TextureDesc {
                dimension: Dimension::Tex2d,
                width: 17,
                height: 9,
                depth_or_array: 1,
                mips: 2,
                format,
                samples: 1,
                swizzle: Swizzle::SW_4KB_S,
            };
            let tex = Texture::new(&desc).unwrap();
            let layout = &tex.planes[0].layout;
            let mut tiled = vec![0u8; layout.size as usize];
            let mut expected_mips = Vec::new();

            for mip in 0..2u32 {
                let m = layout.mips[mip as usize];
                let row_bytes = m.width as usize * 2;
                let mut src_mip = vec![0u8; row_bytes * m.height as usize];
                for y in 0..m.height as usize {
                    for x in 0..m.width as usize {
                        let px = ((mip as u16 + 1) * 0x1111)
                            .wrapping_add((y as u16) * 0x0103 + x as u16);
                        let at = y * row_bytes + x * 2;
                        src_mip[at..at + 2].copy_from_slice(&px.to_le_bytes());
                    }
                }
                layout
                    .tile(&mut tiled, mip, 0, &src_mip, row_bytes, src_mip.len())
                    .unwrap();
                expected_mips.push(src_mip);
            }

            let mut packed = Vec::new();
            for (mip, src_mip) in expected_mips.iter().enumerate() {
                let m = layout.mips[mip];
                let width = m.width as usize;
                let rows = m.height as usize;
                let row_pitch = (width * 4).next_multiple_of(256);
                let exact_len = row_pitch * (rows - 1) + width * 4;
                let mut staging = vec![0x5au8; exact_len];
                detile_subresource(
                    layout,
                    &tiled,
                    mip as u32,
                    0,
                    &mut staging,
                    row_pitch,
                    rows,
                    1,
                    Some(format),
                    &mut packed,
                )
                .unwrap();
                for y in 0..rows {
                    let mut expected = vec![0u8; width * 4];
                    crate::resource::expand_packed16(
                        format,
                        &src_mip[y * width * 2..][..width * 2],
                        &mut expected,
                    );
                    assert_eq!(
                        &staging[y * row_pitch..][..width * 4],
                        &expected[..],
                        "format {format} mip {mip} row {y}"
                    );
                }
            }
        }
    }
}

fn physical_reader() -> Option<crate::resource::TexturePageReader> {
    #[cfg(test)]
    if sparse_tests::registered() {
        return Some(sparse_tests::read_registered_fixture);
    }
    crate::resource::TEXTURE_PAGE_READER.get().copied()
}
/// Hashes only the resident byte intervals (and their bounds) and returns `(hash, resident_bytes)`.
fn hash_intervals(data: &[u8], intervals: &[(usize, usize)]) -> (blake3::Hash, usize) {
    let mut hasher = blake3::Hasher::new();
    let mut resident_bytes = 0usize;
    for &(start, end) in intervals {
        hasher.update(&(start as u64).to_le_bytes());
        hasher.update(&(end as u64).to_le_bytes());
        hasher.update(&data[start..end]);
        resident_bytes += end - start;
    }
    (hasher.finalize(), resident_bytes)
}

/// Zeroes unmapped gaps between sorted `intervals` in `data` so evicted tiles leave no stale bytes.
fn zero_gaps(data: &mut [u8], intervals: &[(usize, usize)]) {
    let mut cursor = 0usize;
    for &(start, end) in intervals {
        if cursor < start {
            data[cursor..start].fill(0);
        }
        cursor = end;
    }
    if cursor < data.len() {
        data[cursor..].fill(0);
    }
}

#[cfg(test)]
mod sparse_tests {
    use super::*;

    thread_local! {
        static REGISTRATION: std::cell::Cell<Option<(usize,usize)>> = const { std::cell::Cell::new(None) };
    }
    pub(super) fn registered() -> bool {
        REGISTRATION.get().is_some()
    }

    // The GPU-only crate's fixture has no HLE section allocator. Synthetic
    // registration restricts the CPU and WARP tests to their own reservation.
    // The companion XMem test exercises the real production section reader.
    pub(super) fn read_registered_fixture(start: usize, dst: &mut [u8]) -> Vec<(usize, usize)> {
        let Some((base, n)) = REGISTRATION.get() else {
            return vec![];
        };
        if start < base
            || start
                .checked_add(dst.len())
                .is_none_or(|end| end > base + n)
        {
            return vec![];
        }
        let mut result: Vec<(usize, usize)> = Vec::new();
        for offset in (0..dst.len()).step_by(4096) {
            let next = (offset + 4096).min(dst.len());
            if crate::host::read_into((start + offset) as *const u8, &mut dst[offset..next]) {
                if let Some(last) = result.last_mut()
                    && last.1 == offset
                {
                    last.1 = next
                } else {
                    result.push((offset, next))
                }
            }
        }
        result
    }
    #[derive(Debug, PartialEq, Eq)]
    enum Residency {
        Unmapped,
        Full,
        Partial(Vec<(usize, usize)>),
    }
    fn read_resident(src: *const u8, dst: &mut [u8]) -> Residency {
        let ranges = read_registered_fixture(src as usize, dst);
        if ranges.is_empty() {
            Residency::Unmapped
        } else if ranges == [(0, dst.len())] {
            Residency::Full
        } else {
            Residency::Partial(ranges)
        }
    }

    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree,
    };

    /// RAII guard for a `VirtualAlloc(..., MEM_RESERVE, ...)` address range.
    struct VirtualRegion {
        ptr: *mut u8,
        len: usize,
    }

    impl VirtualRegion {
        fn reserve(len: usize) -> Self {
            // SAFETY: reserves uncommitted virtual address space of `len` bytes, released on drop.
            let ptr = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE, PAGE_NOACCESS) }
                .cast::<u8>();
            assert!(!ptr.is_null(), "VirtualAlloc MEM_RESERVE failed");
            REGISTRATION.set(Some((ptr as usize, len)));
            Self { ptr, len }
        }

        fn commit(&self, offset: usize, len: usize) {
            assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
            // SAFETY: commits pages within our reserved virtual address range.
            let committed = unsafe {
                VirtualAlloc(self.ptr.add(offset).cast(), len, MEM_COMMIT, PAGE_READWRITE)
            };
            assert!(!committed.is_null(), "VirtualAlloc MEM_COMMIT failed");
        }

        fn decommit(&self, offset: usize, len: usize) {
            assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
            // SAFETY: decommits pages within our reserved virtual address range.
            let ok = unsafe { VirtualFree(self.ptr.add(offset).cast(), len, MEM_DECOMMIT) };
            assert_ne!(ok, 0, "VirtualFree MEM_DECOMMIT failed");
        }

        /// Writes `bytes` at `offset`; caller must ensure `[offset..offset + bytes.len())` is committed.
        unsafe fn write_slice(&self, offset: usize, bytes: &[u8]) {
            assert!(
                offset
                    .checked_add(bytes.len())
                    .is_some_and(|end| end <= self.len)
            );
            // SAFETY: caller guarantees the destination range is committed and writable.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), bytes.len());
            }
        }
    }

    impl Drop for VirtualRegion {
        fn drop(&mut self) {
            // SAFETY: `self.ptr` was allocated with `VirtualAlloc(..., MEM_RESERVE, ...)` and not yet freed.
            REGISTRATION.set(None);
            unsafe { VirtualFree(self.ptr.cast(), 0, MEM_RELEASE) };
        }
    }

    /// RAII guard releasing a console COM resource so `STAGING` entries are cleaned up on drop.
    struct ResourceGuard(This);

    impl Drop for ResourceGuard {
        fn drop(&mut self) {
            type Release = extern "system" fn(This) -> u32;
            crate::object::slot::<Release>(self.0, 0x10)(self.0);
        }
    }

    #[test]
    fn partial_residency_reads_only_mapped_regions_and_detects_new_mips() {
        const TILE: usize = 64 * 1024;
        const TOTAL: usize = 256 * 1024;
        let region = VirtualRegion::reserve(TOTAL);
        let mut dst = vec![0xAAu8; TOTAL];

        assert_eq!(read_resident(region.ptr, &mut dst), Residency::Unmapped);

        region.commit(0, TILE);
        let tile0: Vec<u8> = (0..TILE)
            .map(|i| ((i * 37 + 11) & 0xff) as u8 | 1)
            .collect();
        // SAFETY: `[0..TILE)` was committed above.
        unsafe { region.write_slice(0, &tile0) };

        assert_eq!(
            read_resident(region.ptr, &mut dst),
            Residency::Partial(vec![(0, TILE)])
        );
        assert_eq!(dst[..TILE], tile0[..]);
        let (hash0, bytes0) = hash_intervals(&dst, &[(0, TILE)]);
        assert_eq!(bytes0, TILE);

        // Mutating unmapped tail bytes in `dst` must not affect the resident hash.
        dst[TILE..].fill(0x55);
        assert_eq!(
            read_resident(region.ptr, &mut dst),
            Residency::Partial(vec![(0, TILE)])
        );
        let (hash0_repeat, bytes0_repeat) = hash_intervals(&dst, &[(0, TILE)]);
        assert_eq!((hash0_repeat, bytes0_repeat), (hash0, TILE));

        // Commit a disjoint second 64 KiB tile at `[128 KiB..192 KiB)`.
        region.commit(2 * TILE, TILE);
        let tile2: Vec<u8> = (0..TILE)
            .map(|i| ((i * 53 + 29) & 0xff) as u8 | 1)
            .collect();
        // SAFETY: `[2 * TILE..3 * TILE)` was committed above.
        unsafe { region.write_slice(2 * TILE, &tile2) };

        let res = read_resident(region.ptr, &mut dst);
        assert_eq!(
            res,
            Residency::Partial(vec![(0, TILE), (2 * TILE, 3 * TILE)])
        );
        assert_eq!(dst[..TILE], tile0[..]);
        assert_eq!(dst[2 * TILE..3 * TILE], tile2[..]);
        let (hash_two, bytes_two) = hash_intervals(&dst, &[(0, TILE), (2 * TILE, 3 * TILE)]);
        assert_eq!(bytes_two, 2 * TILE);
        assert_ne!(hash_two, hash0);

        // Decommit the second tile while leaving junk across unmapped gaps; `zero_gaps` must clear it.
        region.decommit(2 * TILE, TILE);
        dst[TILE..].fill(0xCC);
        let res_after_evict = read_resident(region.ptr, &mut dst);
        assert_eq!(res_after_evict, Residency::Partial(vec![(0, TILE)]));
        let Residency::Partial(intervals) = res_after_evict else {
            unreachable!();
        };
        let (hash_after_evict, bytes_after_evict) = hash_intervals(&dst, &intervals);
        assert_eq!((hash_after_evict, bytes_after_evict), (hash0, TILE));
        zero_gaps(&mut dst, &intervals);
        assert_eq!(dst[..TILE], tile0[..]);
        assert!(dst[TILE..].iter().all(|&b| b == 0));

        // Committing the remaining gaps yields `Residency::Full`.
        region.commit(TILE, TILE);
        region.commit(2 * TILE, 2 * TILE);
        assert_eq!(read_resident(region.ptr, &mut dst), Residency::Full);
    }

    fn readback_mip(
        host: &crate::host::Host,
        resource: This,
        host_tex: &ID3D12Resource,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let row_pitch = width * 4;
        assert_eq!(row_pitch % D3D12_TEXTURE_DATA_PITCH_ALIGNMENT, 0);
        let byte_len = row_pitch as usize * height as usize;

        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: byte_len as u64,
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
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a D3D12 readback buffer.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
        }
        let readback = readback.unwrap();

        let back = host.new_list().unwrap();
        crate::list::ensure_state(&back, resource, 0x800);
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&readback) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: width,
                        Height: height,
                        Depth: 1,
                        RowPitch: row_pitch,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(host_tex) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: subresource,
            },
        };
        // SAFETY: `back.list` is open and both resources are live.
        unsafe { back.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        back.close();
        host.execute(&[&back]);

        let mut mapped = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: byte_len,
        };
        // SAFETY: `host.execute` waited for GPU completion and the buffer spans `byte_len` bytes.
        unsafe {
            readback.Map(0, Some(&range), Some(&mut mapped)).unwrap();
            let out = std::slice::from_raw_parts(mapped.cast::<u8>(), byte_len).to_vec();
            readback.Unmap(0, None);
            out
        }
    }

    #[test]
    fn partially_resident_mip_tail_uploads_to_host_texture() {
        let Some(host) = crate::host::host() else {
            assert!(
                std::env::var_os("VICTORY_D3D12_WARP").is_none(),
                "requested WARP fixture requires a host"
            );
            return;
        };
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

        let tex = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: 256,
            height: 256,
            depth_or_array: 1,
            mips: 3,
            format: DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            samples: 1,
            swizzle: Swizzle::SW_64KB_S,
        })
        .unwrap();
        let layout = &tex.planes[0].layout;
        assert_eq!(layout.first_tail_mip, 2);
        assert_eq!(layout.mips[2].offset, 0);
        assert!(layout.mips[2].tail.is_some());
        assert_eq!(layout.mips[1].offset, 65536);
        assert_eq!(layout.mips[0].offset, 131072);
        assert_eq!(layout.size, 393216);

        let region = VirtualRegion::reserve(layout.size as usize);
        region.commit(0, 65536);

        let expected_mip2: Vec<u8> = (0..64 * 64 * 4)
            .map(|i| (((i * 13 + 7) % 251) as u8) | 1)
            .collect();
        let mut tiled = vec![0u8; layout.size as usize];
        layout
            .tile(&mut tiled, 2, 0, &expected_mip2, 256, 64 * 256)
            .unwrap();
        assert!(tiled[65536..].iter().all(|&b| b == 0));
        // SAFETY: tile 0 `[0..65536)` was committed above.
        unsafe { region.write_slice(0, &tiled[..65536]) };
        assert!(!committed(region.ptr, layout.size as usize));

        const _: () =
            assert!(size_of::<D3D12_RESOURCE_DESC>() == size_of::<crate::resource::ResourceDesc>());
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 3,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT(0x109),
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let mut resource = std::ptr::null();
        let hr = crate::resource::create_at_address(
            std::ptr::null(),
            region.ptr as u64,
            (&raw const desc).cast(),
            0,
            std::ptr::null(),
            &crate::resource::PAGEABLE_IIDS[0],
            &mut resource,
        );
        assert_eq!(hr, crate::object::S_OK);
        let resource_guard = ResourceGuard(resource);
        let host_tex = crate::resource::host_resource(resource_guard.0).unwrap();

        let list = host.new_list().unwrap();
        upload(&list, resource_guard.0, host_tex);
        list.close();
        host.execute(&[&list]);

        assert_eq!(
            readback_mip(host, resource_guard.0, host_tex, 2, 64, 64),
            expected_mip2
        );
        assert!(
            readback_mip(host, resource_guard.0, host_tex, 1, 128, 128)
                .iter()
                .all(|&b| b == 0)
        );

        // Stream in mip 1 while leaving mip 0 `[131072..393216)` unmapped.
        region.commit(65536, 65536);
        let expected_mip1: Vec<u8> = (0..128 * 128 * 4)
            .map(|i| (((i * 29 + 19) % 241) as u8) | 1)
            .collect();
        tiled.fill(0);
        layout
            .tile(&mut tiled, 1, 0, &expected_mip1, 512, 128 * 512)
            .unwrap();
        // SAFETY: tile 1 `[65536..131072)` was committed above.
        unsafe { region.write_slice(65536, &tiled[65536..131072]) };
        assert!(!committed(region.ptr, layout.size as usize));

        list.reset();
        upload(&list, resource_guard.0, host_tex);
        list.close();
        host.execute(&[&list]);

        assert_eq!(
            readback_mip(host, resource_guard.0, host_tex, 1, 128, 128),
            expected_mip1
        );
        assert_eq!(
            readback_mip(host, resource_guard.0, host_tex, 2, 64, 64),
            expected_mip2
        );

        // Evict mip 1: `zero_gaps` must clear stale cached bytes so host mip 1 becomes zeroed.
        region.decommit(65536, 65536);
        list.reset();
        upload(&list, resource_guard.0, host_tex);
        list.close();
        host.execute(&[&list]);

        assert!(
            readback_mip(host, resource_guard.0, host_tex, 1, 128, 128)
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(
            readback_mip(host, resource_guard.0, host_tex, 2, 64, 64),
            expected_mip2
        );
        // Full eviction clears the final resident mip. Re-streaming the
        // identical payload must upload it again.
        region.decommit(0, 65536);
        list.reset();
        upload(&list, resource_guard.0, host_tex);
        list.close();
        host.execute(&[&list]);
        assert!(
            readback_mip(host, resource_guard.0, host_tex, 2, 64, 64)
                .iter()
                .all(|&b| b == 0)
        );
        region.commit(0, 65536);
        tiled.fill(0);
        layout
            .tile(&mut tiled, 2, 0, &expected_mip2, 256, 64 * 256)
            .unwrap();
        unsafe { region.write_slice(0, &tiled[..65536]) };
        list.reset();
        upload(&list, resource_guard.0, host_tex);
        list.close();
        host.execute(&[&list]);
        assert_eq!(
            readback_mip(host, resource_guard.0, host_tex, 2, 64, 64),
            expected_mip2
        );
    }
}
