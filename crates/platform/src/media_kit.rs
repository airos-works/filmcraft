//! Hardware decoding through air/OS's Media Kit decoder add-ons (Haiku): the graphics card's or
//! board's video engine (NVDEC on NVIDIA, and, with `FILMCRAFT_HW_DECODERS=all`, AMD UVD, the
//! RK3588's, Allwinner's, Qualcomm's and the Raspberry Pi's, which have not been measured with
//! FilmCraft yet). H.264 8-bit and HEVC 8- / 10-bit, 4:2:0 progressive; everything else stays with
//! FilmCraft's own decoders, as does anything an add-on refuses or fails on ([`HybridDecoder`]).
//!
//! The add-on is loaded and fed by `native/media_kit.cpp` (the FFI is [`ffi`]); access units go
//! in as Annex B, pictures come back as NV12 or P010 and become planar frames through
//! [`crate::biplanar`], as NVDEC's do on Linux. `FILMCRAFT_NO_HW_DECODE=1` (or Settings ▸
//! Playback ▸ Hardware decoding Off) leaves every stream to the software decoders.
//!
//! [`HybridDecoder`]: crate::HybridDecoder

use filmcraft_codecs::hw::{NalCodec, NalStreamInfo};
use filmcraft_codecs::{CodecError, DecodedFrame, Result, VideoDecoder};

use crate::annexb::to_annex_b;
use crate::biplanar::{Biplanar, Geometry, to_frame};

#[allow(unsafe_code)]
pub mod ffi {
    //! The C functions of `native/media_kit.cpp`, behind safe wrappers.
    use std::ffi::{CStr, c_char, c_void};

    unsafe extern "C" {
        fn fc_hwdec_probe() -> *mut c_char;
        fn fc_hwdec_free(data: *mut c_void);
        fn fc_hwdec_open(hevc: i32, width: i32, height: i32, bits: i32, name: *mut c_char, name_len: usize, error: *mut c_char, error_len: usize) -> *mut c_void;
        fn fc_hwdec_feed(handle: *mut c_void, data: *const u8, size: usize, time: i64) -> i32;
        fn fc_hwdec_flush(handle: *mut c_void) -> i32;
        fn fc_hwdec_next(handle: *mut c_void, data: *mut *mut u8, size: *mut usize, time: *mut i64) -> i32;
        fn fc_hwdec_discard(handle: *mut c_void);
        fn fc_hwdec_geometry(handle: *mut c_void, row_bytes: *mut usize, rows: *mut usize);
        fn fc_hwdec_close(handle: *mut c_void);
    }

    /// The decoder add-ons installed (their file names), loaded once.
    pub fn probe() -> Vec<String> {
        // SAFETY: no arguments; the result is a strdup'd string (or NULL).
        let p = unsafe { fc_hwdec_probe() };
        if p.is_null() {
            return Vec::new();
        }
        // SAFETY: NUL-terminated, from strdup.
        let names = unsafe { CStr::from_ptr(p) }.to_string_lossy().lines().map(str::to_string).collect();
        // SAFETY: allocated by the host's malloc.
        unsafe { fc_hwdec_free(p.cast()) };
        names
    }

    fn text(buf: &[c_char]) -> String {
        // SAFETY: the host writes NUL-terminated text into the zeroed buffer.
        unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
    }

    /// One add-on decoder and the thread it runs on.
    pub struct Session {
        handle: std::ptr::NonNull<c_void>,
        pub add_on: String,
        pub row_bytes: usize,
        pub rows: usize,
    }

    // SAFETY: the C++ decoder object locks its own state; a `Session` is used by one thread at a
    // time (`&mut self`).
    unsafe impl Send for Session {}

    impl Session {
        pub fn open(hevc: bool, width: u32, height: u32, bits: u32) -> Result<Self, String> {
            let int = |v: u32| i32::try_from(v).map_err(|_| "picture too large".to_string());
            let (mut name, mut error) = ([0 as c_char; 64], [0 as c_char; 256]);
            // SAFETY: plain values and two writable, zeroed buffers of the given lengths.
            let handle = unsafe {
                fc_hwdec_open(i32::from(hevc), int(width)?, int(height)?, int(bits)?, name.as_mut_ptr(), name.len(), error.as_mut_ptr(), error.len())
            };
            let handle = std::ptr::NonNull::new(handle).ok_or_else(|| text(&error))?;
            let (mut row_bytes, mut rows) = (0usize, 0usize);
            // SAFETY: a live handle and two writable values.
            unsafe { fc_hwdec_geometry(handle.as_ptr(), &mut row_bytes, &mut rows) };
            Ok(Self { handle, add_on: text(&name), row_bytes, rows })
        }

        /// Hands over one Annex B access unit; returns once the add-on asks for the next.
        pub fn feed(&mut self, data: &[u8], time: i64) -> Result<(), String> {
            // SAFETY: a live handle and a readable buffer the host copies.
            match unsafe { fc_hwdec_feed(self.handle.as_ptr(), data.as_ptr(), data.len(), time) } {
                0 => Ok(()),
                e => Err(format!("the decoder add-on failed ({e:#x})")),
            }
        }

        /// Ends the stream (every picture held comes out) and starts over.
        pub fn flush(&mut self) -> Result<(), String> {
            // SAFETY: a live handle.
            match unsafe { fc_hwdec_flush(self.handle.as_ptr()) } {
                0 => Ok(()),
                e => Err(format!("the decoder add-on failed while flushing ({e:#x})")),
            }
        }

