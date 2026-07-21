fn main() {
    unsafe {
        let mut t: libc::time_t = 0;
        let r = libc::time(&mut t);
        println!("time={} out={}", r, t);
    }
}
