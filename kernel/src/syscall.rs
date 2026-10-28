//! The Ring 3 -> Ring 0 system call path (`syscall`/`sysret`) and the one
//! place the syscall ABI is defined.
//!
//! # ABI
//!
//! | register | on `syscall`                | on return                  |
//! |----------|-----------------------------|----------------------------|
//! | `rax`    | syscall number (`SYS_*`)    | result, or an error code   |
//! | `rdi`    | argument 1                  | preserved                  |
//! | `rsi`    | argument 2                  | preserved                  |
//! | `rdx`    | argument 3                  | preserved                  |
//! | `r10`    | argument 4 (not `rcx`)      | preserved                  |
//! | `r8`     | argument 5                  | preserved                  |
//! | `r9`     | argument 6                  | preserved                  |
//! | `rcx`    | -                           | clobbered (CPU: user RIP)  |
//! | `r11`    | -                           | clobbered (CPU: user RFLAGS) |
//! | others   | -                           | preserved                  |
//!
//! Argument 4 is in `r10` rather than SysV's `rcx` because the `syscall`
//! instruction itself overwrites `rcx` with the return address.
//!
//! Results: a value with the top bit clear is success. Errors come back as
//! the negated `SyscallError` code (`-1`, `-2`, ...), so a user program checks
//! `rax as i64 < 0`. No syscall's success value can reach `1 << 63`.
//!
//! | number | name              | arguments  | returns          |
//! |--------|-------------------|------------|------------------|
//! | 0      | (reserved)        | -          | `NoSuchSyscall`  |
//! | 1      | `SYS_WRITE`       | ptr, len   | bytes written    |
//! | 2      | `SYS_EXIT`        | code       | does not return  |
//! | 3      | `SYS_YIELD`       | -          | 0                |
//!
//! 0 is reserved so a program that forgot to set `rax` (fresh tasks start with
//! every GPR zeroed, see `user::enter_user`) gets an error, not a write.

use crate::user::USER_SPACE_END;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_YIELD: u64 = 3;

/// Why a syscall failed. Returned to Ring 3 as `-(code)`; never a panic or a
/// kill, since a bad request is the user program's problem, not the kernel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SyscallError {
    /// `rax` named no syscall.
    NoSuchSyscall = 1,
    /// A pointer argument wasn't wholly inside mapped, user-accessible memory.
    BadAddress = 2,
}

impl SyscallError {
    /// The `rax` value Ring 3 sees for this error.
    pub const fn to_return_value(self) -> u64 {
        (self as u64).wrapping_neg()
    }
}

/// RFLAGS bits `syscall` clears on entry, before the stub's first instruction:
/// - IF: the stub runs on the user's RSP until it switches stacks, and an
///   interrupt there would push the CPU's frame onto it (Ring 0 -> Ring 0
///   takes no stack switch). Also guards the single `USER_RSP` scratch slot.
/// - TF: single-stepping would raise `#DB` after the first stub instruction,
///   again on the user's stack. There is no `#DB` handler either.
/// - DF: the SysV ABI promises DF=0 at every call, and compiled kernel code
///   (`memcpy`/`memset` from `compiler_builtins` use `rep movs`/`rep stos`)
///   relies on it without ever executing `cld`.
/// - AC: with SMAP on, a user-set AC would act as a standing `stac` and let
///   the kernel touch user memory unchecked (design-review item 26).
/// - NT: a leftover nested-task flag makes `iretq` raise `#GP` in long mode.
///
/// `sysretq` restores the user's own RFLAGS from R11, so none of this leaks
/// back to Ring 3.
const SYSCALL_FLAG_MASK: RFlags = RFlags::INTERRUPT_FLAG
    .union(RFlags::TRAP_FLAG)
    .union(RFlags::DIRECTION_FLAG)
    .union(RFlags::ALIGNMENT_CHECK)
    .union(RFlags::NESTED_TASK);

/// Top of the running task's kernel stack, where `syscall_entry` moves RSP.
/// The same value as TSS.RSP0 (`Scheduler::rotate` sets both), held here
/// too because the TSS lives behind a `lazy_static` with no link-time
/// address the stub could `mov` from. Atomic for the convention; the stub
/// reads it as a plain `u64` (same layout), with IF=0.
static KERNEL_STACK: AtomicU64 = AtomicU64::new(0);

/// Where the stub parks the user RSP between the stack switch and the first
/// push. One slot is enough on one CPU: SFMASK keeps IF=0 from `syscall`
/// until it has been pushed onto the task's own kernel stack, so no other
/// syscall can enter in between. SMP would need a per-CPU slot (`swapgs`).
static USER_RSP: AtomicU64 = AtomicU64::new(0);

/// Points `syscall_entry` at `top` for the next syscall. IF=0 like
/// `gdt::set_rsp0`, and for the same reason: it must change in the same step
/// as the scheduler's choice of task.
pub(crate) fn set_kernel_stack(top: VirtAddr) {
    KERNEL_STACK.store(top.as_u64(), Ordering::Relaxed);
}

