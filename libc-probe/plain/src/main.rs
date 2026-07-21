//! Mirrors wowemulation-dev/rilua's self-declared externs in `src/platform.rs`
//! (no libc involved): plain `localtime_s`/`gmtime_s` names, exactly what their
//! February 2026 release-binaries failure exercised.

#[repr(C)]
struct Tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
}

extern "C" {
    fn localtime_s(result: *mut Tm, timep: *const i64) -> i32;
    fn gmtime_s(result: *mut Tm, timep: *const i64) -> i32;
}

fn main() {
    unsafe {
        let t: i64 = 1_700_000_000;
        let mut tm = Tm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
        };
        let r1 = localtime_s(&mut tm, &t);
        let r2 = gmtime_s(&mut tm, &t);
        println!("plain localtime_s={} gmtime_s={} tm_year={}", r1, r2, tm.tm_year);
    }
}
