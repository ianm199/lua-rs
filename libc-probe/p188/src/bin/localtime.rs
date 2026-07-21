fn main() {
    unsafe {
        let t: libc::time_t = 1_700_000_000;
        let mut tm: libc::tm = core::mem::zeroed();
        let r1 = libc::localtime_s(&mut tm, &t);
        let r2 = libc::gmtime_s(&mut tm, &t);
        println!("localtime_s={} gmtime_s={} tm_year={}", r1, r2, tm.tm_year);
    }
}
