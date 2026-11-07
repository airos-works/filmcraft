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

Small GPUs (since `0.17.0~airos1-3`): the device asks for the adapter's own limits, with textures
capped at 16384 px, instead of egui-wgpu's default (WebGPU's limits: 8192 px textures, 8 colour
attachments), which the arm64 boards' GPUs refuse (V3DV on the Raspberry Pi 4: 4096 px textures, 4
attachments), sending the window, the monitors and exports to the processor. On the X399's NVK it
gets 16384 px textures (8192 before) and otherwise the adapter's limits. The adapter is the first
discrete GPU, else the first integrated one, that can draw the windows (BGRA render targets,
compute for the presenter, textures of 2048 px or more); every adapter passed over is logged with
the reason (`GPU adapter … not used: …`), and the limits the device got are logged
(`GPU device: textures … px, …`). What runs on the device checks `device.limits()`: the
compositor needs 4096 px textures (else the monitors composite on the CPU), the effect stage its
compute limits (else effects render on the CPU), and now, so that one large source never costs the
whole session its GPU:

- a monitor frame with a layer larger than the device's textures (a 6K or 8K source, or a large
  still, on a 4096 px GPU) is composited on the CPU and shown through the GPU, which goes on
  compositing the frames that fit (before, the first such frame turned the GPU compositor off for
  the session; it still is when the frame itself is larger than a texture);
- an export frame larger than the device's textures goes to the CPU renderer instead of raising a
  validation error on the device the windows share;
- an egui texture larger than the device takes is uploaded at a fraction of its size (logged).

`FILMCRAFT_GPU_LIMITS=small` creates the device with a small GPU's limits (wgpu's downlevel ones,
with 4096 px textures) to check this on a big GPU. On the X399 (7 November 2026: the H.264 demo
clip and a 5000 × 2800 still over V1 of a 1920 × 1080 sequence, shown in the Program monitor, and
a 48-frame Quick Export): with normal limits the same as the previous build (the monitors on the
GPU, the export in 58 s, the same file); with `small` the window stayed on Vulkan, the Program
monitor's frames with the still were composited on the CPU and shown through the GPU compositor,
which stayed on (`RUST_LOG=filmcraft_ui_egui=debug` logs each such frame), the export's frame with
the still went to the CPU renderer (no 56 MB upload), the same file came out in 57 s, and no GPU
error was raised.

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
- **MIME types**: FilmCraft becomes the preferred application of `application/x-filmcraft`
  (`.fcproj`) only; the package's post-install script registers that type. It lists the movie types
  it opens, so Tracker offers it in Open With, but leaves them to the system's player: a fresh
  system runs post-install scripts before anyone starts AirTime, which only takes types over from
  MediaPlayer or nobody, so claiming them made double-clicked movies open in the editor.
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

## ArtCraft marks

The ArtCraft name, wordmark and logos are the ArtCraft Team's trademarks, and
`docs/brand/LICENSE-brand.txt` asks a distributed fork to remove them and not to present itself as
an ArtCraft product. This fork has none: the image files are gone from `docs/brand/`
(`docs/brand/README.md` says why), the Import screen and About draw nothing where upstream shows
the wordmark (`crates/ui-egui/src/brand.rs`), About says in plain text that this is the air/OS port
of FilmCraft by the ArtCraft team, and the README credits upstream the same way. The community
links (Discord, website, GitHub) stay, as text.

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

## Checked on the X399 (7 November 2026)

The x86_64 build (the CI package's executable, 0.17.0~airos1-1, run from its own folder) on the
X399 (GTX 1070, NVK Mesa 25.3.0-devel, hrev60207+2472), driven over the control channel:

- It starts on Vulkan ("NVIDIA GeForce GTX 1070 (NVK GP104-A)", changed-tile readback) with the
  native menu bar, and the monitors composite on the GPU. The demo project plays with the Media
  Kit's clock (`audioClock: true`, the meters move); quitting with unsaved changes asks first
  (`hey FilmCraft quit`, then Don't Save), and the process exits with status 0.
- Export: 4 s of the demo to H.264 with GPU rendering Auto composited on the window's device
  ("export rendering on the host's GPU device"): 5.2 frames a second, against 2.4 with GPU
  rendering Off (the pure-Rust H.264 encoder is the rest of the time).
- Media Kit import: an MPEG-4 Part 2 AVI (which FilmCraft's AVI reader opens but can't decode) and
  an ASF movie (written with `tools/haiku/media-fixture.cpp`) open through the Media Kit and show
  in the Source monitor and as thumbnails; an MPEG-2 Matroska file stays with FilmCraft's own
  decoder. A movie with sound through the Media Kit was not tried (the Media Kit's audio encoders
  wouldn't write a fixture).
- Hardware decoding: the 1080p24 H.264 export, played for 8 s in the Source monitor at full
  resolution, decoded every picture through nvdec (`perf.stats`: 192 hardware frames, no
  fallback). Per picture 18.7 ms through nvdec (the add-on's engine about 5 ms, its copy out of
  the card's memory 10 to 17 ms, NVDEC_TIMING=1) against 9.0 ms for FilmCraft's own frame-threaded
  decoder; the process used 13.5 s of processor time over the 8 s against 15.9 s in software.
  Both keep up with the film; hardware decoding stays on for the processor time it saves (more
  for HEVC and 4K, where the software decoder does far more work).
- The 60 frames a second cap halved the process's processor time during playback (25.4 s to
  13.5 s over 8 s with hardware decoding).
- `FILMCRAFT_RENDERER=cpu`: the software rasteriser and the CPU compositor draw the same
  interface and play the clip.
- Nothing was logged in the syslog; no team crashed.

The arm64 package builds with the airos-ci toolchain (`aarch64-unknown-haiku`); it has not been
run on an arm64 machine. Voice-over recording was not tried (the X399 has no audio input), nor
the direct presentation mode.
