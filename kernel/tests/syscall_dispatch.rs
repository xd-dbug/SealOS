#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::{in_interrupt, ticks, user_faults};
use kernel::scheduler::{finished_count, yield_now};
use kernel::syscall::SyscallError;
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, user_program};

// The #39 proof: unknown numbers (the reserved 0, the first unused one, and
// two far out of range) each come back as an error, and the program keeps
// running after every one of them to leave its marker.
user_program!(
    unknown_program,
    "
    mov rbx, rsp
    xor eax, eax
    syscall
    mov [rbx - 8], rax
    mov eax, 4
    syscall
    mov [rbx - 16], rax
    mov eax, 0x7fffffff
    syscall
    mov [rbx - 24], rax
    mov rax, -1
    syscall
    mov [rbx - 32], rax
    mov qword ptr [rbx - 40], 0x600d
    hlt
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

#[test_case]
fn unknown_syscall_returns_error_and_task_continues() {
    assert_eq!(user_faults(), 0);
    spawn_user(unknown_program()).expect("spawn_user failed");

    let deadline = ticks() + 300;
    while user_faults() == 0 {
        assert!(ticks() < deadline, "user program never reached its final hlt");
        x86_64::instructions::hlt();
    }
    // Exactly one kill: the closing `hlt`, not any of the syscalls.
    assert_eq!(user_faults(), 1);

    let no_such = SyscallError::NoSuchSyscall.to_return_value();
    assert_eq!(no_such as i64, -1);
    for n in 1..=4 {
        assert_eq!(user_result(n), no_such, "unknown syscall #{}", n);
    }
    assert_eq!(user_result(5), 0x600d, "program did not keep running");

    assert!(!in_interrupt());
    yield_now();
    assert_eq!(finished_count(), 0);
}
