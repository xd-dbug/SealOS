#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kernel::interrupts::{in_interrupt, ticks, user_faults};
use kernel::scheduler::{current_stack_top, finished_count, yield_now};
use kernel::syscall::{SyscallError, set_write_tap};
use kernel::task::STACK_SIZE;
use kernel::user::{USER_STACK_TOP, spawn_user};
use kernel::{BOOTLOADER_CONFIG, user_program};

// The #38 proof: seeds every register the ABI promises to preserve, makes a
// syscall (an unknown number, so the only effect is the value in rax), then
// checks from Ring 3 that it is still at CPL 3 with RSP, DS and all of those
// registers intact. Results go to the top of its own stack for the kernel to
// read after `hlt` (#GP) ends the task.
user_program!(
    preserve_program,
    "
    mov rbx, rsp
    movabs rbp, 0x1111111111111111
    movabs r12, 0x2222222222222222
    movabs r13, 0x3333333333333333
    movabs r14, 0x4444444444444444
    movabs r15, 0x5555555555555555
    movabs rdi, 0x6666666666666666
    movabs rsi, 0x7777777777777777
    movabs rdx, 0x8888888888888888
    movabs r10, 0x9999999999999999
    movabs r8, 0xaaaaaaaaaaaaaaaa
    movabs r9, 0xbbbbbbbbbbbbbbbb
    mov eax, 0x7fff
    syscall
    mov [rbx - 8], rax
    cmp rsp, rbx
    jne 9f
    movabs rcx, 0x1111111111111111
    cmp rbp, rcx
    jne 9f
    movabs rcx, 0x2222222222222222
    cmp r12, rcx
    jne 9f
    movabs rcx, 0x3333333333333333
    cmp r13, rcx
    jne 9f
    movabs rcx, 0x4444444444444444
    cmp r14, rcx
    jne 9f
    movabs rcx, 0x5555555555555555
    cmp r15, rcx
    jne 9f
    movabs rcx, 0x6666666666666666
    cmp rdi, rcx
    jne 9f
    movabs rcx, 0x7777777777777777
    cmp rsi, rcx
    jne 9f
    movabs rcx, 0x8888888888888888
    cmp rdx, rcx
    jne 9f
    movabs rcx, 0x9999999999999999
    cmp r10, rcx
    jne 9f
    movabs rcx, 0xaaaaaaaaaaaaaaaa
    cmp r8, rcx
    jne 9f
    movabs rcx, 0xbbbbbbbbbbbbbbbb
    cmp r9, rcx
    jne 9f
    mov ecx, cs
    and ecx, 3
    cmp ecx, 3
    jne 9f
    mov ecx, ds
    cmp ecx, 0x1b
    jne 9f
    mov qword ptr [rbx - 16], 0x600d
    hlt
9:
    mov qword ptr [rbx - 16], 0xbad
    hlt
    "
);

// Enters with DF set and a 1-byte write, so the `SYS_WRITE` tap (which runs
// inside the syscall) can inspect the kernel-side state; then checks DF came
// back set, i.e. `sysretq` restored the user's own RFLAGS.
user_program!(
    context_program,
    "
    mov rbx, rsp
    std
    mov eax, 1
    lea rdi, [rip + 2f]
    mov esi, 1
    syscall
    pushfq
    pop rcx
    cld
    bt rcx, 10
    jnc 9f
    mov qword ptr [rbx - 8], 0x600d
    hlt
9:
    mov qword ptr [rbx - 8], 0xbad
    hlt
2:
    .ascii \"!\"
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

const PASSED: u64 = 0x600d;

/// Slot `n` (1-based) below the user stack top, where the programs leave results.
fn user_result(n: u64) -> u64 {
    // Safety: the user stack page stays mapped after the task dies (until the
    // next `map_user_program`), and the kernel reads it at CPL 0.
    unsafe { ((USER_STACK_TOP - 8 * n) as *const u64).read_volatile() }
}

/// Waits for the program's closing `hlt` to kill it, then reaps it so the
/// next test may remap the single user image.
fn wait_for_kill(expected_faults: u64) {
    let deadline = ticks() + 300;
    while user_faults() < expected_faults {
        assert!(ticks() < deadline, "user program never reached its final hlt");
        x86_64::instructions::hlt();
    }
    assert_eq!(user_faults(), expected_faults);
    assert!(!in_interrupt());
    yield_now();
    assert_eq!(finished_count(), 0);
}

#[test_case]
fn syscall_returns_to_ring3_with_registers_intact() {
    let faults = user_faults();
    spawn_user(preserve_program()).expect("spawn_user failed");
    wait_for_kill(faults + 1);
    assert_eq!(user_result(1), SyscallError::NoSuchSyscall.to_return_value(), "rax");
    assert_eq!(user_result(2), PASSED, "a register, RSP, CPL or DS changed across syscall");
}

static TAP_RAN: AtomicBool = AtomicBool::new(false);
static TAP_FAILURE: AtomicU64 = AtomicU64::new(0);

/// Runs inside `SYS_WRITE`, i.e. in syscall context, and records the first
/// expectation that failed (asserting here would panic mid-syscall, which
/// works but reports less clearly).
fn context_tap(bytes: &[u8]) {
    use x86_64::registers::rflags::{self, RFlags};
    use x86_64::registers::segmentation::{DS, ES, Segment};

    let kernel_data = kernel::gdt::kernel_data_selector();
    let rsp: u64;
    // Safety: reads rsp only.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)) };
    let top = current_stack_top().expect("syscall from the boot task").as_u64();
    let flags = rflags::read();
    let failure = if bytes != b"!" {
        1
    } else if DS::get_reg() != kernel_data || ES::get_reg() != kernel_data {
        2
    } else if !flags.contains(RFlags::INTERRUPT_FLAG) {
        3
    } else if flags.contains(RFlags::DIRECTION_FLAG) {
        4
    } else if in_interrupt() {
        5
    } else if !(top - STACK_SIZE as u64..top).contains(&rsp) {
        6
    } else {
        0
    };
    TAP_FAILURE.store(failure, Ordering::Relaxed);
    TAP_RAN.store(true, Ordering::Relaxed);
}

/// Syscall context is mainline, not interrupt context: no `IsrGuard`, IF
/// back on, kernel DS/ES, DF cleared by SFMASK, on the task's own kernel
/// stack. And the user's DF comes back on return.
#[test_case]
fn syscall_runs_as_mainline_on_task_kernel_stack() {
    let faults = user_faults();
    set_write_tap(Some(context_tap));
    spawn_user(context_program()).expect("spawn_user failed");
    wait_for_kill(faults + 1);
    set_write_tap(None);
    assert!(TAP_RAN.load(Ordering::Relaxed), "SYS_WRITE never reached the tap");
    let what = match TAP_FAILURE.load(Ordering::Relaxed) {
        0 => None,
        1 => Some("wrong bytes"),
        2 => Some("DS/ES not kernel data"),
        3 => Some("interrupts off"),
        4 => Some("DF set"),
        5 => Some("in interrupt context"),
        _ => Some("not on the task's kernel stack"),
    };
    assert_eq!(what, None, "syscall context");
    assert_eq!(user_result(1), PASSED, "user DF not restored by sysretq");
}
