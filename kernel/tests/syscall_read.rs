#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #53: `SYS_READ`. One Ring 3 program makes every call and stores each
//! result on its stack for the kernel to check afterwards.
//!
//! "hello" is injected before the program starts and stays buffered through
//! all the bad-pointer calls. The good reads that follow then get exactly
//! "hello", in order, which is the proof that a rejected read consumed
//! nothing.

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::user_faults;
use kernel::scheduler::{finished_count, ready_count, set_preemption, spawn, yield_now};
use kernel::syscall::{SyscallError, last_exit_code, user_exits};
use kernel::user::{USER_CODE_START, USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, input, user_program};

// rbx = the initial RSP = `USER_STACK_TOP`. Layout below it:
//   [rbx - 8]        sentinel, the tail of the range call 4 runs off the end of
//   [rbx - 8 - 8n]   result of call n
//   [rbx - 512 ..]   three 64-byte buffers (A at -512, B at -448, C at -384)
//
// Calls, in order:
//   1  kernel heap address (mapped, supervisor-only)
//   2  non-canonical address
//   3  the program's own code page (user-readable, not writable)
//   4  starts in the last stack page, runs past the top into unmapped memory
//   5  a len that wraps the address space
//   6  len 0 with a bad pointer: 0, without blocking
//   7  buffer A, len 2: fewer bytes than are buffered
//   8  buffer B, len 64: more than are buffered, returns what is there
//   9  (marker 0x600d: everything above returned)
//   10 buffer C, len 64, with nothing buffered: blocks until input arrives
// then exit(0).
user_program!(
    read_program,
    "
    mov rbx, rsp
    movabs rax, 0x5a5a5a5a5a5a5a5a
    mov [rbx - 8], rax

    mov eax, 4
    movabs rdi, 0xffffc00000000000
    mov esi, 8
    syscall
    mov [rbx - 16], rax

    mov eax, 4
    movabs rdi, 0xdead000000000000
    mov esi, 8
    syscall
    mov [rbx - 24], rax

    mov eax, 4
    movabs rdi, 0x0000008000000000
    mov esi, 8
    syscall
    mov [rbx - 32], rax

    mov eax, 4
    lea rdi, [rbx - 4]
    mov esi, 8
    syscall
    mov [rbx - 40], rax

    mov eax, 4
    lea rdi, [rbx - 512]
    mov rsi, -1
    syscall
    mov [rbx - 48], rax

    mov eax, 4
    movabs rdi, 0xffffc00000000000
    xor esi, esi
    syscall
    mov [rbx - 56], rax

    mov eax, 4
    lea rdi, [rbx - 512]
    mov esi, 2
    syscall
    mov [rbx - 64], rax

    mov eax, 4
    lea rdi, [rbx - 448]
    mov esi, 64
    syscall
    mov [rbx - 72], rax

    mov qword ptr [rbx - 80], 0x600d

    mov eax, 4
    lea rdi, [rbx - 384]
    mov esi, 64
    syscall
    mov [rbx - 88], rax

    mov eax, 2
    xor edi, edi
    syscall
2:
    pause
    jmp 2b
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
    // Switches only at yields and exits, so "the program is blocked in
    // `read`" is a state the test can observe, not a race.
    set_preemption(false);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

fn user_result(n: u64) -> u64 {
    // Safety: the user stack stays mapped until the next load, and the
    // kernel reads it at CPL 0.
    unsafe { ((USER_STACK_TOP - 8 - 8 * n) as *const u64).read_volatile() }
}

fn user_bytes(below_top: u64, len: usize) -> &'static [u8] {
    // Safety: as above; the program has exited (or is blocked in a syscall)
    // whenever this is called, so nothing is writing the stack.
    unsafe { core::slice::from_raw_parts((USER_STACK_TOP - below_top) as *const u8, len) }
}

/// Runs as its own task, as the issue asks: the blocked reader is woken by
/// input from *another* task, not by anything it did itself.
extern "C" fn typist() {
    assert_eq!(input::inject(b"ok"), 2);
}

#[test_case]
fn read_fills_writable_buffers_and_rejects_the_rest() {
    let faults = user_faults();
    let exits = user_exits();
    input::clear();
    assert_eq!(input::inject(b"hello"), 5);

    spawn_user(read_program()).expect("spawn_user failed");
    // Each yield runs the program until it yields back, which it only does
    // from inside a `read` that found nothing. Several rounds prove it stays
    // there instead of returning 0.
    for _ in 0..10 {
        yield_now();
        assert_eq!(ready_count(), 1, "the program ended instead of blocking");
    }
    assert_eq!(user_result(9), 0x600d, "program did not reach the blocking read");
    assert_eq!(user_result(10), 0, "the blocking read returned with no input");
    assert_eq!(input::len(), 0, "the good reads did not drain the buffer");

    let bad = SyscallError::BadAddress.to_return_value();
    let cases = [
        "kernel address",
        "non-canonical address",
        "read-only code page",
        "range runs past the stack top",
        "len wraps",
    ];
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(user_result(i as u64 + 1), bad, "{}", case);
    }
    assert_eq!(user_result(6), 0, "len 0 reads nothing, whatever the pointer");

    // No rejected read consumed input: all of "hello" was still there, in
    // order, for the two good ones.
    assert_eq!(user_result(7), 2, "read of 2 with 5 buffered");
    assert_eq!(user_bytes(512, 2), b"he");
    assert_eq!(user_result(8), 3, "read of 64 with 3 buffered");
    assert_eq!(user_bytes(448, 3), b"llo");
    // Only the bytes reported were written.
    assert_eq!(user_bytes(512, 64)[2..], [0; 62]);
    assert_eq!(user_bytes(448, 64)[3..], [0; 61]);

    // No rejected read wrote anything: the code page still holds exactly the
    // program, and the part of the straddling range that *is* writable (the
    // last 4 bytes of the stack) still holds the sentinel.
    let program = read_program();
    // Safety: the code page is mapped (the task is alive) and readable.
    let code = unsafe { core::slice::from_raw_parts(USER_CODE_START as *const u8, program.len()) };
    assert_eq!(code, program, "a read into the code page rewrote it");
    assert_eq!(user_bytes(8, 8), [0x5a; 8], "a rejected read wrote part of its range");

    // Now wake it from another task.
    spawn(typist).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    assert_eq!(user_result(10), 2, "the blocked read did not return the injected bytes");
    assert_eq!(user_bytes(384, 2), b"ok");
    assert_eq!(input::len(), 0);

    assert_eq!(user_exits(), exits + 1);
    assert_eq!(last_exit_code(), 0);
    assert_eq!(user_faults(), faults, "no call may fault the program");
    assert_eq!(finished_count(), 0);
}
