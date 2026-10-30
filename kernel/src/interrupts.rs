use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::registers::control::Cr2;
use crate::println;
use crate::gdt;
use pic8259::ChainedPics;
use spin::Mutex;
use crate::print;
use pc_keyboard::{layouts, DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use lazy_static::lazy_static;

// Built once, lazily, on first access; `static` requires a `const` initializer
// but table construction (method calls) isn't const, hence lazy_static.
lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
        unsafe {
            // Double faults run on their own dedicated stack (set up in gdt.rs) so a
            // kernel stack overflow, which is itself a double fault, doesn't triple fault.
            idt.double_fault.set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt[InterruptIndex::Timer.as_u8()]
            .set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()]
            .set_handler_fn(keyboard_interrupt_handler);

        idt
    };
}

/// Remapped off the default 0x08/0x70 (which overlaps CPU exception vectors)
/// to one past the last CPU exception vector (31), so hardware IRQs and CPU
/// exceptions can never land on the same IDT entry.
pub const PIC_1_OFFSET: u8 = 32;
/// The slave PIC's 8 IRQ lines follow the master's contiguously.
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// Locked from both timer and keyboard ISRs (to send EOI) and from
/// `kernel::init` (to remap/unmask), so it's one shared `Mutex` rather than
/// per-handler state.
pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

// Single core, so this only has to order against code on the same core.
// Acquire on enter / Release on exit (free on x86) keeps the compiler from
// moving work guarded by "we are in an ISR" outside the guard's lifetime.
//
// The depth describes the *currently running* context, not the CPU: any
// code that switches tasks from inside an ISR must save it before `switch`
// and restore it after, so each task resumes with its own depth (and a fresh
// task starts at 0). `switch_from_isr` is that one place.
static ISR_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// True while any `IsrGuard` is alive, i.e. we are inside an interrupt or
/// exception handler. `allocator::GuardedHeap` panics on heap use when set.
pub fn in_interrupt() -> bool {
    ISR_DEPTH.load(Ordering::Acquire) != 0
}

/// RAII marker for "this code runs in interrupt context". Every handler
/// (including any scheduler code the timer ISR calls) must create one first
/// thing; it is a counter, not a flag, so nested handlers (e.g. a breakpoint
/// inside the timer ISR) unwind correctly. Drop it only after the handler's
/// last heap-free work, i.e. just let it fall out of scope at the end.
///
/// The guard lives on the interrupted task's stack, so dropping it early
/// before a task switch is wrong: it would decrement again when that task
/// resumes and its handler returns. Save/restore `ISR_DEPTH` around the
/// switch instead (see the comment on `ISR_DEPTH`).
#[must_use = "the guard marks interrupt context only while it is alive"]
pub struct IsrGuard(());

impl IsrGuard {
    pub fn enter() -> Self {
        ISR_DEPTH.fetch_add(1, Ordering::Acquire);
        IsrGuard(())
    }
}

impl Drop for IsrGuard {
    fn drop(&mut self) {
        ISR_DEPTH.fetch_sub(1, Ordering::Release);
    }
}

/// Switches tasks from inside an interrupt handler, keeping `ISR_DEPTH`
/// per-task. The preempted task's `IsrGuard` stays alive on its own stack
/// across the switch (dropping it early would decrement a second time when the
/// task resumes and its handler returns), so instead the depth is parked in a
/// local on that same stack: the incoming context starts at 0 (a fresh task
/// is plain mainline code; a cooperatively yielded one never was in an ISR),
/// or, if it was itself preempted, restores its own saved depth right after
/// its `switch` returns, below.
///
/// Both `swap` and `store` run with interrupts masked (the handler's gate
/// cleared IF, and the resumed side is still inside its handler), so nothing
/// can observe the transient 0.
///
/// # Safety
/// Everything `context::switch` requires, and the caller must be an interrupt
/// handler running on the interrupted task's own stack that has already sent
/// its EOI and holds no locks.
pub(crate) unsafe fn switch_from_isr(
    old_rsp: *mut x86_64::VirtAddr,
    new_rsp: x86_64::VirtAddr,
) {
    let saved_depth = ISR_DEPTH.swap(0, Ordering::AcqRel);
    // Safety: forwarded under this function's contract.
    unsafe { crate::context::switch(old_rsp, new_rsp) };
    // Only reached when this task is resumed, on its own stack.
    ISR_DEPTH.store(saved_depth, Ordering::Release);
}

