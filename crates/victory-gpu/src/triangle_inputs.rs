//! Exact GPU address operands of the title's LoadTriangles compute program.
use crate::host::HostList;
use crate::shadow::Shadow;
use std::sync::{Arc, OnceLock};

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VICTORY_RETAINED_TRIANGLES").as_deref() == Ok("1"))
}

pub(super) struct Span {
    shadow: Arc<Shadow>,
    address: u64,
    len: u64,
    write: Option<(u64, u64)>,
}

pub(super) fn resolve(
    constants: &[u32],
    known: u16,
    stage: &victory_shader::emit::HostShader,
) -> Result<Vec<Span>, &'static str> {
    if known & 0x3f != 0x3f {
        return Err("triangle-root-constants");
    }
    for u in &stage.user_data {
        if u.slot == 8
            && u.root == victory_shader::RootType::Constants
            && (u.dword >= 14 || known & (1 << u.dword) == 0)
        {
            return Err("triangle-root-constants");
        }
    }
    let mut spans = Vec::with_capacity(3);
    // Header counter and six scene bounds occupy [12,40); its rotation
    // index is read from +60. The optional matrix is three float4 rows.
    // The optional second reduction writes six bounds of AABB centers.
    for (i, len, read, write, reason) in [
        (0, 64, true, Some((12, 28)), "triangle-header-owner"),
        (2, 48, true, None, "triangle-matrix-owner"),
        (4, 24, true, Some((0, 24)), "triangle-centers-owner"),
    ] {
        let lo = *constants.get(i).ok_or("triangle-root-constants")?;
        let hi = *constants.get(i + 1).ok_or("triangle-root-constants")?;
        let address = u64::from(lo) | (u64::from(hi) << 32);
        if address == 0 && i != 0 {
            continue;
        }
        let shadow =
            crate::shadow::owned_compute_span(address, len, read, write.is_some()).ok_or(reason)?;
        spans.push(Span {
            shadow,
            address,
            len,
            write,
        });
    }
    Ok(spans)
}

pub(super) fn prepare(list: &HostList, spans: &[Span]) {
    for s in spans {
        s.shadow.refresh(list, s.address, s.len);
        // The shader map uses raw UAV handles for both reads and atomics.
        list.buffer_state(&s.shadow.buffer, crate::host::BUFFER_WRITE);
    }
}

pub(super) fn issued(list: &HostList, spans: Vec<Span>) {
    #[cfg(feature = "diagnostics")]
    if let Some(header) = spans.first() {
        let frame = crate::queue::presented() + 1;
        if crate::capture::trace_window(frame) {
            static BUDGET: std::sync::Mutex<(u64, u8)> = std::sync::Mutex::new((0, 0));
            let mut b = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
            if frame > b.0 {
                *b = (frame, 0);
            }
            if b.0 == frame
                && b.1 < 2
                && let Some(host) = crate::host::host()
            {
                b.1 += 1;
                use crate::capture::buffer_sample::{Binding, SampleLocation, record_copy};
                let binding = Binding {
                    hash: 0xf6e47efa50226beb,
                    kind: victory_shader::ResourceType::Cbv,
                    slot: 8,
                    offset: 0,
                    whole: true,
                };
                record_copy(
                    list,
                    &host.device,
                    &header.shadow.buffer,
                    header.shadow.offset(header.address),
                    SampleLocation {
                        binding,
                        frame,
                        address: header.address,
                        size: 64,
                    },
                );
            }
        }
    }
    for s in spans {
        if let Some((offset, len)) = s.write {
            s.shadow.gpu_writes(list, s.address + offset, len);
        }
    }
}

#[cfg(feature = "diagnostics")]
pub(super) fn trace(
    list: &HostList,
    roots: [u64; 5],
    constants: &[u32],
    known: u16,
    views: u8,
    failure: Option<&'static str>,
) {
    if !crate::capture::trace_window(crate::queue::presented() + 1) {
        return;
    }
    let mut args = [0u32; 14];
    let n = constants.len().min(args.len());
    args[..n].copy_from_slice(&constants[..n]);
    list.defer_host(move|| {
        let frame=crate::queue::presented()+1;
        if !crate::capture::listed(frame){return;}
        static SAMPLES:std::sync::Mutex<(u64,u8)>=std::sync::Mutex::new((0,0));
        let mut samples=SAMPLES.lock().unwrap_or_else(|e|e.into_inner());
        if samples.0!=frame {*samples=(frame,0);}
        if samples.1>=2 {return;} samples.1+=1;drop(samples);
        tracing::info!(frame, roots_0_4_5_6_7=?roots,slot8=?args,known,views,failure=?failure,
            "LoadTriangles recorded geometry operands");
        for (slot,address) in [0,4,5,6,7].into_iter().zip(roots) {
            if address!=0 {
                let owner=crate::shadow::range_evidence(address,4);
                tracing::info!(frame,slot,address=format_args!("{address:#x}"),%owner,
                    "LoadTriangles direct-root registration");
            }
        }
        for (i,len) in [(0,64),(2,48),(4,24)] {
            let address=u64::from(args[i])|(u64::from(args[i+1])<<32);
            if address!=0 {
                let owner=crate::shadow::range_evidence(address,len);
                tracing::info!(frame,constant=i,bytes=len,address=format_args!("{address:#x}"),%owner,
                    "LoadTriangles operand registration");
            }
        }
    });
}
