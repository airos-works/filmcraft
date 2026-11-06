//! The native menu bar: a `BMenuBar` at the top of the main window instead of the in-window menus,
//! built from the app's menu tree (`filmcraft_ui_egui::menus::menu_items`, which the app hands to
//! `HostHooks::menu_changed` whenever it changes, as it does for the desktop app's macOS menu
//! bar). A click sends the item's command id to the app's command inbox, as a macOS menu does.
//!
//! - **Layout.** The app's menus in their order (File, Edit, Clip, Sequence, Markers, Graphics and
//!   Titles, View, Window, Help), submenus by path (Edit › Preferences holds the settings pages,
//!   which macOS shows in its app menu). A Haiku app has no app menu: About stays at the end of
//!   Help, and File ends with Quit (the `app.quit` command, which asks about unsaved changes).
//! - **Keys stay with the interface**: the window hands every Command key except Command+Q to the
//!   interface before `BWindow` looks for a menu shortcut (`DispatchMessage` in host.cpp), so the
//!   menus only show the shortcuts. Command shortcuts with one character are drawn by `BMenuItem`
//!   itself; the others (a function key, a bare letter) as text.
//! - A structure change rebuilds the whole bar (fast enough with a few hundred items); labels,
//!   enabled and checked are updated in place, only for the items that changed.
//! - `FILMCRAFT_NATIVE_MENU=0` keeps the in-window menus.

use std::fmt::Write as _;

use filmcraft_ui_egui::menus::{MENUS, MenuItem};

/// `B_SHIFT_KEY`, `B_COMMAND_KEY`, `B_CONTROL_KEY`, `B_OPTION_KEY` (`<InterfaceDefs.h>`).
const SHIFT: u32 = 0x1;
const COMMAND: u32 = 0x2;
const CONTROL: u32 = 0x4;
const OPTION: u32 = 0x40;

/// The command File › Quit runs.
pub const QUIT: &str = "app.quit";

/// Where the bar goes: the host in the app, a recorder in tests.
pub trait Sink {
    fn build(&mut self, spec: &str);
    fn update(&mut self, spec: &str);
}

/// The main window's menu bar.
#[cfg(target_os = "haiku")]
pub struct HostSink;

#[cfg(target_os = "haiku")]
impl Sink for HostSink {
    fn build(&mut self, spec: &str) {
        crate::host::menu_build(spec);
    }

    fn update(&mut self, spec: &str) {
        crate::host::menu_update(spec);
    }
}

/// The menu bar as last sent to the host.
pub struct NativeMenu<S: Sink> {
    sink: S,
    /// File › Quit's label and shortcut.
    quit: (String, String),
    /// What the items were built from (their ids, paths and shortcuts in order): another
    /// structure rebuilds the bar.
    structure: Vec<(String, Vec<String>, Option<String>)>,
    /// The items by their index in the spec.
    entries: Vec<MenuItem>,
}

impl<S: Sink> NativeMenu<S> {
    pub fn new(sink: S, quit_label: &str, quit_shortcut: &str) -> Self {
        Self { sink, quit: (quit_label.to_string(), quit_shortcut.to_string()), structure: Vec::new(), entries: Vec::new() }
    }

    /// The command of the item the host reported clicked.
    pub fn command(&self, index: i32) -> Option<&str> {
        usize::try_from(index).ok().and_then(|i| self.entries.get(i)).map(|e| e.id.as_str())
    }

    /// The app's menu tree changed.
    pub fn changed(&mut self, items: &[MenuItem]) {
        let items = with_quit(items, &self.quit);
        let structure: Vec<_> = items.iter().map(|i| (i.id.clone(), i.path.clone(), i.shortcut.clone())).collect();
        if structure != self.structure {
            let (spec, entries) = spec(&items);
            self.sink.build(&spec);
            self.structure = structure;
            self.entries = entries;
            return;
        }
        let mut out = String::new();
        let mut entries = std::mem::take(&mut self.entries);
        for (index, shown) in entries.iter_mut().enumerate() {
            let Some(it) = items.iter().find(|i| i.id == shown.id) else { continue };
            if it.label != shown.label || it.enabled != shown.enabled || it.checked != shown.checked {
                let _ = writeln!(out, "{index}\t{}\t{}\t{}", field(&it.label), i32::from(it.enabled), checked(it.checked));
                *shown = it.clone();
            }
        }
        self.entries = entries;
        if !out.is_empty() {
            self.sink.update(&out);
        }
    }
}

