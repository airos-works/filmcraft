//! The interface thread's event loop: Haiku's input goes to the window it happened in
//! (`viewports`, which gives every egui viewport a window of its own: the floating panels'),
//! FilmcraftApp runs its frame in the root viewport's pass, the main window's, and egui's output
//! goes back to the windows and the system.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use egui::{ViewportCommand, ViewportId};
use filmcraft_ui_egui::FilmcraftApp;

use crate::gpu::DeviceHealth;
use crate::host::{self, RawEvent, kind};
use crate::native_menu::{HostSink, NativeMenu};
use crate::platform::Haiku;
use crate::viewports::{Viewports, WindowInput};

const LOG: &str = "filmcraft_haiku";

pub struct Shell {
    pub app: FilmcraftApp,
    ctx: egui::Context,
    viewports: Viewports<Haiku>,
    /// The main window (the root viewport's), with the menu bar.
    root: i32,
    /// The GPU device's health while the windows draw with Vulkan.
    gpu_health: Option<DeviceHealth>,
    frame_info: filmcraft_ui_egui::integration::Frame,
    /// The interface scale (the system font against 12 pt) and device pixels per logical pixel
    /// ([`host::device_scale`]), and when the latter was last asked.
    ui_scale: f32,
    dev: f32,
    dev_checked: Instant,
    quit: bool,
    /// The native menu bar, and where its clicks go (the app's command inbox).
    menu: Option<(Rc<RefCell<NativeMenu<HostSink>>>, Sender<String>)>,
    /// When the main window's last frame started, and the shortest time between two
    /// ([`frame_interval`]).
    last_root: Instant,
    min_interval: Duration,
}

/// The shortest time between two frames of the main window: a display's refresh (60 Hz;
/// `FILMCRAFT_MAX_FPS` sets another rate, 0 none). Haiku has no vertical sync for a window's
/// bitmap, and while video plays the app asks for a frame as soon as the last one is out, which
/// drew the interface 300 times a second and kept a processor busy for nothing.
fn frame_interval() -> Duration {
    let fps = std::env::var("FILMCRAFT_MAX_FPS").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|f| f.is_finite() && *f >= 0.0).unwrap_or(60.0);
    if fps < 1.0 { Duration::ZERO } else { Duration::from_secs_f64(1.0 / fps.min(1000.0)) }
}

/// How the windows draw: `FILMCRAFT_RENDERER`, else `--graphics software`, else auto.
fn renderer(graphics: Option<crate::graphics::Mode>) -> Result<String, String> {
    let mode = std::env::var("FILMCRAFT_RENDERER")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| if graphics == Some(crate::graphics::Mode::Software) { "cpu" } else { "auto" }.into());
    if !matches!(mode.as_str(), "auto" | "cpu" | "vulkan") {
        return Err("FILMCRAFT_RENDERER must be auto, cpu or vulkan".into());
    }
    Ok(mode)
}

