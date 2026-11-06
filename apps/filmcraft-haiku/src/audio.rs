//! Playback through the Media Kit: a `BSoundPlayer` ("FilmCraft" in Cortex and the Media
//! preferences) feeding the system mixer, which mixes it with every other application's sound and
//! resamples it to the output's rate (`native/audio.cpp`). Its buffer callback runs the app's
//! play-ahead mixer and counts the frames handed over: the playback master clock, as the desktop
//! app's cpal stream does.
//!
//! Settings ▸ Audio Hardware: the system mixer is the only output (the Media preferences choose
//! the sound card), so Device Class and Output list it alone; Sample Rate (or the sequence's, with
//! "Attempt to force hardware to document sample rate") and I/O Buffer Size are what the player
//! asks the mixer for.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, TryLockError};

use filmcraft_engine::settings::AudioHardwarePrefs;
use filmcraft_ui_egui::{AudioDevices, AudioOut};

/// The playback mixer callback handed to [`AudioOut::start`].
type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

unsafe extern "C" {
    fn fc_audio_open(
        rate: *mut f32,
        channels: *mut i32,
        buffer_frames: i32,
        fill: unsafe extern "C" fn(*mut c_void, *mut f32, usize, i32),
        cookie: *mut c_void,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn fc_audio_start(handle: *mut c_void) -> i32;
    fn fc_audio_close(handle: *mut c_void);
}

/// The Media Kit's name for the only output.
pub const MIXER: &str = "System mixer";
/// Settings ▸ Audio Hardware ▸ Device Class.
pub const HOST: &str = "Media Kit";

/// What the buffer callback shares with the app.
struct Shared {
    fill: Mutex<Fill>,
    played: AtomicU64,
    failed: AtomicBool,
}

/// The sound player's buffer callback, on the Media Kit's thread.
///
/// # Safety
/// `cookie` is the `Shared` of a running [`MediaKitOut`], which `fc_audio_close` outlives;
/// `samples` holds `count` floats.
unsafe extern "C" fn fill(cookie: *mut c_void, samples: *mut f32, count: usize, channels: i32) {
    if cookie.is_null() || samples.is_null() {
        return;
    }
    // SAFETY: as documented above; the reference doesn't escape the call.
    let shared = unsafe { &*cookie.cast::<Shared>() };
    // SAFETY: the player hands over a buffer of `count` floats for this call only.
    let buf = unsafe { std::slice::from_raw_parts_mut(samples, count) };
    let channels = usize::try_from(channels).unwrap_or(0).max(1);
    let mut guard = match shared.fill.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            buf.fill(0.0);
            return;
        }
    };
    if shared.failed.load(Ordering::SeqCst) || catch_unwind(AssertUnwindSafe(|| (*guard)(buf, channels))).is_err() {
        buf.fill(0.0);
        if !shared.failed.swap(true, Ordering::SeqCst) {
            log::error!("the audio mixer failed; playing without sound");
        }
        return;
    }
    // The mixer leaves what it doesn't fill as it was: never anything but samples.
    for s in buf.iter_mut() {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
    shared.played.fetch_add((count / channels) as u64, Ordering::SeqCst);
}

/// A running sound player.
struct Player {
    handle: std::ptr::NonNull<c_void>,
    /// Kept alive until `fc_audio_close` has stopped the callback.
    shared: Box<Shared>,
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: a handle from `fc_audio_open`; closing waits for the callback to return, so
        // `shared` is no longer used once it does.
        unsafe { fc_audio_close(self.handle.as_ptr()) };
    }
}

// SAFETY: the handle is only used from the thread that owns the `MediaKitOut` (open, start,
// close); the Media Kit's callback thread only reaches `shared`, which is `Sync`.
unsafe impl Send for Player {}

pub struct MediaKitOut {
    player: Option<Player>,
    rate: u32,
    channels: u16,
    hw: AudioHardwarePrefs,
    document_rate: Option<u32>,
    note: Option<String>,
}

impl MediaKitOut {
    pub fn new() -> Self {
        Self { player: None, rate: 48_000, channels: 2, hw: AudioHardwarePrefs::default(), document_rate: None, note: None }
    }

    /// The rate asked of the mixer: the sequence's when forced, else Settings ▸ Audio Hardware's.
    fn wanted_rate(&self) -> u32 {
        let rate = if self.hw.force_document_rate { self.document_rate.unwrap_or(self.hw.sample_rate) } else { self.hw.sample_rate };
        if (8_000..=384_000).contains(&rate) { rate } else { 48_000 }
    }
}

impl AudioOut for MediaKitOut {
    fn start(&mut self, mixer: Fill) -> Result<u32, String> {
        self.stop();
        let shared = Box::new(Shared { fill: Mutex::new(mixer), played: AtomicU64::new(0), failed: AtomicBool::new(false) });
        let mut rate = self.wanted_rate() as f32;
        let mut channels = 2i32;
        let frames = i32::try_from(self.hw.buffer_size.clamp(64, 16_384)).unwrap_or(512);
        let mut error = [0 as c_char; 256];
        let cookie = std::ptr::from_ref::<Shared>(&shared).cast_mut().cast::<c_void>();
        // SAFETY: valid out-pointers and error buffer; `cookie` stays valid until the player is
        // closed (it is owned by the `Player` that closes it).
        let handle = unsafe { fc_audio_open(&mut rate, &mut channels, frames, fill, cookie, error.as_mut_ptr(), error.len()) };
        let Some(handle) = std::ptr::NonNull::new(handle) else {
            // SAFETY: the host wrote a NUL-terminated message (or left the zeroed buffer).
            let why = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
            self.note = Some(format!("No audio output: {why}"));
            return Err(why);
        };
        let player = Player { handle, shared };
        // SAFETY: a handle from `fc_audio_open`.
        let status = unsafe { fc_audio_start(player.handle.as_ptr()) };
        if status != 0 {
            return Err(format!("the sound player couldn't start ({status:#x})"));
        }
        self.rate = if rate.is_finite() && rate >= 1.0 { rate.round() as u32 } else { 48_000 };
        self.channels = u16::try_from(channels).unwrap_or(2).max(1);
        self.note = None;
        log::info!("Media Kit playback: {} Hz, {} channels", self.rate, self.channels);
        self.player = Some(player);
        Ok(self.rate)
    }

    fn stop(&mut self) {
        self.player = None;
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        usize::from(self.channels)
    }

    fn played_frames(&self) -> Option<u64> {
        self.player.as_ref().filter(|p| !p.shared.failed.load(Ordering::SeqCst)).map(|p| p.shared.played.load(Ordering::SeqCst))
    }

    fn devices(&self) -> AudioDevices {
        AudioDevices { hosts: vec![HOST.into()], inputs: vec![crate::audio_in::INPUT.into()], outputs: vec![MIXER.into()], output_channels: self.channels }
    }

    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        self.hw = hw.clone();
        self.document_rate = document_rate;
        // The mixer resamples whatever the player asks for, so the rate playback mixes at is the
        // one asked.
        self.rate = self.wanted_rate();
    }

    fn note(&self) -> Option<String> {
        self.note.clone()
    }
}
