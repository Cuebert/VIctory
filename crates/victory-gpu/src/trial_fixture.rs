use super::*;

/// A windowless DXGI composition swap chain goes through the normal multi-plane
/// present path. An odd-sized source exercises the last row of a padded footprint.
#[test]
#[ignore = "run alone with VICTORY_CAPTURE, VICTORY_DUMP_TARGETS and VICTORY_CAPTURE_FRAMES=1"]
fn captures_original_composite_and_selected_scene() {
    let folder = crate::capture::folder().expect("provide an empty fixture capture directory");
    assert!(crate::capture::listed(1) && crate::capture::dump_on());
    let host = create().expect("D3D12 device");
    let (width, height) = (19, 9);
    let format = DXGI_FORMAT_R8G8B8A8_UNORM;
    let samples = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
    // SAFETY: fixture-owned device, command list, texture, view and swap chain.
    unsafe {
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: format,
            SampleDesc: samples,
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swap_chain = factory.CreateSwapChainForComposition(&host.queue, &swap_desc, None).unwrap().cast().unwrap();
        let copy = host.new_list().unwrap();
        copy.close();
        *host.presenter.lock().unwrap() = Some(Presenter {
            swap_chain, width, height, format, copy, hwnd: HWND::default(), meter: Meter::new(), compositor: None,
        });
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width.into(), Height: height, DepthOrArraySize: 1, MipLevels: 1,
            Format: format, SampleDesc: samples, Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let make = || {
            let mut texture: Option<ID3D12Resource> = None;
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
                D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut texture).unwrap();
            texture.unwrap()
        };
        let native = make();
        let scene = make();
        let overlay = make();
        let producer = host.new_list().unwrap();
        let float_desc=D3D12_RESOURCE_DESC{Format:DXGI_FORMAT_R16G16B16A16_FLOAT,..desc};
        let mut float_target=None;
        host.device.CreateCommittedResource(&heap,D3D12_HEAP_FLAG_NONE,&float_desc,D3D12_RESOURCE_STATE_RENDER_TARGET,None,&mut float_target).unwrap();
        let float=std::sync::Arc::new(crate::resource::HostTexture {
            texture:float_target.unwrap(),state:std::sync::atomic::AtomicU32::new(D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32),twin:None
        });
        producer.track(&float,D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32);
        producer.list.ClearRenderTargetView(host.rtv(&float.texture,None),&[1.,0.,0.,1.],None);
        producer.sample_target(&host.device,&float,1,0x0123456789abcdef,0,0x456);
        producer.list.ClearRenderTargetView(host.rtv(&float.texture,None),&[0.,0.,0.,1.],None);
        let length: usize = 3072;
        let (upload, mapped) = host.upload_buffer((2 * length) as u64).unwrap();
        let (test_buffer, _) = host.shared_buffer(65536).unwrap();
        let pattern: Vec<u8> = (0..2*length).map(|n| ((n*3+5)%251) as u8).collect();
        std::ptr::copy_nonoverlapping(pattern.as_ptr(),mapped,pattern.len());
        producer.buffer_state(&test_buffer,D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
        producer.list.CopyBufferRegion(&test_buffer,64,&upload,0,length as u64);
        producer.buffer_state(&test_buffer,BUFFER_WRITE);
        use crate::capture::buffer_sample::{Binding,SampleLocation,record_copy};
        for whole in [false,true] {
            record_copy(&producer,&host.device,&test_buffer,64,SampleLocation {
                binding:Binding {hash:0x0123456789abcdef, kind:victory_shader::ResourceType::Uav, slot:1, offset:0, whole},
                frame:1,address:0x1040,size:length as u64
            });
        }
        assert_eq!(producer.sampled_buffer_state(&test_buffer),Some(BUFFER_WRITE));
        producer.buffer_state(&test_buffer,D3D12_RESOURCE_STATE_COPY_DEST.0 as u32);
        producer.list.CopyBufferRegion(&test_buffer,64,&upload,length as u64,length as u64);
        for (texture, color) in [(&native, [0., 0., 1., 1.]), (&scene, [1., 0., 0., 1.]), (&overlay, [0.; 4])] {
            producer.list.ClearRenderTargetView(host.rtv(texture, None), &color, None);
        }
        let area = RECT { left: 8, top: 4, right: 19, bottom: 9 };
        producer.list.ClearRenderTargetView(host.rtv(&overlay, None), &[0., 1., 0., 1.], Some(&[area]));
        producer.close();
        host.execute(&[&producer]);
        assert_eq!(producer.deferred().samples[0].completed_bytes().unwrap(),pattern[..16]);
        assert_eq!(producer.deferred().samples[1].completed_bytes().unwrap(),pattern[..length]);
        {
            let deferred=producer.deferred();
            let sample=&deferred.target_samples[0];
            let got=crate::capture::save_read_back("fixture-fp16-after-clear",&sample.readback,&sample.footprint,true).unwrap();
            let expected=[0u8,0x3c,0,0,0,0,0,0x3c].repeat((width*height) as usize);
            assert_eq!(got,blake3::hash(&expected));
        }
        let state = D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32;
        assert!(host.present(&[(&native, state), (&overlay, state)], 1, PresentPath::CaptureTitle).is_some());
        assert!(host.present(&[(&scene, state), (&overlay, state)], 1, PresentPath::ScenePreview).is_some());
        host.device.GetDeviceRemovedReason().unwrap();
    }
    for (name, base, mode, result) in [
        ("frame-1-native", [0, 0, 255, 255], "CaptureTitle", "capture-only"),
        ("frame-1", [255, 0, 0, 255], "ScenePreview", "0x00000000")
    ] {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width { rgba.extend(if x >= 8 && y >= 4 { [0, 255, 0, 255] } else { base }); }
        }
        let digest = blake3::hash(&rgba).to_hex();
        let receipt = std::fs::read_to_string(folder.join(format!("{name}-copy.txt"))).unwrap();
        assert_eq!(receipt, format!("path={mode}\nplanes=2\nsource=unavailable\nbackbuffer={digest}\nmatched=composited\npresent={result}\n"));
        assert!(folder.join(format!("{name}.png")).exists());
    }
    std::fs::write(folder.join("capture-next.txt"), []).unwrap();
    std::fs::write(folder.join("preview-target.txt"), "pass 0123456789abcdef rtv 1").unwrap();
    std::fs::write(folder.join("trace-shaders.txt"), "FEDCBA9876543210,0123456789abcdef").unwrap();
    crate::capture::poll_request(97);
    assert!(crate::capture::watches(Some(" | pixel 0123456789abcdef")));
    assert!(!crate::capture::watches(Some(" | pixel 1111111111111111")));
    assert_eq!(crate::capture::preview_pass(), Some(crate::capture::PreviewPass {
        hash: 0x0123456789abcdef, output: crate::capture::OutputBinding::Rtv(1),
    }));
    crate::capture::submitted_pass(1, "fixture", true, 0x123, "1=0x456", "PS input", 2);
    crate::capture::save_passes(1);
    let passes = std::fs::read_to_string(folder.join("frame-1-passes.tsv")).unwrap();
    assert!(passes.contains("1\t1\tfixture\ttrue\t0x123\t1=0x456\tPS input"));
    for (frame, recorded) in [(7, 6), (8, 5), (10, 7)] {
        crate::capture::remember_writes(frame, recorded, "fixture", None,
            std::iter::once((0x456, Some(crate::capture::OutputBinding::Rtv(2)))));
    }
    crate::capture::save_history(11);
    let history = std::fs::read_to_string(folder.join("frame-11-history.tsv")).unwrap();
    assert!(history.contains("8\t5\t0x456\tfixture\trtv 2"));
    assert!(history.contains("10\t7\t0x456\tfixture\trtv 2"));
    assert!(!history.contains("7\t6\t"));
    assert!(crate::capture::trace_window(98));
    assert!(!crate::capture::listed(98));
    assert!(crate::capture::listed(101));
}
