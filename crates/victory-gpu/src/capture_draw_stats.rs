//! Native rasterization evidence around selected draws. Each recorded list
//! owns its query slots and results across submission and closed-list replay.
//!
//! Every sampled command is bracketed by an occlusion query (the samples that
//! passed depth and stencil, whatever colour they were given) and by the
//! pipeline statistics query its pipeline allows:
//!
//! * vertex pipelines (`draw`, `indexed`, `indirect-draw`, `indirect-indexed`):
//!   `D3D12_QUERY_TYPE_PIPELINE_STATISTICS`, on every device;
//! * mesh pipelines (`mesh`, `indirect-mesh`):
//!   `D3D12_QUERY_TYPE_PIPELINE_STATISTICS1` from a heap of its own type, only
//!   where `D3D12_FEATURE_DATA_D3D12_OPTIONS9` reports
//!   `MeshShaderPipelineStatsSupported`. Elsewhere no statistics query is
//!   issued around mesh work: the occlusion count stands alone.
//!
//! `frame-<n>-native-draws.tsv` has one row per sampled command (an indirect
//! command is one row: totals over the argument records the GPU ran). The
//! first fourteen columns are as they were; four follow:
//!
//! * `statistics`: `pipeline_statistics`, `pipeline_statistics1` or
//!   `occlusion_only`, the query the counters came from;
//! * `as_calls`, `ms_calls`, `ms_primitives`: amplification and mesh shader
//!   invocations and the primitives mesh shaders output.
//!
//! A counter the row's query does not have reads `n/a` (the mesh columns of a
//! vertex pipeline); one the host could not count reads `unavailable` (every
//! statistics column of an `occlusion_only` row). `passed_samples` is always
//! a number.

use crate::object::This;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once, RwLock};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

const HASH_LIMIT: usize = 8;
const CAPACITY: usize = 64;
/// Bytes kept for a slot's statistics: the mesh record, which extends the
/// classic one (88 bytes) by three counters.
const STATISTICS: usize = size_of::<D3D12_QUERY_DATA_PIPELINE_STATISTICS1>();
/// A slot of the readback: its statistics record, then its passed samples.
const STRIDE: usize = STATISTICS + size_of::<u64>();
// What the native resolves write, and both destinations eight-byte aligned.
const _: () = assert!(
    size_of::<D3D12_QUERY_DATA_PIPELINE_STATISTICS>() == 88 && STATISTICS == 112 && STRIDE % 8 == 0
);
const UNAVAILABLE: &str = "unavailable";
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HASHES: RwLock<Vec<u64>> = RwLock::new(Vec::new());
static STAMP: Mutex<Option<std::time::SystemTime>> = Mutex::new(None);
static BUDGET: Mutex<(u64, [u32; HASH_LIMIT])> = Mutex::new((0, [0; HASH_LIMIT]));

pub(crate) fn poll(folder: &std::path::Path) {
    let path = folder.join("sample-draws.txt");
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut stamp = STAMP.lock().unwrap_or_else(|e| e.into_inner());
    if *stamp == modified {
        return;
    }
    *stamp = modified;
    let mut text = String::new();
    let mut hashes = Vec::new();
    if modified.is_some() {
        let read = std::fs::File::open(&path).and_then(|f| f.take(257).read_to_string(&mut text));
        if matches!(read, Ok(0..=256)) {
            for word in text.trim_start_matches('\u{feff}').split_ascii_whitespace() {
                let Ok(hash) = u64::from_str_radix(word, 16) else {
                    hashes.clear();
                    break;
                };
                if word.len() != 16 || hashes.len() == HASH_LIMIT {
                    hashes.clear();
                    break;
                }
                if !hashes.contains(&hash) {
                    hashes.push(hash);
                }
            }
        }
    }
    let count = hashes.len();
    *HASHES.write().unwrap_or_else(|e| e.into_inner()) = hashes;
    ACTIVE.store(count > 0, Ordering::Release);
    tracing::info!(count, "updated native draw statistics selection");
}

