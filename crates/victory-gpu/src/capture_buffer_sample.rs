use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use victory_shader::ResourceType;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub hash: u64,
    pub kind: ResourceType,
    pub slot: u8,
    pub offset: u32,
    pub whole: bool,
}

const MAX_VIEW_BYTES: u64 = 256 * 1024;
const MAX_FRAME_BYTES: u64 = 1024 * 1024;

fn sample_bytes(binding: Binding, size: u64) -> Option<u64> {
    if size == 0 || (binding.whole && size > MAX_VIEW_BYTES) {
        None
    } else {
        Some(if binding.whole { size } else { size.min(16) })
    }
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SPECS: RwLock<Vec<Binding>> = RwLock::new(Vec::new());
static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);

pub(crate) fn poll(folder: &std::path::Path) {
    let file = folder.join("sample-buffers.txt");
    let stamp = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    let mut last = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *last == stamp {
        return;
    }
    *last = stamp;
    let mut text = String::new();
    if stamp.is_some() {
        let Ok(n) = std::fs::File::open(file).and_then(|f| f.take(513).read_to_string(&mut text))
        else {
            return;
        };
        if n > 512 {
            return;
        }
    }
    let mut specs = Vec::new();
    for line in text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        let v: Vec<_> = line.split_ascii_whitespace().collect();
        let (hash, kind, slot, offset, whole) = match v.as_slice() {
            [hash, kind, slot, offset] => (*hash, *kind, *slot, *offset, false),
            [hash, kind, slot, offset, "whole"] => (*hash, *kind, *slot, *offset, true),
            _ => return,
        };
        if hash.len() != 16 || specs.len() == 8 {
            return;
        }
        let (Ok(hash), Ok(slot), Ok(offset)) =
            (u64::from_str_radix(hash, 16), slot.parse(), offset.parse())
        else {
            return;
        };
        let kind = match kind {
            "uav" => ResourceType::Uav,
            "srv" => ResourceType::Srv,
            _ => return,
        };
        specs.push(Binding {
            hash,
            kind,
            slot,
            offset,
            whole,
        });
    }
    let count = specs.len();
    *SPECS.write().unwrap_or_else(|e| e.into_inner()) = specs;
    ACTIVE.store(count != 0, Ordering::Release);
    tracing::info!(count, "updated GPU buffer sample bindings");
}

pub(crate) fn matching(pipeline: crate::object::This, record_frame: u64) -> Vec<Binding> {
    if !ACTIVE.load(Ordering::Acquire) || !super::trace_window(record_frame) {
        return Vec::new();
    }
    SPECS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .filter(|s| crate::pipeline::contains_shader(pipeline, s.hash))
        .collect()
}

pub(crate) struct Sample {
    binding: Binding,
    record_frame: u64,
    address: u64,
    view_bytes: u64,
    bytes: usize,
    readback: ID3D12Resource,
    material: Option<crate::draw::materials::CompletedRecord>,
    bc: Option<BcBlock>,
    tile: Option<super::light_tiles::Layout>,
}

#[derive(Clone, Copy)]
pub(crate) struct BcBlock {
    pub heap: u64,
    pub index: u32,
    pub address: u64,
    pub format: u32,
    pub mip: u32,
}

