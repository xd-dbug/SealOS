#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use kernel::interrupts::ticks;
use kernel::scheduler::spawn;
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{user_program, BOOTLOADER_CONFIG};
use x86_64::registers::control::{Cr4, Cr4Flags};

// Counts in rax and publishes every value to the top of its own stack page,
// where the kernel can read it through the shared mapping. It never yields and
// never leaves Ring 3, so the only way the kernel regains the CPU is a timer
// interrupt taken from Ring 3. rax must survive each preemption: if it were
// corrupted the published count would jump backwards or sideways.
user_program!(
    counter_program,
    "
    xor eax, eax
2:  inc rax
    mov [rsp - 8], rax
    jmp 2b
    "
);

const COUNTER: *const u64 = (USER_STACK_TOP - 8) as *const u64;

/// Ticks to observe, i.e. preempt-and-resume round trips of the user task.
const OBSERVATIONS: u64 = 10;
/// Hard stop (15 s at 100 Hz) so a hang fails instead of stalling the harness.
const BUDGET_TICKS: u64 = 1500;

static KERNEL_WORK: AtomicU64 = AtomicU64::new(0);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("BOOTLOADER_CONFIG enables physical memory mapping");
    let memory_regions = &boot_info.memory_regions;
    if let Some(framebuffer) = boot_info.framebuffer.as_mut() {
        kernel::framebuffer::init(framebuffer);
    }
    // Preemption stays at its post-`init` default (on): it is under test.
    kernel::init(physical_memory_offset, memory_regions);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

extern "C" fn kernel_worker() {
    loop {
        KERNEL_WORK.fetch_add(1, Relaxed);
        core::hint::spin_loop();
    }
}

/// The kernel reads user pages directly in this test (and `ring3_gp`), which
/// only works with SMAP off. QEMU's default CPU model has no SMAP; if that
/// ever changes, reads like these need `stac`/`clac`.
#[test_case]
fn smap_is_off() {
    assert!(!Cr4::read().contains(Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION));
}

#[test_case]
fn ring3_task_is_preempted_and_resumed() {
    spawn(kernel_worker).expect("spawn kernel worker");
    spawn_user(counter_program()).expect("spawn_user failed");

    let start = ticks();
    let mut last_count = 0;
    let mut last_work = 0;
    let mut seen = 0;
    let mut last_tick = start;
    while seen < OBSERVATIONS {
        assert!(ticks() - start < BUDGET_TICKS, "timed out after {} observations", seen);
        // Sleeps until a tick; the tick preempts this task, and getting here
        // again means a later tick preempted the other tasks in turn.
        x86_64::instructions::hlt();
        if ticks() == last_tick {
            continue;
        }
        last_tick = ticks();

        // Safety: mapped by `spawn_user`; read at CPL 0 with SMAP off.
        let count = unsafe { COUNTER.read_volatile() };
        let work = KERNEL_WORK.load(Relaxed);
        // Once the user task has started it must have run since the last
        // sample; before its first slice both are 0 and we just wait.
        if seen > 0 || count > 0 {
            assert!(count > last_count, "user task made no progress: {} -> {}", last_count, count);
            assert!(work > last_work, "kernel worker made no progress");
            seen += 1;
        }
        last_count = count;
        last_work = work;
    }
}
