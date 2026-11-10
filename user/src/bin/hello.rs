//! The W5 loader proof program (`kernel/tests/elf_exec.rs`). Beyond printing,
//! it checks that the loader really built its address space: an initialised
//! static (`.data`, copied from the file) holds its value, and a `.bss`
//! static spanning several pages (no file bytes, zeroed by the loader) reads
//! zero. A jump to the right entry point alone would prove neither.
//!
//! Exit codes: 42 on success, a distinct small code per failed check, and
//! `seal_user::PANIC_EXIT_CODE` on a panic.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};
use seal_user::{println, write};

seal_user::entry!(main);

/// Atomics, not plain statics, for two reasons: interior mutability puts them
/// in writable `.data`/`.bss` (a never-written plain `static` lands in
/// `.rodata`), and the compiler can't fold their loads to the initial value,
/// so each check really reads memory.
static INITIALISED: AtomicU64 = AtomicU64::new(0x5ea1_05ea_1d47_a000);

/// 8 KiB of zeroes, so `.bss` crosses page boundaries and the loader has to
/// zero pages that have no file bytes at all, not just a page's tail.
static ZEROED: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];

pub const EXIT_OK: u64 = 42;
const EXIT_BAD_DATA: u64 = 1;
const EXIT_BAD_BSS: u64 = 2;
const EXIT_WRITE_FAILED: u64 = 3;
const EXIT_NOT_WRITABLE: u64 = 4;

fn main() -> u64 {
    if write(b"hello from an ELF\n").is_err() {
        return EXIT_WRITE_FAILED;
    }
    if INITIALISED.load(Ordering::Relaxed) != 0x5ea1_05ea_1d47_a000 {
        return EXIT_BAD_DATA;
    }
    if ZEROED.iter().any(|v| v.load(Ordering::Relaxed) != 0) {
        return EXIT_BAD_BSS;
    }
    // The data segment must also be writable (RW-, not R--).
    INITIALISED.store(7, Ordering::Relaxed);
    ZEROED[1023].store(7, Ordering::Relaxed);
    if INITIALISED.load(Ordering::Relaxed) != 7 || ZEROED[1023].load(Ordering::Relaxed) != 7 {
        return EXIT_NOT_WRITABLE;
    }
    println!("statics ok, exiting with {}", EXIT_OK);
    EXIT_OK
}
