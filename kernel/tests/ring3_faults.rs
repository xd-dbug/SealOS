#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kernel::allocator::used_bytes;
use kernel::interrupts::{in_interrupt, ticks, user_faults};
use kernel::scheduler::{finished_count, ready_count, spawn, yield_now};
use kernel::user::spawn_user;
use kernel::{BOOTLOADER_CONFIG, user_program};

user_program!(privileged_program, "hlt\n ud2");

// Reads the kernel heap: mapped but supervisor-only, so a Ring 3 #PF.
user_program!(
    read_kernel_program,
    "
    movabs rax, 0xffffc00000000000
    mov rbx, [rax]
    ud2
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

static PROGRESS: AtomicU64 = AtomicU64::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

/// Never yields: it only advances because preemption keeps scheduling it,
/// which is what the test needs to see survive the faults.
extern "C" fn worker() {
    while !STOP.load(Ordering::Relaxed) {
        PROGRESS.fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn does_nothing() {}

fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = ticks() + 300;
    while !cond() {
        assert!(ticks() < deadline, "timed out waiting for {}", what);
        x86_64::instructions::hlt();
    }
}

/// Waits for the next fault, then for the worker to advance past it, then
/// reaps the dead user task so the (single) user image may be remapped.
fn survive_one_fault(expected_faults: u64) {
    wait_until("user fault", || user_faults() == expected_faults);
    let seen = PROGRESS.load(Ordering::Relaxed);
    wait_until("worker progress after the kill", || PROGRESS.load(Ordering::Relaxed) > seen);
    assert!(!in_interrupt());
    // The killed task is queued nowhere; the boot task's yield reaps it.
    yield_now();
    assert_eq!(finished_count(), 0);
}

#[test_case]
fn kernel_survives_two_privilege_violations() {
    // Warm-up so one-time allocations aren't counted in the baseline.
    spawn(does_nothing).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    let baseline = used_bytes();
    assert_eq!(user_faults(), 0);

    spawn(worker).unwrap();

    spawn_user(privileged_program()).unwrap();
    survive_one_fault(1); // #GP

    spawn_user(read_kernel_program()).unwrap();
    survive_one_fault(2); // #PF

    STOP.store(true, Ordering::Relaxed);
    while ready_count() > 0 {
        yield_now();
    }
    yield_now();
    assert_eq!(user_faults(), 2);
    assert_eq!(finished_count(), 0);
    assert_eq!(used_bytes(), baseline);
}