/// Installs the IDT so the CPU starts routing exceptions/interrupts through it.
pub fn init_idt() {
    IDT.load();
}

/// Handles `int3`/breakpoint exceptions by logging and returning (execution resumes).
extern "x86-interrupt" fn breakpoint_handler(
    stack_frame: InterruptStackFrame)
{
    let _isr = IsrGuard::enter();
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

/// Double faults are unrecoverable here, so this just reports and panics
/// instead of returning (the `!` return type is required for this vector).
extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, _error_code: u64) -> !
{
    let _isr = IsrGuard::enter();
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}
/// A page fault from Ring 3 ends the offending task (`kill_user_task`) and the
/// kernel carries on. One from Ring 0 is a kernel bug and unrecoverable (no
/// demand paging/heap growth yet), so it reports the faulting address/cause
/// and halts. That path must loop rather than `hlt()` once: with the timer IRQ
/// unmasked, a single `hlt()` just sleeps until the next tick wakes it, and
/// returning re-executes (and re-faults on) the same instruction forever.
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::PrivilegeLevel;

    let _isr = IsrGuard::enter();
    println!("EXCEPTION: PAGE FAULT");
    println!("Accessed Address: {:?}", Cr2::read());
    println!("Error Code: {:?}", error_code);
    println!("{:#?}", stack_frame);
    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        kill_user_task("PAGE FAULT", &stack_frame);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

static USER_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Number of user tasks killed by a Ring 3 fault since boot. Lets tests
/// observe a kill without relying on a panic. Relaxed, like `TICKS`.
pub fn user_faults() -> u64 {
    USER_FAULTS.load(Ordering::Relaxed)
}

/// Counts a user task the kernel is about to end for misbehaving outside an
/// exception handler (`syscall::syscall_handler`'s unsafe-return kill), so
/// `user_faults` covers every kill, not just `#GP`/`#PF`.
pub(crate) fn count_user_fault() {
    USER_FAULTS.fetch_add(1, Ordering::Relaxed);
}

/// Ends the current task after a Ring 3 fault and switches away; returns only
/// if the task can't be killed (the boot task, which never enters Ring 3), so
/// the caller falls through to its kernel-fault path.
///
/// The fault is a protection check working as designed, not a kernel bug, so
/// the kernel must survive it. The handler's `IsrGuard` and the fault frame
/// stay on the dead stack, which is simply abandoned and later freed by
/// `reap`; see `scheduler::exit_from_isr` for the `ISR_DEPTH` hand-off.
fn kill_user_task(kind: &str, stack_frame: &InterruptStackFrame) {
    if crate::scheduler::current_stack_top().is_none() {
        return;
    }
    count_user_fault();
    println!(
        "Killing user task {:?}: {} at rip {:?}",
        crate::scheduler::current_id(),
        kind,
        stack_frame.instruction_pointer
    );
    // Safety: called from an exception handler on the task's own stack with
    // no lock held (user code holds none); the task never runs again.
    unsafe { crate::scheduler::exit_from_isr() }
}