/// The app's items and File › Quit, which the desktop app keeps in the macOS app menu.
fn with_quit(items: &[MenuItem], quit: &(String, String)) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = items.to_vec();
    if !items.iter().any(|i| i.id == QUIT) {
        let at = items.iter().rposition(|i| i.path.first().is_some_and(|p| p == "File")).map_or(items.len(), |i| i + 1);
        let shortcut = Some(quit.1.clone()).filter(|s| !s.is_empty());
        items.insert(at, MenuItem { id: QUIT.into(), label: quit.0.clone(), path: vec!["File".into()], shortcut, enabled: true, checked: None });
    }
    items
}

/// Text for one field of a spec line.
fn field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn checked(c: Option<bool>) -> i32 {
    c.map_or(-1, i32::from)
}

/// A shortcut as the menu shows it: a character and modifiers `BMenuItem` draws (Command
/// shortcuts with one character), or text. FilmCraft writes them as `Cmd+Alt+K`, `Shift+F5`.
pub fn shortcut(s: &str) -> (u8, u32, String) {
    let parts: Vec<&str> = s.split('+').collect();
    // `Cmd++` (a plus key) splits into an empty part before the last
    let (key, mods) = match parts.as_slice() {
        [mods @ .., "", ""] => ("+", mods),
        [mods @ .., key] => (*key, mods),
        [] => return (0, 0, String::new()),
    };
    let has = |m: &str| mods.iter().any(|x| x.eq_ignore_ascii_case(m));
    let cmd = has("Cmd") || has("Command");
    if cmd {
        let ch = match key {
            k if k.len() == 1 && k.is_ascii() && !k.chars().all(char::is_whitespace) => k.as_bytes()[0].to_ascii_uppercase(),
            "Delete" => 0x7f,
            "Backspace" => 0x08,
            "Enter" | "Return" => 0x0a,
            "Tab" => 0x09,
            "Space" => 0x20,
            _ => 0,
        };
        if ch != 0 {
            let mut bits = COMMAND;
            for (on, bit) in [(has("Ctrl") || has("Control"), CONTROL), (has("Alt") || has("Option"), OPTION), (has("Shift"), SHIFT)] {
                if on {
                    bits |= bit;
                }
            }
            return (ch, bits, String::new());
        }
    }
    (0, 0, s.to_string())
}

/// One node of a menu: a submenu with its children, or an item by its index in `items`.
enum Node {
    Sub(String, Vec<Node>),
    Item(usize),
}

/// Puts item `index` with `path` (below the top menu) into `nodes`, making the submenus it needs.
fn insert(nodes: &mut Vec<Node>, path: &[String], index: usize) {
    let Some((first, rest)) = path.split_first() else {
        nodes.push(Node::Item(index));
        return;
    };
    let at = nodes.iter().position(|n| matches!(n, Node::Sub(label, _) if label == first)).unwrap_or_else(|| {
        nodes.push(Node::Sub(first.clone(), Vec::new()));
        nodes.len() - 1
    });
    if let Some(Node::Sub(_, children)) = nodes.get_mut(at) {
        insert(children, rest, index);
    }
}

