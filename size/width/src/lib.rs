#![no_std]
#[panic_handler]
fn p(_: &core::panic::PanicInfo<'_>) -> ! { loop {} }

macro_rules! probe {
    ($name:ident, $t:ty) => {
        #[no_mangle]
        pub extern "C" fn $name(p: *const u8, n: usize) -> $t {
            let input = unsafe { core::slice::from_raw_parts(p, n) };
            let mut acc: $t = 0;
            let _ = dacodec::pull::select(input, &[b"v"], |got| {
                acc = acc.wrapping_add(got[0].and_then(|v| v.as_int::<$t>()).unwrap_or(0));
                Ok(())
            });
            acc
        }
    };
}

/// The baseline: locate the field, convert nothing. Every other column
/// is reported as the bytes it adds over this.
#[cfg(feature = "w0")]
#[no_mangle]
pub extern "C" fn probe_none(p: *const u8, n: usize) -> i64 {
    let input = unsafe { core::slice::from_raw_parts(p, n) };
    let mut acc: i64 = 0;
    let _ = dacodec::pull::select(input, &[b"v"], |got| {
        acc = acc.wrapping_add(got[0].map(|v| v.bytes().len() as i64).unwrap_or(0));
        Ok(())
    });
    acc
}

#[cfg(feature = "w8")]  probe!(probe_i8, i8);
#[cfg(feature = "w16")] probe!(probe_i16, i16);
#[cfg(feature = "w32")] probe!(probe_i32, i32);
#[cfg(feature = "w64")] probe!(probe_i64, i64);

#[cfg(feature = "wi64")]
#[no_mangle]
pub extern "C" fn probe_as_i64(p: *const u8, n: usize) -> i64 {
    let input = unsafe { core::slice::from_raw_parts(p, n) };
    let mut acc: i64 = 0;
    let _ = dacodec::pull::select(input, &[b"v"], |got| {
        acc = acc.wrapping_add(got[0].and_then(|v| v.as_i64()).unwrap_or(0));
        Ok(())
    });
    acc
}
