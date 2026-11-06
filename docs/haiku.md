# FilmCraft on air/OS

This independent fork of FilmCraft 0.17.0 (upstream `da535c18`) runs the full Rust video editor on
air/OS (Haiku) x86_64 and arm64: the engine, timeline, monitors, effects, Lumetri, scopes, audio
mixer, captions, text to speech, voice-over, recovery, export and automation are upstream's. The
native host in `apps/filmcraft-haiku` (package `filmcraft-haiku`, executable `FilmCraft`) drives
upstream's `FilmcraftApp` itself, the way the air/OS PhotoCraft host drives PhotoCraft's: a
`BApplication` with a `BWindow` per egui viewport (`native/host.cpp`, `src/viewports.rs`,
`src/platform.rs`), instead of eframe and winit, which have no Haiku backend.

## Rendering and the GPU

The window draws with wgpu/Vulkan (the vendored wgpu 30.0.1 with Haiku's Vulkan backend enabled,
`vendor/README.md`) into an off-screen frame texture that `src/present.rs` gets onto the screen,
as PhotoCraft's host does (shared design: see PhotoCraft's `docs/haiku.md`).
`FILMCRAFT_PRESENT=readback|full|direct` picks how: `readback` (the default) compares each frame
with the last 64 px tile by tile on the GPU and copies back only the changed tiles; `direct`
(experimental, opt-in) puts frames straight into the frame buffer.

That one Vulkan device does all of FilmCraft's GPU work:

- **The monitors.** Program, Source and Reference compositing (`filmcraft-gpu`'s
  `GpuCompositor`: YUV sampling, transforms, every blend mode, transitions, masks, LUTs and the
  standard effects as WGSL compute passes) runs on the window's device (`FilmcraftApp::set_wgpu`),
  and its output texture is drawn by egui directly. A frame showing it is always drawn (the hash
  that skips unchanged frames can't see a texture that changes under the same id).
- **Exports.** `filmcraft_gpu::export_renderer::share_device` (a small addition to the gpu crate)
  hands the window's device to the export renderer, which the desktop app gives a device of its
  own: with Export ▸ GPU rendering Auto, export frames composite on the GPU and fall back to the
  CPU renderer per frame where the GPU can't.
- **What stays on the processor:** decoding (FilmCraft's pure-Rust codecs, or a hardware decoder
  add-on, below), the scopes' maths (`filmcraft-scopes` counts a decimated frame on rayon), audio,
  and encoding. NVK on the X399 spills indexed WGSL accumulator arrays and runs such shaders about
  20 times slower; FilmCraft's shaders accumulate in single `vec4`s and index only two small
  constant vertex tables (`crates/gpu/src/*.wgsl` were checked), so none is known to be slow.

The main window draws at most 60 frames a second (`FILMCRAFT_MAX_FPS`, 0 for no limit): Haiku has
no vertical sync for a window, and during playback the app asks for its next frame as soon as one
is out, which drew the interface about 300 times a second.

The presenter never submits an empty batch when it shows the last frame again (RADV before
mesa-amdgpu `1a02b91f` lost the device on such a submission, which waits on wgpu's semaphore), and
direct mode accepts a frame within a pixel of the window's size (fractional scales round them
differently); readback stays the default.

`FILMCRAFT_RENDERER=auto|cpu|vulkan` chooses the window's renderer (`--graphics software` is
`cpu`). Auto falls back to the software rasteriser (`src/raster.rs`) when there is no hardware
Vulkan adapter (a Raspberry Pi 4 or another board without a usable driver still runs), and a GPU
error or a lost device at run time switches the monitors, exports and every window to software at
the next frame, with the project untouched. `FILMCRAFT_CPU_COMPOSITE=1` keeps the window on Vulkan
but composites the monitors on the CPU, as on the desktop.

## Sound: the Media Kit

- **Playback** is a `BSoundPlayer` node ("FilmCraft" in Cortex and the Media preferences) feeding
  the system mixer (`native/audio.cpp`, `src/audio.rs`). Its buffer callback runs FilmCraft's
  play-ahead mixer and counts the frames it handed over: the playback master clock, as the desktop
  app's cpal stream is. Settings ▸ Audio Hardware lists the Media Kit and the system mixer; the
  Sample Rate (or the sequence's, with "Attempt to force hardware to document sample rate") and
  I/O Buffer Size are what the player asks for, and the mixer resamples to the sound card.
- **Voice-over recording** is a `BMediaRecorder` connected to the system's audio input (the Media
  preferences choose it), whose buffers are converted to floats (`src/audio_in.rs`).

## Codecs: the Media Kit, and hardware decoding

- **Files FilmCraft's own decoders don't read** go through the Media Kit
  (`src/media_kit.rs`, `native/media_source.cpp`): Windows Media/ASF, Flash Video, DV, RealMedia,
  Ogg Theora, MPEG-4 Part 2 (DivX, Xvid) in AVI or Matroska, Windows Media Audio, Monkey's Audio
  and whatever else the system's ffmpeg add-on reads. A reader opener in front of FilmCraft's own
  runs them first; a file they don't open, or whose first picture they can't decode, is opened as
  a `BMediaFile` through FilmCraft's own byte reader instead, its frames decoded to RGBA and its
  sound to floats. They play, scrub, cut and export like any clip. On air/OS the import filters list
  those extensions too (`filmcraft_media::VIDEO_EXTENSIONS`). `FILMCRAFT_NO_MEDIA_KIT=1` turns it
  off.
