//! FilmCraft's video editor hosted by the native Be API: a `BApplication` and a `BWindow` per egui
//! viewport (`native/host.cpp`, `viewports`, `platform`): the main window and the floating
//! panels' own windows the desktop app opens. Playback and voice-over go through the Media Kit
//! (`audio.rs`, `audio_in.rs`), the Media Kit's decoders read what FilmCraft's own can't
//! (`media_kit.rs`), and a hardware video decoder add-on takes H.264 and HEVC where there is one
//! (`hw_decode.rs`). The window, the monitors' compositor and exports draw with one Vulkan device
//! (`gpu.rs`), or in software.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![cfg_attr(not(target_os = "haiku"), allow(dead_code))]

#[cfg(target_os = "haiku")]
#[path = "../../filmcraft/src/args.rs"]
mod args;
#[cfg(target_os = "haiku")]
mod audio;
#[cfg(target_os = "haiku")]
mod audio_in;
#[cfg(target_os = "haiku")]
#[path = "../../filmcraft/src/control_server.rs"]
mod control_server;
mod damage;
#[cfg(target_os = "haiku")]
mod dialogs;
#[cfg(target_os = "haiku")]
mod gpu;
mod graphics;
#[cfg(target_os = "haiku")]
mod host;
#[cfg(target_os = "haiku")]
mod hw_decode;
mod keys;
#[cfg(target_os = "haiku")]
#[path = "../../filmcraft/src/logging.rs"]
mod logging;
#[cfg(target_os = "haiku")]
mod media_kit;
mod native_menu;
#[cfg(target_os = "haiku")]
mod platform;
#[cfg(target_os = "haiku")]
mod present;
mod raster;
#[cfg(target_os = "haiku")]
mod scanout;
#[cfg(target_os = "haiku")]
mod services;
#[cfg(target_os = "haiku")]
mod shell;
mod viewports;

#[cfg(not(target_os = "haiku"))]
fn main() {
    eprintln!("FilmCraft for air/OS requires Haiku; run `filmcraft` on other platforms.");
    std::process::exit(1);
}

#[cfg(target_os = "haiku")]
fn main() {
    if let Err(e) = native_main() {
        eprintln!("FilmCraft: {e}");
        std::process::exit(1);
    }
}

/// The application's MIME signature (`resources/FilmCraft.rdef`).
#[cfg(target_os = "haiku")]
pub const SIGNATURE: &str = "application/x-vnd.storytold-FilmCraft";

/// The commit this build is from (`FILMCRAFT_BUILD_SHA`, set by tools/haiku/build.sh and CI).
#[cfg(target_os = "haiku")]
fn build_sha() -> &'static str {
    option_env!("FILMCRAFT_BUILD_SHA").filter(|s| !s.is_empty()).unwrap_or("dev")
}

/// Worker threads for rayon's global pool. air/OS gets slower past about four threads for heavy
/// parallel work (6.8 s with 4 against 18 s with 32 on a 32-thread Threadripper), so the pool is
/// capped there; `FILMCRAFT_THREADS` overrides it.
#[cfg(target_os = "haiku")]
fn worker_threads() -> usize {
    if let Some(n) = std::env::var("FILMCRAFT_THREADS").ok().and_then(|s| s.trim().parse::<usize>().ok()).filter(|n| (1..=256).contains(n)) {
        return n;
    }
    std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 4))
}

