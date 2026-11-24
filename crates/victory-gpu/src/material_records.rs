//! An opt-in material residency experiment. The three native pixel programs
//! below fetch descriptor indices from root table 0 (688 dwords), using
//! the byte offset in root constant 4, word 1. Only actual placed BC SRVs
//! can be prepared. This never changes a shader index or its material data.
use super::{HostList, Root, State, This};
use crate::descriptor::View;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use victory_shader::{ResourceType, RootType};

static MODE: AtomicU8 = AtomicU8::new(0);
static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);
pub(crate) fn poll(folder: &std::path::Path) {
    let path = folder.join("material-mode.txt");
    let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut prev = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *prev == m {
        return;
    }
    *prev = m;
    let mut text = String::new();
    if m.is_some() {
        let Ok(n) = std::fs::File::open(path).and_then(|f| f.take(17).read_to_string(&mut text))
        else {
            return;
        };
        if n > 16 {
            return;
        }
    }
    let mode = match text.trim() {
        "" | "off" => 0,
        "observe" => 1,
        "apply" => 2,
        "gpu" => 3,
        _ => return,
    };
    MODE.store(mode, Ordering::Release);
    tracing::info!(mode, "updated directly indexed material texture experiment");
}

type ReadSet = (u64, HashSet<(u64, u64)>);
static OBSERVED: LazyLock<Mutex<ReadSet>> = LazyLock::new(|| Mutex::new((0, HashSet::new())));
fn observe_once(frame: u64, hash: u64, address: u64) -> bool {
    if !crate::capture::trace_window(frame) {
        return false;
    }
    let mut seen = OBSERVED.lock().unwrap_or_else(|e| e.into_inner());
    if frame > seen.0 {
        seen.0 = frame;
        seen.1.clear()
    }
    frame == seen.0 && seen.1.len() < 32 && seen.1.insert((hash, address))
}

// Lists recorded before a marked present are common in the material pass.
// Report each distinct reason once per shader in this short recording window.
fn skipped(frame: u64, hash: u64, reason: &'static str) {
    if !crate::capture::trace_window(frame) {
        return;
    }
    type Failures = (u64, HashSet<(u64, &'static str)>);
    static FAILURES: LazyLock<Mutex<Failures>> = LazyLock::new(|| Mutex::new((0, HashSet::new())));
    let mut prior = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    if frame > prior.0 {
        prior.0 = frame;
        prior.1.clear();
    }
    if frame == prior.0 && prior.1.insert((hash, reason)) {
        tracing::info!(
            record_frame = frame,
            hash = format_args!("{hash:016x}"),
            reason,
            "deferred material observation unavailable"
        );
    }
}

// Keep optional material warming bounded, including small textures which
// take at least one D3D allocation granule. No base-address reuse assumptions.
type TextureBudget = (u64, HashSet<(u64, [u32; 14])>);
static PLACEMENTS: LazyLock<Mutex<TextureBudget>> =
    LazyLock::new(|| Mutex::new((0, HashSet::new())));
fn allow_texture(address: u64, desc: [u32; 14]) -> bool {
    let width = u64::from(desc[4]) | u64::from(desc[5]) << 32;
    let height = u64::from(desc[6]);
    let depth = desc[7] & 0xffff;
    let mips = desc[7] >> 16;
    if address == 0
        || !(1..=4096).contains(&width)
        || !(1..=4096).contains(&height)
        || depth != 1
        || !(1..=14).contains(&mips)
    {
        return false;
    }
    let bytes = (width * height * 2).max(65536);
    let mut b = PLACEMENTS.lock().unwrap_or_else(|e| e.into_inner());
    if b.1.contains(&(address, desc)) {
        return true;
    }
    if b.1.len() >= 256 || b.0 + bytes > 512 << 20 {
        return false;
    }
    b.1.insert((address, desc));
    b.0 += bytes;
    true
}

#[derive(Debug, Clone)]
pub(super) struct Pending {
    hash: u64,
    address: u64,
    view_start: u64,
    view_size: u64,
    heap: Arc<HeapPin>,
}

// The title may release this heap before its recorded list retires.
#[derive(Debug)]
struct HeapPin(usize);
impl HeapPin {
    fn new(id: usize) -> Arc<Self> {
        crate::object::add_ref(id as This);
        Arc::new(Self(id))
    }
}
impl Drop for HeapPin {
    fn drop(&mut self) {
        crate::object::release_boxed::<crate::descriptor::Heap>(self.0 as This);
    }
}

pub(crate) struct CompletedRecord {
    heap: Arc<HeapPin>,
    copy: Weak<crate::shadow::Shadow>,
    shader: u64,
    address: u64,
}
type CacheKey = (usize, u64, u64, u64);
struct Prepared {
    copy: Weak<crate::shadow::Shadow>,
    frame: u64,
    slots: Vec<(u32, u64)>,
}
static PREPARED: LazyLock<Mutex<HashMap<CacheKey, Prepared>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn prepared_slots(
    copy: &Arc<crate::shadow::Shadow>,
    heap_identity: u64,
    shader: u64,
    address: u64,
    frame: u64,
) -> Vec<(u32, u64)> {
    let cache = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .get(&(Arc::as_ptr(copy) as usize, heap_identity, shader, address))
        .filter(|entry| {
            frame.saturating_sub(entry.frame) <= 120
                && entry
                    .copy
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, copy))
        })
        .map(|entry| entry.slots.clone())
        .unwrap_or_default()
}

