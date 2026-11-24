//! A full-overwrite counter reset into one retained title physical section.
use std::sync::{Arc, OnceLock};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use crate::descriptor::host::{HostHeap, MapSlots};
use crate::host::{BUFFER_READ, BUFFER_WRITE, HostList, host};
use crate::resource::RetainedSectionOutput;
use crate::shadow::Shadow;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VICTORY_RETAINED_RESET").as_deref() == Ok("1"))
}

pub(crate) fn footprint(groups: [u32; 3], count: u32) -> Option<usize> {
    if groups.iter().any(|&g| g > 65535) {
        return None;
    }
    if groups.contains(&0) {
        return Some(0);
    }
    usize::try_from(16u64.checked_mul(u64::from(count).min(u64::from(groups[0]) * 32))?).ok()
}

pub(crate) struct Output {
    shadow: Arc<Shadow>,
    lease: Arc<dyn RetainedSectionOutput>,
    readback: ID3D12Resource,
    scratch: Option<ID3D12Resource>,
    // The native heap and its bitmap reservation both outlive all replays.
    _heap: ID3D12DescriptorHeap,
    _slot: MapSlots,
    gpu: D3D12_GPU_DESCRIPTOR_HANDLE,
    address: usize,
    len: usize,
}

impl Output {
    pub(crate) fn prepare(address: u64, len: usize, heap: &HostHeap, cached: &mut Option<(u64,ID3D12Resource)>) -> Option<Self> {
        let (shadow, lease) = crate::shadow::section_output(address, len)?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len as u64,
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
        let props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let mut readback = None;
        // SAFETY: a private readback allocation of exactly the proven output length.
        unsafe {
            host()?
                .device
                .CreateCommittedResource(
                    &props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .ok()?;
        }
        let readback = readback?;
        let slot = heap.map_slots(1)?;
        let offset=shadow.offset(address);
        let scratch=if offset.is_multiple_of(u64::from(D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT)) {
            None
        }else {
            // Every byte is overwritten by this exact shader. A private aligned
            // view needs no input copy, and the later native copy has byte offsets.
            if cached.as_ref().is_none_or(|(size,_)|*size<len as u64) {
                let props = D3D12_HEAP_PROPERTIES {Type:D3D12_HEAP_TYPE_DEFAULT,..Default::default()};
                let desc=D3D12_RESOURCE_DESC{Flags:D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,..desc};
                let mut resource=None;
                // SAFETY: a list-private GPU buffer large enough for the exact raw view.
                unsafe {host()?.device.CreateCommittedResource(&props,D3D12_HEAP_FLAG_NONE,
                    &desc,D3D12_RESOURCE_STATE_COMMON,None,&mut resource).ok()?;}
                *cached=Some((len as u64,resource?));
            }
            Some(cached.as_ref()?.1.clone())
        };
        heap.raw_uav_at(
            slot.first,
            scratch.as_ref().unwrap_or(&shadow.buffer),
            if scratch.is_some(){0}else{offset/4},
            (len / 4) as u32,
        );
        let gpu = heap.gpu_handle(slot.first);
        Some(Self {
            shadow,
            lease,
            readback,
            scratch,
            _heap: heap.heap.clone(),
            _slot: slot,
            gpu,
            address: usize::try_from(address).ok()?,
            len,
        })
    }

    pub(crate) fn bind(&self, list: &HostList, param: u32) -> u64 {
        list.buffer_state(self.scratch.as_ref().unwrap_or(&self.shadow.buffer), BUFFER_WRITE);
        unsafe {
            list.list.SetComputeRootDescriptorTable(param, self.gpu);
        }
        self.len as u64
    }

    /// Must follow the issued native Dispatch and its ordering barrier. A
    /// later consumer uses the shared registered shadow. The readback freezes
    /// this command's output before any later command may modify the shadow.
    pub(crate) fn record(self, list: &HostList) {
        if let Some(scratch)=&self.scratch {
            list.buffer_state(scratch,BUFFER_READ);
            list.buffer_state(&self.shadow.buffer,D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
            // SAFETY: every Reset store precedes this transition; both exact
            // intervals were checked and are held until the list retires.
            unsafe {list.list.CopyBufferRegion(&self.shadow.buffer,self.shadow.offset(self.address as u64),
                    scratch,0,self.len as u64);}
        }
        list.buffer_state(&self.shadow.buffer, BUFFER_READ);
        unsafe {
            list.list.CopyBufferRegion(
                &self.readback,
                0,
                &self.shadow.buffer,
                self.shadow.offset(self.address as u64),
                self.len as u64,
            );
        }
        list.buffer_state(&self.shadow.buffer, BUFFER_WRITE);
        list.retain_section_reset(self);
    }

    pub(crate) fn protect(&self, begin: bool) {
        self.shadow.protect_section(self.address, self.len, begin);
    }

    #[cfg(feature="diagnostics")]
    pub(crate) fn published_range(&self) -> Option<crate::as_join::Range> {
        let (generation,offset)=self.lease.physical_identity()?;
        Some(crate::as_join::Range{kind:1,generation,offset,bytes:self.len as u64})
    }

    pub(crate) fn publish(&self) -> bool {
        let mut ptr = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: self.len,
        };
        // SAFETY: the caller holds the submission lock and proved the queue
        // fence retired. Only private, immutable readback bytes are exposed.
        unsafe {
            if self.readback.Map(0, Some(&range), Some(&mut ptr)).is_err() || ptr.is_null() {
                return false;
            }
            let written = self
                .lease
                .store(std::slice::from_raw_parts(ptr.cast(), self.len));
            self.readback
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            #[cfg(feature = "diagnostics")]
            if written {
                if self.scratch.is_some(){
                    static FIRST_DWORD:std::sync::Once=std::sync::Once::new();
                    FIRST_DWORD.call_once(||tracing::info!(address=format_args!("{:#x}",self.address),
                        bytes=self.len,"first dword-offset counter reset retired to retained section"));
                }
                static FIRST: std::sync::Once = std::sync::Once::new();
                FIRST.call_once(|| {
                    tracing::info!(
                        address = format_args!("{:#x}", self.address),
                        bytes = self.len,
                        "first retired counter reset published to retained section"
                    )
                });
            }
            written
        }
    }
}