#[derive(Clone, Copy)]
pub(crate) struct Draw {
    pub frame: u64,
    pub hash: u64,
    pub kind: &'static str,
    pub occurrence: u32,
    /// The command runs a mesh pipeline (`DispatchMesh`, or an indirect
    /// mesh dispatch): its statistics are the mesh ones or none.
    pub mesh: bool,
}

/// The sample for an issued command of `pipeline`, if it is one. Vertex and
/// mesh commands share a shader's occurrence count and its cap.
pub(crate) fn selected(pipeline: This, kind: &'static str, mesh: bool) -> Option<Draw> {
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let frame = crate::queue::presented() + 1;
    if !super::trace_window(frame) {
        return None;
    }
    let hashes = HASHES.read().unwrap_or_else(|e| e.into_inner());
    let (slot, &hash) = hashes
        .iter()
        .enumerate()
        .find(|(_, hash)| crate::pipeline::contains_shader(pipeline, **hash))?;
    let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    if frame < budget.0 {
        return None;
    }
    if frame > budget.0 {
        *budget = (frame, [0; HASH_LIMIT]);
    }
    let n = &mut budget.1[slot];
    *n = n.saturating_add(1);
    // Sample across the pass, without serializing every draw in a marked frame.
    (n.is_power_of_two() && *n <= 128).then_some(Draw {
        frame,
        hash,
        kind,
        occurrence: *n,
        mesh,
    })
}

/// Whether the host counts amplification and mesh shader work
/// (`MeshShaderPipelineStatsSupported` of D3D12 options 9), which the mesh
/// statistics heap and query need. A runtime older than the options fails
/// the check: unsupported.
pub(crate) fn mesh_statistics_supported(device: &ID3D12Device) -> bool {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS9::default();
    // SAFETY: the query writes this structure of the given size.
    let supported = unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS9,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        )
    }
    .is_ok()
        && options.MeshShaderPipelineStatsSupported.as_bool();
    static LOGGED: Once = Once::new();
    LOGGED.call_once(|| tracing::info!(supported, "host mesh shader pipeline statistics"));
    supported
}

/// The statistics query that brackets a slot's command.
#[derive(Clone, Copy)]
enum Source {
    /// `D3D12_QUERY_TYPE_PIPELINE_STATISTICS`: a vertex pipeline.
    Classic,
    /// `D3D12_QUERY_TYPE_PIPELINE_STATISTICS1`: a mesh pipeline on a host
    /// that counts its work.
    Mesh,
    /// None: a mesh pipeline elsewhere, with its occlusion query only.
    Occlusion,
}

/// A slot's resolved pipeline statistics.
#[derive(Clone, Copy)]
pub(crate) enum Statistics {
    Classic(D3D12_QUERY_DATA_PIPELINE_STATISTICS),
    /// The classic counters (input assembly and vertex shading idle in a
    /// mesh pipeline), then `ASInvocations`, `MSInvocations`, `MSPrimitives`.
    Mesh(D3D12_QUERY_DATA_PIPELINE_STATISTICS1),
    /// Mesh work on a host without mesh statistics: nothing was queried.
    Unavailable,
}