impl Shell {
    /// The shell around `app`, whose main window (the root viewport's) is native window `root`.
    pub fn new(mut app: FilmcraftApp, ctx: egui::Context, root: i32, ui_scale: f32, graphics: Option<crate::graphics::Mode>) -> Result<Self, String> {
        let mode = renderer(graphics)?;
        let gpu = if mode == "cpu" {
            None
        } else {
            match crate::guard("GPU startup", crate::gpu::Gpu::new) {
                Ok(gpu) => Some(gpu),
                Err(e) if mode == "vulkan" => return Err(e),
                Err(e) => {
                    log::warn!(target: LOG, "GPU startup: {e}; using software rendering");
                    app.ui.status = format!("{} ({e})", filmcraft_ui_egui::i18n::t("GPU acceleration is unavailable; preview renders on the CPU"));
                    None
                }
            }
        };
        let gpu_health = gpu.as_ref().map(|gpu| {
            // The monitors composite on the window's device (`FILMCRAFT_CPU_COMPOSITE` keeps them
            // on the CPU, as on the desktop), and its error handler is the device's health.
            if std::env::var_os("FILMCRAFT_CPU_COMPOSITE").is_none() {
                app.set_wgpu(gpu.state.clone());
            }
            let health = match &app.gpu {
                Some(state) => DeviceHealth(state.broken.clone()),
                None => {
                    let health = DeviceHealth(Default::default());
                    let flag = health.clone();
                    gpu.state.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
                        log::error!(target: LOG, "GPU error, switching to software rendering: {e}");
                        flag.mark();
                        host::wake();
                    }));
                    health
                }
            };
            // Exports render on the same device (GPU rendering Auto), and fall back to the CPU
            // renderer for what it can't do or once it fails.
            let lost = health.clone();
            filmcraft_gpu::export_renderer::share_device(
                gpu.instance.clone(),
                gpu.state.device.clone(),
                gpu.state.queue.clone(),
                Box::new(move |why| {
                    log::error!(target: LOG, "{why}");
                    lost.mark();
                    host::wake();
                }),
            );
            register_gpu_frame_renderer();
            health
        });
        log::info!(
            target: LOG,
            "air/OS renderer: {}; monitors composite on the {}",
            gpu.as_ref().map_or_else(|| "CPU".to_string(), |g| format!("Vulkan ({})", g.present_mode().name())),
            if app.gpu.is_some() { "GPU" } else { "CPU" }
        );
        let dev = host::device_scale();
        let viewports = Viewports::new(&ctx, Haiku::new(gpu, gpu_health.clone()), root, ui_scale * dev, dev);
        let menu = if std::env::var("FILMCRAFT_NATIVE_MENU").as_deref() == Ok("0") {
            None
        } else {
            let quit_keys = app.session.shortcuts.primary("app.quit").unwrap_or_else(|| "Cmd+Q".into());
            let menu = Rc::new(RefCell::new(NativeMenu::new(HostSink, filmcraft_ui_egui::i18n::t("Quit FilmCraft"), &quit_keys)));
            let (tx, rx) = std::sync::mpsc::channel();
            let shown = menu.clone();
            app.hooks.menu_changed = Some(Box::new(move |items| shown.borrow_mut().changed(items)));
            app.command_inbox = Some(rx);
            app.ui.show_menu_bar = false;
            Some((menu, tx))
        };
        Ok(Self {
            app,
            ctx,
            viewports,
            root,
            gpu_health,
            frame_info: Default::default(),
            ui_scale,
            dev,
            dev_checked: Instant::now(),
            quit: false,
            menu,
            last_root: Instant::now(),
            min_interval: frame_interval(),
        })
    }

    /// Runs until the app quits.
    pub fn run(&mut self) {
        while !self.quit {
            host::wait(self.viewports.timeout());
            self.drain();
            if self.dev_checked.elapsed() > Duration::from_secs(2) {
                self.check_density();
            }
            for viewport in self.viewports.take_due() {
                if viewport == ViewportId::ROOT {
                    let since = self.last_root.elapsed();
                    if since < self.min_interval {
                        self.viewports.request(ViewportId::ROOT, self.min_interval - since);
                        continue;
                    }
                    self.last_root = Instant::now();
                    self.frame();
                } else if self.viewports.run_deferred(viewport).close_root {
                    self.quit = true;
                }
            }
            self.viewports.chores();
        }
    }

    /// Asks for the desktop's density again (the monitors may have changed).
    fn check_density(&mut self) {
        self.dev_checked = Instant::now();
        let dev = host::device_scale();
        if (dev - self.dev).abs() > f32::EPSILON {
            log::info!(target: LOG, "display density {} -> {dev}", self.dev);
            self.dev = dev;
            self.viewports.set_density(self.ui_scale * dev, dev);
        }
    }

    /// Hands the queued native events to their windows; the application's own are handled here.
    fn drain(&mut self) {
        while let Some((ev, text)) = crate::dialogs::next_event() {
            if let Some(input) = window_input(&ev, &text) {
                if !self.viewports.input(ev.window, input) {
                    log::debug!(target: LOG, "event {} for window {}, which is gone", ev.kind, ev.window);
                }
                continue;
            }
            match ev.kind {
                // Quit (Command+Q, the Deskbar's Quit) closes the main window, which asks about
                // unsaved changes first.
                kind::QUIT_REQUESTED => {
                    self.viewports.input(self.root, WindowInput::Close);
                }
                // Tracker's Open, Open With and drops on the icon, and the files of a later launch
                // (one copy runs: B_SINGLE_LAUNCH): they arrive as files dropped on the main
                // window, so a project opens (asking about unsaved changes) and media is imported.
                kind::OPEN_FILES => {
                    let paths: Vec<PathBuf> = text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                    if !paths.is_empty() {
                        self.viewports.input(self.root, WindowInput::Dropped { x: -1.0, y: -1.0, paths });
                    }
                    host::window_command(self.root, &host::NativeCommand::Activate);
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                kind::MENU => {
                    if let Some((menu, tx)) = &self.menu
                        && let Some(command) = menu.borrow().command(ev.id)
                    {
                        let _ = tx.send(command.to_string());
                    }
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                kind::DIALOG_ANSWER => {
                    crate::dialogs::answer(ev.id, ev.x != 0.0, &text);
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                kind::COLORS => {
                    self.check_density();
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                _ => {}
            }
        }
    }

    /// Back to software for good: the monitors' compositor and every window. A device fault (the
    /// compositor, an export or a window's frame reported it) waits for this, the start of a
    /// frame: the last frame's compositor textures were GPU work the rasteriser can't draw.
    fn check_gpu(&mut self) {
        if !self.gpu_health.as_ref().is_some_and(DeviceHealth::faulted) {
            return;
        }
        log::warn!(target: LOG, "GPU fault; switching to software rendering");
        filmcraft_gpu::export_renderer::retire_shared_device("the window's GPU device failed");
        // The app drops its compositor at its next frame (it watches the same flag); drop it now,
        // before the rasteriser draws a frame that would use its texture.
        if let Some(state) = self.app.gpu.take()
            && let Some(id) = state.texture
        {
            state.render_state.renderer.write().free_texture(&id);
        }
        self.app.ui.status = filmcraft_ui_egui::i18n::t("Graphics problem: switched to software compositing").into();
        self.viewports.platform(Haiku::drop_gpu);
        self.gpu_health = None;
    }

    /// The root viewport's pass: the app's frame (the desktop app's `logic` and `ui`), which draws
    /// the other windows' viewports on the way.
    fn frame(&mut self) {
        self.check_gpu();
        let Some(mut raw) = self.viewports.root_input() else { return };
        self.app.frame_input(&self.ctx, &mut raw);
        let ctx = self.ctx.clone();
        let t0 = Instant::now();
        let mut closing = false;
        let out = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            closing = ctx.input(|i| i.viewport().close_requested());
            self.app.frame_logic(&ctx, &mut self.frame_info);
            self.app.frame_ui(ui, &mut self.frame_info);
        });
        let t1 = Instant::now();
        crate::dialogs::type_saved();
        // The main window's close button, Close Window or Quit: the app asks about unsaved
        // changes and cancels the close while it does (`CancelClose`), as eframe honours.
        let cancelled = out.viewport_output.get(&ViewportId::ROOT).is_some_and(|vp| vp.commands.iter().any(|c| matches!(c, ViewportCommand::CancelClose)));
        if closing && !cancelled {
            self.quit = true;
        }
        if self.viewports.finish(ViewportId::ROOT, out).close_root {
            self.quit = true;
        }
        if self.gpu_health.as_ref().is_some_and(DeviceHealth::faulted) {
            // The next frame draws in software.
            self.viewports.request(ViewportId::ROOT, Duration::ZERO);
        }
        log::debug!(target: LOG, "frame: interface {:.1} ms, output and drawing {:.1} ms", (t1 - t0).as_secs_f64() * 1e3, t1.elapsed().as_secs_f64() * 1e3);
    }

    /// What quitting does besides closing the windows (`on_exit` in the desktop app): the
    /// auto-save journal is flushed, or removed when nothing is unsaved.
    pub fn finish(&mut self) {
        self.app.frame_exit();
    }
}

/// The GPU export frame renderer (filmcraft-gpu's off-screen compositor behind filmcraft-export's
/// frame-renderer hook), as the desktop app registers it: exports with GPU rendering Auto
/// composite on the GPU and fall back to the CPU reference renderer wherever it cannot.
struct GpuFrameRenderer(filmcraft_gpu::ExportRenderer);

impl filmcraft_export::FrameRenderer for GpuFrameRenderer {
    fn render(
        &mut self,
        project: &filmcraft_project::Project,
        seq: filmcraft_project::ItemId,
        t: filmcraft_time::Tick,
        opts: filmcraft_render::RenderOptions,
        sources: &dyn filmcraft_render::SourceProvider,
    ) -> Option<filmcraft_render::Image> {
        self.0.render(project, seq, t, opts, sources)
    }

    fn caps(&self) -> filmcraft_render::plan::DeviceCaps {
        self.0.caps()
    }
}

fn register_gpu_frame_renderer() {
    filmcraft_export::register_frame_renderer(|| {
        filmcraft_gpu::ExportRenderer::new().map(|r| Box::new(GpuFrameRenderer(r)) as Box<dyn filmcraft_export::FrameRenderer>)
    });
}

/// A native event that happened in a window, as that window's input.
fn window_input<'a>(ev: &RawEvent, text: &'a str) -> Option<WindowInput<'a>> {
    Some(match ev.kind {
        kind::MOUSE_MOVED | kind::MOUSE_DOWN | kind::MOUSE_UP => WindowInput::Pointer { x: ev.x, y: ev.y, buttons: ev.buttons, modifiers: ev.modifiers },
        kind::POINTER_GONE => WindowInput::PointerGone,
        // A wheel's whole steps, or (`repeat` 1) a touchpad's scroll gesture with its unrounded
        // steps and its phase in `clicks` (`host.cpp`).
        kind::WHEEL => WindowInput::Wheel { dx: ev.dx, dy: ev.dy, modifiers: ev.modifiers, gesture: (ev.repeat != 0).then_some(ev.clicks) },
        kind::KEY_DOWN | kind::KEY_UP => {
            WindowInput::Key { pressed: ev.kind == kind::KEY_DOWN, code: ev.key, raw_char: ev.raw_char, modifiers: ev.modifiers, text }
        }
        kind::MODIFIERS => WindowInput::Modifiers(host::modifiers()),
        kind::FOCUS => WindowInput::Focus(ev.x != 0.0),
        kind::CLOSE => WindowInput::Close,
        // Files dropped on a window go to the app as the desktop app's do (the Project panel,
        // or the Timeline with Timeline ▸ drop imports to the timeline).
        kind::DROPPED_FILES => WindowInput::Dropped { x: ev.x, y: ev.y, paths: text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect() },
        // The next frame picks up the new size, position or state.
        kind::RESIZED | kind::MOVED | kind::MINIMIZED => WindowInput::Changed,
        _ => return None,
    })
}
