//! User programs embedded in the kernel image. There is no filesystem yet, so
//! `kernel/build.rs` builds them from `../user` and `include_bytes!` bakes
//! each ELF in at compile time.
//!
//! The bytes have no alignment guarantee (`include_bytes!` yields a `[u8; N]`,
//! align 1), so `elf::parse` reads every header field byte by byte and never
//! casts the slice to a header struct in place.

/// `user/src/bin/hello.rs`: the W5 loader proof program.
pub static HELLO: &[u8] = include_bytes!(env!("USER_HELLO_ELF"));

/// Exit code `HELLO` returns when every check passes (`EXIT_OK` there).
pub const HELLO_EXIT_OK: u64 = 42;

#[test_case]
fn test_hello_is_embedded_whole() {
    assert!(!HELLO.is_empty());
    assert_eq!(&HELLO[..4], b"\x7fELF");
    // The size of the file `build.rs` embedded: nothing was cut off.
    let file_len: usize = env!("USER_HELLO_ELF_LEN").parse().expect("build.rs exports a number");
    assert_eq!(HELLO.len(), file_len);
}
