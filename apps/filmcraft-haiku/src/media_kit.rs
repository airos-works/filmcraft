//! The Media Kit as FilmCraft's codecs for the files its own decoders don't read
//! (`native/media_source.cpp`): Windows Media, Flash Video, DV, RealMedia, MPEG-4 Part 2 (DivX,
//! Xvid), Theora, Windows Media Audio and whatever else the system's codecs (the ffmpeg add-on)
//! read. FilmCraft's own readers and decoders go first, for every file: a file they don't open, or
//! whose video or audio they can't decode, is opened through a `BMediaFile` instead. Its frames
//! come as RGBA and its sound as floats, through the same `MediaSource` interface as every other
//! source, so they play, scrub, cut and export like any clip.
//!
//! `FILMCRAFT_NO_MEDIA_KIT=1` leaves these files unsupported.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_color::ColorInfo;
use filmcraft_frame::{AudioBuffer, PixelData, VideoFrame};
use filmcraft_media::{AudioStreamInfo, FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource, ReaderOpener, SharedReader, SharedSource, VideoStreamInfo};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};

#[repr(C)]
struct RawInfo {
    has_video: i32,
    has_audio: i32,
    video_decodable: i32,
    audio_decodable: i32,
    width: i32,
    height: i32,
    fps: f32,
    channels: i32,
    rate: f32,
    par_x: i32,
    par_y: i32,
    duration_us: i64,
    video_frames: i64,
    audio_frames: i64,
    container: [c_char; 64],
    video_codec: [c_char; 96],
    audio_codec: [c_char; 96],
}

type ReadFn = unsafe extern "C" fn(*mut c_void, i64, *mut c_void, usize) -> i64;

unsafe extern "C" {
    fn fc_media_open(read: ReadFn, cookie: *mut c_void, size: i64, info: *mut RawInfo) -> *mut c_void;
    fn fc_media_video(handle: *mut c_void, time: i64, out: *mut u8, out_len: usize, shown: *mut i64) -> i32;
    fn fc_media_audio(handle: *mut c_void, start: i64, out: *mut f32, count: i64) -> i64;
    fn fc_media_row_bytes(handle: *mut c_void) -> usize;
    fn fc_media_close(handle: *mut c_void);
}

fn text(buf: &[c_char]) -> String {
    if !buf.contains(&0) {
        return String::new();
    }
    // SAFETY: NUL-terminated (checked above), written by the host into a zeroed buffer.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// The reader callback: FilmCraft's reader for the Media Kit's `BPositionIO`.
///
/// # Safety
/// `cookie` is the `SharedReader` of an open source (it outlives the `BMediaFile`); `buffer` holds
/// `length` writable bytes.
unsafe extern "C" fn read(cookie: *mut c_void, offset: i64, buffer: *mut c_void, length: usize) -> i64 {
    if cookie.is_null() || buffer.is_null() || offset < 0 {
        return -1;
    }
    // SAFETY: as documented above.
    let reader = unsafe { &*cookie.cast::<SharedReader>() };
    // SAFETY: as documented above.
    let buf = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length) };
    let offset = offset as u64;
    let len = reader.len();
    if offset >= len {
        return 0;
    }
    let n = usize::try_from(len - offset).unwrap_or(usize::MAX).min(length);
    match reader.read_at(offset, &mut buf[..n]) {
        Ok(()) => i64::try_from(n).unwrap_or(-1),
        Err(_) => -1,
    }
}

/// An open `BMediaFile` and what it reads through.
struct Handle {
    raw: std::ptr::NonNull<c_void>,
    /// The reader the host reads through, boxed so its address stays put.
    _reader: Box<SharedReader>,
    /// The last picture's pixels, reused.
    pixels: Vec<u8>,
}

// SAFETY: the `BMediaFile` and its tracks are used by one thread at a time (the source's mutex);
// the reader is `Send + Sync`.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle from `fc_media_open`, closed once, before the reader goes.
        unsafe { fc_media_close(self.raw.as_ptr()) }
    }
}