/// The host's spec of the bar (`BuildMenuBar` in host.cpp), and its items by index.
pub fn spec(items: &[MenuItem]) -> (String, Vec<MenuItem>) {
    fn walk(out: &mut String, entries: &mut Vec<MenuItem>, items: &[MenuItem], nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Sub(label, children) => {
                    let _ = writeln!(out, "S\t{}", field(filmcraft_ui_egui::i18n::t(label)));
                    walk(out, entries, items, children);
                    out.push_str("E\n");
                }
                Node::Item(i) => {
                    let Some(it) = items.get(*i) else { continue };
                    if it.id == QUIT {
                        out.push_str("-\n");
                    }
                    let (ch, mods, text) = it.shortcut.as_deref().map(shortcut).unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "I\t{}\t{}\t{ch}\t{mods}\t{}\t{}\t{}",
                        entries.len(),
                        field(&it.label),
                        field(&text),
                        i32::from(it.enabled),
                        checked(it.checked)
                    );
                    entries.push(it.clone());
                }
            }
        }
    }
    let mut out = String::new();
    let mut entries = Vec::new();
    for top in MENUS {
        let mut nodes = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if it.path.first().map(String::as_str) == Some(top) {
                insert(&mut nodes, &it.path[1..], i);
            }
        }
        if nodes.is_empty() {
            continue;
        }
        let _ = writeln!(out, "M\t{}", field(filmcraft_ui_egui::i18n::t(top)));
        walk(&mut out, &mut entries, items, &nodes);
        out.push_str("E\n");
    }
    (out, entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec {
        built: Vec<String>,
        updated: Vec<String>,
    }

    impl Sink for &mut Rec {
        fn build(&mut self, spec: &str) {
            self.built.push(spec.into());
        }
        fn update(&mut self, spec: &str) {
            self.updated.push(spec.into());
        }
    }

    fn item(id: &str, label: &str, path: &[&str], shortcut: Option<&str>) -> MenuItem {
        MenuItem { id: id.into(), label: label.into(), path: path.iter().map(|p| p.to_string()).collect(), shortcut: shortcut.map(Into::into), enabled: true, checked: None }
    }

    #[test]
    fn shortcuts_with_command_and_one_key_are_native() {
        assert_eq!(shortcut("Cmd+M"), (b'M', COMMAND, String::new()));
        assert_eq!(shortcut("Cmd+Alt+K"), (b'K', COMMAND | OPTION, String::new()));
        assert_eq!(shortcut("Shift+Cmd+,"), (b',', COMMAND | SHIFT, String::new()));
        assert_eq!(shortcut("Cmd++"), (b'+', COMMAND, String::new()));
        assert_eq!(shortcut("F5").0, 0);
        assert_eq!(shortcut("F5").2, "F5");
        assert_eq!(shortcut("Cmd+F5").0, 0);
    }

    #[test]
    fn submenus_follow_paths_and_file_ends_with_quit() {
        let items = vec![
            item("file.new", "New", &["File"], Some("Cmd+N")),
            item("app.settings.general", "General…", &["Edit", "Preferences"], Some("Cmd+,")),
            item("edit.undo", "Undo", &["Edit"], Some("Cmd+Z")),
            item("app.about", "About FilmCraft", &["Help"], None),
        ];
        let items = with_quit(&items, &("Quit FilmCraft".into(), "Cmd+Q".into()));
        let (spec, entries) = spec(&items);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["file.new", QUIT, "app.settings.general", "edit.undo", "app.about"]);
        assert!(spec.starts_with("M\tFile\nI\t0\tNew\t78\t2\t\t1\t-1\n-\nI\t1\tQuit FilmCraft\t81\t2"), "{spec}");
        assert!(spec.contains("M\tEdit\nS\tPreferences\nI\t2\tGeneral…"), "{spec}");
    }

    #[test]
    fn state_changes_update_in_place_and_structure_changes_rebuild() {
        let mut rec = Rec::default();
        let mut menu = NativeMenu::new(&mut rec, "Quit FilmCraft", "Cmd+Q");
        let mut items = vec![item("edit.undo", "Undo", &["Edit"], Some("Cmd+Z")), item("edit.redo", "Redo", &["Edit"], None)];
        menu.changed(&items);
        items[1].enabled = false;
        menu.changed(&items);
        assert_eq!(menu.command(0), Some("edit.undo"));
        items.push(item("edit.cut", "Cut", &["Edit"], None));
        menu.changed(&items);
        drop(menu);
        assert_eq!(rec.built.len(), 2);
        assert_eq!(rec.updated, ["1\tRedo\t0\t-1\n"]);
    }
}
