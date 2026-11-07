//! The app and the system services it asks its host for (`HostHooks`), the Haiku way: file
//! panels, Tracker, the clipboard, the Locale Kit's languages, the Appearance preferences' colours,
//! and Media Kit audio. Mirrors `app_creator` in the desktop app's `main.rs`.

use std::sync::atomic::Ordering;

use chrono::TimeZone;
use filmcraft_engine::Session;
use filmcraft_engine::autosave::{AutosaveConfig, default_data_dir};
use filmcraft_ui_egui::FilmcraftApp;
use serde_json::json;

use crate::args::{self, Launch};
use crate::dialogs::{self, Mode};
use crate::host;

/// The document types FilmCraft becomes the preferred application of: its projects only. It opens
/// the movie types too (its `file_types` resource lists them, so Tracker offers it in Open With),
/// but double-clicking a movie should play it: a fresh system runs the post-install scripts before
/// anyone starts AirTime, the system's player, and AirTime only takes types over from MediaPlayer
/// or nobody, so claiming them here would leave movies opening in the editor.
const DOCUMENT_TYPES: &[&str] = &["application/x-filmcraft"];

/// Makes FilmCraft the preferred application of its project type if it has none (the
/// post-install script does the same when the package is installed).
pub fn claim_document_types() {
    let claimed = host::claim_types(crate::SIGNATURE, DOCUMENT_TYPES);
    if claimed > 0 {
        log::info!("preferred application for {claimed} document types");
    }
}

pub fn clipboard_text() -> Option<String> {
    host::clipboard_get("text/plain").map(|v| String::from_utf8_lossy(&v).trim_end_matches('\0').to_string())
}

/// An image on the clipboard (Paste in the Timeline and the Project panel): PNG, as browsers and
/// screenshot tools put it there.
fn clipboard_media() -> Option<filmcraft_ui_egui::ClipboardMedia> {
    let png = host::clipboard_get("image/png")?;
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(filmcraft_ui_egui::ClipboardMedia::Image { width: w as usize, height: h as usize, rgba: img.into_raw() })
}

/// Opens a file with its preferred application, or (`reveal`) shows its folder in Tracker.
fn open_path(path: &str, reveal: bool) -> Result<(), String> {
    let p = std::path::Path::new(path);
    let target = if reveal { p.parent().unwrap_or(p) } else { p };
    std::process::Command::new("/bin/open").arg(target).spawn().map(|_| ()).map_err(|e| format!("can't open {path}: {e}"))
}

/// Local UTC offset at a unix time (auto-save file names and recovery times use local time).
fn local_offset(unix: i64) -> i32 {
    chrono::Local.timestamp_opt(unix, 0).single().map(|d| d.offset().local_minus_utc()).unwrap_or(0)
}

