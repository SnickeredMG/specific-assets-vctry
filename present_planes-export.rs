// Exported from victory-gta6-anaro, crates/victory-gpu/src/host.rs lines 8154-8592 (2026-11-25).
// The two-plane compositor and Host::present_planes / present_now. Not standalone: it uses the crate's
// Host, host(), compile(), transition(), settle_now(), swap chain helpers and the windows crate.

/// Two present planes in one image: the first (the game) scaled to fill
/// it, the second (the interface: menus, prompts, the HUD) blended over it
/// by its alpha. The Series S path presents two planes; the window showed
/// only the first, which is black in menus, so its prompts were invisible.
/// shortcut: no source rectangles or placement (each plane fills the
/// image), straight alpha, no colour-space conversion.
const COMPOSE: &str = r#"
#define RS "RootConstants(num32BitConstants = 2, b0), DescriptorTable(SRV(t0, numDescriptors = 2), visibility = SHADER_VISIBILITY_PIXEL), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_LINEAR, addressU = TEXTURE_ADDRESS_CLAMP, addressV = TEXTURE_ADDRESS_CLAMP, visibility = SHADER_VISIBILITY_PIXEL)"
cbuffer size : register(b0) { float2 inverse_size; };
Texture2D<float4> game : register(t0);
Texture2D<float4> ui : register(t1);
SamplerState clamped : register(s0);
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
[RootSignature(RS)]
float4 compose(float4 p : SV_Position) : SV_Target {
    float2 uv = p.xy * inverse_size;
    float4 g = game.Sample(clamped, uv);
    float4 o = ui.Sample(clamped, uv);
    return float4(lerp(g.rgb, o.rgb, o.a), 1.0);
}
"#;

/// Views of the two planes, in a heap of ours used as a ring (two slots a
/// present; a ring of this size is far from coming round while a present
/// is in the air).
const COMPOSE_VIEWS: u32 = 64;

struct Compositor {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    /// The image both planes are drawn into, and its size.
    target: Mutex<Option<(ID3D12Resource, u32, u32)>>,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Compositor {}
// SAFETY: as above.
unsafe impl Sync for Compositor {}

const COMPOSITE_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

/// Whether the two present planes are drawn into an image the size of the
/// first (the game) rather than of the larger one
/// (`VICTORY_COMPOSITE_SIZE=base`; off by default). On the Series S path the
/// game's plane is 1920x1080 and the interface's 3840x2160: the image, the
/// swap chain and the present were 4K, four times the game's pixels, with
/// the game stretched up to fill it. With this the interface is scaled down
/// into the game's size instead.
fn composite_base_size() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMPOSITE_SIZE").is_some_and(|v| v == "base"))
}

fn compositor() -> Option<&'static Compositor> {
    static ONE: OnceLock<Option<Compositor>> = OnceLock::new();
    ONE.get_or_init(|| match make_compositor() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "no compositor for two present planes: the first is shown alone");
            None
        }
    })
    .as_ref()
}

fn make_compositor() -> windows::core::Result<Compositor> {
    use windows::core::s;
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let vs = compile(COMPOSE, s!("corner"), s!("vs_5_1"))?;
    let ps = compile(COMPOSE, s!("compose"), s!("ps_5_1"))?;
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
    // SAFETY: plain object creation; the description points at live data
    // and never releases its pointer.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
        let mut targets = [DXGI_FORMAT_UNKNOWN; 8];
        targets[0] = COMPOSITE_FORMAT;
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: code(&vs[..]),
            PS: code(&ps[..]),
            BlendState: blend,
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: targets,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
        let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: COMPOSE_VIEWS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        tracing::info!("compositor for two present planes made");
        Ok(Compositor {
            cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
            step: device.GetDescriptorHandleIncrementSize(kind),
            views,
            next: AtomicU32::new(0),
            root,
            pipeline,
            target: Mutex::new(None),
        })
    }
}

/// The format a plane is read as: its family's plain form.
fn plane_view_format(format: DXGI_FORMAT) -> DXGI_FORMAT {
    DXGI_FORMAT(match format.0 {
        27..=32 => 28,
        87 | 90 | 91 => 87,
        23..=25 => 24,
        9..=14 => 10,
        other => other,
    })
}

