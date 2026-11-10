#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #54: the framebuffer console as a user program's output device. No pixel
//! comparisons: the writer's cursor (in character cells) is checked after
//! text, a newline, backspaces and a backspace at column 0, and then after a
//! Ring 3 `SYS_WRITE`, which must move the same cursor.
//!
//! The test harness reports over serial only, so nothing but these tests
//! moves the cursor. Each test starts with a newline (column 0, some row) and
//! stays within a few rows, far from the clear-screen at the bottom.

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::framebuffer::{columns, cursor};
use kernel::scheduler::{ready_count, set_preemption, yield_now};
use kernel::syscall::user_exits;
use kernel::user::spawn_user;
use kernel::{BOOTLOADER_CONFIG, print, user_program};

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

fn at() -> (usize, usize) {
    cursor().expect("no framebuffer: QEMU was started without a display device")
}

/// Column 0 of a fresh row; returns that row.
fn fresh_line() -> usize {
    print!("\n");
    let (column, row) = at();
    assert_eq!(column, 0);
    row
}

#[test_case]
fn text_advances_one_cell_per_character() {
    let row = fresh_line();
    print!("abc");
    assert_eq!(at(), (3, row));
    print!("de");
    assert_eq!(at(), (5, row));
}

#[test_case]
fn newline_returns_to_column_zero_of_the_next_row() {
    let row = fresh_line();
    print!("abc\n");
    assert_eq!(at(), (0, row + 1));
    // A carriage return is ignored, as before.
    print!("ab\rc");
    assert_eq!(at(), (3, row + 1));
}

#[test_case]
fn backspace_moves_back_one_cell() {
    let row = fresh_line();
    print!("abcd\x08");
    assert_eq!(at(), (3, row));
    print!("\x08\x08");
    assert_eq!(at(), (1, row));
}

/// What the shell writes to erase a character: back, blank, back. Net effect
/// on the cursor is one cell left.
#[test_case]
fn rub_out_sequence_nets_one_cell_left() {
    let row = fresh_line();
    print!("abc\x08 \x08");
    assert_eq!(at(), (2, row));
    print!("X");
    assert_eq!(at(), (3, row));
}

/// The documented choice: at column 0 a backspace stays put. It neither
/// underflows nor climbs into the row above.
#[test_case]
fn backspace_at_column_zero_stays_put() {
    let row = fresh_line();
    print!("\x08");
    assert_eq!(at(), (0, row));
    print!("a\x08\x08\x08");
    assert_eq!(at(), (0, row));
    print!("b");
    assert_eq!(at(), (1, row));
}

/// Text that reaches the right edge wraps to the next row; a backspace there
/// (column 0 again) does not undo the wrap.
#[test_case]
fn backspace_does_not_cross_a_wrapped_line() {
    let row = fresh_line();
    let columns = columns().unwrap();
    for _ in 0..columns {
        print!("x");
    }
    // Exactly full: the wrap happens when the next glyph needs room.
    assert_eq!(at(), (columns, row));
    print!("y");
    assert_eq!(at(), (1, row + 1));
    print!("\x08\x08");
    assert_eq!(at(), (0, row + 1));
}

// write("hi!\x08", 4), then exit(0).
user_program!(
    write_program,
    "
    mov eax, 1
    lea rdi, [rip + 3f]
    mov esi, 4
    syscall
    mov eax, 2
    xor edi, edi
    syscall
2:
    pause
    jmp 2b
3:
    .ascii \"hi!\\x08\"
    "
);

/// `SYS_WRITE` reaches the framebuffer, not only serial: three glyphs and a
/// backspace from Ring 3 leave the console cursor two cells along.
#[test_case]
fn user_write_moves_the_console_cursor() {
    let row = fresh_line();
    let exits = user_exits();
    spawn_user(write_program()).expect("spawn_user failed");
    while ready_count() > 0 {
        yield_now();
    }
    assert_eq!(user_exits(), exits + 1, "the program did not exit cleanly");
    assert_eq!(at(), (2, row));
}
