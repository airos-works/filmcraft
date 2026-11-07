//! Where upstream draws the ArtCraft wordmark (a first-party trademark, `docs/brand/`).
//!
//! This fork (airos-works/filmcraft, distributed as air/OS packages) is a modified version of
//! FilmCraft, which `docs/brand/LICENSE-brand.txt` asks to remove the ArtCraft marks: the image
//! files are gone from `docs/brand/` and nothing is drawn in their place, so the callers lay out
//! as they do when the image can't be loaded. Upstream's version draws the wordmark here.

/// Draws nothing: the ArtCraft wordmark isn't part of this fork. Callers keep their layout for a
/// missing wordmark (`None`).
pub fn paint_wordmark(_ui: &egui::Ui, _left_center: egui::Pos2, _height: f32, _dark: bool) -> Option<egui::Rect> {
    None
}
