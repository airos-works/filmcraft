//! Frame information supplied by the platform host. The app does not own a window: eframe's
//! `Frame` on the desktop, the air/OS host's own on Haiku (apps/filmcraft-haiku), which has no
//! eframe or winit.
#[cfg(not(target_os = "haiku"))]
pub use eframe::Frame;

/// What the air/OS host tells the app about its frame.
#[cfg(target_os = "haiku")]
#[derive(Default)]
pub struct Frame {
    /// The interface's language as a BCP 47 tag ([`Frame::set_ui_language`]).
    pub ui_language: Option<String>,
}

#[cfg(target_os = "haiku")]
impl Frame {
    /// eframe hands the language to the accessibility layer; the air/OS host keeps it.
    pub fn set_ui_language(&mut self, language: Option<&str>) {
        self.ui_language = language.map(str::to_string);
    }
}