// Only the texture descriptors explicitly named by the completed original
// GPU program are eligible. The fence completed under the submission lock.
impl CompletedRecord {
    pub(crate) fn finish(&self, frame: u64, bytes: &[u8]) {
        if bytes.len() != 28 || self.copy.strong_count() == 0 {
            return;
        }
        let heap = crate::descriptor::heap(self.heap.0 as This);
        let Some(host) = heap.host() else { return };
        let apply = MODE.load(Ordering::Acquire) == 3;
        let mut slots = Vec::new();
        let mut observations = Vec::new();
        let mut texture_addresses = Vec::new();
        for (word, part) in bytes.chunks_exact(4).enumerate() {
            let index = u32::from_le_bytes(part.try_into().unwrap());
            let Some((key, view)) = heap.material_view(index) else {
                continue;
            };
            let Some((memory, desc, data)) = bc_texture(&view) else {
                continue;
            };
            let before = host.has_material(index, key);
            let after = before
                || apply
                    && allow_texture(memory, *desc)
                    && host.materialize_texture(index, key, &view);
            if apply && after {
                slots.push((index, key));
            }
            observations.push((word, index, data[0], desc[4], desc[6], before, after));
            texture_addresses.push((index, memory));
        }
        tracing::info!(
            frame,
            hash = format_args!("{:016x}", self.shader),
            record = format_args!("{:#x}", self.address),
            apply,
            ?observations,
            ?texture_addresses,
            heap_identity = host.material_identity,
            "registered BC descriptors named by completed GPU material record"
        );
        if !apply {
            return;
        }
        let cache_key = (
            self.copy.as_ptr() as usize,
            host.material_identity,
            self.shader,
            self.address,
        );
        let mut cache = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, entry| {
            entry.copy.strong_count() > 0 && frame.saturating_sub(entry.frame) <= 120
        });
        if cache.len() >= 512 && !cache.contains_key(&cache_key) {
            if let Some(key) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.frame)
                .map(|(key, _)| *key)
            {
                cache.remove(&key);
            }
        }
        cache.insert(
            cache_key,
            Prepared {
                copy: self.copy.clone(),
                frame,
                slots,
            },
        );
    }
}

fn bc_texture(view: &View) -> Option<(u64, &[u32; 14], &[u32; 16])> {
    let View::Placed(false, desc, data) = view else {
        return None;
    };
    let mips = desc[7] >> 16;
    let (first, levels) = (data[4], data[5]);
    let family = |f: u32| match f {
        70..=84 => Some((f - 70) / 3),
        94..=99 => Some(5 + (f - 94) / 3),
        _ => None,
    };
    if desc[0] != 3
        || data[1] != 4
        || family(data[0])? != family(desc[8])?
        || first >= mips
        || levels == 0
        || (levels != u32::MAX && levels > mips - first)
    {
        return None;
    }
    Some((u64::from(data[10]) | u64::from(data[11]) << 32, desc, data))
}
impl Pending {
    pub(super) fn record(&self, list: &HostList, frame: u64) {
        use crate::capture::buffer_sample::{Binding, SampleLocation, record_scoped_copy};
        let Some(host) = crate::host::host() else {
            return;
        };
        let Some(shadow) = crate::shadow::find(self.view_start, self.view_size) else {
            return;
        };
        let record = CompletedRecord {
            heap: self.heap.clone(),
            copy: Arc::downgrade(&shadow),
            shader: self.hash,
            address: self.address,
        };
        record_scoped_copy(
            list,
            &host.device,
            &shadow.buffer,
            shadow.offset(self.address),
            SampleLocation {
                binding: Binding {
                    hash: self.hash,
                    kind: ResourceType::Srv,
                    slot: 0,
                    offset: 688,
                    whole: true,
                },
                frame,
                address: self.address,
                size: 28,
            },
            Some(record),
        );
    }
}