/// The center BC block of the first mip the real descriptor exposes.
pub(crate) fn native_bc_block(
    list: &crate::host::HostList,
    device: &ID3D12Device,
    id: crate::object::This,
    texture: &ID3D12Resource,
    frame: u64,
    shader: u64,
    spec: BcBlock,
) -> Option<Sample> {
    let desc = unsafe { texture.GetDesc() };
    let bytes = match spec.format {
        70..=72 | 79..=81 => 8,
        73..=78 | 82..=84 | 94..=99 => 16,
        _ => return None,
    };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.DepthOrArraySize != 1
        || desc.SampleDesc.Count != 1
        || spec.mip >= u32::from(desc.MipLevels)
    {
        return None;
    }
    let width = (u32::try_from(desc.Width).ok()?.checked_shr(spec.mip)?).max(1);
    let height = (desc.Height.checked_shr(spec.mip)?).max(1);
    let (x, y) = ((width / 2) / 4 * 4, (height / 2) / 4 * 4);
    let area = D3D12_BOX {
        left: x,
        top: y,
        front: 0,
        right: (x + 4).min(width),
        bottom: (y + 4).min(height),
        back: 1,
    };
    let fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: desc.Format,
            Width: area.right - area.left,
            Height: area.bottom - area.top,
            Depth: 1,
            RowPitch: 256,
        },
    };
    let readback = readback(device, 512)?;
    crate::list::ensure_state(list, id, 0x800);
    // SAFETY: both native resources remain live and the block/footprint were bounded above.
    unsafe {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: fp,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: spec.mip,
            },
        };
        list.list
            .CopyTextureRegion(&to, 0, 0, 0, &from, Some(&area));
    }
    crate::list::ensure_state(list, id, 0x40 | 0x80);
    Some(Sample {
        binding: Binding {
            hash: shader,
            kind: ResourceType::Srv,
            slot: 0,
            offset: 0,
            whole: false,
        },
        record_frame: frame,
        address: spec.address,
        view_bytes: bytes as u64,
        bytes,
        readback,
        material: None,
        bc: Some(spec),
        tile: None,
    })
}

pub(crate) fn record(
    list: &crate::host::HostList,
    binding: Binding,
    frame: u64,
    address: u64,
    size: u64,
) {
    let Some(len) = sample_bytes(binding, size) else {
        return;
    };
    static BUDGET: Mutex<(u64, u32, u64)> = Mutex::new((0, 0, 0));
    {
        let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
        if frame > budget.0 {
            *budget = (frame, 0, 0);
        }
        if frame != budget.0 || budget.1 >= 32 || budget.2 + len > MAX_FRAME_BYTES {
            return;
        }
        budget.1 += 1;
        budget.2 += len;
    }
    // Use the same view range as the bound descriptor when ranges overlap.
    let Some(shadow) = crate::shadow::find(address, size) else {
        return;
    };
    let Some(host) = crate::host::host() else {
        return;
    };
    record_copy(
        list,
        &host.device,
        &shadow.buffer,
        shadow.offset(address),
        SampleLocation {
            binding,
            frame,
            address,
            size,
        },
    );
}

pub(crate) struct SampleLocation {
    pub binding: Binding,
    pub frame: u64,
    pub address: u64,
    pub size: u64,
}

pub(crate) fn record_copy(
    list: &crate::host::HostList,
    device: &ID3D12Device,
    source: &ID3D12Resource,
    offset: u64,
    location: SampleLocation,
) {
    record_scoped_copy(list, device, source, offset, location, None);
}

pub(crate) fn record_scoped_copy(
    list: &crate::host::HostList,
    device: &ID3D12Device,
    source: &ID3D12Resource,
    offset: u64,
    location: SampleLocation,
    material: Option<crate::draw::materials::CompletedRecord>,
) {
    let SampleLocation {
        binding,
        frame,
        address,
        size,
    } = location;
    let Some(len) = sample_bytes(binding, size) else {
        return;
    };
    let Some(before) = list.sampled_buffer_state(source) else {
        return;
    };
    let source_size = unsafe { source.GetDesc().Width };
    if len == 0 || offset.checked_add(len).is_none_or(|end| end > source_size) {
        return;
    }
    let Some(readback) = readback(device, len) else {
        return;
    };
    // SAFETY: a confirmed bounded view on an open command list.
    unsafe {
        list.buffer_state(source, crate::host::BUFFER_READ);
        list.list
            .CopyBufferRegion(&readback, 0, source, offset, len);
        list.buffer_state(source, before);
    }
    list.add_sample(Sample {
        binding,
        record_frame: frame,
        address,
        view_bytes: size,
        bytes: len as usize,
        readback,
        material,
        bc: None,
        tile: None,
    });
}

