//! Native light-list range texture read by PS_NoDir_PBR_MATERIAL_STANDARD.
use crate::host::HostList;
use crate::object::This;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub(crate) const HASH: &str = "48e74d16cbfa45e3";
static LAST: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Prescalar,
    Prune,
    Shade,
    MaterialTiles,
    Minimum,
    Maximum,
    Gate,
    Coarse,
}
impl Stage {
    pub(crate) fn for_binding(hash: &str, slot: u8, offset: u32) -> Option<Self> {
        match (hash, slot, offset) {
            ("11eeb27c2d15980d", 1, 32) => Some(Self::Prescalar),
            ("307e048e4cdd99d4", 1, 8) => Some(Self::Prune),
            (HASH, 1, 80) => Some(Self::Shade),
            ("bad01f53f53248e5", 1, 48) => Some(Self::MaterialTiles),
            ("11eeb27c2d15980d", 1, 0) => Some(Self::Minimum),
            ("11eeb27c2d15980d", 1, 8) => Some(Self::Maximum),
            ("11eeb27c2d15980d", 1, 16) => Some(Self::Gate),
            ("11eeb27c2d15980d", 1, 24) => Some(Self::Coarse),
            _ => None,
        }
    }
    pub(super) fn hash(self) -> &'static str {
        match self {
            Self::Prescalar => "11eeb27c2d15980d",
            Self::Prune => "307e048e4cdd99d4",
            Self::Shade => HASH,
            Self::MaterialTiles => "bad01f53f53248e5",
            Self::Minimum | Self::Maximum | Self::Gate | Self::Coarse => "11eeb27c2d15980d",
        }
    }
    pub(crate) fn writes(self) -> bool {
        matches!(self, Self::Prescalar | Self::Prune | Self::MaterialTiles)
    }
    fn volume(self) -> bool {
        matches!(self, Self::Prescalar | Self::Prune | Self::Shade)
    }
    pub(super) fn offset(self) -> u32 {
        match self {
            Self::Prescalar => 32,
            Self::Prune => 8,
            Self::Shade => 80,
            Self::MaterialTiles => 48,
            Self::Minimum => 0,
            Self::Maximum => 8,
            Self::Gate => 16,
            Self::Coarse => 24,
        }
    }
}

pub(crate) fn uniforms(list: &HostList, views: [Option<(u64, u32)>; 2], frame: u64) {
    static LAST_UNIFORMS: AtomicU64 = AtomicU64::new(0);
    if !super::listed(frame) || LAST_UNIFORMS.fetch_max(frame, Ordering::Relaxed) >= frame {
        return;
    }
    for (view, (table, relative, width)) in views
        .into_iter()
        .zip([(1416, 0x730u32, 32u32), (1424, 0x40, 4)])
    {
        let Some((base, length)) = view else {
            continue;
        };
        if relative + width > length {
            continue;
        }
        let Some(address) = base.checked_add(u64::from(relative)) else {
            continue;
        };
        let Some((native, offset)) = list.sampled_scratch(address, u64::from(width)) else {
            continue;
        };
        let Some(host) = crate::host::host() else {
            return;
        };
        let Some(read) = super::buffer_sample::readback(&host.device, u64::from(width)) else {
            continue;
        };
        // Dynamic CBVs refer to persistent upload buffers held by this same command list,
        // whose native state is GENERIC_READ and therefore includes COPY_SOURCE.
        unsafe {
            list.list
                .CopyBufferRegion(&read, 0, &native, offset, u64::from(width));
        }
        list.add_sample(super::buffer_sample::Sample::light_uniform(
            frame, table, relative, width, length, read,
        ));
    }
}

#[derive(Clone, Copy)]
pub(crate) struct View {
    pub stage: Stage,
    pub resource: usize,
    pub host_index: u32,
    pub desc: Option<[u32; 10]>,
    pub mirrored: bool,
}

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub element_bytes: usize,
    pub stage: Stage,
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub mip: u32,
    pub slices: usize,
}