        /// The pictures given out so far, oldest first: each picture's bytes and time to `f`, whose
        /// results are returned (the bytes are freed after the call, so nothing is copied twice).
        pub fn take<T>(&mut self, mut f: impl FnMut(&[u8], i64) -> T) -> Vec<T> {
            let mut out = Vec::new();
            loop {
                let (mut data, mut size, mut time) = (std::ptr::null_mut(), 0usize, 0i64);
                // SAFETY: a live handle and writable values.
                if unsafe { fc_hwdec_next(self.handle.as_ptr(), &mut data, &mut size, &mut time) } == 0 || data.is_null() {
                    return out;
                }
                // SAFETY: the host malloc'd `size` bytes at `data` and gave them to us; the slice
                // doesn't outlive this iteration.
                let bytes = unsafe { std::slice::from_raw_parts(data, size) };
                out.push(f(bytes, time));
                // SAFETY: allocated by the host's malloc, no longer referenced.
                unsafe { fc_hwdec_free(data.cast()) };
            }
        }

        /// Drops the pictures given out and not taken.
        pub fn discard(&mut self) {
            // SAFETY: a live handle.
            unsafe { fc_hwdec_discard(self.handle.as_ptr()) }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: a handle from `fc_hwdec_open`, closed once; this joins its thread.
            unsafe { fc_hwdec_close(self.handle.as_ptr()) }
        }
    }
}

/// Whether the user turned the add-ons off for this process.
pub fn disabled() -> bool {
    std::env::var_os("FILMCRAFT_NO_HW_DECODE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The add-ons installed, or why there are none.
pub fn probe() -> std::result::Result<String, String> {
    if disabled() {
        return Err("FILMCRAFT_NO_HW_DECODE is set".into());
    }
    let names = ffi::probe();
    if names.is_empty() { Err("no Media Kit hardware decoder add-on (nvdec) is installed".into()) } else { Ok(names.join(", ")) }
}

/// A Media Kit add-on decoder for one H.264 or HEVC stream.
pub struct MkDecoder {
    session: ffi::Session,
    info: NalStreamInfo,
    geometry: Geometry,
    name: String,
    /// Whether the add-on has the stream's parameter sets (sent with the first sample after a reset).
    primed: bool,
    /// HEVC, after starting at a CRA / BLA picture: its RASL pictures are not fed until the next
    /// random access point.
    skip_rasl: bool,
}

impl MkDecoder {
    /// A decoder for the stream `info` describes, or why no add-on takes it.
    pub fn new(info: NalStreamInfo) -> std::result::Result<Self, String> {
        let bits = info.bit_depth_luma;
        let taken = match info.codec {
            NalCodec::H264 => bits == 8,
            NalCodec::Hevc => matches!(bits, 8 | 10),
        };
        if !taken || info.chroma_format_idc != 1 || info.bit_depth_chroma != bits || info.interlaced {
            return Err(format!(
                "{} chroma format {} at {bits}/{}-bit is not taken",
                if info.interlaced { "interlaced" } else { "progressive" },
                info.chroma_format_idc,
                info.bit_depth_chroma
            ));
        }
        let (_, _, w, h) = info.crop;
        let session = ffi::Session::open(info.codec == NalCodec::Hevc, w, h, bits)?;
        // The add-ons give the cropped picture.
        let geometry = Geometry { crop: (0, 0, w, h), bits, color: info.color, par: info.par };
        let name = format!("{} {} (Media Kit)", session.add_on, if info.codec == NalCodec::Hevc { "HEVC" } else { "H.264" });
        Ok(Self { session, info, geometry, name, primed: false, skip_rasl: false })
    }

    fn frames(&mut self) -> Result<Vec<DecodedFrame>> {
        let stride = self.session.row_bytes;
        let rows = usize::try_from(self.geometry.crop.3).unwrap_or(0).min(self.session.rows);
        let geometry = self.geometry;
        self.session
            .take(|bytes, pts| {
                let split = stride.saturating_mul(rows).min(bytes.len());
                let (luma, chroma) = bytes.split_at(split);
                to_frame(&Biplanar { luma, chroma, stride, rows }, &geometry).map(|frame| DecodedFrame { pts, frame, draft: false })
            })
            .into_iter()
            .collect::<std::result::Result<Vec<_>, String>>()
            .map_err(CodecError::Decode)
    }
}

impl VideoDecoder for MkDecoder {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if self.info.codec == NalCodec::Hevc {
            let types = self.info.nal_types(sample);
            if self.info.is_irap(sample) {
                // IDR pictures (19, 20) have no RASL pictures
                self.skip_rasl = !self.primed && !types.iter().any(|t| matches!(t, 19 | 20));
            } else if self.skip_rasl && types.iter().any(|t| matches!(t, 8 | 9)) {
                return Ok(Vec::new());
            }
        }
        let data = to_annex_b(&self.info, sample, !self.primed).map_err(CodecError::Decode)?;
        self.primed = true;
        self.session.feed(&data, pts).map_err(CodecError::Decode)?;
        self.frames()
    }

    fn flush(&mut self) -> Vec<DecodedFrame> {
        self.primed = false;
        if let Err(e) = self.session.flush() {
            log::warn!("{}: {e}", self.name);
            return Vec::new();
        }
        self.frames().unwrap_or_else(|e| {
            log::warn!("{}: {e} while flushing", self.name);
            Vec::new()
        })
    }

    fn reset(&mut self) {
        self.primed = false;
        if let Err(e) = self.session.flush() {
            log::warn!("{}: {e} while resetting", self.name);
        }
        self.session.discard();
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn is_random_access(&self, sample: &[u8]) -> Option<bool> {
        self.info.is_random_access(sample)
    }

    fn is_disposable(&self, sample: &[u8]) -> bool {
        self.info.is_disposable(sample)
    }
}
