use super::*;
use crate::capture::draw_stats::{Batch, Draw, Statistics, mesh_statistics_supported};

/// Scissor width and vertex count of each fixture draw of a triangle that
/// covers the 8x8 target: every pixel, a quarter of them, no primitive.
const CASES: [(i32, u32); 3] = [(8, 3), (2, 3), (8, 0)];

/// Records the cases once per entry of `sources` (true: measured the way a
/// mesh command is) into one list, runs the closed list twice and returns
/// what each run resolved, read only once that run's fence had completed.
fn measure(host: &Host, sources: &[bool]) -> Vec<Vec<(Statistics, u64)>> {
    let format = DXGI_FORMAT_R8G8B8A8_UNORM;
    let compositor = Compositor::new(&host.device, format).unwrap();
    let list = host.new_list().unwrap();
    let mut batch = Batch::new(&host.device).expect("native statistics and occlusion query heaps");
    let mut target: Option<ID3D12Resource> = None;
    let mut runs = Vec::new();
    // SAFETY: a fixture-owned 8x8 render target, pipeline and read-only null
    // texture view. Every emitted pixel is black even when rasterization works.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 8,
                    Height: 8,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut target,
            )
            .unwrap();
        let target = target.as_ref().unwrap();
        let rtv = host.rtv(target, None);
        host.device.CreateShaderResourceView(
            None::<&ID3D12Resource>,
            Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            }),
            compositor.srv_heap.GetCPUDescriptorHandleForHeapStart(),
        );
        list.list
            .SetDescriptorHeaps(&[Some(compositor.srv_heap.clone())]);
        list.list.SetGraphicsRootSignature(&compositor.root);
        list.list.SetPipelineState(&compositor.opaque);
        list.list.SetGraphicsRootDescriptorTable(
            0,
            compositor.srv_heap.GetGPUDescriptorHandleForHeapStart(),
        );
        list.list.OMSetRenderTargets(1, Some(&rtv), false, None);
        list.list
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: 8.0,
            Height: 8.0,
            MaxDepth: 1.0,
            ..Default::default()
        }]);
        let mut occurrence = 0;
        for &mesh in sources {
            for (width, vertices) in CASES {
                list.list.RSSetScissorRects(&[RECT {
                    right: width,
                    bottom: 8,
                    ..Default::default()
                }]);
                occurrence += 1;
                let tag = Draw {
                    frame: 1,
                    hash: 1,
                    kind: "fixture",
                    occurrence,
                    mesh,
                };
                let slot = batch.begin(&list.list, tag).unwrap();
                list.list.DrawInstanced(vertices, 1, 0, 0);
                batch.end(&list.list, slot);
            }
        }
        list.close();
        for _ in 0..2 {
            host.execute(&[&list]);
            host.device.GetDeviceRemovedReason().unwrap();
            let fence = host.fence.GetCompletedValue();
            assert!(fence != u64::MAX && fence >= *host.submitted.lock().unwrap());
            runs.push(batch.results().unwrap());
        }
    }
    runs
}

#[test]
fn native_draw_stats_distinguish_black_pixels_from_zero_coverage() {
    let host = create().expect("D3D12 device");
    for values in measure(&host, &[false]) {
        assert_eq!(
            values.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            [64, 16, 0]
        );
        let classic: Vec<_> = values
            .iter()
            .map(|(statistics, _)| match statistics {
                Statistics::Classic(p) => *p,
                _ => panic!("a vertex pipeline draw left the classic statistics query"),
            })
            .collect();
        assert_eq!(
            classic.iter().map(|p| p.IAPrimitives).collect::<Vec<_>>(),
            [1, 1, 0]
        );
        assert!(classic[..2].iter().all(|p| p.PSInvocations > 0));
        assert_eq!(classic[2].PSInvocations, 0);
        eprintln!(
            "native draw samples and PS invocations: {:?}",
            values
                .iter()
                .zip(&classic)
                .map(|((_, n), p)| (*n, p.PSInvocations))
                .collect::<Vec<_>>()
        );
    }
}

/// What the classic fixture cannot show: the measurement mesh commands get.
/// On a host that reports mesh pipeline statistics, the mesh heap and query
/// type resolve their longer record beside classic ones in the same list and
/// readback without disturbing either's passed samples; on any other host no
/// statistics query is issued and the samples are still exact. The commands
/// are vertex pipeline draws (the extended query measures those too), so the
/// amplification and mesh counters must read zero: counting real mesh shader
/// work needs a mesh pipeline, which this fixture has no compiler for.
#[test]
fn native_draw_stats_measure_mesh_commands_with_their_own_query_or_occlusion_only() {
    let host = create().expect("D3D12 device");
    let supported = mesh_statistics_supported(&host.device);
    for values in measure(&host, &[false, true]) {
        assert_eq!(
            values.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            [64, 16, 0, 64, 16, 0]
        );
        let (classic, mesh) = values.split_at(CASES.len());
        let mut extended = Vec::new();
        for (i, ((c, _), (m, samples))) in classic.iter().zip(mesh).enumerate() {
            let Statistics::Classic(c) = c else {
                panic!("a vertex pipeline draw left the classic statistics query");
            };
            match m {
                Statistics::Mesh(m) => {
                    assert!(supported, "mesh statistics queried on a host without them");
                    // The classic counters lead the extended record unchanged.
                    assert_eq!(
                        (m.IAVertices, m.IAPrimitives),
                        (c.IAVertices, c.IAPrimitives)
                    );
                    assert_eq!(m.IAPrimitives, [1, 1, 0][i]);
                    assert_eq!(m.PSInvocations > 0, i < 2);
                    assert_eq!((m.ASInvocations, m.MSInvocations, m.MSPrimitives), (0, 0, 0));
                    extended.push((*samples, m.PSInvocations, m.MSInvocations));
                }
                Statistics::Unavailable => {
                    assert!(!supported, "a host with mesh statistics recorded none");
                }
                Statistics::Classic(_) => {
                    panic!("a mesh command measured with the classic statistics query")
                }
            }
        }
        eprintln!(
            "mesh pipeline statistics supported: {supported}; mesh-path samples: {:?}; \
             samples, PS and MS invocations where counted: {extended:?}",
            mesh.iter().map(|(_, n)| *n).collect::<Vec<_>>()
        );
    }
}
