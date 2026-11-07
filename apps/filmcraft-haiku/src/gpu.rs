//! Hardware rendering of the windows. Haiku has no wgpu surface; egui's triangles and the
//! document canvas draw into a presenter's frame texture (`present.rs`), which gets them onto the
//! screen. One device and one egui renderer (the textures, and the canvas's paint callback
//! resources, are the whole app's) draw every window, each into a [`Surface`] of its own (its
//! presenter).
use egui_wgpu::{RenderState, Renderer, RendererOptions, ScreenDescriptor, WgpuConfiguration, WgpuSetup};
use std::sync::Arc;
use std::time::Instant;

use crate::host::Native;
use crate::present::{self, Presenter, Shown};
use crate::texture_fit::TextureFit;

const LOG: &str = "filmcraft_haiku";

/// The device's health, shared with the monitors' compositor: set by the compositor's
/// uncaptured-error handler (`GpuState::broken`, installed by `FilmcraftApp::set_wgpu`), by the
/// device-lost callback, or by a window frame that failed. The host then draws in software from
/// the next frame on (`shell.rs`).
#[derive(Clone, Debug)]
pub struct DeviceHealth(pub Arc<std::sync::atomic::AtomicBool>);

impl DeviceHealth {
    pub fn faulted(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn mark(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct Gpu {
    pub state: RenderState,
    /// The instance the device is from (exports render on the same device: `share_device`).
    pub instance: wgpu::Instance,
    mode: present::Mode,
    /// Bumped by every texture upload, which can change any window's pixels.
    textures: u64,
    /// egui textures larger than the device takes, uploaded smaller.
    fit: TextureFit,
}

/// One window's presenter.
pub struct Surface {
    presenter: Presenter,
    native: Native,
    /// What the frame on the window was drawn from ([`frame_key`], and the texture uploads so
    /// far): a frame drawn from the same is not drawn again. egui repaints a panel's window with
    /// every frame of the main window (its viewport is drawn in the main window's pass), and here
    /// each would cost a submission and two waits for the GPU.
    last: Option<(u64, u64)>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.presenter.detach(&self.native);
    }
}

impl Gpu {
    pub fn new() -> Result<Self, String> {
        let mut setup = WgpuSetup::without_display_handle();
        let WgpuSetup::CreateNew(ref mut create) = setup else { return Err("Expected a new GPU device".into()) };
        create.instance_descriptor.backends = wgpu::Backends::VULKAN;
        create.native_adapter_selector = Some(Arc::new(|adapters, _surface| choose_adapter(adapters)));
        // The monitors' compositor and the exports (`filmcraft-gpu`) run on this device too: they
        // check `device.limits()` for what they need (textures for every layer, the effect stage's
        // compute) and composite what doesn't fit on the CPU.
        create.device_descriptor =
            Arc::new(|adapter| wgpu::DeviceDescriptor { label: Some("FilmCraft"), required_limits: device_limits(adapter), ..Default::default() });
        let instance = pollster::block_on(setup.new_instance());
        let config = WgpuConfiguration { wgpu_setup: setup, ..Default::default() };
        let mut state = pollster::block_on(RenderState::create(&config, &instance, None, RendererOptions::default())).map_err(|e| e.to_string())?;
        // Frames are drawn in the screen's byte order: they go to the frame buffer, or into a
        // B_RGB32 bitmap, as they are.
        state.target_format = present::FORMAT;
        state.renderer = Arc::new(egui::mutex::RwLock::new(Renderer::new(&state.device, present::FORMAT, RendererOptions::default())));
        log::info!(target: LOG, "air/OS GPU: {:?}", state.adapter.get_info());
        log::info!(target: LOG, "GPU device: {}", describe(&state.device));
        Ok(Self { state, instance, mode: present::Mode::from_env("FILMCRAFT_PRESENT"), textures: 0, fit: TextureFit::default() })
    }

    /// A presenter for native window `window`.
    pub fn surface(&self, window: i32) -> Surface {
        let native = Native(window);
        let presenter = Presenter::new(&self.state.device, &self.state.queue, self.mode, &native);
        log::info!(target: LOG, "air/OS present (window {window}): {}", presenter.mode().name());
        Surface { presenter, native, last: None }
    }

    pub fn max_texture_side(&self) -> usize {
        self.state.device.limits().max_texture_dimension_2d as usize
    }

    /// How frames reach the windows (`FILMCRAFT_PRESENT`).
    pub fn present_mode(&self) -> present::Mode {
        self.mode
    }

    /// egui's texture uploads, before the frames that use them.
    pub fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        if delta.set.is_empty() {
            return;
        }
        self.textures += 1;
        let state = &self.state;
        let max = state.device.limits().max_texture_dimension_2d as usize;
        let mut renderer = state.renderer.write();
        for (id, images) in &delta.set {
            for image in images {
                if let Some(image) = self.fit.fit(*id, image, max) {
                    renderer.update_texture(&state.device, &state.queue, *id, &image);
                }
            }
        }
    }

    /// Textures egui freed, once every window's frame that used them is drawn.
    pub fn free_textures(&mut self, ids: &[egui::TextureId]) {
        let mut renderer = self.state.renderer.write();
        for id in ids {
            renderer.free_texture(id);
            self.fit.free(*id);
        }
    }

    /// Draws a window's frame and shows it (`scale` device pixels per logical pixel). The
    /// textures it uses are uploaded ([`Gpu::set_textures`]).
    pub fn render(
        &mut self,
        surface: &mut Surface,
        size: [u32; 2],
        primitives: &[egui::ClippedPrimitive],
        ppp: f32,
        clear: egui::Color32,
        scale: f32,
    ) -> Result<Shown, String> {
        let key = frame_key(size, primitives, ppp, clear).map(|k| (k, self.textures));
        if key.is_some() && key == surface.last && !surface.presenter.wants_present() {
            return Ok(Shown { direct: false, pixels: 0, rects: 0, wait: std::time::Duration::ZERO });
        }
        surface.last = None;
        let state = &self.state;
        let view = surface.presenter.target(size)?;
        let start = Instant::now();
        let screen = ScreenDescriptor { size_in_pixels: size, pixels_per_point: ppp };
        let mut encoder = state.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("FilmCraft native frame") });
        let mut renderer = state.renderer.write();
        let mut commands = renderer.update_buffers(&state.device, &state.queue, &mut encoder, primitives, &screen);
        {
            let rgba = clear.to_array().map(|c| f64::from(c) / 255.0);
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("FilmCraft native UI"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3] }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            renderer.render(&mut pass, primitives, &screen);
        }
        commands.push(encoder.finish());
        drop(renderer);
        let encoded = start.elapsed();
        let shown = surface.presenter.present(commands, &surface.native, scale)?;
        surface.last = key;
        log::debug!(target: LOG,
            "GPU frame {}x{} (window {}): encode {:.2} ms, present {:.2} ms ({}, {} px in {} rects)",
            size[0],
            size[1],
            surface.native.0,
            encoded.as_secs_f64() * 1e3,
            shown.wait.as_secs_f64() * 1e3,
            if shown.direct { "direct" } else { "readback" },
            shown.pixels,
            shown.rects
        );
        Ok(shown)
    }
}

