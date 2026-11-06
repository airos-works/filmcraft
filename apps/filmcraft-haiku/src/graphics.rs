//! The graphics modes of `--graphics` (the desktop app's `graphics.rs`, which `args.rs` names). On
//! air/OS there are two: the default (Vulkan when there is a hardware adapter, else software) and
//! software (`FILMCRAFT_RENDERER=cpu`). There is no OpenGL or DirectX to fall back to, and the
//! host falls back to software by itself when Vulkan fails (`shell.rs`), so no start-up state is
//! kept.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Vulkan when there is a hardware adapter, else software.
    Default,
    /// DirectX 12 only (Windows); never available here.
    Dx12,
    /// The software rasteriser and the CPU compositor.
    Software,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::Dx12 => "dx12",
            Mode::Software => "software",
        }
    }

    pub fn parse(name: &str) -> Option<Mode> {
        [Mode::Default, Mode::Dx12, Mode::Software].into_iter().find(|m| m.name() == name)
    }
}

/// The modes air/OS has.
pub const LADDER: &[Mode] = &[Mode::Default, Mode::Software];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_only_default_and_software_are_offered() {
        for m in [Mode::Default, Mode::Dx12, Mode::Software] {
            assert_eq!(Mode::parse(m.name()), Some(m));
        }
        assert!(!LADDER.contains(&Mode::Dx12));
        assert_eq!(Mode::parse("vulkan"), None);
    }
}
