#![no_std]
#![no_main]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::in_interrupt;
use kernel::{exit_qemu, serial_print, serial_println, QemuExitCode};

/// Unlike `page_fault.rs`, this keeps the kernel's *real* IDT: the point is
/// that `interrupts::general_protection_fault_handler` exists and classifies
/// the fault, not just that the CPU raises `#GP`. Before that handler existed
/// this test escalated to a double fault.
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

    serial_print!("general_protection::kernel_mode_gp_is_caught...\t");
    // Bits 48..64 must all copy bit 47; this address breaks that rule, and a
    // non-canonical (non-stack) memory reference raises #GP(0), not #PF.
    // Inline asm rather than `read_volatile`: dereferencing an address we
    // don't own is UB in Rust even if it reliably faults, while to the
    // compiler this is just an opaque instruction. The address goes through a
    // general register, not rsp/rbp, so the fault is #GP(0) and not #SS(0).
    let non_canonical: u64 = 0x8000_0000_0000_0000;
    // Safety: the load never completes; it faults before reading anything.
    unsafe {
        core::arch::asm!(
            "mov {tmp}, qword ptr [{addr}]",
            addr = in(reg) non_canonical,
            tmp = out(reg) _,
            options(nostack, readonly),
        );
    }

    serial_println!("[failed: non-canonical read did not fault]");
    exit_qemu(QemuExitCode::Failed);
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &kernel::BOOTLOADER_CONFIG);

/// Only the handler's kernel-mode panic counts, raised from inside the handler.
/// Any other panic (a failed `init`, or the fault misclassified as user mode)
/// fails, and a missing handler shows up as a double-fault panic.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let expected = info
        .message()
        .as_str()
        .is_some_and(|m| m.contains("GENERAL PROTECTION FAULT in kernel mode"));
    if expected && in_interrupt() {
        serial_println!("[ok]");
        exit_qemu(QemuExitCode::Success);
    } else {
        serial_println!("[failed]\n");
        serial_println!("Error: {}\n", info);
        exit_qemu(QemuExitCode::Failed);
    }
    loop {}
}
