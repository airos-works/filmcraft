# Haiku Vulkan support

`wgpu`, `wgpu-core` and `wgpu-core-deps-windows-linux-android` 30.0.1 are copied from crates.io
(the same copies the air/OS PhotoCraft, WordCraft and DeckCraft forks use), with their upstream
MIT/Apache-2.0 licenses. The only changes add `target_os = "haiku"` alongside FreeBSD in platform
dependency and backend cfg lists (`Cargo.toml`, `Cargo.toml.orig`, `build.rs`). This makes their
existing Vulkan backend available on air/OS. There is no new Vulkan implementation or window
surface; the native host (`apps/filmcraft-haiku`) renders off-screen and presents the finished
frame.
