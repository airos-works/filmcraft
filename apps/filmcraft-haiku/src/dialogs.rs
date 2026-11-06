//! File dialogs as Haiku file panels (`BFilePanel`, `pc_host_file_panel` in host.cpp).
//!
//! The app asks for its dialogs through `HostHooks::run_dialog` ([`run`]): the panel opens, the
//! frame loop keeps running, and the panel's answer (a `DIALOG_ANSWER` event, [`answer`]) goes back
//! on the channel the app watches, as on Linux, where the desktop app shows its dialogs on a thread
//! of their own. A few places still call the synchronous pickers ([`pick`]): there the interface
//! thread waits for the answer, and the events that arrive meanwhile are kept for the shell
//! ([`next_event`]).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use filmcraft_ui_egui::native_dialogs::FileDialog;

use crate::host::{self, RawEvent, kind};

/// The panel kinds of `pc_host_file_panel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Open = 0,
    Save = 1,
    Folder = 2,
    OpenMany = 3,
}

thread_local! {
    static NEXT: Cell<i32> = const { Cell::new(1) };
    static WAITING: RefCell<HashMap<i32, (Sender<Vec<String>>, bool)>> = RefCell::new(HashMap::new());
    static DEFERRED: RefCell<VecDeque<(RawEvent, String)>> = const { RefCell::new(VecDeque::new()) };
    /// Files a Save panel named, until they exist and get their MIME type ([`type_saved`]).
    static SAVED: RefCell<Vec<(String, std::time::Instant)>> = const { RefCell::new(Vec::new()) };
}

/// Gives the files Save panels named their MIME type (`BEOS:TYPE`) once the app has written
/// them: files written with `std::fs` have none, and Tracker then can't tell what opens them.
/// Called after every frame; a name nothing was written to is forgotten after a minute.
pub fn type_saved() {
    SAVED.with(|s| {
        s.borrow_mut().retain(|(path, at)| {
            let p = std::path::Path::new(path);
            if p.is_file() {
                host::type_file(p);
                return false;
            }
            at.elapsed() < Duration::from_secs(60)
        })
    });
}

fn note_saved(paths: &[String]) {
    SAVED.with(|s| s.borrow_mut().extend(paths.iter().map(|p| (p.clone(), std::time::Instant::now()))));
}

fn t(s: &str) -> String {
    filmcraft_ui_egui::i18n::t(s).to_string()
}

/// What a dialog shows: the panel kind, the suggested name or starting folder, the extensions it
/// offers (none: any file) and the title.
fn panel(dialog: &FileDialog) -> (Mode, String, Vec<String>, String) {
    match dialog {
        FileDialog::OpenFiles { exts } => (Mode::OpenMany, String::new(), exts.clone(), t("Import")),
        FileDialog::OpenFile { filter, exts } => (Mode::Open, String::new(), exts.clone(), t(filter)),
        FileDialog::Relink { exts, .. } => (Mode::Open, String::new(), exts.clone(), t("Link Media")),
        FileDialog::OpenProject => (Mode::Open, String::new(), vec!["fcproj".into()], t("Open Project")),
        FileDialog::SaveProject { name } => (Mode::Save, name.clone(), vec!["fcproj".into()], t("Save Project")),
        FileDialog::SaveAs { filter, exts, name, .. } => (Mode::Save, name.clone(), exts.clone(), t(filter)),
        FileDialog::Folder { at } => (Mode::Folder, at.clone().unwrap_or_default(), Vec::new(), t("Choose Folder")),
    }
}

fn show(mode: Mode, name: &str, exts: &[String], title: &str) -> i32 {
    let id = NEXT.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1).max(1));
        id
    });
    let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
    host::file_panel(id, mode as i32, name, &exts, &format!("FilmCraft: {title}"));
    id
}

/// `HostHooks::run_dialog`: shows the panel and answers on the returned channel.
pub fn run(dialog: FileDialog) -> Receiver<Vec<String>> {
    let (mode, name, exts, title) = panel(&dialog);
    let (tx, rx) = channel();
    let id = show(mode, &name, &exts, &title);
    WAITING.with(|w| w.borrow_mut().insert(id, (tx, mode == Mode::Save)));
    rx
}

/// A panel's answer: the chosen paths, one per line (`picked` false: cancelled).
pub fn answer(id: i32, picked: bool, text: &str) {
    let paths: Vec<String> = if picked { text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect() } else { Vec::new() };
    if let Some((tx, save)) = WAITING.with(|w| w.borrow_mut().remove(&id)) {
        if save {
            note_saved(&paths);
        }
        // The app may have stopped waiting (it then takes the dialog as cancelled).
        let _ = tx.send(paths);
    }
}

/// Shows a panel and waits for its answer (the synchronous pickers).
pub fn pick(mode: Mode, name: &str, exts: &[&str], title: &str) -> Vec<String> {
    let exts: Vec<String> = exts.iter().map(|e| e.to_string()).collect();
    let id = show(mode, name, &exts, &t(title));
    loop {
        host::wait(Some(Duration::from_millis(100)));
        while let Some((ev, text)) = host::next_event() {
            if ev.kind == kind::DIALOG_ANSWER && ev.id == id {
                let paths: Vec<String> = if ev.x != 0.0 { text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect() } else { Vec::new() };
                if mode == Mode::Save {
                    note_saved(&paths);
                }
                return paths;
            }
            DEFERRED.with(|d| d.borrow_mut().push_back((ev, text)));
        }
    }
}

/// The next native event: those that arrived while a synchronous picker waited first.
pub fn next_event() -> Option<(RawEvent, String)> {
    DEFERRED.with(|d| d.borrow_mut().pop_front()).or_else(host::next_event)
}
