use std::{ffi::CStr, io};

use tracing::instrument;

use crate::{
    Error, Result,
    common::Compression,
    io::wia::{WIACompression, WIADisc},
};

#[derive(Debug, Clone)]
pub enum DecompressionKind {
    None,
    #[cfg(feature = "compress-zlib")]
    Deflate,
    #[cfg(feature = "compress-bzip2")]
    Bzip2,
    #[cfg(feature = "compress-lzma")]
    Lzma(Box<[u8]>),
    #[cfg(feature = "compress-lzma")]
    Lzma2(Box<[u8]>),
    #[cfg(feature = "compress-zstd")]
    Zstandard,
}

impl DecompressionKind {
    pub fn from_wia(disc: &WIADisc) -> Result<Self> {
        let _data = &disc.compr_data[..disc.compr_data_len as usize];
        match disc.compression() {
            WIACompression::None => Ok(Self::None),
            #[cfg(feature = "compress-bzip2")]
            WIACompression::Bzip2 => Ok(Self::Bzip2),
            #[cfg(feature = "compress-lzma")]
            WIACompression::Lzma => Ok(Self::Lzma(Box::from(_data))),
            #[cfg(feature = "compress-lzma")]
            WIACompression::Lzma2 => Ok(Self::Lzma2(Box::from(_data))),
            #[cfg(feature = "compress-zstd")]
            WIACompression::Zstandard => Ok(Self::Zstandard),
            comp => Err(Error::DiscFormat(format!("Unsupported WIA/RVZ compression: {:?}", comp))),
        }
    }

    #[instrument(name = "DecompressionKind::decompress", skip_all)]
    pub fn decompress(&self, buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        match self {
            DecompressionKind::None => {
                if buf.len() > out.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Decompressed data too large: {} > {}", buf.len(), out.len()),
                    ));
                }
                out[..buf.len()].copy_from_slice(buf);
                Ok(buf.len())
            }
            #[cfg(feature = "compress-zlib")]
            DecompressionKind::Deflate => zlib_api::decompress(buf, out),
            #[cfg(feature = "compress-bzip2")]
            DecompressionKind::Bzip2 => bzip2_api::decompress(buf, out),
            #[cfg(feature = "compress-lzma")]
            DecompressionKind::Lzma(data) => lzma_api::decompress_lzma(data, buf, out),
            #[cfg(feature = "compress-lzma")]
            DecompressionKind::Lzma2(data) => lzma_api::decompress_lzma2(data, buf, out),
            #[cfg(feature = "compress-zstd")]
            DecompressionKind::Zstandard => zstd_api::decompress(buf, out),
        }
    }

    pub fn get_content_size(&self, buf: &[u8]) -> io::Result<Option<usize>> {
        match self {
            DecompressionKind::None => Ok(Some(buf.len())),
            #[cfg(feature = "compress-zstd")]
            DecompressionKind::Zstandard => zstd_api::get_content_size(buf),
            #[allow(unreachable_patterns)] // if compression features are disabled
            _ => Ok(None),
        }
    }
}

pub struct Compressor {
    pub kind: Compression,
    pub buffer: Vec<u8>,
}

impl Clone for Compressor {
    fn clone(&self) -> Self {
        Self { kind: self.kind, buffer: Vec::with_capacity(self.buffer.capacity()) }
    }
}

impl Compressor {
    pub fn new(kind: Compression, buffer_size: usize) -> Self {
        Self { kind, buffer: Vec::with_capacity(buffer_size) }
    }