- **Hardware video decoding.** air/OS's video engines are Media Kit decoder add-ons (nvdec on
  NVIDIA, amduvd on AMD, the RK3588's, Allwinner's, Qualcomm's and the Raspberry Pi's). The
  Media Kit gives a format to the first add-on that claims it and never falls back, so
  `crates/platform` (FilmCraft's OS media crate, beside VideoToolbox, Media Foundation, VA-API and
  NVDEC) loads the add-on by name and feeds it H.264 and HEVC access units itself
  (`crates/platform/src/media_kit.rs`, `native/media_kit.cpp`): the decoder runs on a thread of its
  own, whose chunk requests wait for FilmCraft's next access unit, and pictures come back as NV12 or
  P010 and become FilmCraft's planar frames. It is registered in front of FilmCraft's own decoders
  (`filmcraft_platform::register`) inside upstream's `HybridDecoder`, so a stream the add-on
  refuses, or a failure mid-stream, continues in software. Only nvdec is used by default (it is the
  one measured with FilmCraft); `FILMCRAFT_HW_DECODERS=all` also tries the others.
  `FILMCRAFT_NO_HW_DECODE=1`, or Settings ▸ Playback ▸ Hardware decoding Off, keeps every stream in
  software.

## Native integration

- **Windows and input**: one `BWindow` per egui viewport (floating panels in their own windows),
  keyboard, mouse, touchpad scrolling, drops from Tracker, cursors, density changes; the shared
  host of the air/OS Craft ports.
- **Menu bar** (`src/native_menu.rs`): upstream's menu tree, which it hands to native menu bars,
  as a `BMenuBar`; File ends with Quit, Edit › Preferences holds the settings pages, Help ends
  with About. Keys stay with the editor; the menus show them. `FILMCRAFT_NATIVE_MENU=0` keeps the
  in-window menus.
- **File panels** (`src/dialogs.rs`): Open, Import, Save and folder panels are `BFilePanel`s that
  don't stop the frame loop (upstream's `run_dialog` hook). Files a Save panel named get their MIME
  type once written.
- **One running copy** (`B_SINGLE_LAUNCH`): projects and media opened from Tracker, or given to a
  later launch, arrive in the running copy (a project opens, media is imported).
- **MIME types**: FilmCraft claims `application/x-filmcraft` (`.fcproj`) and the movie types no
  other application claimed; the package's post-install script registers the project type.
- **Data**: settings, auto-save, recovery and logs live in `~/config/settings/FilmCraft`
  (`FILMCRAFT_DATA_DIR` or `--data-dir` move them). Help › Reveal Log Files opens the folder.
- **System**: links open in the preferred browser; Settings ▸ General ▸ System Language reads
  the Locale preferences; Appearance › Sync with system follows the panel colour; a PNG on the
  clipboard pastes as media.
- **Updates**: FilmCraft is a package, updated by SoftwareUpdater; the start-up check is off and
  Help › Check for Updates is left out (automation's `app.checkForUpdates` says so).
- **Threads**: rayon's pool is capped at four threads (air/OS gets slower past about four for
  heavy parallel work); `FILMCRAFT_THREADS` overrides it.
- **Speech**: Whisper transcription and the neural voices are off in upstream's releases and here
  (they need the `whisper` / `neural-voices` features); Transcribe says so, and the built-in
  voices of Text to Speech work.

## Build and package

```sh
CRAFT_FONTS_DIR=/path/to/craft-fonts JOBS=16 bash tools/haiku/build.sh
bash tools/haiku/package.sh --no-build
```

The scripts cross-build the tier-3 Rust target `x86_64-unknown-haiku` (std from rust-src) and the
C++ Be API boundary with the Haiku cross compiler; `tools/haiku/env.sh` has the defaults. Like
upstream's releases the build embeds the craft-fonts collection (`CRAFT_FONTS_DIR`, upstream's pin
`b076417b5880`; `CRAFT_FONTS_DIR=` builds without). The air/OS CI (airos-works/airos-ci,
`build_filmcraft`) builds the x86_64 and arm64 packages; `.github/workflows/airos.yml` runs it on
every push to `main`.

`tools/haiku/control.py` drives the control server (`--control PORT` with
`FILMCRAFT_CONTROL_TOKEN`, upstream's protocol: `docs/control-protocol.md`).
`tools/haiku/media-fixture.cpp` writes test movies with the Media Kit's encoders (MPEG-4 Part 2
in AVI and ASF, Theora, MJPEG…), for trying the Media Kit import without ffmpeg's command line.

## Native application icon

An original clapperboard with a play mark, in the Haiku icon style (perspective slab, light from
the top left, floor shadow), MIT OR Apache-2.0: `python3 tools/haiku/make-icon.py` writes
`apps/filmcraft-haiku/resources/FilmCraft.hvif` and an SVG preview.