pub(crate) fn record(list: &HostList, view: View, frame: u64) {
    if !super::listed(frame)
        || LAST[view.stage as usize].fetch_max(frame, Ordering::Relaxed) >= frame
    {
        return;
    }
    let Some(copy) = crate::resource::host_copy(view.resource as This) else {
        return;
    };
    let desc = unsafe { copy.texture.GetDesc() };
    let fmt = view.desc.map_or(desc.Format.0 as u32, |d| d[0]);
    let write = view.stage.writes();
    let volume = view.stage.volume();
    let mip = view.desc.map_or(0, |d| if write { d[2] } else { d[4] });
    let mapping = if write {
        D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING
    } else {
        view.desc
            .map_or(D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, |d| d[2])
    };
    let first_w = if write && volume {
        view.desc.map_or(0, |d| d[3])
    } else {
        0
    };
    let w_size = if write && volume {
        view.desc.map_or(u32::MAX, |d| d[4])
    } else {
        u32::MAX
    };
    let state = list.sampled_texture_state(&copy);
    tracing::info!(
        frame,
        shader = view.stage.hash(),
        descriptor_offset = view.stage.offset(),
        write,
        first_w,
        w_size,
        view_dimension = view.desc.map(|d| d[1]),
        resource = format_args!("{:#x}", view.resource),
        native_slot = view.host_index,
        mirrored = view.mirrored,
        format = fmt,
        native_format = desc.Format.0,
        dimension = desc.Dimension.0,
        width = desc.Width,
        height = desc.Height,
        depth = desc.DepthOrArraySize,
        ?mip,
        ?mapping,
        ?state,
        "native light-list tile view at issued draw"
    );
    if !view.mirrored
        || copy.twin.is_some()
        || desc.Dimension
            != (if volume {
                D3D12_RESOURCE_DIMENSION_TEXTURE3D
            } else {
                D3D12_RESOURCE_DIMENSION_TEXTURE2D
            })
        || (!volume && desc.DepthOrArraySize != 1)
        || desc.SampleDesc.Count != 1
        || !(matches!(fmt, 41..=43) && matches!(desc.Format.0, 39 | 41..=43)
            || !volume && matches!(fmt, 57 | 62) && desc.Format.0 == fmt as i32)
        || first_w != 0
        || w_size == 0
        || view.desc.is_some_and(|d| {
            d[1] != (if volume {
                D3D12_SRV_DIMENSION_TEXTURE3D
            } else {
                D3D12_SRV_DIMENSION_TEXTURE2D
            })
            .0 as u32
        })
        || mip >= u32::from(desc.MipLevels)
        || mip >= 32
        || mapping & 7 != 0
    {
        return;
    }
    let Some(state) = state.filter(|s| {
        s & (if write {
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS
        } else if matches!(view.stage, Stage::Shade) {
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
        } else {
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
        })
        .0 as u32
            != 0
    }) else {
        return;
    };
    let Some(host) = crate::host::host() else {
        return;
    };
    if let Some(sample) = copy_slice(
        list,
        &host.device,
        &copy.texture,
        frame,
        view.resource,
        mip,
        state,
        view.stage,
        w_size,
    ) {
        list.add_sample(sample);
    }
}

/// Copies mip-relative z=0 and up to eight mask slices used by the following image_load.
fn copy_slice(
    list: &HostList,
    device: &ID3D12Device,
    texture: &ID3D12Resource,
    frame: u64,
    resource: usize,
    mip: u32,
    state: u32,
    stage: Stage,
    w_size: u32,
) -> Option<super::buffer_sample::Sample> {
    let desc = unsafe { texture.GetDesc() };
    let width = (u32::try_from(desc.Width).ok()?.checked_shr(mip)?).max(1);
    let height = desc.Height.checked_shr(mip)?.max(1);
    if width > 512 || height > 512 {
        return None;
    }
    let (copy_format, element_bytes) = match desc.Format {
        DXGI_FORMAT_R8_UINT => (desc.Format, 1),
        DXGI_FORMAT_R16_UINT => (desc.Format, 2),
        DXGI_FORMAT_R32_UINT
        | DXGI_FORMAT_R32_FLOAT
        | DXGI_FORMAT_R32_SINT
        | DXGI_FORMAT_R32_TYPELESS => (DXGI_FORMAT_R32_UINT, 4),
        _ => return None,
    };
    let pitch = (width * element_bytes).next_multiple_of(256);
    let slice_bytes = u64::from(pitch) * u64::from(height);
    let slices = u32::from(desc.DepthOrArraySize)
        .checked_shr(mip)?
        .max(1)
        .min(13)
        .min(w_size)
        .min(((4 << 20) / slice_bytes) as u32);
    if slices == 0 {
        return None;
    }
    let readback = super::buffer_sample::readback(device, slice_bytes * u64::from(slices))?;
    let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: copy_format,
            Width: width,
            Height: height,
            Depth: slices,
            RowPitch: pitch,
        },
    };
    let area = D3D12_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: slices,
    };
    let before = D3D12_RESOURCE_STATES(state as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // The source is already held in the command list. Restore its exact read state.
    unsafe {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: mip,
            },
        };
        if before.0 & copy.0 == 0 {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, before, copy)]);
        }
        list.list
            .CopyTextureRegion(&to, 0, 0, 0, &from, Some(&area));
        if before.0 & copy.0 == 0 {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, before)]);
        }
    }
    let layout = Layout {
        element_bytes: element_bytes as usize,
        stage,
        width: width as usize,
        height: height as usize,
        pitch: pitch as usize,
        mip,
        slices: slices as usize,
    };
    Some(super::buffer_sample::Sample::light_tiles(
        frame, resource, readback, layout,
    ))
}