    /// Compresses the given buffer into `out`. `out`'s capacity will not be extended. Instead, if
    /// the compressed data is larger than `out`, this function will bail and return `false`.
    #[instrument(name = "Compressor::compress", skip_all)]
    pub fn compress(&mut self, buf: &[u8]) -> io::Result<bool> {
        self.buffer.clear();
        match self.kind {
            Compression::None => {
                if self.buffer.capacity() >= buf.len() {
                    self.buffer.extend_from_slice(buf);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[cfg(feature = "compress-zlib")]
            Compression::Deflate(level) => zlib_api::compress(buf, level, &mut self.buffer),
            #[cfg(feature = "compress-bzip2")]
            Compression::Bzip2(level) => bzip2_api::compress(buf, level, &mut self.buffer),
            #[cfg(feature = "compress-lzma")]
            Compression::Lzma(level) => lzma_api::compress_lzma(level, buf, &mut self.buffer),
            #[cfg(feature = "compress-lzma")]
            Compression::Lzma2(level) => lzma_api::compress_lzma2(level, buf, &mut self.buffer),
            #[cfg(feature = "compress-zstd")]
            Compression::Zstandard(level) => zstd_api::compress(buf, level, &mut self.buffer),
            #[allow(unreachable_patterns)] // if compression is disabled
            _ => Err(io::Error::other(format!("Unsupported compression: {:?}", self.kind))),
        }
    }
}

// Decode through EOF even when the destination is exactly full, so trailers,
// checksums and truncated streams are checked rather than silently accepted.
#[cfg(any(feature = "compress-zlib", feature = "compress-bzip2", feature = "compress-lzma"))]
fn decompress_into(mut reader: impl io::Read, out: &mut [u8]) -> io::Result<usize> {
    let mut len = 0;
    while len < out.len() {
        match reader.read(&mut out[len..]) {
            Ok(0) => return Ok(len),
            Ok(n) => len += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let mut extra = [0];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => return Ok(len),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Decompression output buffer too small",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

// A slice-backed writer cannot grow the caller's output buffer. WriteZero means
// the compressed block did not fit and the disc writer should store it verbatim.
#[cfg(any(feature = "compress-zlib", feature = "compress-lzma"))]
fn compress_into(
    out: &mut Vec<u8>,
    encode: impl FnOnce(&mut io::Cursor<&mut [u8]>) -> io::Result<()>,
) -> io::Result<bool> {
    out.resize(out.capacity(), 0);
    let mut writer = io::Cursor::new(out.as_mut_slice());
    let result = encode(&mut writer);
    let len = writer.position() as usize;
    match result {
        Ok(()) => {
            out.truncate(len);
            Ok(true)
        }
        Err(e) => {
            out.clear();
            if e.kind() == io::ErrorKind::WriteZero { Ok(false) } else { Err(e) }
        }
    }
}

#[cfg(feature = "compress-zlib")]
mod zlib_api {
    use std::io::{self, Write};

    pub fn decompress(buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        super::decompress_into(flate2::bufread::ZlibDecoder::new(buf), out)
    }

    pub fn compress(buf: &[u8], level: u8, out: &mut Vec<u8>) -> io::Result<bool> {
        if level > 9 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid zlib level"));
        }
        super::compress_into(out, |writer| {
            let mut encoder =
                flate2::write::ZlibEncoder::new(writer, flate2::Compression::new(level.into()));
            encoder.write_all(buf)?;
            encoder.finish()?;
            Ok(())
        })
    }
}

#[cfg(feature = "compress-bzip2")]
mod bzip2_api {
    use std::io;

    pub fn decompress(buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        super::decompress_into(bzip2::bufread::BzDecoder::new(buf), out)
    }

    pub fn compress(buf: &[u8], level: u8, out: &mut Vec<u8>) -> io::Result<bool> {
        if !(1..=9).contains(&level) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid bzip2 level"));
        }
        if out.capacity() == 0 {
            return Ok(false);
        }
        out.resize(out.capacity(), 0);
        let mut encoder = bzip2::Compress::new(bzip2::Compression::new(level.into()), 30);
        loop {
            let before = (encoder.total_in(), encoder.total_out());
            let status = encoder
                .compress(
                    &buf[before.0 as usize..],
                    &mut out[before.1 as usize..],
                    bzip2::Action::Finish,
                )
                .map_err(io::Error::other)?;
            if status == bzip2::Status::StreamEnd {
                out.truncate(encoder.total_out() as usize);
                return Ok(true);
            }
            if encoder.total_out() as usize == out.len() {
                out.clear();
                return Ok(false);
            }
            if before == (encoder.total_in(), encoder.total_out()) {
                return Err(io::Error::other("bzip2 compressor made no progress"));
            }
        }
    }
}

#[cfg(feature = "compress-zstd-vendored")]
use zstd_sys as zstd_raw;

#[cfg(all(feature = "compress-zstd", not(feature = "compress-zstd-vendored")))]
mod zstd_raw {
    use core::ffi::{c_char, c_int, c_uint, c_ulonglong, c_void};

    pub const ZSTD_CONTENTSIZE_UNKNOWN: i32 = -1;
    pub const ZSTD_CONTENTSIZE_ERROR: i32 = -2;

    #[cfg_attr(not(target_env = "msvc"), link(name = "zstd"))]
    #[cfg_attr(target_env = "msvc", link(name = "zstd", kind = "static"))]
    unsafe extern "C" {
        pub fn ZSTD_compress(
            dst: *mut c_void,
            dstCapacity: usize,
            src: *const c_void,
            srcSize: usize,
            compressionLevel: c_int,
        ) -> usize;

        pub fn ZSTD_decompress(
            dst: *mut c_void,
            dstCapacity: usize,
            src: *const c_void,
            srcSize: usize,
        ) -> usize;

        pub fn ZSTD_getFrameContentSize(src: *const c_void, srcSize: usize) -> c_ulonglong;
        pub fn ZSTD_compressBound(srcSize: usize) -> usize;
        pub fn ZSTD_isError(result: usize) -> c_uint;
        pub fn ZSTD_getErrorName(result: usize) -> *const c_char;
    }
}

#[cfg(feature = "compress-zstd")]
pub(crate) mod zstd_api {
    use std::{ffi::c_void, io};

    use super::{CStr, zstd_raw};

    const ZSTD_ERROR_DST_SIZE_TOO_SMALL: usize = 70usize.wrapping_neg();

    pub fn compress_bound(size: usize) -> usize { unsafe { zstd_raw::ZSTD_compressBound(size) } }

    fn map_error_code(code: usize) -> io::Error {
        let msg = unsafe { CStr::from_ptr(zstd_raw::ZSTD_getErrorName(code)) }
            .to_string_lossy()
            .into_owned();
        io::Error::other(msg)
    }

    pub fn decompress(buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        let code = unsafe {
            zstd_raw::ZSTD_decompress(
                out.as_mut_ptr().cast::<c_void>(),
                out.len(),
                buf.as_ptr().cast::<c_void>(),
                buf.len(),
            )
        };
        if unsafe { zstd_raw::ZSTD_isError(code) } != 0 {
            return Err(map_error_code(code));
        }
        Ok(code)
    }

    pub fn compress(buf: &[u8], level: i8, out: &mut Vec<u8>) -> io::Result<bool> {
        out.resize(out.capacity(), 0);
        let code = unsafe {
            zstd_raw::ZSTD_compress(
                out.as_mut_ptr().cast::<c_void>(),
                out.len(),
                buf.as_ptr().cast::<c_void>(),
                buf.len(),
                level as i32,
            )
        };
        if unsafe { zstd_raw::ZSTD_isError(code) } != 0 {
            // dstSize_tooSmall means compressed data doesn't fit; signal caller to store uncompressed
            if code == ZSTD_ERROR_DST_SIZE_TOO_SMALL {
                out.clear();
                return Ok(false);
            }
            return Err(map_error_code(code));
        }
        out.truncate(code);
        Ok(true)
    }

    pub fn get_content_size(buf: &[u8]) -> io::Result<Option<usize>> {
        let size =
            unsafe { zstd_raw::ZSTD_getFrameContentSize(buf.as_ptr().cast::<c_void>(), buf.len()) };
        if size == zstd_raw::ZSTD_CONTENTSIZE_UNKNOWN as u64 {
            return Ok(None);
        } else if size == zstd_raw::ZSTD_CONTENTSIZE_ERROR as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid Zstandard frame header",
            ));
        }
        usize::try_from(size)
            .map(Some)
            .map_err(|_| io::Error::other("Zstandard frame size exceeds usize"))
    }
}