/// Enables `syscall`/`sysret` and points them at `syscall_entry`. Needs the
/// GDT loaded (STAR is built from its selectors), so it runs right after
/// `gdt::init` in `kernel::init`.
pub fn init() {
    let efer = Efer::read();
    // Safety: SCE only makes `syscall`/`sysret` legal instead of `#UD`.
    // Read-modify-write keeps LME, LMA and NXE as they were (same pattern as
    // `user::ensure_nx_enabled`).
    unsafe { Efer::write(efer | EferFlags::SYSTEM_CALL_EXTENSIONS) };

    // `Star::write` checks the layout the GDT order exists for: user code 16
    // and user data 8 above STAR's sysret base, kernel data 8 above kernel
    // code, and the RPLs. With our GDT the sysret base is 0x13 (index 2, RPL
    // 3), so `sysretq` loads SS = 0x1b, CS = 0x23.
    Star::write(
        crate::gdt::user_code_selector(),
        crate::gdt::user_data_selector(),
        crate::gdt::kernel_code_selector(),
        crate::gdt::kernel_data_selector(),
    )
    .expect("GDT is not in syscall/sysret order");
    LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
    SFMask::write(SYSCALL_FLAG_MASK);
}

/// User registers as `syscall_entry` pushes them, lowest address first.
/// `rcx`/`r11` are the CPU's saved user RIP/RFLAGS, not the program's own
/// values (the instruction overwrote those).
#[repr(C)]
#[derive(Debug)]
pub struct SyscallFrame {
    pub es: u64,
    pub ds: u64,
    pub r9: u64,
    pub r8: u64,
    pub r10: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rax: u64,
    pub rflags: u64,
    pub rip: u64,
    pub rsp: u64,
}

/// Bytes `syscall_entry` pushes: 12 slots, a multiple of 16, so the stack is
/// still 16-aligned at the `call` and the handler starts at 8 mod 16 as SysV
/// requires (the kernel stack top itself is 16-aligned, see `task.rs`).
const _: () = assert!(core::mem::size_of::<SyscallFrame>() % 16 == 0);

/// `LSTAR` target. The CPU has already set CS/SS from STAR, saved RIP in RCX
/// and RFLAGS in R11, and cleared `SYSCALL_FLAG_MASK`, but RSP is still
/// whatever Ring 3 left in it: possibly unmapped, possibly pointing at kernel
/// memory. So nothing may touch the stack before the switch, which is why
/// the user RSP goes to a fixed slot rather than a `push`.
///
/// Syscall context is ordinary mainline task context, not interrupt context:
/// no `IsrGuard`, and IF goes back on once the stack is switched. Handlers may
/// allocate, yield and exit, and the timer can preempt a long one; a
/// preemption here is a plain Ring 0 -> Ring 0 interrupt on the task's kernel
/// stack.
///
/// DS/ES get the kernel data selector on the way in and the user's own back on
/// the way out (design-review item 27). Long mode ignores their bases, so this
/// is hygiene, not a guarantee: DS/ES are CPU-wide rather than per task, so a
/// task preempted mid-syscall hands whatever is loaded to the next one, and an
/// interrupt from Ring 3 still runs with the user's selectors.
///
/// Interrupts go off again before the user RSP is restored: from there to
/// `sysretq` the CPU is at CPL 0 on a user stack, and an interrupt would push
/// its frame there.
///
/// IF=0 masks maskable interrupts only. NMI, `#MC` and a hardware `#DB` can
/// still arrive in the two windows that run at CPL 0 on the user's RSP (entry
/// before `mov rsp`, exit after `pop rsp`), and would push onto that stack.
/// None has a handler yet (QEMU doesn't inject NMIs); when they get one, it
/// needs its own IST stack, as `#DF` has.
///
/// Must never be reached by a `syscall` from Ring 0: the stub would reset RSP
/// to the top of the stack that kernel code is already running on.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        "mov [rip + {user_rsp}], rsp",
        "mov rsp, [rip + {kernel_stack}]",
        // Pushes match `SyscallFrame`, last pushed = first field.
        "push qword ptr [rip + {user_rsp}]",
        "push rcx",
        "push r11",
        "push rax",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",
        // rcx is saved above, so it's free as scratch.
        "mov ecx, ds",
        "push rcx",
        "mov ecx, es",
        "push rcx",
        // `syscall` loaded SS with the kernel data selector from STAR.
        "mov cx, ss",
        "mov ds, cx",
        "mov es, cx",
        // Off the user stack and the scratch slot is consumed.
        "sti",
        "mov rdi, rsp",
        "call {handler}",
        "cli",
        // rax holds the result; everything else comes back from the frame.
        "pop rcx",
        "mov es, cx",
        "pop rcx",
        "mov ds, cx",
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        // Skip the saved rax (the number); rax is the result now.
        "add rsp, 8",
        "pop r11",
        "pop rcx",
        "pop rsp",
        "sysretq",
        user_rsp = sym USER_RSP,
        kernel_stack = sym KERNEL_STACK,
        handler = sym syscall_handler,
    )
}

