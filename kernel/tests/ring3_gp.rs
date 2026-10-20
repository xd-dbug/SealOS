#![no_std]
#![no_main]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::{in_interrupt, ticks};
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{exit_qemu, serial_print, serial_println, user_program, QemuExitCode};

// Proves the Ring 3 entry: the program checks its own CPL, records that it
// passed, then executes a privileged instruction. `hlt` is privileged, so at
// CPL 3 it raises #GP(0).
user_program!(
    privileged_program,
    "
    mov ax, cs
    and ax, 3
    cmp ax, 3
    jne 3f
    mov qword ptr [rsp - 8], 0x33
    hlt
    ud2
3:
    mov qword ptr [rsp - 8], 0xbad
    hlt
    ud2
    "
);

/// What the program leaves at the top of its stack: proof it saw CPL 3 before
/// the fault, so a #GP alone can't pass for a program that never ran in Ring 3.
const RING3_CONFIRMED: u64 = 0x33;
const MARKER: *const u64 = (USER_STACK_TOP - 8) as *const u64;

/// Uses the kernel's *real* IDT and observes the real `#GP` handler through
/// its classifying panic message (as `general_protection.rs` does), rather
/// than a test-local IDT that would copy handlers and test the copy.
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

    serial_print!("ring3_gp::privileged_instruction_faults_at_cpl3...\t");
    spawn_user(privileged_program()).expect("spawn_user failed");

    // Preemption is on, so the boot task gets control back from a user task
    // that fails to fault; the budget turns a hang into a failure.
    let deadline = ticks() + 300;
    while ticks() < deadline {
        x86_64::instructions::hlt();
    }
    serial_println!("[failed: no #GP within the tick budget]");
    exit_qemu(QemuExitCode::Failed);
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &kernel::BOOTLOADER_CONFIG);

/// Passes only for the handler's user-mode panic, raised inside the handler,
/// after the program confirmed it was at CPL 3.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let message = info.message().as_str().unwrap_or("");
    if message.contains("GENERAL PROTECTION FAULT in user mode") && in_interrupt() {
        // Safety: the user stack page is mapped and the kernel reads it at CPL 0.
        let marker = unsafe { MARKER.read_volatile() };
        if marker == RING3_CONFIRMED {
            serial_println!("[ok]");
            exit_qemu(QemuExitCode::Success);
        } else {
            serial_println!("[failed: program did not confirm CPL 3, marker {:#x}]", marker);
            exit_qemu(QemuExitCode::Failed);
        }
    } else {
        serial_println!("[failed]\n");
        serial_println!("Error: {}\n", info);
        exit_qemu(QemuExitCode::Failed);
    }
    loop {}
}