fn upload_prepared(
    list: &HostList,
    heap: &crate::descriptor::Heap,
    slots: &[(u32, u64)],
    shader: u64,
) {
    use windows::core::Interface;
    let Some(native) = heap.host() else { return };
    for &(index, key) in slots {
        if let Some((current, view)) = heap.material_view(index)
            && current == key
            && native.has_material(index, key)
            && let Some(View::Shader(id, _)) = crate::descriptor::host::placed_texture(&view)
            && let Some(texture) = crate::resource::host_resource(id as This)
            && native.trace_texture(index) == Some(texture.as_raw() as usize)
        {
            if crate::texture::title_written(texture) {
                crate::texture::upload_material(list, id as This, texture);
            }
            crate::list::ensure_state(list, id as This, 0x40 | 0x80);
            let frame = crate::queue::presented() + 1;
            if crate::capture::trace_window(frame)
                && let Some((address, _, data)) = bc_texture(&view)
            {
                static SEEN: LazyLock<Mutex<(u64, HashSet<(u64, u32)>)>> =
                    LazyLock::new(|| Mutex::new((0, HashSet::new())));
                let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                if frame > seen.0 {
                    seen.0 = frame;
                    seen.1.clear();
                }
                let needed = frame == seen.0
                    && seen.1.len() < 12
                    && seen.1.insert((native.material_identity, index));
                drop(seen);
                if needed && let Some(host) = crate::host::host() {
                    use crate::capture::buffer_sample::{BcBlock, native_bc_block};
                    let spec = BcBlock {
                        heap: native.material_identity,
                        index,
                        address,
                        format: data[0],
                        mip: data[4],
                    };
                    if let Some(sample) = native_bc_block(
                        list,
                        &host.device,
                        id as This,
                        texture,
                        frame,
                        shader,
                        spec,
                    ) {
                        list.add_sample(sample);
                    }
                }
            }
        }
    }
}

