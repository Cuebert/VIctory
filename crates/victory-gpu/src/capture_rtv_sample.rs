use super::{OutputBinding, parse_preview};
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SELECTED: Mutex<Option<(u64, usize)>> = Mutex::new(None);
static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);
static LAST_RECORD: AtomicU64 = AtomicU64::new(0);

pub(crate) fn poll(folder: &std::path::Path) {
    let path = folder.join("sample-target.txt");
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut stamp = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *stamp == modified {
        return;
    }
    *stamp = modified;
    let mut text = String::new();
    let mut requested = None;
    if modified.is_some() {
        let Ok(n) = std::fs::File::open(path).and_then(|f| f.take(65).read_to_string(&mut text))
        else {
            return;
        };
        if n > 64 {
            return;
        }
        let Some((_, pass)) = parse_preview(text.trim_start_matches('\u{feff}').trim()) else {
            return;
        };
        if let Some(pass) = pass {
            let OutputBinding::Rtv(slot) = pass.output else {
                return;
            };
            requested = Some((pass.hash, slot));
        }
    }
    *SELECTED.lock().unwrap_or_else(|e| e.into_inner()) = requested;
    ACTIVE.store(requested.is_some(), Ordering::Release);
    tracing::info!(?requested, "updated post-draw render target sample");
}

pub(crate) fn selected(record_frame: u64, pipeline: crate::object::This) -> Option<(u64, usize)> {
    if !ACTIVE.load(Ordering::Acquire)
        || !super::listed(record_frame)
        || LAST_RECORD.load(Ordering::Relaxed) >= record_frame
    {
        return None;
    }
    let (hash, slot) = (*SELECTED.lock().unwrap_or_else(|e| e.into_inner()))?;
    crate::pipeline::contains_shader(pipeline, hash).then_some((hash, slot))
}

pub(crate) fn claim(frame: u64) -> bool {
    LAST_RECORD.fetch_max(frame, Ordering::AcqRel) < frame
}

pub(crate) struct Sample {
    pub hash: u64,
    pub slot: usize,
    pub resource: usize,
    pub record_frame: u64,
    pub readback: ID3D12Resource,
    pub footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
}

impl Sample {
    pub fn finish(&self, frame: u64) {
        if !super::listed(frame) {
            return;
        }
        let name = format!(
            "frame-{frame}-after-{:016x}-rtv{}-0x{:x}",
            self.hash, self.slot, self.resource
        );
        if let Some(digest) = super::save_read_back(&name, &self.readback, &self.footprint, true) {
            tracing::info!(frame,record_frame=self.record_frame,hash=format_args!("{:016x}",self.hash),
                slot=self.slot,resource=format_args!("{:#x}",self.resource),%digest,"render target immediately after selected draw");
        }
    }
}