pub struct MediaKitSource {
    info: MediaInfo,
    handle: Mutex<Handle>,
    width: u32,
    height: u32,
    channels: usize,
    rate: f64,
    par: (u32, u32),
}

/// `t` in microseconds.
fn micros(t: Tick) -> i64 {
    i64::try_from(i128::from(t.0) * 1_000_000 / i128::from(TICKS_PER_SECOND)).unwrap_or(i64::MAX)
}

impl MediaKitSource {
    pub fn open(name: &str, reader: SharedReader) -> Result<Self, MediaError> {
        let size = i64::try_from(reader.len()).map_err(|_| MediaError::Unsupported(format!("{name}: too large")))?;
        let reader = Box::new(reader);
        let cookie = std::ptr::from_ref::<SharedReader>(&reader).cast_mut().cast::<c_void>();
        let mut raw = RawInfo {
            has_video: 0,
            has_audio: 0,
            video_decodable: 0,
            audio_decodable: 0,
            width: 0,
            height: 0,
            fps: 0.0,
            channels: 0,
            rate: 0.0,
            par_x: 0,
            par_y: 0,
            duration_us: 0,
            video_frames: 0,
            audio_frames: 0,
            container: [0; 64],
            video_codec: [0; 96],
            audio_codec: [0; 96],
        };
        // SAFETY: `read` matches the callback type; `cookie` lives in the `Handle` (boxed) for as
        // long as the media file; `raw` is a writable `FcMediaInfo`.
        let handle = unsafe { fc_media_open(read, cookie, size, &mut raw) };
        let raw_handle = std::ptr::NonNull::new(handle).ok_or_else(|| MediaError::Unsupported(format!("{name}: the Media Kit doesn't read this file")))?;
        let handle = Handle { raw: raw_handle, _reader: reader, pixels: Vec::new() };
        let video = raw.video_decodable != 0;
        let audio = raw.audio_decodable != 0;
        if !video && !audio {
            return Err(MediaError::Unsupported(format!(
                "{name}: the Media Kit has no decoder for its {}",
                [text(&raw.video_codec), text(&raw.audio_codec)].into_iter().filter(|c| !c.is_empty()).collect::<Vec<_>>().join(" or ")
            )));
        }
        let fps = f64::from(raw.fps);
        let frame_rate = if fps.is_finite() && fps > 0.0 && fps < 1000.0 { FrameRate::from_f64(fps) } else { FrameRate::FPS_25 };
        let par = match (u32::try_from(raw.par_x), u32::try_from(raw.par_y)) {
            (Ok(x @ 1..), Ok(y @ 1..)) => (x, y),
            _ => (1, 1),
        };
        let (width, height) = (u32::try_from(raw.width).unwrap_or(0), u32::try_from(raw.height).unwrap_or(0));
        let duration = Tick(i64::try_from(i128::from(raw.duration_us.max(0)) * i128::from(TICKS_PER_SECOND) / 1_000_000).unwrap_or(i64::MAX));
        let rate = f64::from(raw.rate);
        let channels = usize::try_from(raw.channels).unwrap_or(0);
        let container = text(&raw.container);
        let info = MediaInfo {
            name: name.to_string(),
            kind: if video { MediaKind::Movie } else { MediaKind::AudioOnly },
            duration,
            video: video.then(|| VideoStreamInfo {
                width,
                height,
                frame_rate,
                par,
                codec: format!("{} (Media Kit)", text(&raw.video_codec)),
                pixel_format: "rgba8".into(),
                color: ColorInfo::SRGB_FULL,
                has_alpha: false,
                bitrate: None,
                hdr: None,
                color_assumed: None,
            }),
            audio_streams: if audio {
                vec![AudioStreamInfo { sample_rate: rate.round() as u32, channels: channels as u32, codec: format!("{} (Media Kit)", text(&raw.audio_codec)), bits_per_sample: None }]
            } else {
                Vec::new()
            },
            container: if container.is_empty() { "media".into() } else { container },
            start_timecode: None,
            file_size: u64::try_from(size).ok(),
        };
        log::info!("{name}: opened through the Media Kit ({:?}, {:?})", info.video.as_ref().map(|v| &v.codec), info.audio_streams.first().map(|a| &a.codec));
        Ok(Self { info, handle: Mutex::new(handle), width, height, channels, rate, par })
    }