impl Surface {
    /// The last frame again, where the window's place on the screen changed.
    pub fn present_again(&mut self, scale: f32) -> Result<Shown, String> {
        self.presenter.present(Vec::new(), &self.native, scale)
    }

    pub fn wants_present(&self) -> bool {
        self.presenter.wants_present()
    }

    pub fn sync_due(&self) -> Option<Instant> {
        self.presenter.sync_due()
    }

    /// Brings app_server's copy of the window up to date with what went straight to the screen.
    pub fn sync(&mut self, scale: f32) -> Result<Shown, String> {
        self.presenter.sync(&self.native, scale)
    }

    pub fn image(&mut self) -> Result<egui::ColorImage, String> {
        self.presenter.image()
    }
}

/// `FILMCRAFT_GPU_LIMITS=small`: create the device as on a small GPU (wgpu's downlevel limits: 4
/// storage buffers per stage, 16 KiB uniform bindings, 4 colour attachments; with V3DV's 4096 px
/// textures), to check on a big one that the app still runs, or falls back per operation, without
/// losing the device.
const LIMITS_VAR: &str = "FILMCRAFT_GPU_LIMITS";
/// The smallest texture side worth drawing a window into.
const MIN_TEXTURE_SIDE: u32 = 2048;
/// The largest texture side asked for: room for any window, and EffectCraft's cap.
const MAX_TEXTURE_SIDE: u32 = 16384;

/// Whether [`LIMITS_VAR`] asks for a small GPU's limits.
fn small_gpu() -> bool {
    std::env::var(LIMITS_VAR).is_ok_and(|v| v.trim().eq_ignore_ascii_case("small"))
}

/// `limits`, lowered to a small GPU's when [`LIMITS_VAR`] asks for it.
fn as_small_gpu(limits: wgpu::Limits) -> wgpu::Limits {
    if !small_gpu() {
        return limits;
    }
    limits.or_worse_values_from(&wgpu::Limits { max_texture_dimension_1d: 4096, max_texture_dimension_2d: 4096, ..wgpu::Limits::downlevel_defaults() })
}

/// The limits the device is created with on `adapter`: its own, which it always grants, with
/// textures capped at [`MAX_TEXTURE_SIDE`]. egui-wgpu's default asks for WebGPU's (8192 px
/// textures, 8 colour attachments), which the arm64 boards' GPUs refuse (V3DV: 4096 px textures,
/// 4 attachments), sending them to the rasteriser; NVIDIA's and AMD's get the same or more.
fn device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let have = as_small_gpu(adapter.limits());
    wgpu::Limits { max_texture_dimension_2d: have.max_texture_dimension_2d.min(MAX_TEXTURE_SIDE), ..have }
}