#[cfg(feature = "compress-lzma-vendored")]
use liblzma_sys as lzma_raw;

#[cfg(all(feature = "compress-lzma", not(feature = "compress-lzma-vendored")))]
mod lzma_raw {
    #![allow(non_camel_case_types)]

    use core::ffi::{c_uchar, c_uint, c_void};

    #[cfg(target_env = "msvc")]
    pub type __enum_ty = core::ffi::c_int;
    #[cfg(not(target_env = "msvc"))]
    pub type __enum_ty = core::ffi::c_uint;

    pub type lzma_bool = c_uchar;
    pub type lzma_ret = __enum_ty;
    pub type lzma_vli = u64;
    pub type lzma_mode = __enum_ty;
    pub type lzma_match_finder = __enum_ty;

    pub const LZMA_OK: lzma_ret = 0;
    pub const LZMA_OPTIONS_ERROR: lzma_ret = 8;
    pub const LZMA_DATA_ERROR: lzma_ret = 9;
    pub const LZMA_BUF_ERROR: lzma_ret = 10;
    pub const LZMA_PROG_ERROR: lzma_ret = 11;

    pub const LZMA_PRESET_DEFAULT: u32 = 6;

    pub const LZMA_DICT_SIZE_MIN: u32 = 4096;

    pub const LZMA_VLI_UNKNOWN: lzma_vli = u64::MAX;