fn counts(rows: &[u8], layout: Layout) -> [usize; 8] {
    let mut n = [0; 8];
    for (y, row) in rows
        .chunks_exact(layout.pitch)
        .take(layout.height)
        .enumerate()
    {
        for (x, v) in row[..layout.width * 4].chunks_exact(4).enumerate() {
            let word = u32::from_le_bytes(v.try_into().unwrap());
            let (low, high) = (word & 0xffff, word >> 16);
            n[0] += usize::from(word == u32::MAX);
            n[1] += usize::from(word == 0);
            if word == u32::MAX {
                continue;
            }
            match low.cmp(&high) {
                std::cmp::Ordering::Less => n[2] += 1,
                std::cmp::Ordering::Equal => n[3] += 1,
                std::cmp::Ordering::Greater => {
                    n[4] += 1;
                    continue;
                }
            }
            if high as usize + 1 >= layout.slices {
                n[7] += 1;
                continue;
            }
            let bitset = (low..=high).any(|group| {
                let offset = ((group as usize + 1) * layout.height + y) * layout.pitch + x * 4;
                rows[offset..offset + 4] != [0; 4]
            });
            n[if bitset { 5 } else { 6 }] += 1;
        }
    }
    n
}

pub(super) fn report(frame: u64, record_frame: u64, resource: u64, layout: Layout, rows: &[u8]) {
    for (z, slice) in rows.chunks_exact(layout.pitch * layout.height).enumerate() {
        let mut zeros = 0usize;
        let mut ones = 0usize;
        let mut set_bits = 0u64;
        let mut sign_only = 0usize;
        let mut first = None;
        let mut minimum = u32::MAX;
        let mut maximum = 0u32;
        let maximum_value = u32::MAX >> (32 - layout.element_bytes * 8);
        let mut digest = blake3::Hasher::new();
        for row in slice.chunks_exact(layout.pitch) {
            let pixels = &row[..layout.width * layout.element_bytes];
            digest.update(pixels);
            for chunk in pixels.chunks_exact(layout.element_bytes) {
                let mut le = [0u8; 4];
                le[..layout.element_bytes].copy_from_slice(chunk);
                let word = u32::from_le_bytes(le);
                minimum = minimum.min(word);
                maximum = maximum.max(word);
                zeros += usize::from(word == 0);
                ones += usize::from(word == maximum_value);
                set_bits += u64::from(word.count_ones());
                sign_only += usize::from(word == 0x80000000);
                if word != 0 && first.is_none() {
                    first = Some(word)
                }
            }
        }
        tracing::info!(frame,record_frame,shader=layout.stage.hash(),offset=layout.stage.offset(),resource=format_args!("{resource:#x}"),
            z, width=layout.width,height=layout.height,element_bytes=layout.element_bytes,zeros,ones,set_bits,sign_only,minimum,maximum,?first,digest=%digest.finalize(),"fenced integer lighting texture slice");
    }
    if !layout.stage.volume() {
        return;
    }
    let [
        sentinel,
        zero,
        ascending,
        single,
        inverted,
        nonempty_masks,
        empty_masks,
        uncaptured_masks,
    ] = counts(rows, layout);
    tracing::info!(
        frame,
        record_frame,
        shader = layout.stage.hash(),
        resource = format_args!("{resource:#x}"),
        width = layout.width,
        height = layout.height,
        mip = layout.mip,
        slices = layout.slices,
        sentinel,
        zero,
        ascending,
        single,
        inverted,
        nonempty_masks,
        empty_masks,
        uncaptured_masks,
        "native R32_UINT light index pairs at shader z=0"
    );
}

