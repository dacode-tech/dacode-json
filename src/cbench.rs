//! Safe wrappers over the vendored C/C++ baselines.
//!
//! Enabled by the `cbench` feature. These exist so tier 3 can be measured
//! against the library it was modelled on — Vela's `AUTHORS` file credits
//! yyjson for tier 3's "flat 16-byte value nodes in a pre-allocated pool"
//! and simdjson for tier 2's structural indexing.
//!
//! Every entry point is a **whole workload** implemented in C, matching a
//! workload the Rust side runs. Nothing crosses the FFI boundary per field,
//! so the comparison measures parsers rather than calling conventions. If
//! anything, this favours the C libraries: they do all their work inside C
//! with no boundary crossings at all.
//!
//! # Padding
//!
//! simdjson requires [`SIMDJSON_PADDING`] readable bytes past the end of the
//! input, and yyjson's in-situ mode requires `YYJSON_PADDING_SIZE` writable
//! bytes. [`Padded`] handles both.

use std::ffi::{c_char, c_int, c_longlong, CString};

// --- raw declarations -------------------------------------------------

extern "C" {
    fn vj_yy_parse(dat: *const c_char, len: usize) -> usize;
    fn vj_yy_parse_insitu(dat: *mut c_char, len: usize) -> usize;
    fn vj_pool_new(size: usize) -> *mut core::ffi::c_void;
    fn vj_pool_free(p: *mut core::ffi::c_void);
    fn vj_yy_parse_pool(
        p: *mut core::ffi::c_void,
        dat: *const c_char,
        len: usize,
    ) -> usize;
    fn vj_yy_sum_field(dat: *const c_char, len: usize, key: *const c_char) -> c_longlong;
    fn vj_yy_first_field(dat: *const c_char, len: usize, key: *const c_char) -> c_longlong;
    fn vj_yy_extract_all(dat: *const c_char, len: usize) -> u64;
    fn vj_yy_object_get_len(dat: *const c_char, len: usize, key: *const c_char) -> c_longlong;
    fn vj_yy_array_count(dat: *const c_char, len: usize) -> usize;
    fn vj_yy_validate(dat: *const c_char, len: usize) -> c_int;
    fn vj_yy_roundtrip(dat: *const c_char, len: usize) -> usize;
    fn vj_yy_version() -> *const c_char;

    fn vj_sj_parse_dom(dat: *const c_char, len: usize, cap: usize) -> usize;
    fn vj_sj_parse_ondemand(dat: *const c_char, len: usize, cap: usize) -> usize;
    fn vj_sj_sum_field(
        dat: *const c_char,
        len: usize,
        cap: usize,
        key: *const c_char,
    ) -> c_longlong;
    fn vj_sj_first_field(
        dat: *const c_char,
        len: usize,
        cap: usize,
        key: *const c_char,
    ) -> c_longlong;
    fn vj_sj_extract_all(dat: *const c_char, len: usize, cap: usize) -> u64;
    fn vj_sj_validate(dat: *const c_char, len: usize, cap: usize) -> c_int;
    fn vj_sj_version() -> *const c_char;
    fn vj_sj_implementation() -> *const c_char;
    fn vj_sj_padding() -> usize;
}

fn cstr<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "<null>";
    }
    // SAFETY: every caller passes a pointer to a C string literal or to
    // simdjson's static implementation name, both NUL-terminated and valid
    // for the life of the process.
    unsafe { core::ffi::CStr::from_ptr(p) }
        .to_str()
        .unwrap_or("<invalid utf8>")
}

// --- padded buffer ----------------------------------------------------

/// An input buffer with trailing slack, as both C libraries require.
///
/// simdjson reads up to `SIMDJSON_PADDING` bytes past the logical end;
/// yyjson's in-situ mode writes there. `len()` stays the logical length.
#[derive(Debug, Clone)]
pub struct Padded {
    buf: Vec<u8>,
    len: usize,
}

/// Slack allocated past the end. Comfortably above both libraries' needs.
pub const PADDING: usize = 128;

