//! Runtime for seal_os user programs: the syscall wrappers, `print!`, the
//! panic handler and the `entry!` macro that supplies `_start`.
//!
//! Target notes (`x86_64-seal_os-user.json`):
//! - Soft-float, SSE off, like the kernel: `context::switch` saves no FPU/SSE
//!   state (design-review item 21), so XMM registers would leak between tasks.
//! - Small code model + PIC, linked as a static non-PIE `ET_EXEC`. The image
//!   sits above 2 GiB (`0x80_0040_0000`), out of reach of the 32-bit absolute
//!   addresses non-PIC small-model code uses, so code addresses everything
//!   RIP-relative instead; the linker still resolves every address at link
//!   time, so the kernel loader applies no relocations.
//! - The red zone stays *on*, unlike the kernel's. The kernel needs it off
//!   because an interrupt in Ring 0 pushes onto the interrupted stack, over
//!   the 128 bytes below RSP. From Ring 3 the CPU switches to TSS.RSP0 first,
//!   and `syscall_entry` never touches the user stack, so nothing ever writes
//!   below a user RSP.

#![no_std]

use core::fmt;

/// Mirrors the ABI table in `kernel/src/syscall.rs`, the one place it is
/// defined. Keep the two in sync.
pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_YIELD: u64 = 3;

/// Exit code of a program that panicked.
pub const PANIC_EXIT_CODE: u64 = 101;

/// The kernel's `SyscallError`, decoded from a negative `rax`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallError {
    NoSuchSyscall,
    BadAddress,
    /// A code this runtime doesn't know (the kernel is newer).
    Unknown(u64),
}

fn decode(ret: u64) -> Result<u64, SyscallError> {
    if (ret as i64) >= 0 {
        return Ok(ret);
    }
    Err(match ret.wrapping_neg() {
        1 => SyscallError::NoSuchSyscall,
        2 => SyscallError::BadAddress,
        code => SyscallError::Unknown(code),
    })
}

/// `write(ptr, len)`: sends `buf` to the kernel's serial output.
pub fn write(buf: &[u8]) -> Result<u64, SyscallError> {
    let ret: u64;
    // Safety: the ABI clobbers only rcx/r11 (the CPU's saved RIP/RFLAGS)
    // and rax (the result). The kernel reads `buf` but never writes user
    // memory, and never touches the user stack, so `nostack` holds and the
    // red zone is safe.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_WRITE => ret,
            in("rdi") buf.as_ptr(),
            in("rsi") buf.len(),
            out("rcx") _,
            out("r11") _,
            options(nostack, readonly),
        );
    }
    decode(ret)
}

/// `yield()`: lets another task run, returning when this one is next picked.
pub fn yield_now() {
    let ret: u64;
    // Safety: as for `write`; no memory is read or written.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_YIELD => ret,
            out("rcx") _,
            out("r11") _,
            options(nostack, nomem),
        );
    }
    debug_assert_eq!(ret, 0);
}

/// `exit(code)`: ends this task. The kernel never returns from it; the loop
/// after `syscall` only exists so this asm block can honestly be `noreturn`.
/// It spins rather than `ud2`s: `#UD` has no handler and would escalate to a
/// kernel `#DF` panic (design-review item 34).
pub fn exit(code: u64) -> ! {
    // Safety: `SYS_EXIT` takes one argument and does not return.
    unsafe {
        core::arch::asm!(
            "syscall",
            "2:",
            "pause",
            "jmp 2b",
            in("rax") SYS_EXIT,
            in("rdi") code,
            options(nostack, noreturn),
        );
    }
}

/// `fmt::Write` over `SYS_WRITE`, so `print!` needs no buffer or heap.
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(s.as_bytes()).map(|_| ()).map_err(|_| fmt::Error)
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = fmt::Write::write_fmt(&mut Console, args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// Reports the panic, then exits with `PANIC_EXIT_CODE`. A panic is the
/// program failing, not the kernel, so it ends the way any program does.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("user panic: {}", info);
    exit(PANIC_EXIT_CODE)
}

/// Defines `_start`, the ELF entry point, for a program whose `main` is
/// `fn() -> u64`; `main`'s return value becomes the exit code.
///
/// `_start` is naked because nobody `call`ed it. `enter_user` arrives by
/// `iretq` with RSP = `USER_STACK_TOP` (16-aligned) and no return address,
/// whereas a compiled SysV function assumes RSP = 8 mod 16 at entry (the
/// `call` just pushed 8 bytes) and that `ret` goes somewhere. So it:
/// - zeroes RBP, ending the frame-pointer chain for debuggers;
/// - aligns RSP down to 16, then `call`s, so `main` sees the ABI's 8 mod 16;
/// - never returns: there is nothing to return to (a `ret` would pop
///   whatever sits at `USER_STACK_TOP`, an unmapped page), so it passes
///   `main`'s result straight to `SYS_EXIT`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        #[unsafe(naked)]
        pub unsafe extern "C" fn _start() -> ! {
            core::arch::naked_asm!(
                "xor ebp, ebp",
                "and rsp, -16",
                "call {start}",
                "2:",
                "pause",
                "jmp 2b",
                start = sym __seal_start,
            )
        }

        extern "C" fn __seal_start() -> ! {
            let main: fn() -> u64 = $main;
            $crate::exit(main())
        }
    };
}