#[test]
fn light_range_histogram_keeps_zero_as_valid_pair_and_excludes_row_padding() {
    let mut rows = vec![255; 512];
    for (i, v) in [
        0u32,
        u32::MAX,
        (5 << 16) | 2,
        (2 << 16) | 5,
        0x00010001,
        0x00030003,
    ]
    .iter()
    .enumerate()
    {
        let p = (i / 3) * 256 + (i % 3) * 4;
        rows[p..p + 4].copy_from_slice(&v.to_le_bytes());
    }
    rows.resize(512 * 3, 0);
    rows[512..516].copy_from_slice(&0x100u32.to_le_bytes());
    assert_eq!(
        counts(
            &rows,
            Layout {
                element_bytes: 4,
                stage: Stage::Shade,
                width: 3,
                height: 2,
                pitch: 256,
                mip: 0,
                slices: 3
            }
        ),
        [1, 1, 1, 3, 1, 1, 1, 2]
    );
}

#[test]
fn native_volume_light_ranges_and_bitmasks_follow_the_bound_mip() {
    native_integer_view_fixture(true, DXGI_FORMAT_R32_UINT, 4);
}

#[test]
fn native_plane_preserves_material_tile_flags_and_compute_read_state() {
    for (format, bytes) in [
        (DXGI_FORMAT_R32_UINT, 4),
        (DXGI_FORMAT_R16_UINT, 2),
        (DXGI_FORMAT_R8_UINT, 1),
    ] {
        native_integer_view_fixture(false, format, bytes);
    }
}

#[cfg(test)]
fn native_integer_view_fixture(volume: bool, format: DXGI_FORMAT, element_bytes: usize) {
    let host = crate::host::host().expect("native D3D12 host");
    let list = host.new_list().unwrap();
    let mut texture: Option<ID3D12Resource> = None;
    let desc = D3D12_RESOURCE_DESC {
        Dimension: if volume {
            D3D12_RESOURCE_DIMENSION_TEXTURE3D
        } else {
            D3D12_RESOURCE_DIMENSION_TEXTURE2D
        },
        Width: 6,
        Height: 4,
        DepthOrArraySize: if volume { 6 } else { 1 },
        MipLevels: 2,
        Format: format,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    let layout = Layout {
        element_bytes,
        stage: Stage::Shade,
        width: 3,
        height: 2,
        pitch: 256,
        mip: 1,
        slices: if volume { 3 } else { 1 },
    };
    let mut bytes = vec![0u8; 1536];
    for (i, word) in [
        0u32,
        u32::MAX,
        (5 << 16) | 2,
        (2 << 16) | 5,
        0x00010001,
        0x00030003,
    ]
    .iter()
    .enumerate()
    {
        let at = (i / 3) * 256 + (i % 3) * element_bytes;
        bytes[at..at + element_bytes].copy_from_slice(&word.to_le_bytes()[..element_bytes]);
    }
    bytes[512..516].copy_from_slice(&0x100u32.to_le_bytes());
    let (upload, ptr) = host.upload_buffer(1536).unwrap();
    // The immutable upload, native volume and dedicated sample all outlive the reported queue fence.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut texture,
            )
            .unwrap();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    }
    let texture = texture.unwrap();
    let read = if volume {
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
    } else {
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
    };
    unsafe {
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&upload),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: 3,
                        Height: 2,
                        Depth: if volume { 3 } else { 1 },
                        RowPitch: 256,
                    },
                },
            },
        };
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 1,
            },
        };
        list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        list.list.ResourceBarrier(&[crate::host::transition(
            &texture,
            D3D12_RESOURCE_STATE_COPY_DEST,
            read,
        )]);
    }
    let sample = copy_slice(
        &list,
        &host.device,
        &texture,
        1,
        0,
        1,
        read.0 as u32,
        if volume { Stage::Shade } else { Stage::Gate },
        u32::MAX,
    )
    .unwrap();
    let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&texture, read, uav)]);
    }
    let writer_sample = copy_slice(
        &list,
        &host.device,
        &texture,
        1,
        0,
        1,
        uav.0 as u32,
        if volume {
            Stage::Prune
        } else {
            Stage::MaterialTiles
        },
        2,
    )
    .unwrap();
    list.close();
    host.execute(&[&list]);
    let got = sample.completed_bytes().unwrap();
    let writer_got = writer_sample.completed_bytes().unwrap();
    assert_eq!(writer_got.len(), if volume { 1024 } else { 512 });
    for (a, b) in bytes.chunks_exact(256).zip(writer_got.chunks_exact(256)) {
        assert_eq!(&a[..3 * element_bytes], &b[..3 * element_bytes]);
    }
    for (a, b) in bytes.chunks_exact(256).zip(got.chunks_exact(256)) {
        assert_eq!(&a[..3 * element_bytes], &b[..3 * element_bytes]);
    }
    if volume {
        assert_eq!(counts(&got, layout), [1, 1, 1, 3, 1, 1, 1, 2]);
    }
}