/// Why `adapter` can't draw the windows, or `None` when it can.
fn unusable(adapter: &wgpu::Adapter) -> Option<String> {
    let info = adapter.get_info();
    if !matches!(info.device_type, wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu) {
        // Software Vulkan (lavapipe) draws slower than the rasteriser does.
        return Some(format!("not a hardware GPU ({:?})", info.device_type));
    }
    use wgpu::TextureUsages as U;
    let needed = U::RENDER_ATTACHMENT | U::COPY_SRC | U::COPY_DST | U::TEXTURE_BINDING;
    let usages = adapter.get_texture_format_features(present::FORMAT).allowed_usages;
    if !usages.contains(needed) {
        return Some(format!("{:?} textures lack {:?}", present::FORMAT, needed.difference(usages)));
    }
    if !adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
        return Some("no compute shaders (the presenter compares frames with one)".into());
    }
    let l = as_small_gpu(adapter.limits());
    if l.max_texture_dimension_2d < MIN_TEXTURE_SIDE {
        return Some(format!("textures up to {} px only", l.max_texture_dimension_2d));
    }
    // The presenter's comparison runs 16 × 16 workgroups writing one storage buffer.
    if l.max_compute_invocations_per_workgroup < 256
        || l.max_compute_workgroup_size_x < 16
        || l.max_compute_workgroup_size_y < 16
        || l.max_storage_buffers_per_shader_stage < 1
    {
        return Some(format!(
            "compute too small for the presenter ({} invocations per workgroup, {} storage buffers per stage)",
            l.max_compute_invocations_per_workgroup, l.max_storage_buffers_per_shader_stage
        ));
    }
    None
}

/// The hardware adapter to draw with, a discrete GPU before an integrated one; every adapter
/// left out is logged with the reason.
fn choose_adapter(adapters: &[wgpu::Adapter]) -> Result<wgpu::Adapter, String> {
    let mut best: Option<&wgpu::Adapter> = None;
    for adapter in adapters {
        let info = adapter.get_info();
        if let Some(why) = unusable(adapter) {
            log::info!(target: LOG, "GPU adapter {} ({}, {}) not used: {why}", info.name, info.driver, info.driver_info);
            continue;
        }
        let rank = |a: &wgpu::Adapter| u8::from(a.get_info().device_type == wgpu::DeviceType::DiscreteGpu);
        if best.is_none_or(|b| rank(adapter) > rank(b)) {
            best = Some(adapter);
        }
    }
    best.cloned().ok_or_else(|| "No usable hardware Vulkan adapter; using software rendering".to_string())
}

/// What the device got, for the log.
fn describe(device: &wgpu::Device) -> String {
    let l = device.limits();
    format!(
        "textures {} px, buffers {} MiB, storage bindings {} MiB, {} storage buffers and {} storage textures per stage, \
         uniform bindings {} KiB, {} colour attachments, {} invocations per workgroup, features {:?}{}",
        l.max_texture_dimension_2d,
        l.max_buffer_size >> 20,
        l.max_storage_buffer_binding_size >> 20,
        l.max_storage_buffers_per_shader_stage,
        l.max_storage_textures_per_shader_stage,
        l.max_uniform_buffer_binding_size >> 10,
        l.max_color_attachments,
        l.max_compute_invocations_per_workgroup,
        device.features(),
        if small_gpu() { format!(" ({LIMITS_VAR}=small)") } else { String::new() }
    )
}

/// A small, fast hash (FxHash's mix, as `raster.rs`) of everything that decides a frame's pixels,
/// or `None` for a frame that must be drawn (a paint callback or a user texture draws what the
/// hash can't see).
/// Texture uploads are checked by the caller.
fn frame_key(size: [u32; 2], primitives: &[egui::ClippedPrimitive], ppp: f32, clear: egui::Color32) -> Option<u64> {
    let mut k: u64 = 0;
    let mut add = |v: u64| k = (k.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    add(u64::from(size[0]) << 32 | u64::from(size[1]));
    add(u64::from(ppp.to_bits()));
    add(u64::from(u32::from_le_bytes(clear.to_array())));
    for p in primitives {
        let r = p.clip_rect;
        add(u64::from(r.min.x.to_bits()) << 32 | u64::from(r.min.y.to_bits()));
        add(u64::from(r.max.x.to_bits()) << 32 | u64::from(r.max.y.to_bits()));
        let egui::epaint::Primitive::Mesh(mesh) = &p.primitive else { return None };
        // A user texture is the monitors' compositor output (`FilmcraftApp::gpu_present`), which
        // changes under the same id while video plays: such a frame is always drawn (the
        // presenter still copies back only the tiles that changed).
        add(match mesh.texture_id {
            egui::TextureId::Managed(n) => n,
            egui::TextureId::User(_) => return None,
        });
        add(mesh.indices.len() as u64);
        for &i in &mesh.indices {
            add(u64::from(i));
        }
        for v in &mesh.vertices {
            add(u64::from(v.pos.x.to_bits()) << 32 | u64::from(v.pos.y.to_bits()));
            add(u64::from(v.uv.x.to_bits()) << 32 | u64::from(v.uv.y.to_bits()));
            add(u64::from(u32::from_le_bytes(v.color.to_array())));
        }
    }
    Some(k)
}
