#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #57, the W6 deliverable: the embedded shell, at Ring 3, driven by nothing
//! but input. Keys are scripted through `input::inject` (the keyboard ISR's
//! own push path) and everything the shell writes is recorded by the
//! `SYS_WRITE` tap, so the transcript below is exactly what a user would see
//! on screen, their own typing (echoed by the shell, not the kernel)
//! included.
//!
//! Preemption is off and the shell is the only other task, so one
//! `yield_now` from here runs it until it next blocks in `read` with the
//! input buffer empty. That makes "inject, yield, look" a deterministic step.

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::allocator::used_bytes;
use kernel::interrupts::user_faults;
use kernel::programs::{SHELL, SHELL_PROMPT};
use kernel::scheduler::{finished_count, ready_count, set_preemption, spawn, yield_now};
use kernel::syscall::{last_exit_code, set_write_tap, user_exits};
use kernel::user::spawn_user_elf;
use kernel::{BOOTLOADER_CONFIG, input};
use spin::Mutex;

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("BOOTLOADER_CONFIG enables physical memory mapping");
    let memory_regions = &boot_info.memory_regions;
    if let Some(framebuffer) = boot_info.framebuffer.as_mut() {
        kernel::framebuffer::init(framebuffer);
    }
    kernel::init(physical_memory_offset, memory_regions);
    set_preemption(false);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

/// Everything the shell wrote, concatenated. Fixed-size so the tap never
/// allocates (the heap check below is exact); a write that doesn't fit sets
/// the overflow flag instead of being cut short unnoticed.
struct Transcript {
    bytes: [u8; 1024],
    len: usize,
    overflowed: bool,
}

static TRANSCRIPT: Mutex<Transcript> =
    Mutex::new(Transcript { bytes: [0; 1024], len: 0, overflowed: false });

fn record(bytes: &[u8]) {
    let mut transcript = TRANSCRIPT.lock();
    let start = transcript.len;
    match transcript.bytes.get_mut(start..start + bytes.len()) {
        Some(slot) => {
            slot.copy_from_slice(bytes);
            transcript.len += bytes.len();
        }
        None => transcript.overflowed = true,
    }
}

/// Asserts the transcript so far is exactly `expected`.
fn assert_transcript(expected: &[&str], step: &str) {
    let transcript = TRANSCRIPT.lock();
    assert!(!transcript.overflowed, "transcript buffer too small");
    let got = core::str::from_utf8(&transcript.bytes[..transcript.len]).expect("shell wrote non-UTF-8");
    let mut rest = got;
    for part in expected {
        match rest.strip_prefix(part) {
            Some(tail) => rest = tail,
            None => panic!("after {}: expected {:?} next, transcript is {:?}", step, part, got),
        }
    }
    assert!(rest.is_empty(), "after {}: unexpected extra output {:?}", step, rest);
}

/// Types `keys` and lets the shell run until it has consumed them all and is
/// blocked in `read` again (or has exited).
fn type_keys(keys: &[u8]) {
    assert_eq!(input::inject(keys), keys.len(), "input buffer overflowed");
    yield_now();
    assert_eq!(input::len(), 0, "the shell stopped reading with input left");
}

extern "C" fn does_nothing() {}

/// What erasing one character looks like on the wire: back, blank, back.
const RUB_OUT: &str = "\x08 \x08";

const HELP: &str = "built-in commands:\n\
\x20 help         list the built-in commands\n\
\x20 echo [args]  print the arguments\n\
\x20 uptime       time since boot\n\
\x20 exit [code]  leave the shell (default code 0)\n";

#[test_case]
fn scripted_session_matches_the_expected_transcript() {
    // Warm-up so one-time allocations aren't counted in the baseline.
    spawn(does_nothing).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    let baseline = used_bytes();
    let faults = user_faults();
    let exits = user_exits();
    input::clear();
    set_write_tap(Some(record));

    // The already-buffered path: a whole line is waiting before the shell
    // exists, so its first `read` returns at once. The doubled space checks
    // `echo` joins its arguments with single ones.
    assert_eq!(input::inject(b"echo hello  world\n"), 18);
    spawn_user_elf(SHELL).expect("shell failed to load");
    yield_now();
    assert_eq!(input::len(), 0);
    assert_eq!(ready_count(), 1, "the shell ended instead of waiting for input");
    let mut expected = [""; 32];
    let mut n = 0;
    let mut expect = |parts: &[&'static str], step: &str| {
        for part in parts {
            expected[n] = part;
            n += 1;
        }
        assert_transcript(&expected[..n], step);
    };
    expect(
        &[SHELL_PROMPT, "echo hello  world", "\n", "hello world\n", SHELL_PROMPT],
        "the pre-typed echo",
    );

    // The blocking path: the shell is parked in `read` with nothing
    // buffered. More yields change nothing; it neither returns nor prints.
    for _ in 0..5 {
        yield_now();
    }
    expect(&[], "idling at the prompt");

    // A typo fixed with backspace, arriving in two pieces so the shell
    // blocks mid-line as well: "eco", rub out the "o", then "ho fixed".
    type_keys(b"ec");
    expect(&["ec"], "half a word");
    type_keys(b"o\x08ho fixed\n");
    expect(&["o", RUB_OUT, "ho fixed", "\n", "fixed\n", SHELL_PROMPT], "the corrected command");

    // Backspace on an empty line must not rub out the prompt.
    type_keys(b"\x08\x08");
    expect(&[], "backspace on an empty line");

    // An unknown command names itself in one error line; the prompt returns.
    type_keys(b"frobnicate now\n");
    expect(
        &["frobnicate now", "\n", "frobnicate: command not found\n", SHELL_PROMPT],
        "an unknown command",
    );

    // An empty line just prompts again.
    type_keys(b"   \n");
    expect(&["   ", "\n", SHELL_PROMPT], "a blank line");

    type_keys(b"help\n");
    expect(&["help", "\n", HELP, SHELL_PROMPT], "help");

    // `exit 7` ends the task: the line is echoed, and no prompt follows.
    type_keys(b"exit 7\n");
    while ready_count() > 0 {
        yield_now();
    }
    set_write_tap(None);
    expect(&["exit 7", "\n"], "exit");

    assert_eq!(last_exit_code(), 7, "101 means the shell panicked");
    assert_eq!(user_exits(), exits + 1);
    assert_eq!(user_faults(), faults, "a clean exit is not a fault");
    assert_eq!(finished_count(), 0);
    assert_eq!(used_bytes(), baseline, "the shell's stack was not reaped");
}