/// Rust side of `syscall_entry`: makes sure `sysretq` will be able to return
/// safely, then dispatches.
///
/// `sysretq` loads RIP from RCX, and on Intel a non-canonical RCX raises `#GP`
/// *after* the switch to CPL 0 state is undone, i.e. in Ring 0 on the user's
/// RSP (CVE-2012-0217). A program can arrange that by putting `syscall` in the
/// last two bytes of the lower half, so the return address is the first
/// non-canonical one. Such a task is killed like a faulting one instead,
/// before its request runs (nothing here ever rewrites `frame.rip`, so the
/// value checked is the one `sysretq` loads).
extern "C" fn syscall_handler(frame: *mut SyscallFrame) -> u64 {
    // Safety: `syscall_entry` built the frame on this task's kernel stack and
    // nothing else references it until this returns.
    let frame = unsafe { &mut *frame };
    if frame.rip >= USER_SPACE_END {
        crate::interrupts::count_user_fault();
        crate::serial_println!(
            "Killing user task {:?}: syscall return address {:#x} is not a user address",
            crate::scheduler::current_id(),
            frame.rip
        );
        crate::scheduler::exit();
    }
    match dispatch(frame.rax, [frame.rdi, frame.rsi, frame.rdx]) {
        Ok(value) => value,
        Err(e) => e.to_return_value(),
    }
}

/// Number -> handler. Kept as one `match` rather than a table of function
/// pointers: three entries don't need one, and the compiler checks every
/// handler's argument use.
fn dispatch(number: u64, args: [u64; 3]) -> Result<u64, SyscallError> {
    match number {
        SYS_WRITE => sys_write(args[0], args[1]),
        SYS_EXIT => sys_exit(args[0]),
        SYS_YIELD => sys_yield(),
        _ => Err(SyscallError::NoSuchSyscall),
    }
}

/// Test hook: if set, also receives every buffer `SYS_WRITE` accepts, so a
/// test can assert on what Ring 3 wrote without reading serial back.
static WRITE_TAP: Mutex<Option<fn(&[u8])>> = Mutex::new(None);

/// Installs or removes the `SYS_WRITE` tap.
pub fn set_write_tap(tap: Option<fn(&[u8])>) {
    *WRITE_TAP.lock() = tap;
}

/// `write(ptr, len)`: copies `len` bytes at `ptr` to serial. The range is
/// checked by `user::with_user_bytes` before a single byte is read, so a bad
/// pointer is an error, never a kernel `#PF`.
///
/// SMAP is off (design-review item 26), so the kernel can read the bytes
/// directly. With SMAP on, the read would be bracketed by `stac`/`clac`
/// inside `with_user_bytes`.
fn sys_write(ptr: u64, len: u64) -> Result<u64, SyscallError> {
    crate::user::with_user_bytes(ptr, len, |bytes| {
        // Invalid UTF-8 is printed as U+FFFD rather than refused: the bytes
        // are the program's, the kernel only forwards them.
        for chunk in bytes.utf8_chunks() {
            crate::serial_print!("{}", chunk.valid());
            if !chunk.invalid().is_empty() {
                crate::serial_print!("\u{FFFD}");
            }
        }
        // Copied out so the tap runs without the lock held.
        let tap = *WRITE_TAP.lock();
        if let Some(tap) = tap {
            tap(bytes);
        }
    })
    .map(|()| len)
    .map_err(|_| SyscallError::BadAddress)
}

static USER_EXITS: AtomicU64 = AtomicU64::new(0);
static LAST_EXIT_CODE: AtomicU64 = AtomicU64::new(0);

/// User tasks that ended through `SYS_EXIT`. Separate from
/// `interrupts::user_faults` (design-review item 28): an exit is the program
/// asking to stop, a fault is the kernel stopping it.
pub fn user_exits() -> u64 {
    USER_EXITS.load(Ordering::Relaxed)
}

/// The code passed to the most recent `SYS_EXIT`. One global, since there is
/// no process table to keep it in yet (and nobody to `wait` for it).
pub fn last_exit_code() -> u64 {
    LAST_EXIT_CODE.load(Ordering::Relaxed)
}

/// `exit(code)`: the normal `scheduler::exit` path. The syscall and handler
/// frames on this kernel stack own nothing (no heap values, no lock guards),
/// so abandoning them leaks nothing; `reap` frees the stack later.
fn sys_exit(code: u64) -> ! {
    LAST_EXIT_CODE.store(code, Ordering::Relaxed);
    USER_EXITS.fetch_add(1, Ordering::Relaxed);
    crate::serial_println!(
        "User task {:?} exited with code {}",
        crate::scheduler::current_id(),
        code
    );
    crate::scheduler::exit()
}

/// `yield()`: an ordinary `yield_now`. The user's registers wait in the
/// `SyscallFrame` on this task's kernel stack while others run, and the
/// return to Ring 3 happens when the task is next resumed.
fn sys_yield() -> Result<u64, SyscallError> {
    crate::scheduler::yield_now();
    Ok(0)
}