    /// Native frames `[first, first + count)` interleaved; fewer at the end.
    fn native_audio(&self, first: i64, count: usize) -> Vec<f32> {
        let h = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = vec![0f32; count.saturating_mul(self.channels)];
        // SAFETY: a live handle; `out` holds `count` frames of `channels` floats.
        let got = unsafe { fc_media_audio(h.raw.as_ptr(), first, out.as_mut_ptr(), i64::try_from(count).unwrap_or(0)) };
        out.truncate(usize::try_from(got).unwrap_or(0).saturating_mul(self.channels));
        out
    }
}

/// Most frames one `audio` call reads (a minute at 48 kHz).
const MAX_AUDIO_FRAMES: usize = 48_000 * 60;

impl MediaSource for MediaKitSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        if self.info.video.is_none() {
            return Err(MediaError::NoStream("video"));
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let mut guard = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        let handle = &mut *guard;
        // SAFETY: a live handle.
        let row_bytes = unsafe { fc_media_row_bytes(handle.raw.as_ptr()) };
        let size = row_bytes.checked_mul(h).ok_or_else(|| MediaError::Decode("picture too large".into()))?;
        handle.pixels.resize(size, 0);
        let mut shown = 0i64;
        // SAFETY: a live handle and a writable buffer of `size` bytes.
        let status = unsafe { fc_media_video(handle.raw.as_ptr(), micros(req.time), handle.pixels.as_mut_ptr(), size, &mut shown) };
        if status != 0 {
            return Err(MediaError::Decode(format!("the Media Kit couldn't decode the picture ({status:#x})")));
        }
        // B_RGB32 is B, G, R, A in memory; the alpha byte isn't meaningful.
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in handle.pixels.chunks_exact(row_bytes.max(1)).take(h) {
            for px in row.get(..w * 4).unwrap_or_default().as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        if rgba.len() != w * h * 4 {
            return Err(MediaError::Decode("short picture from the Media Kit".into()));
        }
        let pts = Tick(i64::try_from(i128::from(shown) * i128::from(TICKS_PER_SECOND) / 1_000_000).unwrap_or(0));
        Ok(Arc::new(VideoFrame { width: self.width, height: self.height, data: PixelData::Rgba8(Arc::new(rgba)), color: ColorInfo::SRGB_FULL, par: self.par, pts }))
    }

    fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        if self.info.audio_streams.is_empty() {
            return Err(MediaError::NoStream("audio"));
        }
        filmcraft_media::validate_audio_buffer_shape(self.channels, frames)?;
        let mut out = AudioBuffer::silence(sample_rate, self.channels, frames);
        if frames == 0 || sample_rate == 0 || !(self.rate > 0.0) {
            return Ok(out);
        }
        // Linear interpolation from the file's rate to the one asked for, as the WAV source does.
        let ratio = self.rate / f64::from(sample_rate);
        let src = |i: usize| (start as f64 + i as f64) * ratio;
        let (lo, hi) = (src(0), src(frames - 1));
        if hi < 0.0 {
            return Ok(out);
        }
        let first = lo.max(0.0).floor() as i64;
        let count = ((hi.floor() as i64) - first + 2).clamp(0, MAX_AUDIO_FRAMES as i64) as usize;
        let native = self.native_audio(first, count);
        let have = native.len() / self.channels.max(1);
        for i in 0..frames {
            let s = src(i);
            if s < 0.0 {
                continue;
            }
            let k = s.floor() as i64 - first;
            let Ok(k) = usize::try_from(k) else { continue };
            if k >= have {
                break;
            }
            let f = (s - s.floor()) as f32;
            for (c, ch) in out.channels.iter_mut().enumerate() {
                let a = native.get(k * self.channels + c).copied().unwrap_or(0.0);
                let b = if k + 1 < have { native.get((k + 1) * self.channels + c).copied().unwrap_or(a) } else { a };
                ch[i] = a + (b - a) * f;
            }
        }
        Ok(out)
    }
}