    pub const LZMA_FILTER_LZMA1: lzma_vli = 0x4000000000000001;
    pub const LZMA_FILTER_LZMA2: lzma_vli = 0x21;

    #[repr(C)]
    pub struct lzma_filter {
        pub id: lzma_vli,
        pub options: *mut c_void,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct lzma_options_lzma {
        pub dict_size: u32,
        pub preset_dict: *const u8,
        pub preset_dict_size: u32,
        pub lc: u32,
        pub lp: u32,
        pub pb: u32,
        pub mode: lzma_mode,
        pub nice_len: u32,
        pub mf: lzma_match_finder,
        pub depth: u32,

        reserved_int1: u32,
        reserved_int2: u32,
        reserved_int3: u32,
        reserved_int4: u32,
        reserved_int5: u32,
        reserved_int6: u32,
        reserved_int7: u32,
        reserved_int8: u32,
        reserved_enum1: __enum_ty,
        reserved_enum2: __enum_ty,
        reserved_enum3: __enum_ty,
        reserved_enum4: __enum_ty,
        reserved_ptr1: *mut c_void,
        reserved_ptr2: *mut c_void,
    }

    pub type lzma_allocator = c_void;

    #[cfg_attr(not(target_env = "msvc"), link(name = "lzma"))]
    #[cfg_attr(target_env = "msvc", link(name = "lzma", kind = "static"))]
    unsafe extern "C" {
        pub fn lzma_raw_buffer_encode(
            filters: *const lzma_filter,
            allocator: *const lzma_allocator,
            input: *const u8,
            in_size: usize,
            out: *mut u8,
            out_pos: *mut usize,
            out_size: usize,
        ) -> lzma_ret;

        pub fn lzma_raw_buffer_decode(
            filters: *const lzma_filter,
            allocator: *const lzma_allocator,
            input: *const u8,
            in_pos: *mut usize,
            in_size: usize,
            out: *mut u8,
            out_pos: *mut usize,
            out_size: usize,
        ) -> lzma_ret;

        pub fn lzma_lzma_preset(options: *mut lzma_options_lzma, preset: c_uint) -> lzma_bool;
    }
}

#[cfg(feature = "compress-lzma")]
pub(crate) mod lzma_api {
    use std::{
        cmp::Ordering,
        ffi::c_void,
        io::{self, ErrorKind, Write},
    };

    use lzma_rust2::{LzmaOptions, LzmaReader, LzmaWriter};

    use super::lzma_raw;

