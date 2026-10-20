#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::allocator::HEAP_START;
use kernel::interrupts::{InterruptIndex, PICS};
use kernel::user::spawn_user;
use kernel::{exit_qemu, serial_print, serial_println, user_program, QemuExitCode};
use lazy_static::lazy_static;
use x86_64::PrivilegeLevel;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

// Reads the kernel heap, which is mapped and present but supervisor-only: the
// fault must be a protection violation (P=1), not a not-present one. The
// address is asserted equal to `HEAP_START` below.
user_program!(
    read_kernel_program,
    "
    movabs rax, 0xffffc00000000000
    mov rbx, [rax]
    ud2
    "
);

lazy_static! {
    /// The real `#PF` handler halts, so this test swaps in an IDT whose `#PF`
    /// handler checks the error code and exits. Timer and double fault are
    /// carried over because both can arrive while Ring 3 runs.
    static ref TEST_IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.page_fault.set_handler_fn(test_page_fault_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(test_double_fault_handler)
                .set_stack_index(kernel::gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt[InterruptIndex::Timer as u8].set_handler_fn(test_timer_handler);
        idt
    };
}

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
    assert_eq!(HEAP_START as u64, 0xffff_c000_0000_0000, "update the program's literal");

    serial_print!("ring3_page_fault::user_read_of_kernel_memory_faults...\t");
    // Loaded after `init` (the real IDT is installed there). Preemption off:
    // the test timer handler only acknowledges ticks.
    kernel::scheduler::set_preemption(false);
    TEST_IDT.load();
    spawn_user(read_kernel_program()).expect("spawn_user failed");
    // The user task never yields back; the fault handler ends the test.
    loop {
        kernel::scheduler::yield_now();
    }
}

bootloader_api::entry_point!(test_kernel_main, config = &kernel::BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

extern "x86-interrupt" fn test_page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let from_ring3 = stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3;
    let expected = PageFaultErrorCode::USER_MODE | PageFaultErrorCode::PROTECTION_VIOLATION;
    let addr = Cr2::read_raw();
    if from_ring3 && error_code == expected && addr == HEAP_START as u64 {
        serial_println!("[ok]");
        exit_qemu(QemuExitCode::Success);
    } else {
        serial_println!(
            "[failed: ring3 {}, error {:?}, cr2 {:#x}]",
            from_ring3, error_code, addr
        );
        exit_qemu(QemuExitCode::Failed);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn test_double_fault_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("[failed: double fault]");
    exit_qemu(QemuExitCode::Failed);
    loop {
        x86_64::instructions::hlt();
    }
}

/// Without the EOI the PIC would stop delivering ticks; nothing else is needed
/// since preemption is off.
extern "x86-interrupt" fn test_timer_handler(_stack_frame: InterruptStackFrame) {
    // Safety: IF is 0 in a gate, and mainline only locks PICS with IF=0.
    unsafe { PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer as u8) };
}