impl Compositor {
    /// The image to draw `base` and `overlay` into: as large as the larger
    /// of the two, made again when that changes (after every submission has
    /// run: a present in the air may still read the old one). With
    /// [`composite_base_size`], as large as `base` instead.
    fn target(&self, base: &ID3D12Resource, overlay: &ID3D12Resource) -> Option<(ID3D12Resource, u32, u32)> {
        // SAFETY: live resources.
        let (a, b) = unsafe { (base.GetDesc(), overlay.GetDesc()) };
        let (width, height) = if composite_base_size() {
            (a.Width as u32, a.Height)
        } else {
            ((a.Width.max(b.Width)) as u32, a.Height.max(b.Height))
        };
        let mut target = self.target.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, w, h)) = target.as_ref()
            && (*w, *h) == (width, height)
        {
            return Some((t.clone(), width, height));
        }
        if target.is_some() {
            settle_now(WHY_SYNC);
        }
        let device = &host()?.device;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: COMPOSITE_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain object creation.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut made,
                )
                .ok()?;
        }
        let made = made?;
        tracing::info!(width, height, "compositor image for two present planes");
        *target = Some((made.clone(), width, height));
        Some((made, width, height))
    }

    /// Records into `list` the drawing of `base` and `overlay` (each in its
    /// host state, and left in it) into `target` (in the render-target state,
    /// and left in it). The pass sets the list's descriptor heap, root
    /// signature, pipeline, target, viewport, scissor and topology.
    fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        target: (&ID3D12Resource, u32, u32),
        base: (&ID3D12Resource, D3D12_RESOURCE_STATES),
        overlay: (&ID3D12Resource, D3D12_RESOURCE_STATES),
    ) {
        let Some(host) = host() else { return };
        let read = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        let first = (self.next.fetch_add(2, Ordering::Relaxed) % COMPOSE_VIEWS) & !1;
        let (target, width, height) = target;
        // SAFETY: an open list; slots of our heap; live resources.
        unsafe {
            for (i, (resource, _)) in [base, overlay].into_iter().enumerate() {
                let desc = resource.GetDesc();
                let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: plane_view_format(desc.Format),
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
                    },
                };
                let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: self.cpu + ((first + i as u32) * self.step) as usize,
                };
                host.device.CreateShaderResourceView(resource, Some(&view), cpu);
            }
            let mut to_read = Vec::new();
            let mut back = Vec::new();
            for (resource, state) in [base, overlay] {
                if state != read {
                    to_read.push(transition(resource, state, read));
                    back.push(transition(resource, read, state));
                }
            }
            if !to_read.is_empty() {
                list.ResourceBarrier(&to_read);
            }
            let rtv = host.rtv(Some(target), None);
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pipeline);
            let inverse = [(1.0 / width as f32).to_bits(), (1.0 / height as f32).to_bits()];
            list.SetGraphicsRoot32BitConstants(0, 2, inverse.as_ptr().cast(), 0);
            list.SetGraphicsRootDescriptorTable(1, D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: self.gpu + u64::from(first * self.step),
            });
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 }]);
            list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4));
            list.DrawInstanced(3, 1, 0, 0);
            if !back.is_empty() {
                list.ResourceBarrier(&back);
            }
        }
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        self.present_planes(texture, state, None, frame)
    }

    /// Shows `base`, which is in `base_state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on), with a second plane
    /// (`overlay`, in its host state) drawn over it ([`Compositor`]); `base`
    /// alone without one, or if the compositor cannot be made. `None` if
    /// nothing was shown; else, in diagnostics builds, whether the frame was
    /// black.
    pub(crate) fn present_planes(
        &self,
        base: &ID3D12Resource,
        base_state: u32,
        overlay: Option<(&ID3D12Resource, u32)>,
        frame: u64,
    ) -> Option<bool> {
        let showing = std::time::Instant::now();
        let shown = self.present_now(base, base_state, overlay, frame);
        crate::framelog::add(crate::framelog::T::PresentShowUs, showing.elapsed().as_micros() as u64);
        shown
    }

    fn present_now(
        &self,
        base: &ID3D12Resource,
        base_state: u32,
        overlay: Option<(&ID3D12Resource, u32)>,
        frame: u64,
    ) -> Option<bool> {
        let composed = overlay
            .and_then(|(over, over_state)| {
                let c = compositor()?;
                let target = c.target(base, over)?;
                Some((c, target, over, over_state))
            });
        let (texture, state) = match &composed {
            Some((_, (target, _, _), _, _)) => (target, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32),
            None => (base, base_state),
        };
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            if let Some((c, (target, width, height), over, over_state)) = &composed {
                c.record(
                    list,
                    (target, *width, *height),
                    (base, D3D12_RESOURCE_STATES(base_state as i32)),
                    (over, D3D12_RESOURCE_STATES(*over_state as i32)),
                );
            }
            list.ResourceBarrier(&[
                transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
            ]);
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() { None } else { crate::capture::probe(&self.device, list, texture) };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(&[
                transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ),
            ]);
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }
