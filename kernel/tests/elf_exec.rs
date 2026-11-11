#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #51, the W5 deliverable: a real compiled program, embedded in the kernel
//! image, is validated, loaded and run at Ring 3 to a clean exit. `hello`
//! itself checks its `.data` and `.bss` statics, so its exit code proves the
//! segment copy and the zeroing, not just the jump to `e_entry`.

extern crate alloc;

use alloc::vec::Vec;
use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::allocator::used_bytes;
use kernel::elf::ElfError;
use kernel::interrupts::user_faults;
use kernel::memory::MAPPER;
use kernel::programs::{HELLO, HELLO_EXIT_OK};
use kernel::scheduler::{finished_count, ready_count, set_preemption, spawn, yield_now};
use kernel::syscall::{last_exit_code, set_write_tap, user_exits};
use kernel::user::{SpawnUserError, USER_ELF_BASE, USER_STACK_START, spawn_user_elf};
use kernel::BOOTLOADER_CONFIG;
use spin::Mutex;
use x86_64::structures::paging::{Mapper, Page, Size4KiB};
use x86_64::VirtAddr;

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
    // Only one task runs user code; switches at yields and exits keep the
    // write order deterministic.
    set_preemption(false);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

/// Everything `hello` wrote, concatenated. Fixed-size so the tap never
/// allocates (the heap check below is exact).
static LOG: Mutex<([u8; 128], usize)> = Mutex::new(([0; 128], 0));

fn log(bytes: &[u8]) {
    let mut log = LOG.lock();
    let (buf, len) = &mut *log;
    let n = bytes.len().min(buf.len() - *len);
    buf[*len..*len + n].copy_from_slice(&bytes[..n]);
    *len += n;
}

fn is_mapped(addr: u64) -> bool {
    let mapper = MAPPER.get().unwrap().lock();
    mapper.translate_page(Page::<Size4KiB>::containing_address(VirtAddr::new(addr))).is_ok()
}

extern "C" fn does_nothing() {}

/// Runs first, while no image has been loaded in this binary, so "nothing
/// mapped" is checkable: the corrupted copy must fail validation before a
/// single page (segment or stack) is mapped, and spawn no task.
#[test_case]
fn corrupted_copy_is_rejected_before_mapping() {
    let mut bad: Vec<u8> = HELLO.into();
    // e_entry pointed into the (non-executable) data segment's region.
    bad[24..32].copy_from_slice(&(USER_ELF_BASE + 0x2000).to_le_bytes());
    let ready = ready_count();
    match spawn_user_elf(&bad) {
        Err(SpawnUserError::Elf(ElfError::EntryNotExecutable)) => {}
        other => panic!("corrupted ELF not rejected: {:?}", other),
    }
    assert_eq!(ready_count(), ready, "a task was spawned");
    assert!(!is_mapped(USER_ELF_BASE), "a segment was mapped");
    assert!(!is_mapped(USER_STACK_START), "the stack was mapped");
}

#[test_case]
fn hello_elf_runs_to_a_clean_exit() {
    // Warm-up so one-time allocations aren't counted in the baseline.
    spawn(does_nothing).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    let baseline = used_bytes();
    let faults = user_faults();
    let exits = user_exits();

    set_write_tap(Some(log));
    spawn_user_elf(HELLO).expect("hello failed to load");
    while ready_count() > 0 {
        yield_now();
    }
    set_write_tap(None);

    let log = LOG.lock();
    assert_eq!(
        core::str::from_utf8(&log.0[..log.1]).unwrap(),
        "hello from an ELF\nstatics ok, exiting with 42\n"
    );
    drop(log);
    assert_eq!(
        last_exit_code(),
        HELLO_EXIT_OK,
        "1 = bad .data, 2 = bad .bss, 3 = write failed, 4 = data not writable, 101 = panic"
    );
    assert_eq!(user_exits(), exits + 1);
    assert_eq!(user_faults(), faults, "a clean exit is not a fault");
    assert_eq!(finished_count(), 0);
    assert_eq!(used_bytes(), baseline, "the task's stack was not reaped");
}