/// Movie and sound files FilmCraft's own readers open but may not decode (an MPEG-4 Part 2 AVI,
/// a Matroska file with Theora): the Media Kit gets what they can't. Stills and FilmCraft's own
/// project, caption and interchange files keep their own readers.
const SHARED: &[&str] = &["mp4", "m4v", "mov", "qt", "mkv", "mka", "webm", "avi", "3gp", "mpg", "mpeg", "vob", "ts", "mts", "m2ts", "m2t", "mxf", "ogg"];

/// Files only the Media Kit reads (`filmcraft_media::VIDEO_EXTENSIONS` and `AUDIO_EXTENSIONS`
/// name them on air/OS, so they can be imported).
const MEDIA_KIT_ONLY: &[&str] = &["wmv", "asf", "flv", "f4v", "dv", "dif", "rm", "rmvb", "divx", "xvid", "ogv", "ogm", "3g2", "nut", "wma", "ra", "ape", "au", "snd", "wv"];

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Whether a source FilmCraft's own readers opened can be decoded: its first picture and a
/// moment of its sound come out (the containers open whatever codec they hold; an unknown one
/// fails only when it is decoded).
fn decodable(src: &SharedSource) -> bool {
    let info = src.info();
    if info.video.is_some() && matches!(src.video_frame(FrameRequest { time: Tick(0), scale: 0.25 }), Err(MediaError::Unsupported(_))) {
        return false;
    }
    if let Some(a) = info.audio_streams.first()
        && info.video.is_none()
        && matches!(src.audio(0, 64, a.sample_rate.max(1)), Err(MediaError::Unsupported(_)))
    {
        return false;
    }
    true
}

/// The reader opener tried before FilmCraft's own (`filmcraft_codecs::register_reader_opener`):
/// it runs them, and turns to the Media Kit for what they don't take or can't decode.
fn reader_opener(name: &str, head: &[u8], reader: &SharedReader) -> Option<filmcraft_media::Result<SharedSource>> {
    let ext = extension(name);
    let only = MEDIA_KIT_ONLY.contains(&ext.as_str());
    if !only && !SHARED.contains(&ext.as_str()) {
        return None;
    }
    let me: ReaderOpener = reader_opener;
    let mut own: Option<filmcraft_media::Result<SharedSource>> = None;
    for o in filmcraft_codecs::reader_openers() {
        if std::ptr::fn_addr_eq(o, me) {
            continue;
        }
        if let Some(r) = o(name, head, reader) {
            own = Some(r);
            break;
        }
    }
    match &own {
        Some(Ok(src)) if decodable(src) => return own,
        // Not taken by a streaming reader: the byte openers after this may still read it.
        None if !only => return None,
        _ => {}
    }
    match MediaKitSource::open(name, reader.clone()) {
        Ok(src) => Some(Ok(Arc::new(src))),
        Err(e) => {
            log::debug!("{name}: {e}");
            own
        }
    }
}

/// Offer the Media Kit's codecs to the app, unless `FILMCRAFT_NO_MEDIA_KIT` is set.
pub fn install() {
    if std::env::var_os("FILMCRAFT_NO_MEDIA_KIT").is_some_and(|v| !v.is_empty() && v != "0") {
        log::info!("Media Kit codecs off (FILMCRAFT_NO_MEDIA_KIT)");
        return;
    }
    filmcraft_codecs::register_reader_opener(reader_opener);
}
