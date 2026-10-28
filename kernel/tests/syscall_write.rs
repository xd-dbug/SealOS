#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::{in_interrupt, ticks, user_faults};
use kernel::scheduler::{finished_count, yield_now};
use kernel::syscall::{SyscallError, set_write_tap};
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, user_program};
use spin::Mutex;

const MESSAGE: &[u8] = b"hello from ring 3\n";

// The #40 proof: one good write, then every way the range check must refuse,
// each result stored for the kernel to check. The program keeps running
// through all of them (a kernel #PF would halt instead, a user #PF would kill
// it before the marker).
//
// Bad ranges, in order: the kernel heap (mapped, supervisor-only); a
// non-canonical address; the first address past the lower half; a len that
// wraps the address space; the unmapped gap page after the code; a range that
// starts in the code page and runs into the gap; a range that starts in the
// last stack page and runs past `USER_STACK_TOP`.
user_program!(
    write_program,
    "
    mov rbx, rsp
    mov eax, 1
    lea rdi, [rip + 2f]
    mov esi, 18
    syscall
    mov [rbx - 8], rax

    mov eax, 1
    movabs rdi, 0xffffc00000000000
    mov esi, 8
    syscall
    mov [rbx - 16], rax

    mov eax, 1
    movabs rdi, 0xdead000000000000
    mov esi, 8
    syscall
    mov [rbx - 24], rax

    mov eax, 1
    movabs rdi, 0x0000800000000000
    mov esi, 1
    syscall
    mov [rbx - 32], rax

    mov eax, 1
    movabs rdi, 0x0000008000000000
    mov rsi, -1
    syscall
    mov [rbx - 40], rax

    mov eax, 1
    movabs rdi, 0x0000008000001000
    mov esi, 1
    syscall
    mov [rbx - 48], rax

    mov eax, 1
    movabs rdi, 0x0000008000000ff8
    mov esi, 16
    syscall
    mov [rbx - 56], rax

    mov eax, 1
    lea rdi, [rbx - 4]
    mov esi, 8
    syscall
    mov [rbx - 64], rax

    mov qword ptr [rbx - 72], 0x600d
    hlt
2:
    .ascii \"hello from ring 3\\n\"
    "
);

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

fn user_result(n: u64) -> u64 {
    // Safety: the user stack page stays mapped after the task dies, and the
    // kernel reads it at CPL 0.
    unsafe { ((USER_STACK_TOP - 8 * n) as *const u64).read_volatile() }
}

/// Everything `SYS_WRITE` accepted, in order. Fixed-size so the tap never
/// allocates.
static CAPTURED: Mutex<([u8; 64], usize)> = Mutex::new(([0; 64], 0));

fn capture(bytes: &[u8]) {
    let mut captured = CAPTURED.lock();
    let (buf, len) = &mut *captured;
    let room = buf.len() - *len;
    let n = bytes.len().min(room);
    buf[*len..*len + n].copy_from_slice(&bytes[..n]);
    *len += n;
}

#[test_case]
fn write_accepts_user_buffer_and_rejects_bad_pointers() {
    assert_eq!(MESSAGE.len(), 18, "keep the program's `mov esi, 18` in sync");
    set_write_tap(Some(capture));
    spawn_user(write_program()).expect("spawn_user failed");

    let deadline = ticks() + 300;
    while user_faults() == 0 {
        assert!(ticks() < deadline, "user program never reached its final hlt");
        x86_64::instructions::hlt();
    }
    set_write_tap(None);
    assert_eq!(user_faults(), 1, "only the closing hlt may kill the program");

    assert_eq!(user_result(1), MESSAGE.len() as u64, "good write");
    let bad = SyscallError::BadAddress.to_return_value();
    let cases = [
        "kernel address",
        "non-canonical address",
        "first non-user address",
        "len wraps",
        "unmapped page",
        "range runs into unmapped page",
        "range runs past the stack top",
    ];
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(user_result(i as u64 + 2), bad, "{}", case);
    }
    assert_eq!(user_result(9), 0x600d, "program did not keep running");

    // Only the good buffer reached the output.
    let captured = CAPTURED.lock();
    assert_eq!(&captured.0[..captured.1], MESSAGE);
    drop(captured);

    assert!(!in_interrupt());
    yield_now();
    assert_eq!(finished_count(), 0);
}