    fn rust_preset_options(level: u32) -> io::Result<LzmaOptions> {
        if level > 9 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid LZMA preset"));
        }
        Ok(LzmaOptions::with_preset(level))
    }

    fn map_error_code(code: lzma_raw::lzma_ret, context: &str) -> io::Error {
        let reason = match code {
            x if x == lzma_raw::LZMA_OPTIONS_ERROR => "options error",
            x if x == lzma_raw::LZMA_DATA_ERROR => "data error",
            x if x == lzma_raw::LZMA_BUF_ERROR => "output buffer too small",
            x if x == lzma_raw::LZMA_PROG_ERROR => "program error",
            _ => "unknown error",
        };
        io::Error::new(ErrorKind::InvalidData, format!("{context}: {reason} ({code})"))
    }

    fn preset_options(level: u32) -> io::Result<lzma_raw::lzma_options_lzma> {
        let mut options: lzma_raw::lzma_options_lzma = unsafe { std::mem::zeroed() };
        if unsafe { lzma_raw::lzma_lzma_preset(&mut options, level) } != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("Invalid LZMA preset level {level}"),
            ));
        }
        Ok(options)
    }

    fn lzma_lclppb_decode(options: &mut lzma_raw::lzma_options_lzma, byte: u8) -> io::Result<()> {
        let mut d = byte as u32;
        if d >= (9 * 5 * 5) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("Invalid LZMA props byte: {d}"),
            ));
        }
        options.lc = d % 9;
        d /= 9;
        options.pb = d / 5;
        options.lp = d % 5;
        Ok(())
    }

    fn lzma_props_decode(props: &[u8]) -> io::Result<lzma_raw::lzma_options_lzma> {
        if props.len() != 5 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("Invalid LZMA props length: {}", props.len()),
            ));
        }
        let mut options = preset_options(lzma_raw::LZMA_PRESET_DEFAULT)?;
        lzma_lclppb_decode(&mut options, props[0])?;
        options.dict_size = u32::from_le_bytes([props[1], props[2], props[3], props[4]]);
        Ok(options)
    }

    fn lzma2_props_decode(props: &[u8]) -> io::Result<lzma_raw::lzma_options_lzma> {
        if props.len() != 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("Invalid LZMA2 props length: {}", props.len()),
            ));
        }
        let d = props[0] as u32;
        let mut options = preset_options(lzma_raw::LZMA_PRESET_DEFAULT)?;
        options.dict_size = match d.cmp(&40) {
            Ordering::Greater => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("Invalid LZMA2 props byte: {d}"),
                ));
            }
            Ordering::Equal => u32::MAX,
            Ordering::Less => (2 | (d & 1)) << (d / 2 + 11),
        };
        Ok(options)
    }

    fn get_dist_slot(dist: u32) -> u32 {
        if dist <= 4 {
            dist
        } else {
            let i = dist.leading_zeros() ^ 31;
            (i + i) + ((dist >> (i - 1)) & 1)
        }
    }

    fn lzma2_props_encode(options: &lzma_raw::lzma_options_lzma) -> [u8; 1] {
        let mut d = options.dict_size.max(lzma_raw::LZMA_DICT_SIZE_MIN);
        d -= 1;
        d |= d >> 2;
        d |= d >> 3;
        d |= d >> 4;
        d |= d >> 8;
        d |= d >> 16;
        if d == u32::MAX { [40] } else { [(get_dist_slot(d + 1) - 24) as u8] }
    }

    fn make_filters(
        filter_id: lzma_raw::lzma_vli,
        options: &mut lzma_raw::lzma_options_lzma,
    ) -> [lzma_raw::lzma_filter; 2] {
        [
            lzma_raw::lzma_filter {
                id: filter_id,
                options: options as *mut lzma_raw::lzma_options_lzma as *mut c_void,
            },
            lzma_raw::lzma_filter { id: lzma_raw::LZMA_VLI_UNKNOWN, options: std::ptr::null_mut() },
        ]
    }

    fn compress_raw(
        filter_id: lzma_raw::lzma_vli,
        level: u8,
        buf: &[u8],
        out: &mut Vec<u8>,
    ) -> io::Result<bool> {
        let mut options = preset_options(level as u32)?;
        let filters = make_filters(filter_id, &mut options);
        out.resize(out.capacity(), 0);
        let mut out_pos = 0usize;
        let ret = unsafe {
            lzma_raw::lzma_raw_buffer_encode(
                filters.as_ptr(),
                std::ptr::null(),
                buf.as_ptr(),
                buf.len(),
                out.as_mut_ptr(),
                &mut out_pos,
                out.len(),
            )
        };
        match ret {
            x if x == lzma_raw::LZMA_OK => {
                out.truncate(out_pos);
                Ok(true)
            }
            x if x == lzma_raw::LZMA_BUF_ERROR => {
                out.clear();
                Ok(false)
            }
            _ => Err(map_error_code(ret, "LZMA compression failed")),
        }
    }

    fn decompress_raw(
        filter_id: lzma_raw::lzma_vli,
        props: &[u8],
        buf: &[u8],
        out: &mut [u8],
    ) -> io::Result<usize> {
        let mut options = if filter_id == lzma_raw::LZMA_FILTER_LZMA1 {
            lzma_props_decode(props)?
        } else {
            lzma2_props_decode(props)?
        };
        let filters = make_filters(filter_id, &mut options);
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        let ret = unsafe {
            lzma_raw::lzma_raw_buffer_decode(
                filters.as_ptr(),
                std::ptr::null(),
                buf.as_ptr(),
                &mut in_pos,
                buf.len(),
                out.as_mut_ptr(),
                &mut out_pos,
                out.len(),
            )
        };
        if ret != lzma_raw::LZMA_OK {
            return Err(map_error_code(ret, "LZMA decompression failed"));
        }
        if in_pos != buf.len() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("LZMA decompression consumed {} of {} bytes", in_pos, buf.len()),
            ));
        }
        Ok(out_pos)
    }

    pub fn compress_lzma(level: u8, buf: &[u8], out: &mut Vec<u8>) -> io::Result<bool> {
        let options = rust_preset_options(level.into())?;
        super::compress_into(out, |writer| {
            // WIA stores the properties separately and uses an end marker.
            let mut encoder = LzmaWriter::new_no_header(writer, &options, true)?;
            encoder.write_all(buf)?;
            encoder.finish()?;
            Ok(())
        })
    }

    pub fn compress_lzma2(level: u8, buf: &[u8], out: &mut Vec<u8>) -> io::Result<bool> {
        compress_raw(lzma_raw::LZMA_FILTER_LZMA2, level, buf, out)
    }

    pub fn decompress_lzma(props: &[u8], buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        let [prop, a, b, c, d] = *props else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid LZMA properties length",
            ));
        };
        let dict_size = u32::from_le_bytes([a, b, c, d]);
        let mut input = buf;
        let decoder = LzmaReader::new_with_props(&mut input, u64::MAX, prop, dict_size, None)?;
        let len = super::decompress_into(decoder, out)?;
        if !input.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Trailing LZMA data"));
        }
        Ok(len)
    }

    pub fn decompress_lzma2(props: &[u8], buf: &[u8], out: &mut [u8]) -> io::Result<usize> {
        decompress_raw(lzma_raw::LZMA_FILTER_LZMA2, props, buf, out)
    }

    pub fn lzma_props_encode_preset(level: u32) -> io::Result<[u8; 5]> {
        let options = rust_preset_options(level)?;
        let mut props = [0; 5];
        props[0] = options.get_props();
        props[1..].copy_from_slice(&options.dict_size.to_le_bytes());
        Ok(props)
    }

    pub fn lzma2_props_encode_preset(level: u32) -> io::Result<[u8; 1]> {
        let options = preset_options(level)?;
        Ok(lzma2_props_encode(&options))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn reference_data() -> Vec<u8> {
        let mut data = b"nod pure Rust compression fixture\n".repeat(1024);
        data.extend((0..256).flat_map(|_| 0u8..=255));
        data
    }

    fn codecs() -> Vec<(Compression, DecompressionKind, &'static [u8])> {
        vec![
            #[cfg(feature = "compress-zlib")]
            (
                Compression::Deflate(6),
                DecompressionKind::Deflate,
                include_bytes!("../../tests/fixtures/compression/reference.zlib"),
            ),
            #[cfg(feature = "compress-bzip2")]
            (
                Compression::Bzip2(6),
                DecompressionKind::Bzip2,
                include_bytes!("../../tests/fixtures/compression/reference.bz2"),
            ),
            #[cfg(feature = "compress-lzma")]
            (
                Compression::Lzma(6),
                DecompressionKind::Lzma(Box::from([0x5d, 0, 0, 0x80, 0])),
                include_bytes!("../../tests/fixtures/compression/reference.lzma"),
            ),
        ]
    }

    #[test]
    fn native_codec_compatibility() {
        let expected = reference_data();
        for (kind, decoder, compressed) in codecs() {
            for size in [expected.len(), expected.len() + 16] {
                let mut out = vec![0; size];
                let len = decoder
                    .decompress(compressed, &mut out)
                    .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
                assert_eq!(&out[..len], expected, "{kind:?}");
            }
            assert!(
                decoder.decompress(compressed, &mut vec![0; expected.len() - 1]).is_err(),
                "{kind:?}: undersized output"
            );
            assert!(
                decoder
                    .decompress(&compressed[..compressed.len() - 1], &mut vec![0; expected.len()])
                    .is_err(),
                "{kind:?}: truncated input"
            );
            assert!(
                decoder.decompress(b"invalid compressed data", &mut [0; 32]).is_err(),
                "{kind:?}: invalid input"
            );
        }
    }

    #[test]
    fn bounded_roundtrips() {
        // Include multiple Zstd/LZMA2 blocks, empty input and incompressible data.
        let mut random = vec![0; 32768];
        let mut state = 0x12345678u32;
        for byte in &mut random {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        for (kind, decoder, _) in codecs() {
            for data in [Vec::new(), vec![42], random.clone(), reference_data().repeat(4)] {
                let mut compressor = Compressor::new(kind, data.len() * 2 + 1024);
                let capacity = compressor.buffer.capacity();
                assert!(
                    compressor
                        .compress(&data)
                        .unwrap_or_else(|e| panic!("{kind:?}, input {}: {e}", data.len())),
                    "{kind:?}"
                );
                assert_eq!(compressor.buffer.capacity(), capacity);
                let mut out = vec![0; data.len()];
                let len = decoder
                    .decompress(&compressor.buffer, &mut out)
                    .unwrap_or_else(|e| panic!("{kind:?}, {} bytes: {e}", data.len()));
                assert_eq!(len, data.len(), "{kind:?}");
                assert_eq!(out, data, "{kind:?}");
                for capacity in [0, 1, compressor.buffer.len() - 1]
                    .into_iter()
                    .filter(|&n| n < compressor.buffer.len())
                {
                    let mut tiny = Compressor::new(kind, capacity);
                    assert!(
                        !tiny.compress(&data).unwrap_or_else(|e| panic!(
                            "{kind:?}, input {} capacity {capacity}: {e}",
                            data.len()
                        )),
                        "{kind:?}: capacity {capacity}"
                    );
                    assert!(tiny.buffer.is_empty());
                    assert_eq!(tiny.buffer.capacity(), capacity);
                }
            }
        }
    }

    #[cfg(feature = "compress-lzma")]
    #[test]
    fn lzma_properties() {
        assert_eq!(lzma_api::lzma_props_encode_preset(6).unwrap(), [0x5d, 0, 0, 0x80, 0]);
        assert!(lzma_api::lzma_props_encode_preset(10).is_err());
        for props in [&[][..], &[0xff][..], &[0xff; 5][..]] {
            assert!(lzma_api::decompress_lzma(props, &[], &mut []).is_err());
        }
    }
}
