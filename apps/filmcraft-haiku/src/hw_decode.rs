//! Hardware video decoding: the platform crate's Media Kit decoder add-on backend.

/// Puts the hardware decoders in front of FilmCraft's own (`filmcraft_platform::register`).
pub fn install() {
    let hardware = filmcraft_platform::register();
    log::info!("hardware decoding: {hardware:?}");
}
