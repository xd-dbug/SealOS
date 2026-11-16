#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #52: the keyboard input buffer. Everything goes in through the same push
//! path the keyboard ISR uses (`push_key`, or `inject` for raw bytes) and
//! comes out through `input::read`, the call `SYS_READ` makes. No PS/2 device
//! is involved: headless QEMU sends no keys, so the buffer holds only what
//! each test puts there.

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::BOOTLOADER_CONFIG;
use kernel::input::{self, BACKSPACE, CAPACITY};
use pc_keyboard::{DecodedKey, KeyCode};

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
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

/// A byte that never repeats within one lap of the buffer, so a byte read
/// from the wrong slot can't match by accident (251 is prime, `CAPACITY`
/// isn't a multiple of it).
fn pattern(i: usize) -> u8 {
    (i % 251) as u8
}

#[test_case]
fn empty_buffer_reads_nothing() {
    input::clear();
    assert_eq!(input::len(), 0);
    let mut out = [0xaa; 8];
    assert_eq!(input::read(&mut out), 0);
    assert_eq!(out, [0xaa; 8], "an empty read must not touch the caller's buffer");
}

#[test_case]
fn drained_buffer_never_returns_a_stale_byte() {
    input::clear();
    assert_eq!(input::inject(b"abc"), 3);
    let mut out = [0; 8];
    assert_eq!(input::read(&mut out), 3);
    assert_eq!(&out[..3], b"abc");
    // The bytes are still physically in the array; they must not come back.
    let mut again = [0xaa; 8];
    assert_eq!(input::read(&mut again), 0);
    assert_eq!(again, [0xaa; 8]);
    assert_eq!(input::len(), 0);
}

#[test_case]
fn bytes_come_out_in_order() {
    input::clear();
    assert_eq!(input::inject(b"first "), 6);
    assert_eq!(input::inject(b"second"), 6);
    assert_eq!(input::len(), 12);
    // A short read takes the oldest bytes and leaves the rest queued.
    let mut out = [0; 4];
    assert_eq!(input::read(&mut out), 4);
    assert_eq!(&out, b"firs");
    let mut rest = [0; 32];
    assert_eq!(input::read(&mut rest), 8);
    assert_eq!(&rest[..8], b"t second");
}

/// 100 bytes in, 100 out, ten times: 1000 bytes through a 256-byte ring, so
/// head and tail both wrap several times, at offsets that never line up with
/// the array's end.
#[test_case]
fn order_survives_wraparound() {
    input::clear();
    let mut next_in = 0;
    let mut next_out = 0;
    for _ in 0..10 {
        for _ in 0..100 {
            assert_eq!(input::inject(&[pattern(next_in)]), 1);
            next_in += 1;
        }
        let mut out = [0; 100];
        assert_eq!(input::read(&mut out), 100);
        for byte in out {
            assert_eq!(byte, pattern(next_out), "byte {} out of order", next_out);
            next_out += 1;
        }
    }
    assert_eq!(input::len(), 0);
}

/// The documented policy: a full buffer drops the *newest* input and keeps
/// what it already holds, without blocking.
#[test_case]
fn full_buffer_drops_the_newest_byte() {
    input::clear();
    // Start mid-array so "full" is reached across the wrap, not at index 0.
    assert_eq!(input::inject(&[0; 77]), 77);
    let mut skip = [0; 77];
    assert_eq!(input::read(&mut skip), 77);

    for i in 0..CAPACITY {
        assert_eq!(input::inject(&[pattern(i)]), 1, "byte {} refused before full", i);
    }
    assert_eq!(input::len(), CAPACITY);

    let dropped = input::dropped();
    assert_eq!(input::inject(b"XYZ"), 0, "a full buffer accepted input");
    assert_eq!(input::dropped(), dropped + 1, "inject stops at the first drop");
    assert_eq!(input::len(), CAPACITY);

    // Nothing already buffered was overwritten, the oldest byte included.
    let mut out = [0; CAPACITY];
    assert_eq!(input::read(&mut out), CAPACITY);
    for (i, byte) in out.into_iter().enumerate() {
        assert_eq!(byte, pattern(i), "byte {} was overwritten", i);
    }
    // And it accepts input again once there is room.
    assert_eq!(input::inject(b"ok"), 2);
    let mut ok = [0; 2];
    assert_eq!(input::read(&mut ok), 2);
    assert_eq!(&ok, b"ok");
}

/// A multi-byte key goes in whole or not at all: with one free slot, a
/// two-byte character is dropped rather than leaving half a UTF-8 sequence.
#[test_case]
fn multibyte_key_is_never_split() {
    input::clear();
    assert_eq!(input::inject(&[b'.'; CAPACITY - 1]), CAPACITY - 1);
    assert!(!input::push_key(DecodedKey::Unicode('é')), "half a character was buffered");
    assert_eq!(input::len(), CAPACITY - 1);
    assert!(input::push_key(DecodedKey::Unicode('!')), "a one-byte key still fits");
    assert_eq!(input::len(), CAPACITY);
    input::clear();
}

fn read_all(out: &mut [u8; 16]) -> &[u8] {
    let n = input::read(out);
    &out[..n]
}

/// Keys become the bytes a terminal program expects, through the function
/// the keyboard ISR calls.
#[test_case]
fn keys_map_to_terminal_bytes() {
    input::clear();
    let mut out = [0; 16];

    assert!(input::push_key(DecodedKey::Unicode('a')));
    assert_eq!(read_all(&mut out), b"a");

    // What `pc_keyboard` decodes Enter and Backspace to on the US layout.
    assert!(input::push_key(DecodedKey::Unicode('\n')));
    assert_eq!(read_all(&mut out), b"\n");
    assert!(input::push_key(DecodedKey::Unicode('\u{8}')));
    assert_eq!(read_all(&mut out), &[BACKSPACE]);

    // A carriage return is folded into the one line terminator.
    assert!(input::push_key(DecodedKey::Unicode('\r')));
    assert_eq!(read_all(&mut out), b"\n");

    // Printable Unicode arrives as its UTF-8 bytes.
    assert!(input::push_key(DecodedKey::Unicode('é')));
    assert_eq!(read_all(&mut out), "é".as_bytes());
    assert!(input::push_key(DecodedKey::Unicode('€')));
    assert_eq!(read_all(&mut out), "€".as_bytes());

    // Keys with no byte equivalent are dropped, and not counted as overflow.
    let dropped = input::dropped();
    assert!(!input::push_key(DecodedKey::RawKey(KeyCode::ArrowUp)));
    assert!(!input::push_key(DecodedKey::RawKey(KeyCode::F1)));
    assert_eq!(input::len(), 0);
    assert_eq!(input::dropped(), dropped);
}
