fn main() {
    unsafe {
        let t: libc::time_t = 1_700_000_000;
        let d = libc::difftime(t, 0);
        let p = libc::ctime(&t);
        println!("difftime={} ctime_is_null={}", d, p.is_null());
    }
}
