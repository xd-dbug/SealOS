#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use kernel::BOOTLOADER_CONFIG;
use kernel::gdt;
use kernel::interrupts::ticks;
use kernel::scheduler::{current_stack_top, ready_count, set_preemption, spawn, yield_now};
use x86_64::PrivilegeLevel;
use x86_64::instructions::hlt;

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

const WORKERS: usize = 3;
/// Hard stop for any wait: 15 s at 100 Hz, so a hang fails instead of stalling.
const WAIT_BUDGET_TICKS: u64 = 1500;
/// Ticks each preempted worker keeps checking RSP0 for; enough for every
/// worker to be switched in and out several times.
const CHECK_TICKS: u64 = 10;

const ZERO: AtomicU64 = AtomicU64::new(0);
/// Times a worker saw `gdt::rsp0()` differ from its own stack top.
static MISMATCHES: [AtomicU64; WORKERS] = [ZERO; WORKERS];
/// Times a worker checked at all, so a worker that never ran can't pass.
static CHECKS: [AtomicU64; WORKERS] = [ZERO; WORKERS];
/// Each worker's stack top, to show the three differ.
static TOPS: [AtomicU64; WORKERS] = [ZERO; WORKERS];

fn reset() {
    for i in 0..WORKERS {
        MISMATCHES[i].store(0, Relaxed);
        CHECKS[i].store(0, Relaxed);
        TOPS[i].store(0, Relaxed);
    }
}

fn check(idx: usize, own_top: u64) {
    CHECKS[idx].fetch_add(1, Relaxed);
    if gdt::rsp0().as_u64() != own_top {
        MISMATCHES[idx].fetch_add(1, Relaxed);
    }
}

fn own_top(idx: usize) -> u64 {
    let top = current_stack_top().expect("workers have heap stacks").as_u64();
    TOPS[idx].store(top, Relaxed);
    top
}

/// Checks before and after every yield: RSP0 must have followed the switch.
fn coop_worker(idx: usize) {
    let top = own_top(idx);
    for _ in 0..3 {
        check(idx, top);
        yield_now();
        check(idx, top);
    }
}

/// Never yields: every switch away is a timer preemption.
fn preempted_worker(idx: usize) {
    let top = own_top(idx);
    let start = ticks();
    while ticks() - start < CHECK_TICKS {
        check(idx, top);
    }
}

macro_rules! entries {
    ($f:ident, $a:ident, $b:ident, $c:ident) => {
        extern "C" fn $a() { $f(0); }
        extern "C" fn $b() { $f(1); }
        extern "C" fn $c() { $f(2); }
    };
}
entries!(coop_worker, coop0, coop1, coop2);
entries!(preempted_worker, pre0, pre1, pre2);

fn assert_all_followed() {
    for i in 0..WORKERS {
        assert!(CHECKS[i].load(Relaxed) > 0, "worker {} never ran", i);
        assert_eq!(MISMATCHES[i].load(Relaxed), 0, "worker {} saw a stale RSP0", i);
    }
    let (a, b, c) = (TOPS[0].load(Relaxed), TOPS[1].load(Relaxed), TOPS[2].load(Relaxed));
    assert!(a != b && b != c && a != c, "workers share a stack top");
    for t in [a, b, c] {
        assert_eq!(t % 16, 0);
    }
}

#[test_case]
fn test_selectors_indices_and_rpls() {
    // Order is what `sysret` will need: null, kcode, kdata, udata, ucode, tss.
    let kc = gdt::kernel_code_selector();
    let kd = gdt::kernel_data_selector();
    let ud = gdt::user_data_selector();
    let uc = gdt::user_code_selector();
    assert_eq!((kc.index(), kd.index(), ud.index(), uc.index()), (1, 2, 3, 4));
    assert_eq!(kc.rpl(), PrivilegeLevel::Ring0);
    assert_eq!(kd.rpl(), PrivilegeLevel::Ring0);
    assert_eq!(ud.rpl(), PrivilegeLevel::Ring3);
    assert_eq!(uc.rpl(), PrivilegeLevel::Ring3);
    // sysret: SS = base + 8, CS = base + 16 (raw selector values, base = 0x10).
    assert_eq!(uc.0, ud.0 + 8);
}

#[test_case]
fn test_segment_registers_loaded_with_kernel_data() {
    use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
    assert_eq!(SS::get_reg(), gdt::kernel_data_selector());
    assert_eq!(DS::get_reg(), gdt::kernel_data_selector());
    assert_eq!(ES::get_reg(), gdt::kernel_data_selector());
    assert_eq!(CS::get_reg(), gdt::kernel_code_selector());
}

#[test_case]
fn test_rsp0_roundtrip() {
    let saved = gdt::rsp0();
    x86_64::instructions::interrupts::without_interrupts(|| {
        gdt::set_rsp0(x86_64::VirtAddr::new(0xffff_c000_dead_0000));
        assert_eq!(gdt::rsp0().as_u64(), 0xffff_c000_dead_0000);
        gdt::set_rsp0(saved);
    });
}

#[test_case]
fn test_rsp0_follows_task_across_yield() {
    set_preemption(false);
    reset();
    for entry in [coop0, coop1, coop2] {
        spawn(entry).unwrap();
    }
    while ready_count() > 0 {
        yield_now();
    }
    assert_all_followed();
}

#[test_case]
fn test_rsp0_follows_task_across_preemption() {
    set_preemption(true);
    reset();
    for entry in [pre0, pre1, pre2] {
        spawn(entry).unwrap();
    }
    let start = ticks();
    while ready_count() > 0 {
        assert!(ticks() - start < WAIT_BUDGET_TICKS, "tasks did not finish in time");
        hlt();
    }
    // A preempted-away-from task never reaps on resume.
    yield_now();
    assert_all_followed();
}
