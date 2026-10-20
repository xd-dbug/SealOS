#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::interrupts::{in_interrupt, ticks, user_faults};
use kernel::scheduler::{finished_count, ready_count, yield_now};
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, user_program};

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

/// Uses the kernel's *real* IDT: the user-mode #GP must kill the user task and
/// hand control back, with preemption on. The tick budget turns a hang (no
/// fault, or a fault that never returns here) into a failure.
#[test_case]
fn privileged_instruction_kills_user_task() {
    assert_eq!(user_faults(), 0);
    spawn_user(privileged_program()).expect("spawn_user failed");

    let deadline = ticks() + 300;
    while user_faults() == 0 && ticks() < deadline {
        x86_64::instructions::hlt();
    }
    assert_eq!(user_faults(), 1, "no #GP within the tick budget");

    // Safety: the user stack page is mapped and the kernel reads it at CPL 0.
    assert_eq!(unsafe { MARKER.read_volatile() }, RING3_CONFIRMED, "program never ran at CPL 3");

    // The boot task is running again with its own ISR depth, and the dead
    // task is gone from the run queue and reapable.
    assert!(!in_interrupt());
    assert_eq!(ready_count(), 0);
    yield_now();
    assert_eq!(finished_count(), 0);
}