impl Padded {
    #[must_use]
    pub fn new(src: &[u8]) -> Self {
        let mut buf = Vec::with_capacity(src.len() + PADDING);
        buf.extend_from_slice(src);
        buf.resize(src.len() + PADDING, 0);
        Padded {
            len: src.len(),
            buf,
        }
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total allocated size including padding — simdjson's `capacity`.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    fn ptr(&self) -> *const c_char {
        self.buf.as_ptr().cast()
    }

    #[inline]
    fn ptr_mut(&mut self) -> *mut c_char {
        self.buf.as_mut_ptr().cast()
    }

    /// Restore the logical content, undoing any in-situ mutation.
    pub fn reset_from(&mut self, src: &[u8]) {
        let n = src.len().min(self.len);
        if let Some(dst) = self.buf.get_mut(..n) {
            dst.copy_from_slice(src.get(..n).unwrap_or_default());
        }
    }
}

// --- yyjson -----------------------------------------------------------

/// yyjson, the library Vela's tier 3 is modelled on.
#[derive(Debug)]
pub struct YyJson;

impl YyJson {
    /// Version string of the vendored source.
    #[must_use]
    pub fn version() -> &'static str {
        // SAFETY: returns a pointer to a string literal.
        cstr(unsafe { vj_yy_version() })
    }

    /// Full DOM build and free. Returns the value count.
    #[must_use]
    pub fn parse(input: &[u8]) -> usize {
        // SAFETY: `input` is a valid slice; the shim only reads `len` bytes
        // and does not retain the pointer.
        unsafe { vj_yy_parse(input.as_ptr().cast(), input.len()) }
    }

    /// `YYJSON_READ_INSITU` — yyjson's fastest read path. **Mutates the
    /// buffer**, so the caller must reset it between iterations.
    #[must_use]
    pub fn parse_insitu(input: &mut Padded) -> usize {
        let len = input.len;
        // SAFETY: the buffer has PADDING >= YYJSON_PADDING_SIZE writable
        // bytes past `len`, which is in-situ mode's requirement.
        unsafe { vj_yy_parse_insitu(input.ptr_mut(), len) }
    }

    #[must_use]
    pub fn sum_field(input: &[u8], key: &str) -> i64 {
        let Ok(k) = CString::new(key) else { return 0 };
        // SAFETY: both pointers are valid for the duration of the call.
        unsafe { vj_yy_sum_field(input.as_ptr().cast(), input.len(), k.as_ptr()) }
    }

    #[must_use]
    pub fn first_field(input: &[u8], key: &str) -> i64 {
        let Ok(k) = CString::new(key) else { return -1 };
        // SAFETY: as above.
        unsafe { vj_yy_first_field(input.as_ptr().cast(), input.len(), k.as_ptr()) }
    }

    #[must_use]
    pub fn extract_all(input: &[u8]) -> u64 {
        // SAFETY: as above.
        unsafe { vj_yy_extract_all(input.as_ptr().cast(), input.len()) }
    }

    #[must_use]
    pub fn object_get(input: &[u8], key: &str) -> i64 {
        let Ok(k) = CString::new(key) else { return -1 };
        // SAFETY: as above.
        unsafe { vj_yy_object_get_len(input.as_ptr().cast(), input.len(), k.as_ptr()) }
    }

    #[must_use]
    pub fn array_count(input: &[u8]) -> usize {
        // SAFETY: as above.
        unsafe { vj_yy_array_count(input.as_ptr().cast(), input.len()) }
    }

    #[must_use]
    pub fn validate(input: &[u8]) -> bool {
        // SAFETY: as above.
        unsafe { vj_yy_validate(input.as_ptr().cast(), input.len()) != 0 }
    }

    /// Parse and re-serialize; returns the output length.
    #[must_use]
    pub fn roundtrip(input: &[u8]) -> usize {
        // SAFETY: as above.
        unsafe { vj_yy_roundtrip(input.as_ptr().cast(), input.len()) }
    }
}