pub(super) fn prepare(
    state: &State,
    list: &HostList,
    shader: &crate::pipeline::Shader,
    translated: &crate::pipeline::Translated,
) -> Option<Pending> {
    let mode = MODE.load(Ordering::Acquire);
    let frame = crate::queue::presented() + 1;
    if mode == 0 || (mode == 1 && !crate::capture::trace_window(frame)) {
        return None;
    }
    let hash = match shader.hash.as_str() {
        "94d281151ca58b4f" => 0x94d281151ca58b4f,
        "43e24028d4a1faef" => 0x43e24028d4a1faef,
        "d9fcb8a4425fc59b" => 0xd9fcb8a4425fc59b,
        _ => return None,
    };
    let Some(material) = translated.used.iter().find(|u| {
        u.kind == ResourceType::Srv
            && u.root == RootType::Table
            && u.slot == 0
            && u.table_offset_dwords == 688
    }) else {
        skipped(frame, hash, "material buffer missing from stage metadata");
        return None;
    };
    let Some(Root::Constants(words)) = state.roots_ref(false).get(4) else {
        skipped(frame, hash, "root 4 is unset or is not a constant");
        return None;
    };
    let Some(&offset) = words.get(1) else {
        skipped(frame, hash, "second material root constant unset");
        return None;
    };
    let byte_offset = u64::from(offset);
    let Some((_, _, handle, _)) = state.trace_table(false, material) else {
        skipped(
            frame,
            hash,
            "material table does not resolve inside a bound heap",
        );
        return None;
    };
    let Some((view_start, view_size)) = crate::descriptor::view(handle)
        .and_then(|view| crate::descriptor::host::buffer_range(&view))
    else {
        skipped(
            frame,
            hash,
            "material descriptor does not expose a buffer range",
        );
        return None;
    };
    if byte_offset % 4 != 0 || byte_offset.checked_add(28)? > view_size {
        skipped(
            frame,
            hash,
            "material offset outside the seven-word aligned extent",
        );
        return None;
    }
    let address = view_start.checked_add(byte_offset)?;
    let heap_id = state.heaps[0];
    if heap_id == 0 {
        return None;
    }
    let report = observe_once(frame, hash, address);
    let pending = report.then(|| Pending {
        hash,
        address,
        view_start,
        view_size,
        heap: HeapPin::new(heap_id),
    });
    let Some(shadow) = crate::shadow::find(view_start, view_size) else {
        skipped(frame, hash, "material host buffer unavailable");
        return pending;
    };
    let Some(host) = crate::host::host() else {
        return pending;
    };
    if mode == 3 {
        let heap = crate::descriptor::heap(heap_id as This);
        let Some(native) = heap.host() else {
            return pending;
        };
        let slots = prepared_slots(&shadow, native.material_identity, hash, address, frame);
        upload_prepared(list, heap, &slots, hash);
        return pending;
    }
    let Some(_idle) = host.idle_submission() else {
        skipped(frame, hash, "previous host submission in flight");
        return pending;
    };
    let Some(indices) = shadow.material_record(address) else {
        return pending;
    };
    let heap = crate::descriptor::heap(heap_id as This);
    let Some(host_heap) = heap.host() else {
        return pending;
    };
    let mut observations = Vec::new();
    for (word, &index) in indices.iter().enumerate() {
        let Some((key, view)) = heap.material_view(index) else {
            continue;
        };
        let Some((memory, desc, data)) = bc_texture(&view) else {
            continue;
        };
        let before = report && host_heap.has_material(index, key);
        if mode == 2
            && allow_texture(memory, *desc)
            && host_heap.materialize_texture(index, key, &view)
        {
            super::readable(list, material, &view);
        }
        if report {
            observations.push((
                word,
                index,
                data[0],
                desc[4],
                desc[6],
                before,
                host_heap.has_material(index, key),
            ));
        }
    }
    if report {
        tracing::info!(
            frame,
            hash = format_args!("{hash:016x}"),
            record = format_args!("{address:#x}"),
            byte_offset,
            ?indices,
            ?observations,
            mode,
            "material indices in preceding completed host submission and BC descriptor state (word/index/format/width/height/before/after)"
        );
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer_sample::{Binding, SampleLocation, record_scoped_copy};
    use crate::descriptor::{Heap, create_placed_srv, test_view_heap};
    use crate::host::{BUFFER_WRITE, host};
    use crate::shadow::{self, BufferOrigin};
    use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_COPY_DEST;

    const BLOCK: [u8; 8] = [0, 248, 224, 7, 27, 27, 27, 27];
    static ORIGINAL_SECTION: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    fn section_for_fixture(address: usize, output: &mut [u8]) -> Vec<(usize, usize)> {
        if address != ORIGINAL_SECTION.load(Ordering::Acquire) || output.len() < 8 {
            return Vec::new();
        }
        output[..8].copy_from_slice(&BLOCK);
        vec![(0, 8)]
    }

    #[test]
    #[ignore = "requires a fresh process with capture frame 1 and a D3D device"]
    fn fenced_gpu_material_record_uses_original_words_and_heap_until_retirement() {
        let host = host().expect("D3D device");
        assert!(crate::capture::listed(1));
        let title_heap = test_view_heap(8);
        let heap = crate::descriptor::heap(title_heap);
        let native = heap.host().unwrap();
        // The mapped raw bytes disagree with the original registered section.
        let data = vec![0xa5u8; 2 * 65536];
        let a = (data.as_ptr() as u64).next_multiple_of(65536);
        ORIGINAL_SECTION.store(a as usize, Ordering::Release);
        crate::resource::use_texture_page_reader(section_for_fixture);
        let mut desc = [0u32; 14];
        desc[0] = 3;
        desc[4] = 4;
        desc[6] = 4;
        desc[7] = 0x10001;
        desc[8] = 71;
        desc[9] = 1;
        desc[11] = 1;
        let mut srv = [0u32; 16];
        srv[0] = 71;
        srv[1] = 4;
        srv[2] = 5768;
        srv[5] = 1;
        srv[10] = a as u32;
        srv[11] = (a >> 32) as u32;
        for slot in [2u64, 5] {
            create_placed_srv(std::ptr::null(), &desc, &srv, heap.start + 32 * slot)
        }
        let key = heap.material_view(5).unwrap().0;
        let title_words = [0u32; 16];
        let address = title_words.as_ptr() as u64;
        shadow::register(
            address as usize,
            address,
            64,
            None,
            4,
            BufferOrigin::TitleAddress,
        );
        let copy = shadow::find(address, 64).unwrap();
        let shader = 0x43e24028d4a1faef;
        let source = CompletedRecord {
            heap: HeapPin::new(title_heap as usize),
            copy: Arc::downgrade(&copy),
            shader,
            address,
        };
        // The title releases before submission. The recorded command still owns the heap.
        crate::object::release_boxed::<Heap>(title_heap);
        let list = host.new_list().unwrap();
        let (upload, ptr) = host.upload_buffer(56).unwrap();
        let original = [5u32, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0];
        let previous = MODE.swap(3, Ordering::AcqRel);
        unsafe {
            std::ptr::copy_nonoverlapping(original.as_ptr().cast::<u8>(), ptr, 56);
            list.buffer_state(&copy.buffer, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
            list.list.CopyBufferRegion(&copy.buffer, 0, &upload, 0, 28);
            list.buffer_state(&copy.buffer, BUFFER_WRITE);
            record_scoped_copy(
                &list,
                &host.device,
                &copy.buffer,
                0,
                SampleLocation {
                    binding: Binding {
                        hash: shader,
                        kind: ResourceType::Srv,
                        slot: 0,
                        offset: 688,
                        whole: true,
                    },
                    frame: 1,
                    address,
                    size: 28,
                },
                Some(source),
            );
            list.buffer_state(&copy.buffer, D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
            list.list.CopyBufferRegion(&copy.buffer, 0, &upload, 28, 28);
        }
        list.close();
        host.execute(&[&list]);
        MODE.store(previous, Ordering::Release);
        assert!(native.has_material(5, key));
        assert!(native.trace_texture(2).is_none());
        assert_eq!(
            prepared_slots(&copy, native.material_identity, shader, address, 2),
            vec![(5, key)]
        );
        let other = test_view_heap(8);
        let other_native = crate::descriptor::heap(other).host().unwrap();
        assert!(
            prepared_slots(&copy, other_native.material_identity, shader, address, 2).is_empty()
        );
        crate::object::release_boxed::<Heap>(other);
        let placed = heap.material_view(5).unwrap().1;
        let Some(View::Shader(id, _)) = crate::descriptor::host::placed_texture(&placed) else {
            panic!("expected the original BC texture")
        };
        for expected in [BLOCK, [0u8; 8]] {
            let sampling = host.new_list().unwrap();
            upload_prepared(
                &sampling,
                heap,
                &prepared_slots(&copy, native.material_identity, shader, address, 2),
                shader,
            );
            let texture = crate::resource::host_resource(id as This).unwrap();
            let spec = crate::capture::buffer_sample::BcBlock {
                heap: native.material_identity,
                index: 5,
                address: a,
                format: 71,
                mip: 0,
            };
            let center = crate::capture::buffer_sample::native_bc_block(
                &sampling,
                &host.device,
                id as This,
                texture,
                1,
                shader,
                spec,
            )
            .unwrap();
            crate::list::ensure_state(&sampling, id as This, 0x800);
            let (buf, footprint) =
                crate::capture::read_back(&host.device, &sampling.list, texture).unwrap();
            sampling.close();
            host.execute(&[&sampling]);
            assert_eq!(center.completed_bytes().unwrap(), expected);
            let mut ptr = std::ptr::null_mut();
            let first = footprint.Offset as usize;
            let range = windows::Win32::Graphics::Direct3D12::D3D12_RANGE {
                Begin: first,
                End: first + 8,
            };
            // SAFETY: private eight-byte block within the just-fenced native readback.
            unsafe {
                buf.Map(0, Some(&range), Some(&mut ptr)).unwrap();
                let block = ptr
                    .cast::<u8>()
                    .add(first)
                    .cast::<[u8; 8]>()
                    .read_unaligned();
                buf.Unmap(0, None);
                assert_eq!(block, expected);
            }
            // Remove the registered range while the raw address remains readable.
            ORIGINAL_SECTION.store(0, Ordering::Release);
        }
        assert!(prepared_slots(&copy, native.material_identity, shader, address + 4, 2).is_empty());
        assert!(prepared_slots(&copy, native.material_identity, shader, address, 122).is_empty());
        shadow::unregister(address as usize, address);
        shadow::register(
            address as usize,
            address,
            64,
            None,
            4,
            BufferOrigin::TitleAddress,
        );
        let replacement = shadow::find(address, 64).unwrap();
        assert!(
            prepared_slots(&replacement, native.material_identity, shader, address, 2).is_empty()
        );
        shadow::unregister(address as usize, address);
        unsafe { host.device.GetDeviceRemovedReason().unwrap() };
    }
}