/// Without this, a `#GP` has nowhere to go and escalates straight to a double
/// fault, hiding the real cause (design-review item 9: the stale-SS bug in
/// `gdt::init` needed QEMU `-d int` tracing to find for exactly that reason).
///
/// A `#GP` from Ring 3 ends the offending task (`kill_user_task`). One from
/// Ring 0 reports, then panics rather than returning: `#GP` is a fault, so
/// `iretq` would re-run the faulting instruction and fault again forever.
/// Panicking (like `double_fault_handler`) also lets a test observe the real
/// handler through its panic handler.
///
/// The CPL of the faulting code is the RPL of the saved CS: the CPU pushes the
/// interrupted CS, and CS's RPL always equals the CPL it was running at.
/// Distinguishing the two is the point of Ring 3: a user-mode `#GP` is the
/// kernel trapping a privilege violation, a kernel-mode one is a bug.
extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    use x86_64::PrivilegeLevel;
    use x86_64::structures::idt::SelectorErrorCode;

    let _isr = IsrGuard::enter();
    let user_mode = stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3;
    println!("EXCEPTION: GENERAL PROTECTION FAULT");
    // Zero for most causes (non-canonical address, privileged instruction).
    // Non-zero means a segment selector was at fault, and the code *is* that
    // selector's index/table, which is the clue item 9's bug needed.
    if error_code == 0 {
        println!("Error Code: 0 (not selector-related)");
    } else {
        println!("Error Code: {:#x} {:?}", error_code, SelectorErrorCode::new_truncate(error_code));
    }
    println!("{:#?}", stack_frame);
    // Unformatted literals so tests can tell the cases apart through
    // `PanicInfo::message().as_str()` (`tests/general_protection.rs`).
    if user_mode {
        kill_user_task("GENERAL PROTECTION FAULT", &stack_frame);
        // Only reached for the boot task, which never enters Ring 3, so this
        // means the scheduler's idea of the current task is wrong.
        panic!("EXCEPTION: GENERAL PROTECTION FAULT in user mode with no user task to kill");
    } else {
        panic!("EXCEPTION: GENERAL PROTECTION FAULT in kernel mode");
    }
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}

/// IDT vectors for the hardware IRQs this kernel handles, derived from
/// `PIC_1_OFFSET` so they stay correct if the PIC remap offset ever moves.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard = InterruptIndex::Timer as u8 + 1,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

// Relaxed everywhere: this is just a liveness counter, not synchronizing
// access to anything else, so no ordering guarantees beyond atomicity are
// needed.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Number of timer interrupts handled since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Input clock of the 8253/8254 PIT, in Hz (fixed by the hardware).
const PIT_BASE_HZ: u32 = 1_193_182;

/// Timer interrupt rate, and so the preemption time slice (10 ms). The PIT's
/// power-on rate of ~18.2 Hz would give 55 ms slices, which makes tasks feel
/// sluggish and slows tick-based tests; 100 Hz is slow enough that the ISR
/// overhead is negligible and fast enough for sub-second fairness checks.
/// The divisor is rounded, so the real rate is 1193182 / 11932 = 99.998 Hz.
pub const TIMER_HZ: u32 = 100;

/// Programs PIT channel 0 to fire IRQ0 at `TIMER_HZ`. Like the PIC setup, it
/// must run before interrupts are enabled so the first tick already arrives at
/// the new rate.
pub fn init_pit() {
    use x86_64::instructions::port::Port;

    let divisor = (PIT_BASE_HZ + TIMER_HZ / 2) / TIMER_HZ;
    assert!(divisor <= u16::MAX as u32, "PIT divisor must fit in 16 bits");
    let mut command: Port<u8> = Port::new(0x43);
    let mut channel0: Port<u8> = Port::new(0x40);
    // Safety: standard PIT ports; no other code touches them. 0x34 = channel
    // 0, low byte then high byte, mode 2 (rate generator: a periodic pulse),
    // binary counting.
    unsafe {
        command.write(0x34);
        channel0.write(divisor as u8);
        channel0.write((divisor >> 8) as u8);
    }
}