/// A reusable yyjson allocator pool — the closest analogue to
/// [`crate::Workspace`].
#[derive(Debug)]
pub struct YyPool {
    raw: *mut core::ffi::c_void,
}

impl YyPool {
    /// `size` must exceed yyjson's needs for the largest document; roughly
    /// 16 bytes per value plus the string data.
    #[must_use]
    pub fn new(size: usize) -> Option<Self> {
        // SAFETY: allocates and returns an owned handle, or null.
        let raw = unsafe { vj_pool_new(size) };
        if raw.is_null() {
            None
        } else {
            Some(YyPool { raw })
        }
    }

    /// Parse with the pooled allocator. No per-parse malloc.
    #[must_use]
    pub fn parse(&mut self, input: &[u8]) -> usize {
        // SAFETY: `self.raw` is a live pool from `vj_pool_new`; the shim
        // resets it before each parse.
        unsafe { vj_yy_parse_pool(self.raw, input.as_ptr().cast(), input.len()) }
    }
}

impl Drop for YyPool {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `vj_pool_new` and is freed exactly once.
        unsafe { vj_pool_free(self.raw) };
    }
}

// `vj_pool` is a plain malloc'd struct with no thread affinity; the only
// reason it is not `Sync` is the `&mut self` on `parse`, which Rust already
// enforces.
// SAFETY: the pool is owned exclusively by this handle.
unsafe impl Send for YyPool {}

// --- simdjson ---------------------------------------------------------

/// simdjson, the library Vela's tier 2 Stage 1 is modelled on.
#[derive(Debug)]
pub struct SimdJson;

impl SimdJson {
    #[must_use]
    pub fn version() -> &'static str {
        // SAFETY: returns a pointer to a string literal.
        cstr(unsafe { vj_sj_version() })
    }

    /// Which SIMD kernel was selected at runtime (e.g. `arm64`, `haswell`).
    #[must_use]
    pub fn implementation() -> &'static str {
        // SAFETY: points into simdjson's static implementation registry,
        // which lives for the process.
        cstr(unsafe { vj_sj_implementation() })
    }

    /// simdjson's required trailing padding.
    #[must_use]
    pub fn padding() -> usize {
        // SAFETY: returns a compile-time constant.
        unsafe { vj_sj_padding() }
    }

    /// Classic DOM parse.
    #[must_use]
    pub fn parse_dom(input: &Padded) -> usize {
        // SAFETY: `input` has `PADDING` readable bytes past `len`.
        unsafe { vj_sj_parse_dom(input.ptr(), input.len, input.capacity()) }
    }

    /// On Demand, walking the whole document.
    #[must_use]
    pub fn parse_ondemand(input: &Padded) -> usize {
        // SAFETY: as above.
        unsafe { vj_sj_parse_ondemand(input.ptr(), input.len, input.capacity()) }
    }

    #[must_use]
    pub fn sum_field(input: &Padded, key: &str) -> i64 {
        let Ok(k) = CString::new(key) else { return 0 };
        // SAFETY: as above.
        unsafe { vj_sj_sum_field(input.ptr(), input.len, input.capacity(), k.as_ptr()) }
    }

    /// One field from the first record. On Demand stops early here.
    #[must_use]
    pub fn first_field(input: &Padded, key: &str) -> i64 {
        let Ok(k) = CString::new(key) else { return -1 };
        // SAFETY: as above.
        unsafe { vj_sj_first_field(input.ptr(), input.len, input.capacity(), k.as_ptr()) }
    }

    #[must_use]
    pub fn extract_all(input: &Padded) -> u64 {
        // SAFETY: as above.
        unsafe { vj_sj_extract_all(input.ptr(), input.len, input.capacity()) }
    }

    #[must_use]
    pub fn validate(input: &Padded) -> bool {
        // SAFETY: as above.
        unsafe { vj_sj_validate(input.ptr(), input.len, input.capacity()) != 0 }
    }
}