impl Statistics {
    /// A row's cells: the query's name, its six raster counters and its
    /// three mesh counters, each run tab-separated.
    fn cells(&self) -> (&'static str, String, String) {
        fn join<const N: usize>(values: Option<[u64; N]>, missing: &str) -> String {
            match values {
                Some(values) => values.map(|v| v.to_string()).join("\t"),
                None => [missing; N].join("\t"),
            }
        }
        match self {
            Statistics::Classic(p) => (
                "pipeline_statistics",
                join(
                    Some([
                        p.IAVertices,
                        p.IAPrimitives,
                        p.VSInvocations,
                        p.CInvocations,
                        p.CPrimitives,
                        p.PSInvocations,
                    ]),
                    UNAVAILABLE,
                ),
                // Not counters of this query: a vertex pipeline has no such stage.
                join::<3>(None, "n/a"),
            ),
            Statistics::Mesh(p) => (
                "pipeline_statistics1",
                join(
                    Some([
                        p.IAVertices,
                        p.IAPrimitives,
                        p.VSInvocations,
                        p.CInvocations,
                        p.CPrimitives,
                        p.PSInvocations,
                    ]),
                    UNAVAILABLE,
                ),
                join(Some([p.ASInvocations, p.MSInvocations, p.MSPrimitives]), UNAVAILABLE),
            ),
            Statistics::Unavailable => (
                "occlusion_only",
                join::<6>(None, UNAVAILABLE),
                join::<3>(None, UNAVAILABLE),
            ),
        }
    }
}

pub(crate) struct Batch {
    id: u64,
    /// Vertex pipelines' statistics, on every device.
    pipeline: ID3D12QueryHeap,
    /// Mesh pipelines' statistics, on devices that count them.
    mesh_pipeline: Option<ID3D12QueryHeap>,
    occlusion: ID3D12QueryHeap,
    output: ID3D12Resource,
    draws: Vec<(Draw, Source)>,
}