pub(super) fn readback(device: &ID3D12Device, len: u64) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
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
    let mut readback: Option<ID3D12Resource> = None;
    // SAFETY: a committed readback buffer and an issued command's confirmed, bounded host view.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut readback,
            )
            .ok()?;
        readback
    }
}

impl Sample {
    pub(super) fn light_uniform(
        frame: u64,
        table: u32,
        relative: u32,
        width: u32,
        length: u32,
        readback: ID3D12Resource,
    ) -> Self {
        Self {
            binding: Binding {
                hash: 0x48e74d16cbfa45e3,
                kind: ResourceType::Cbv,
                slot: 0,
                offset: table,
                whole: false,
            },
            record_frame: frame,
            address: u64::from(relative),
            view_bytes: u64::from(length),
            bytes: width as usize,
            readback,
            material: None,
            bc: None,
            tile: None,
        }
    }
    pub(super) fn light_tiles(
        frame: u64,
        resource: usize,
        readback: ID3D12Resource,
        layout: super::light_tiles::Layout,
    ) -> Self {
        let bytes = layout.pitch * layout.height * layout.slices;
        Self {
            binding: Binding {
                hash: u64::from_str_radix(layout.stage.hash(),16).expect("known lighting shader hash"),
                kind: if layout.stage.writes() {ResourceType::Uav} else {ResourceType::Srv},
                slot: 1,
                offset: layout.stage.offset(),
                whole: true,
            },
            record_frame: frame,
            address: resource as u64,
            view_bytes: bytes as u64,
            bytes,
            readback,
            material: None,
            bc: None,
            tile: Some(layout),
        }
    }
    pub(crate) fn completed_bytes(&self) -> Option<Vec<u8>> {
        let mut ptr = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: self.bytes,
        };
        // SAFETY: this list completed its queue fence; the dedicated readback location is held by the list.
        unsafe {
            if self.readback.Map(0, Some(&range), Some(&mut ptr)).is_err() || ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), self.bytes).to_vec();
            self.readback
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            Some(bytes)
        }
    }
    pub(crate) fn finish(&self, frame: u64) {
        if !super::listed(frame) {
            return;
        }
        if let Some(bytes) = self.completed_bytes() {
            if self.binding.hash == 0xbad01f53f53248e5 && self.binding.kind==ResourceType::Srv {
                let offsets:&[usize]=match (self.binding.slot,self.binding.offset) {
                    (0,96)=>&[0,4,4092,4096], (0,192)=>&[0x514,0x518,0x658],
                    (1,8)=>&[0x34,0x38,0x44,0x48,0x4c,0x50], _=>&[]
                };
                let words:Vec<_>=offsets.iter().filter_map(|&at|{let value=u32::from_le_bytes(bytes.get(at..at+4)?.try_into().ok()?); Some((at,value))}).collect();
                if !words.is_empty(){
                    tracing::info!(frame,record_frame=self.record_frame,slot=self.binding.slot,descriptor=self.binding.offset,?words,"initial material tile depth classifier operands");
                }
            }
            if self.binding.hash == 0x48e74d16cbfa45e3 && self.binding.kind == ResourceType::Cbv {
                let words: Vec<_> = bytes
                    .chunks_exact(4)
                    .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                    .collect();
                let floats: Vec<_> = words.iter().map(|w| f32::from_bits(*w)).collect();
                tracing::info!(
                    frame,
                    record_frame = self.record_frame,
                    descriptor = self.binding.offset,
                    relative = self.address,
                    actual_view_bytes = self.view_bytes,
                    ?words,
                    ?floats,
                    "native bound late-light uniform words"
                );
                return;
            }
            if self.binding.hash == 0x48e74d16cbfa45e3 && self.binding.kind == ResourceType::Srv {
                let at: &[usize] = match (self.binding.slot, self.binding.offset) {
                    (0, 192) => &[0x384, 0x388, 0x608, 0x70c],
                    (1, 32) => &[0x44, 0x48, 0x4c, 0x50],
                    _ => &[],
                };
                if !at.is_empty() {
                    let fields: Vec<_> = at
                        .iter()
                        .filter_map(|&offset| {
                            let word =
                                u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?);
                            Some((offset, word, f32::from_bits(word)))
                        })
                        .collect();
                    tracing::info!(
                        frame,
                        slot = self.binding.slot,
                        descriptor = self.binding.offset,
                        ?fields,
                        "native declared lighting SRV fields"
                    );
                }
            }
            if let Some(layout) = self.tile {
                super::light_tiles::report(frame, self.record_frame, self.address, layout, &bytes);
                return;
            }
            let prefix = &bytes[..bytes.len().min(16)];
            if self.binding.hash == 0xf6e47efa50226beb
                && self.binding.kind == victory_shader::ResourceType::Cbv
                && self.binding.slot == 8
                && self.binding.offset == 0
                && bytes.len() == 64
            {
                let word =
                    |n: usize| u32::from_le_bytes(bytes[n * 4..n * 4 + 4].try_into().unwrap());
                let count = word(3);
                let bounds: [f32; 6] = std::array::from_fn(|i| f32::from_bits(word(4 + i)));
                tracing::info!(
                    frame,
                    record_frame = self.record_frame,
                    address = format_args!("{:#x}", self.address),
                    count,
                    ?bounds,
                    "retired LoadTriangles scene header"
                );
            }
            if self.binding.hash == 0x3cea4534bbc15632
                && self.binding.kind == victory_shader::ResourceType::Srv
                && self.binding.slot == 0 && self.binding.offset == 0 && bytes.len().is_multiple_of(40)
            {
                crate::draw::init_headers_source::records(frame, self.address, &bytes, "owned GPU readback after retirement");
            }
            let hex = prefix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let floats: Vec<_> = prefix
                .chunks_exact(4)
                .map(|x| f32::from_le_bytes(x.try_into().unwrap()))
                .collect();
            if let Some(bc) = self.bc {
                tracing::info!(frame, record_frame=self.record_frame,shader=format_args!("{:016x}",self.binding.hash),heap=bc.heap,index=bc.index,address=format_args!("{:#x}",bc.address),format=bc.format,mip=bc.mip,bytes=bytes.len(),%hex,"first visible native material mip center BC block");
                return;
            }
            let nonzero_bytes = bytes.iter().filter(|&&v| v != 0).count();
            let first_nonzero = bytes.iter().position(|&v| v != 0);
            let last_nonzero = bytes.iter().rposition(|&v| v != 0);
            if let Some(material) = &self.material {
                material.finish(frame, &bytes);
                let words: Vec<_> = bytes
                    .chunks_exact(4)
                    .map(|x| u32::from_le_bytes(x.try_into().unwrap()))
                    .collect();
                tracing::info!(
                    frame,
                    record_frame = self.record_frame,
                    hash = format_args!("{:016x}", self.binding.hash),
                    address = format_args!("{:#x}", self.address),
                    ?words,
                    "seven original GPU material-record words at issued draw"
                );
            }
            tracing::info!(frame, record_frame=self.record_frame, hash=format_args!("{:016x}",self.binding.hash),
                kind=?self.binding.kind, slot=self.binding.slot, offset=self.binding.offset, address=format_args!("{:#x}",self.address),
                view_bytes=self.view_bytes, sampled=self.bytes, %hex, ?floats, nonzero_bytes, ?first_nonzero, ?last_nonzero,
                digest=%blake3::hash(&bytes), "GPU buffer view prefix at issued command");
        }
    }
}