#[cfg(target_os = "haiku")]
fn native_main() -> Result<(), String> {
    use args::Cli;
    host::export_locale();
    let logger = logging::install();
    let env_port = std::env::var_os("FILMCRAFT_CONTROL_PORT").map(|p| p.to_string_lossy().into_owned());
    let launch = match args::parse(env_port, std::env::args_os().skip(1)) {
        Ok(Cli::Run(l)) => l,
        Ok(Cli::Help) => {
            print!("{}", args::USAGE.replace("dx12 (Windows), software (Windows, Linux)", "software (air/OS: FILMCRAFT_RENDERER=cpu)"));
            return Ok(());
        }
        Ok(Cli::Version) => {
            println!("FilmCraft {} for air/OS ({})", env!("CARGO_PKG_VERSION"), build_sha());
            return Ok(());
        }
        Err(e) => {
            eprint!("filmcraft: {e}\n\n{}", args::USAGE);
            std::process::exit(2);
        }
    };
    // Before the log file, the settings or the recovery records are touched: a second launch
    // (from Terminal; Tracker hands documents to the running copy itself) passes its files to the
    // running copy and ends. Automation keeps a process of its own in the desktop app, which one
    // executable file can't have here.
    if host::init(SIGNATURE)? == host::Launch::HandedOver {
        if launch.control_port.is_some() {
            return Err("FilmCraft is already running from this file; --control needs a copy of its own (run it from another file)".into());
        }
        std::process::exit(0);
    }
    let data_root = launch.data_dir.clone().or_else(filmcraft_engine::autosave::default_data_dir);
    let log_dir = data_root.as_ref().map(|d| d.join(logging::LOG_DIR));
    log::info!("FilmCraft {} for air/OS ({}, {})", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH, build_sha());
    if let Some(logger) = logger {
        match log_dir.as_deref().map(|d| logger.attach_dir(d)) {
            Some(Ok(path)) => log::info!("log file {}", path.display()),
            Some(Err(e)) => log::warn!("no log file: {e}"),
            None => {
                logger.stderr_only();
                log::warn!("no log file: no data directory (set FILMCRAFT_DATA_DIR or --data-dir)");
            }
        }
    }
    // Panics anywhere go to <data dir>/Logs/crash-<day>.log with a backtrace; the UI pass and
    // frame workers catch them and keep running (filmcraft_ui_egui::crash).
    let alert_logs = log_dir.clone();
    filmcraft_ui_egui::crash::install(log_dir);
    services::claim_document_types();
    let workers = worker_threads();
    if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(workers).thread_name(|i| format!("FilmCraft worker {i}")).build_global() {
        log::warn!("worker pool: {e}");
    }
    log::info!("{workers} worker threads");
    // The Media Kit's decoders for what FilmCraft's own don't read, and the hardware video
    // decoder add-ons for H.264 and HEVC.
    media_kit::install();
    hw_decode::install();
    let scale = host::ui_scale();
    // The main window opens before the interface starts, so it shows at once.
    let root = host::open_window(&host::WindowSpec {
        title: "FilmCraft".into(),
        position: None,
        size: [1600.0 * scale, 980.0 * scale],
        min_size: Some([900.0 * scale, 560.0 * scale]),
        max_size: None,
        decorations: true,
        resizable: true,
        visible: true,
        zoomed: false,
        fullscreen: false,
        floating: false,
        buttons: [true; 3],
    })
    .ok_or("couldn't open the window")?;
    let worker = std::thread::Builder::new()
        .name("FilmCraft interface".into())
        .stack_size(16 << 20)
        .spawn(move || {
            let result = guard("Interface", || {
                let ctx = egui::Context::default();
                let app = services::build_app(&ctx, launch.clone())?;
                let mut shell = shell::Shell::new(app, ctx, root, scale, launch.graphics)?;
                shell.run();
                shell.finish();
                Ok(())
            });
            if let Err(e) = &result {
                log::error!("{e}");
                filmcraft_ui_egui::crash::record(e);
                match &alert_logs {
                    Some(dir) => host::alert("FilmCraft", &format!("{e}\n\nThe logs are in {}.", dir.display())),
                    None => host::alert("FilmCraft", e),
                }
            }
            host::quit();
            result
        })
        .map_err(|e| e.to_string())?;
    host::run();
    worker.join().map_err(|_| "Interface thread stopped unexpectedly".to_string())?
}

/// Runs `f`, turning a panic into an error naming `what` (the panic hook has logged it).
#[cfg(target_os = "haiku")]
pub fn guard<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into());
            Err(format!("{what} failed: {msg}"))
        }
    }
}