impl Batch {
    pub(crate) fn new(device: &ID3D12Device) -> Option<Self> {
        let heap = |ty| {
            let mut heap: Option<ID3D12QueryHeap> = None;
            let desc = D3D12_QUERY_HEAP_DESC {
                Type: ty,
                Count: CAPACITY as u32,
                NodeMask: 0,
            };
            // SAFETY: native query slots reserved for this list.
            unsafe {
                device.CreateQueryHeap(&desc, &mut heap).ok()?;
            }
            heap
        };
        let pipeline = heap(D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS)?;
        let occlusion = heap(D3D12_QUERY_HEAP_TYPE_OCCLUSION)?;
        // Never asked of a device that does not report the statistics; one
        // that reports them and still refuses the heap is treated as such.
        let mesh_pipeline = if mesh_statistics_supported(device) {
            let made = heap(D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS1);
            if made.is_none() {
                static WARNED: Once = Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        "no mesh pipeline statistics query heap on a host that reports them; \
                         mesh draws record passed samples only"
                    )
                });
            }
            made
        } else {
            None
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: (STRIDE * CAPACITY) as u64,
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
        let mut output = None;
        // SAFETY: fixed-size owned readback, permanently in COPY_DEST.
        unsafe {
            device
                .CreateCommittedResource(
                    &props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut output,
                )
                .ok()?;
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Some(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            pipeline,
            mesh_pipeline,
            occlusion,
            output: output?,
            draws: Vec::new(),
        })
    }

    /// The statistics heap and query type that measure `source`, if one does.
    fn statistics(&self, source: Source) -> Option<(&ID3D12QueryHeap, D3D12_QUERY_TYPE)> {
        match source {
            Source::Classic => Some((&self.pipeline, D3D12_QUERY_TYPE_PIPELINE_STATISTICS)),
            Source::Mesh => self
                .mesh_pipeline
                .as_ref()
                .map(|heap| (heap, D3D12_QUERY_TYPE_PIPELINE_STATISTICS1)),
            Source::Occlusion => None,
        }
    }

    pub(crate) fn begin(&mut self, list: &ID3D12GraphicsCommandList, draw: Draw) -> Option<u32> {
        let index = self.draws.len();
        if index >= CAPACITY {
            return None;
        }
        let source = match (draw.mesh, &self.mesh_pipeline) {
            (false, _) => Source::Classic,
            (true, Some(_)) => Source::Mesh,
            (true, None) => Source::Occlusion,
        };
        self.draws.push((draw, source));
        let slot = index as u32;
        // SAFETY: this direct list exclusively owns a fresh slot of each heap
        // it uses; a statistics query is of its own heap's type, and none is
        // begun where the device has no heap for the command's pipeline.
        unsafe {
            if let Some((heap, ty)) = self.statistics(source) {
                list.BeginQuery(heap, ty, slot);
            }
            list.BeginQuery(&self.occlusion, D3D12_QUERY_TYPE_OCCLUSION, slot);
        }
        Some(slot)
    }

    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList, slot: u32) {
        let Some(&(_, source)) = self.draws.get(slot as usize) else {
            return;
        };
        let offset = u64::from(slot) * STRIDE as u64;
        // SAFETY: paired with begin around one command and ending exactly the
        // queries it began. Both destination offsets are eight-byte aligned
        // and lie wholly in the owned output resource: a statistics record is
        // at most `STATISTICS` bytes, the occlusion count follows it.
        unsafe {
            list.EndQuery(&self.occlusion, D3D12_QUERY_TYPE_OCCLUSION, slot);
            if let Some((heap, ty)) = self.statistics(source) {
                list.EndQuery(heap, ty, slot);
                list.ResolveQueryData(heap, ty, slot, 1, &self.output, offset);
            }
            list.ResolveQueryData(
                &self.occlusion,
                D3D12_QUERY_TYPE_OCCLUSION,
                slot,
                1,
                &self.output,
                offset + STATISTICS as u64,
            );
        }
    }

    pub(crate) fn results(&self) -> Option<Vec<(Statistics, u64)>> {
        let range = D3D12_RANGE {
            Begin: 0,
            End: STRIDE * self.draws.len(),
        };
        let mut at = std::ptr::null_mut();
        let mut values = Vec::with_capacity(self.draws.len());
        // SAFETY: called only for a retired queue fence. Each recorded slot
        // holds the record its own query resolved, read as that type and no
        // further (none for an occlusion-only slot), then one occlusion count.
        unsafe {
            self.output.Map(0, Some(&range), Some(&mut at)).ok()?;
            if !at.is_null() {
                for (i, (_, source)) in self.draws.iter().enumerate() {
                    let record = at.cast::<u8>().add(i * STRIDE);
                    let statistics = match source {
                        Source::Classic => Statistics::Classic(
                            record
                                .cast::<D3D12_QUERY_DATA_PIPELINE_STATISTICS>()
                                .read_unaligned(),
                        ),
                        Source::Mesh => Statistics::Mesh(
                            record
                                .cast::<D3D12_QUERY_DATA_PIPELINE_STATISTICS1>()
                                .read_unaligned(),
                        ),
                        Source::Occlusion => Statistics::Unavailable,
                    };
                    let samples = record.add(STATISTICS).cast::<u64>().read_unaligned();
                    values.push((statistics, samples));
                }
            }
            self.output
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        }
        (values.len() == self.draws.len()).then_some(values)
    }

    pub(crate) fn finish(&self, frame: u64, fence: u64) {
        let Some(folder) = super::folder() else {
            return;
        };
        let Some(values) = self.results() else {
            return;
        };
        let path = folder.join(format!("frame-{frame}-native-draws.tsv"));
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        if file.metadata().is_ok_and(|m| m.len() == 0) {
            let _ = writeln!(
                file,
                "fence\tlist\tslot\trecord_frame\tshader\tcommand\toccurrence\tia_vertices\tia_primitives\tvs_calls\tclip_in\tclip_out\tps_calls\tpassed_samples\tstatistics\tas_calls\tms_calls\tms_primitives"
            );
        }
        for (slot, ((draw, _), (statistics, samples))) in self.draws.iter().zip(values).enumerate() {
            let (query, raster, mesh) = statistics.cells();
            let _ = writeln!(
                file,
                "{fence}\t{}\t{slot}\t{}\t{:016x}\t{}\t{}\t{raster}\t{samples}\t{query}\t{mesh}",
                self.id, draw.frame, draw.hash, draw.kind, draw.occurrence
            );
        }
    }
}
