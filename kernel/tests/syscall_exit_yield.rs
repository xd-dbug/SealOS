#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::allocator::used_bytes;
use kernel::interrupts::user_faults;
use kernel::scheduler::{finished_count, ready_count, set_preemption, spawn, yield_now};
use kernel::syscall::{last_exit_code, set_write_tap, user_exits};
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, user_program};
use spin::Mutex;

// The W4 deliverable: write, yield, write, exit, all from Ring 3. r12 is
// seeded before the yield and checked after it (another task ran in
// between); the yield's result is stored for the kernel. A failed check
// exits with 0xbad instead of 42.
user_program!(
    deliverable_program,
    "
    mov rbx, rsp
    mov eax, 1
    lea rdi, [rip + 2f]
    mov esi, 1
    syscall
    movabs r12, 0x5ea15ea15ea15ea1
    mov eax, 3
    syscall
    mov [rbx - 8], rax
    movabs rcx, 0x5ea15ea15ea15ea1
    cmp r12, rcx
    jne 9f
    mov ecx, cs
    and ecx, 3
    cmp ecx, 3
    jne 9f
    mov eax, 1
    lea rdi, [rip + 3f]
    mov esi, 1
    syscall
    mov eax, 2
    mov edi, 42
    syscall
    ud2
9:
    mov eax, 2
    mov edi, 0xbad
    syscall
    ud2
2:
    .ascii \"a\"
3:
    .ascii \"b\"
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
    // The interleaving below is exact only if switches happen solely at
    // yields and exits.
    set_preemption(false);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

/// Kernel task markers and user writes, in the order they happened.
/// Fixed-size so logging never allocates (the heap check below is exact).
static LOG: Mutex<([u8; 16], usize)> = Mutex::new(([0; 16], 0));

fn log(bytes: &[u8]) {
    let mut log = LOG.lock();
    let (buf, len) = &mut *log;
    for &b in bytes {
        buf[*len] = b;
        *len += 1;
    }
}

extern "C" fn kernel_task() {
    log(b"1");
    yield_now();
    log(b"2");
}

extern "C" fn does_nothing() {}

/// One user task interleaved with one kernel task (the single shared user
/// image rules out two user tasks). Expected run, with ready = [K, U]:
/// boot yields -> K logs 1, yields -> U writes "a", yields -> boot yields ->
/// K logs 2, returns -> U resumes in Ring 3, writes "b", exits(42) -> boot.
#[test_case]
fn write_yield_exit_from_ring3() {
    // Warm-up so one-time allocations aren't counted in the baseline.
    spawn(does_nothing).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    let baseline = used_bytes();
    let faults = user_faults();
    let exits = user_exits();

    set_write_tap(Some(log));
    spawn(kernel_task).unwrap();
    spawn_user(deliverable_program()).unwrap();
    while ready_count() > 0 {
        yield_now();
    }
    set_write_tap(None);

    let log = LOG.lock();
    assert_eq!(&log.0[..log.1], b"1a2b", "interleaving");
    drop(log);
    assert_eq!(user_exits(), exits + 1);
    assert_eq!(last_exit_code(), 42, "0xbad means a check in Ring 3 failed");
    assert_eq!(user_faults(), faults, "a clean exit is not a fault");
    // Safety: the user stack page stays mapped after the task exits.
    let yield_result = unsafe { ((USER_STACK_TOP - 8) as *const u64).read_volatile() };
    assert_eq!(yield_result, 0, "SYS_YIELD result");

    // The exit's `switch` landed in the boot task's `yield_now`, which reaped.
    assert_eq!(finished_count(), 0);
    assert_eq!(used_bytes(), baseline, "a stack was not reaped");
}