/// IRQ0: counts the tick, acknowledges it, then preempts the running task if
/// preemption is on and another task is ready.
///
/// Why a plain `x86-interrupt` handler can switch tasks directly, instead of
/// a naked stub that pushes every register:
/// - The `x86-interrupt` ABI (LLVM's `x86_intrcc`) preserves *every* register
///   the handler body or anything it calls clobbers, saving them in the
///   prologue and restoring them in the epilogue, which is what makes it safe
///   to interrupt code that made no `call`. The interrupted task's rax, rcx,
///   rdx, rsi, rdi, r8-r11 therefore sit on its own stack above `switch`'s
///   frame; `switch` saves rbx, rbp, r12-r15 itself; rip, cs, rflags, rsp and
///   ss are in the CPU's interrupt frame. Together that is the full context.
///   `tests/preemption.rs` checks it by spinning on known values in all of
///   them.
/// - No vector state: the target (`x86_64-seal_os.json`) is `-sse,+soft-float`
///   so the kernel never touches xmm/mmx/x87 registers and there is nothing
///   more to save. (User tasks, if they ever use SSE, will need this revisited.)
/// - IRQ0 has no IST entry and this is ring 0 to ring 0, so the CPU pushes the
///   interrupt frame onto the interrupted task's own stack. Switching away
///   leaves that frame intact there, and resuming the task later simply
///   returns through this function's epilogue and `iretq`, which also restores
///   the task's own RFLAGS (so IF is per-task for preempted tasks).
///
/// Order matters:
/// 1. EOI before switching. Otherwise the PIC's in-service bit for IRQ0 stays
///    set while the next task runs, and the timer never fires again.
/// 2. Nothing here may allocate, free, reap or print: the handler may have
///    interrupted the allocator or a print mid-operation. `select_next` is
///    allocation-free and takes only the scheduler lock, which mainline code
///    only ever holds with interrupts masked, so the ISR can never find it
///    held. `GuardedHeap` panics if this rule is broken. Mainline locks held
///    with interrupts enabled (e.g. a test's log mutex) can be preempted
///    while held; that merely makes the next task that wants it spin until
///    the holder is rescheduled (each spinner is itself preempted in turn),
///    so it costs time, not correctness.
/// 3. `_isr` is not dropped before the switch; `switch_from_isr` saves and
///    restores `ISR_DEPTH` around it instead.
extern "x86-interrupt" fn timer_interrupt_handler(
    _stack_frame: InterruptStackFrame)
{
    let _isr = IsrGuard::enter();
    TICKS.fetch_add(1, Ordering::Relaxed);

    // Without this, the PIC's in-service bit for IRQ0 never clears, so it
    // never delivers another timer interrupt after this one. The lock is
    // dropped at the end of the statement, before any switch.
    unsafe {
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    if !crate::scheduler::preemption_enabled() {
        return;
    }
    // IF is still 0 (interrupt gate), which `select_next` requires through
    // `switch`, so no other tick can select again in between.
    if let Some((old_rsp, new_rsp)) = crate::scheduler::select_next() {
        // Safety: same contract as `yield_now`'s switch; in addition this is
        // a handler on the interrupted task's own stack, EOI is sent and no
        // lock is held.
        unsafe { switch_from_isr(old_rsp, new_rsp) };
    }
}

// Built once, lazily: the keyboard's decode state (shift/ctrl modifiers,
// multi-byte scancode sequences in flight) has to persist across interrupts,
// so it can't just be a local in the handler.
lazy_static! {
    static ref KEYBOARD: Mutex<PS2Keyboard<layouts::Us104Key, ScancodeSet1>> = Mutex::new(
        PS2Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore)
    );
}

/// Reads the scancode the PS/2 controller left on port 0x60 and feeds it
/// through `pc_keyboard`'s stateful decoder, which may need several scancode
/// bytes (extended/multi-byte sequences) before it yields a `DecodedKey`.
extern "x86-interrupt" fn keyboard_interrupt_handler(
    _stack_frame: InterruptStackFrame)
{
    use x86_64::instructions::port::Port;

    let _isr = IsrGuard::enter();
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };

    let mut keyboard = KEYBOARD.lock();
    if let Ok(Some(key_event)) = keyboard.add_byte(scancode)
        && let Some(key) = keyboard.process_keyevent(key_event)
    {
        match key {
            DecodedKey::Unicode(character) => print!("{}", character),
            DecodedKey::RawKey(key) => print!("{:?}", key),
        }
    }

    // Without this, the PIC's in-service bit for IRQ1 never clears, so it
    // never delivers another keyboard interrupt after this one.
    unsafe {
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

#[test_case]
fn test_timer_ticks_increase() {
    let start = ticks();
    // `hlt` sleeps the CPU until the next interrupt, so this wakes on every
    // timer tick instead of spinning at full speed while waiting.
    while ticks() == start {
        x86_64::instructions::hlt();
    }
    assert!(ticks() > start);
}

#[test_case]
fn test_isr_guard_nests_and_clears() {
    assert!(!in_interrupt());
    {
        let _outer = IsrGuard::enter();
        assert!(in_interrupt());
        {
            let _inner = IsrGuard::enter();
            assert!(in_interrupt());
        }
        assert!(in_interrupt());
    }
    assert!(!in_interrupt());
}
