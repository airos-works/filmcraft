//! Voice-over recording through the Media Kit ([`filmcraft_engine::voiceover::AudioInput`]): a
//! `BMediaRecorder` connected to the system's audio input (the one the Media preferences choose),
//! whose buffers arrive as interleaved floats (`native/audio.cpp`) and are kept planar for the
//! engine to drain, as the desktop app's cpal input does.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex};

use filmcraft_engine::voiceover::{AudioInput, InputFormat};

unsafe extern "C" {
    fn fc_record_open(
        sink: unsafe extern "C" fn(*mut c_void, *const f32, usize, i32, f32),
        cookie: *mut c_void,
        rate: *mut f32,
        channels: *mut i32,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn fc_record_close(handle: *mut c_void);
}

/// The one input, as Settings ▸ Audio Hardware and the voice-over Source list it.
pub const INPUT: &str = "System audio input";

/// Samples kept before the take is refused (about 46 minutes of 48 kHz stereo).
const MAX_SAMPLES: usize = 268_435_456;
/// At most this many trailing frames are measured per meter poll.
const PEAK_WINDOW: usize = 96_000;

#[derive(Default)]
struct Captured {
    planar: Vec<Vec<f32>>,
    error: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The recorder's buffer callback, on its own thread.
///
/// # Safety
/// `cookie` is the `Mutex<Captured>` of a running recording, which `fc_record_close` outlives;
/// `samples` holds `count` floats.
unsafe extern "C" fn sink(cookie: *mut c_void, samples: *const f32, count: usize, channels: i32, _rate: f32) {
    if cookie.is_null() || samples.is_null() {
        return;
    }
    // SAFETY: as documented above.
    let captured = unsafe { &*cookie.cast::<Mutex<Captured>>() };
    // SAFETY: `count` floats for this call only.
    let data = unsafe { std::slice::from_raw_parts(samples, count) };
    let channels = usize::try_from(channels).unwrap_or(0);
    let mut c = lock(captured);
    if c.error.is_some() || channels == 0 || channels > 256 {
        return;
    }
    if c.planar.len() != channels {
        c.planar = vec![Vec::new(); channels];
    }
    let frames = data.len() / channels;
    let total = c.planar.first().map_or(0, Vec::len).saturating_mul(channels).saturating_add(data.len());
    if total > MAX_SAMPLES || c.planar.iter_mut().any(|p| p.try_reserve(frames).is_err()) {
        c.error = Some("voice-over input buffer is full; recording failed".into());
        return;
    }
    for frame in data.chunks_exact(channels) {
        for (plane, s) in c.planar.iter_mut().zip(frame) {
            plane.push(if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 });
        }
    }
}

struct Recording {
    handle: std::ptr::NonNull<c_void>,
    /// Shared with the recorder's callback until it is closed.
    _cookie: Arc<Mutex<Captured>>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        // SAFETY: a handle from `fc_record_open`; closing stops the recorder's thread first.
        unsafe { fc_record_close(self.handle.as_ptr()) };
    }
}

// SAFETY: the handle is only opened and closed by the owner of the `MediaKitIn`.
unsafe impl Send for Recording {}

pub struct MediaKitIn {
    captured: Arc<Mutex<Captured>>,
    recording: Option<Recording>,
    channels: u16,
    /// Frames of the buffer already measured by `take_peaks`.
    metered: usize,
}

impl MediaKitIn {
    pub fn new() -> Self {
        Self { captured: Arc::default(), recording: None, channels: 2, metered: 0 }
    }
}

impl AudioInput for MediaKitIn {
    fn devices(&self) -> Vec<String> {
        vec![INPUT.into()]
    }

    fn channels(&self, _device: &str) -> u16 {
        self.channels
    }

    fn start(&mut self, _device: &str, _sample_rate: u32) -> Result<InputFormat, String> {
        self.stop();
        self.captured = Arc::default();
        self.metered = 0;
        let (mut rate, mut channels) = (0f32, 0i32);
        let mut error = [0 as c_char; 256];
        let cookie = Arc::as_ptr(&self.captured).cast_mut().cast::<c_void>();
        // SAFETY: valid out-pointers; `cookie` lives in `self.captured`, kept in the `Recording`
        // until the recorder is closed.
        let handle = unsafe { fc_record_open(sink, cookie, &mut rate, &mut channels, error.as_mut_ptr(), error.len()) };
        let Some(handle) = std::ptr::NonNull::new(handle) else {
            // SAFETY: a NUL-terminated message (or the zeroed buffer).
            return Err(unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned());
        };
        let recording = Recording { handle, _cookie: self.captured.clone() };
        if !(rate.is_finite() && rate >= 1.0) || !(1..=256).contains(&channels) {
            return Err("the audio input returned an invalid format".into());
        }
        self.channels = u16::try_from(channels).unwrap_or(2);
        self.recording = Some(recording);
        log::info!("Media Kit recording: {rate} Hz, {channels} channels");
        Ok(InputFormat { sample_rate: rate.round() as u32, channels: self.channels })
    }

    fn read(&mut self, frames: usize) -> Vec<Vec<f32>> {
        let mut c = lock(&self.captured);
        self.metered = self.metered.saturating_sub(c.planar.first().map_or(0, |p| frames.min(p.len())));
        c.planar
            .iter_mut()
            .map(|p| {
                let n = frames.min(p.len());
                p.drain(..n).collect()
            })
            .collect()
    }

    fn error(&self) -> Option<String> {
        lock(&self.captured).error.clone()
    }

    fn take_peaks(&mut self) -> Vec<f32> {
        let c = lock(&self.captured);
        let len = c.planar.first().map_or(0, Vec::len);
        let start = self.metered.max(len.saturating_sub(PEAK_WINDOW));
        self.metered = len;
        if start >= len {
            return Vec::new();
        }
        c.planar.iter().map(|p| p.get(start..).unwrap_or_default().iter().fold(0f32, |m, s| m.max(s.abs()))).collect()
    }

    fn discard(&mut self) {
        lock(&self.captured).planar.iter_mut().for_each(Vec::clear);
        self.metered = 0;
    }

    fn stop(&mut self) {
        self.recording = None;
    }
}