/// Settings ▸ Audio Hardware's extensions for a file panel: what `exts` names, no wildcard.
fn exts<'a>(e: &'a [&'a str]) -> Vec<&'a str> {
    e.iter().copied().filter(|x| !x.is_empty()).collect()
}

/// The app as the desktop app's `app_creator` builds it, with the air/OS services.
pub fn build_app(ctx: &egui::Context, launch: Launch) -> Result<FilmcraftApp, String> {
    let Launch { control_port, control_input_only, files, demo, startup_flag, recover, data_dir, graphics: _ } = launch;
    filmcraft_ui_egui::install_ime_reset_callbacks(ctx);
    // Start the control server before anything touches recovery state (#3436).
    let control = control_port.map(|port| crate::control_server::start(port, ctx.clone()));
    if control_input_only && let Some((_, _, Some(why))) = &control {
        return Err(crate::control_server::ControlServerRequired(why.reason()).to_string());
    }
    let mut session = Session::default();
    if let Some(dir) = data_dir.clone().or_else(default_data_dir) {
        let mut cfg = AutosaveConfig::new(dir);
        cfg.local_offset = local_offset;
        if let Err(e) = session.start_autosave(cfg) {
            log::warn!("auto-save and crash recovery unavailable: {e}");
        }
    }
    // Voice-over recording reads the system's audio input through the Media Kit.
    session.voiceover.input = Some(Box::new(crate::audio_in::MediaKitIn::new()));
    let (project, media) = args::project_and_media(&files);
    if let Some(p) = project {
        if let Err(e) = session.execute("file.open", json!({"path": p})) {
            log::error!("{e}");
        }
    } else if !startup_flag && session.prefs.general.at_startup == "openMostRecent" {
        let recent = session.prefs.general.recent_projects.iter().find(|p| std::path::Path::new(p).exists()).cloned();
        match recent {
            Some(p) => {
                if let Err(e) = session.execute("file.open", json!({"path": p})) {
                    log::error!("{e}");
                }
            }
            None => {
                let _ = session.execute("file.openDemoProject", json!({}));
            }
        }
    } else if !startup_flag && session.prefs.general.at_startup == "emptyProject" {
    } else if demo {
        let _ = session.execute("file.openDemoProject", json!({}));
    }
    if !media.is_empty() {
        let _ = session.execute("file.import", json!({"paths": media}));
    }
    if recover == Some(true) && !session.recovery_candidates().is_empty() {
        let id = session.recovery_candidates()[0].id.clone();
        match session.execute("file.recover", json!({"id": id})) {
            Ok(r) => log::info!("recovered {r}"),
            Err(e) => log::error!("recovery failed: {e}"),
        }
    }
    let mut app = FilmcraftApp::new(session);
    // air/OS updates FilmCraft as a package (SoftwareUpdater): no start-up check.
    app.set_startup_update_check(false);
    if recover == Some(false) {
        app.dialog = None;
    }
    app.integrated_titlebar = false;
    // Playback through a Media Kit sound player; Settings ▸ Audio Hardware applies on the first
    // frame.
    app.audio = Some(Box::new(crate::audio::MediaKitOut::new()));
    install_hooks(&mut app);
    if let Some((rx, ready, disabled)) = control {
        app = if control_input_only { app.with_control_input_only(rx) } else { app.with_control(rx) };
        if let Some(crate::control_server::DisabledWhy::Token) = &disabled {
            app = app.with_control_notice(filmcraft_ui_egui::ControlNotice::Token);
        }
        ready.store(true, Ordering::Relaxed);
    }
    Ok(app)
}

fn install_hooks(app: &mut FilmcraftApp) {
    let h = &mut app.hooks;
    // The file panels don't hold up the frame loop (`dialogs.rs`); the synchronous pickers are for
    // the few places that still call them.
    h.run_dialog = Some(Box::new(dialogs::run));
    h.pick_files = Some(Box::new(|e: &[&str]| dialogs::pick(Mode::OpenMany, "", &exts(e), "Import")));
    h.pick_file_for_relink = Some(Box::new(|e: &[&str], _hint| dialogs::pick(Mode::Open, "", &exts(e), "Link Media").into_iter().next()));
    h.pick_save = Some(Box::new(|name: &str| dialogs::pick(Mode::Save, name, &["fcproj"], "Save Project").into_iter().next()));
    h.pick_save_as = Some(Box::new(|filter: &str, e: &[&str], name: &str| dialogs::pick(Mode::Save, name, &exts(e), filter).into_iter().next()));
    h.pick_folder = Some(Box::new(|| dialogs::pick(Mode::Folder, "", &[], "Choose Folder").into_iter().next()));
    h.pick_folder_at = Some(Box::new(|dir: &str| dialogs::pick(Mode::Folder, dir, &[], "Choose Folder").into_iter().next()));
    h.pick_open_file = Some(Box::new(|filter: &str, e: &[&str]| dialogs::pick(Mode::Open, "", &exts(e), filter).into_iter().next()));
    h.pick_open_project = Some(Box::new(|| dialogs::pick(Mode::Open, "", &["fcproj"], "Open Project").into_iter().next()));
    h.open_path = Some(Box::new(open_path));
    h.open_url = Some(Box::new(|url: &str| if host::open_url(url) { Ok(()) } else { Err(format!("couldn't open {url}")) }));
    h.clipboard_media = Some(Box::new(clipboard_media));
    // Settings ▸ General ▸ Interface Language ▸ System Language: the Locale preferences' languages.
    h.system_languages = Some(Box::new(|| host::locale().1));
    // Settings ▸ Appearance ▸ Sync with system: the Appearance preferences' panel colour.
    h.system_theme = Some(Box::new(|_ctx: &egui::Context| Some(if host::dark() { egui::Theme::Dark } else { egui::Theme::Light })));
}
