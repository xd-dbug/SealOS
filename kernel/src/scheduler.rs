use crate::context::switch;
use crate::task::{Task, TaskId, TaskState};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts::without_interrupts;

/// Most tasks that can exist at once, boot task included. The heap is 100 KiB
/// and each spawned task takes a 10 KiB stack, so 8 tasks = 7 stacks = 70 KiB
/// leaves ~30 KiB for the run queue, `Box<Task>`s and everything else. A 9th
/// would leave too little to be safe. Also sizes the preallocated `ready` and
/// `finished` queues, so pushing onto them never grows (allocates).
pub const MAX_TASKS: usize = 8;

/// Why `spawn` refused. Both are expected outcomes of a small fixed heap, not
/// bugs, so they are values rather than panics.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SpawnError {
    /// `MAX_TASKS` tasks already exist.
    TooManyTasks,
    /// The heap couldn't hold another stack (e.g. finished tasks' stacks not
    /// yet reaped, or other allocations eating the budget).
    OutOfMemory,
}

/// `switch` arguments produced by `select_next`: where to save the outgoing
/// rsp, and the rsp to resume.
pub(crate) type SwitchArgs = (*mut VirtAddr, VirtAddr);

/// Round-robin over any number of tasks: `current` plus a FIFO of the rest.
/// Tasks are `Box`ed so the address `switch` writes `saved_rsp` through stays
/// put while a task moves between `current`, `ready` and `finished`.
struct Scheduler {
    current: Box<Task>,
    ready: VecDeque<Box<Task>>,
    /// Tasks whose function returned or called `exit`. Their stacks can't be
    /// freed by the task still running on them, so the next task to run reaps
    /// them.
    finished: Vec<Box<Task>>,
}

impl Scheduler {
    /// Live tasks: `current`, the queue, and finished-but-unreaped ones (their
    /// stacks still occupy the heap, so they count against the budget).
    fn is_full(&self) -> bool {
        1 + self.ready.len() + self.finished.len() >= MAX_TASKS
    }

    /// The one round-robin step behind both `select_next` and `exit`: pops
    /// the front of `ready`, makes it `current`, and files the old `current`
    /// per `outgoing`. Returns the `switch` arguments, or `None` if nothing
    /// else is ready.
    fn rotate(&mut self, outgoing: Outgoing) -> Option<SwitchArgs> {
        let mut next = self.ready.pop_front()?;
        // Checked on every rotation, exit included: a task that overflowed
        // and then exited would otherwise never be checked again.
        assert!(self.current.check_canary(), "stack overflow in task {:?}", self.current.id());
        assert!(next.check_canary(), "stack overflow in task {:?}", next.id());
        debug_assert_eq!(*next.state(), TaskState::Ready);

        let new_rsp = next.saved_rsp();
        // Ring 3 -> 0 entries must land on the incoming task's own kernel
        // stack, both the CPU's (an interrupt, via TSS.RSP0) and the syscall
        // stub's (which switches by hand). IF=0 here (every caller holds the
        // lock via `with_scheduler`). The boot task has no heap stack and
        // never enters ring 3, so both are simply left as the previous task's
        // value instead of being pointed at the bootloader's stack. That
        // value may dangle once the previous task is reaped, which is
        // harmless only because both are read solely on a ring 3 entry.
        if let Some(top) = next.stack_top() {
            crate::gdt::set_rsp0(top);
            crate::syscall::set_kernel_stack(top);
        }
        next.set_state(TaskState::Running);
        let mut prev = core::mem::replace(&mut self.current, next);
        // Taken before the move into a queue: moving a `Box` keeps the
        // pointee where it is.
        let old_rsp = prev.saved_rsp_ptr();
        // Neither push can grow: both queues are preallocated for
        // `MAX_TASKS`, `ready` just lost the entry popped above, and
        // `is_full` counts `finished` toward the cap. A grow would allocate,
        // which an ISR must not do and which could fail mid-exit.
        match outgoing {
            Outgoing::Requeue => {
                prev.set_state(TaskState::Ready);
                debug_assert!(self.ready.len() < self.ready.capacity());
                self.ready.push_back(prev);
            }
            Outgoing::Retire => {
                // Kept alive (not dropped): we are still running on its stack
                // until the caller's `switch` moves rsp away.
                prev.set_state(TaskState::Finished);
                debug_assert!(self.finished.len() < self.finished.capacity());
                self.finished.push(prev);
            }
        }
        Some((old_rsp, new_rsp))
    }
}

/// What `Scheduler::rotate` does with the task it switches away from.
#[derive(Clone, Copy)]
enum Outgoing {
    /// Back of `ready`, to run again (yield, and later preemption).
    Requeue,
    /// Into `finished`, for whoever runs next to reap (exit).
    Retire,
}

lazy_static! {
    /// Lazy (not `OnceCell`) so the first call wraps whoever is running, the
    /// boot task, as `current`. Must first be touched after the heap is up;
    /// `init` forces that from `kernel::init` so it never happens implicitly.
    static ref SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler {
        current: Box::new(Task::boot()),
        // One slot per task other than `current`, so `select_next` (which
        // an ISR will call) can never need to grow it.
        ready: VecDeque::with_capacity(MAX_TASKS - 1),
        // Same reasoning: `exit` pushes here under the lock, and an
        // allocation there could fail mid-exit.
        finished: Vec::with_capacity(MAX_TASKS - 1),
    });
}

/// Whether the timer ISR may preempt. Off until `init` (so the ISR never
/// lazily initializes `SCHEDULER`, which allocates) and switchable by
/// `set_preemption` for tests that assert an exact cooperative interleaving.
/// Relaxed: it gates a decision, it doesn't publish other memory.
static PREEMPTION: AtomicBool = AtomicBool::new(false);

/// Turns timer preemption on or off. On is the default once `kernel::init`
/// has run; off makes scheduling purely cooperative again (switches happen
/// only in `yield_now`/`exit`), which tests that assert an exact interleaving
/// need: a tick landing between a log push and a `yield_now` would otherwise
/// reorder it. Ticks still count while off, they just don't switch.
pub fn set_preemption(enabled: bool) {
    PREEMPTION.store(enabled, Ordering::Relaxed);
}

/// Checked by the timer ISR on every tick.
pub fn preemption_enabled() -> bool {
    PREEMPTION.load(Ordering::Relaxed)
}

/// Locking rule for this module: the lock is only ever held to shuffle
/// queues and is always dropped *before* `switch`. The resumed task would
/// otherwise spin on a lock owned by a task that cannot run. Interrupts are
/// masked while it is held because the timer ISR takes it (via
/// `select_next`): an ISR spinning on a lock the code it interrupted holds
/// would deadlock. This is also why the ISR can never find it held.
fn with_scheduler<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    without_interrupts(|| f(&mut SCHEDULER.lock()))
}

/// Wraps the running context as the boot task and turns timer preemption on.
/// Called from `kernel::init` right after the heap is up, so the boot task
/// deterministically gets the first `TaskId` (otherwise the first `spawn`
/// would claim an ID before the lazy initializer ran) and nothing allocates in
/// a lazy init mid-yield or inside the timer ISR.
pub fn init() {
    lazy_static::initialize(&SCHEDULER);
    set_preemption(true);
}

/// Queues a new task to run on a later `yield_now`. It does not run
/// immediately. Fails instead of panicking when `MAX_TASKS` tasks exist or the
/// heap can't hold the stack.
///
/// No allocation or free happens under the lock (the push never grows the
/// preallocated queue). The cap is checked twice: up front, so the common
/// refusal costs no memory or `TaskId`, and again under the lock so the check
/// and the push are one atomic step. A refusal at the second check (or a
/// failed `Box`) does burn the `TaskId`; IDs are only for identification, so
/// a gap is harmless.
pub fn spawn(entry: extern "C" fn()) -> Result<TaskId, SpawnError> {
    // A task that only ever gets preempted (never yields) never reaps, so
    // dead stacks could otherwise sit in `finished`, counted by `is_full`
    // and the heap, and refuse this spawn. `spawn` is mainline, so freeing
    // here is allowed.
    reap();
    if with_scheduler(|s| s.is_full()) {
        return Err(SpawnError::TooManyTasks);
    }
    let task = Task::spawn(entry).map_err(|_| SpawnError::OutOfMemory)?;
    let id = task.id();
    // Fallible: a failed `Box::new` would abort the kernel.
    let task = Box::try_new(task).map_err(|_| SpawnError::OutOfMemory)?;
    let rejected = with_scheduler(|s| {
        if s.is_full() {
            return Some(task);
        }
        s.ready.push_back(task);
        None
    });
    match rejected {
        // Dropped here, after the lock: freeing the stack takes the heap lock
        // and needn't nest inside the scheduler's.
        Some(_task) => Err(SpawnError::TooManyTasks),
        None => Ok(id),
    }
}

/// Number of tasks other than the caller that could run. Lets the boot task
/// loop `yield_now` until all spawned work is done.
pub fn ready_count() -> usize {
    with_scheduler(|s| s.ready.len())
}

/// The one round-robin selection step, shared by `yield_now` and the timer
/// ISR: pops the front of the queue, makes it `current`, and requeues the old
/// `current` at the back. Returns the `switch` arguments, or `None` if nothing
/// else is ready.
///
/// Never allocates: the queue was preallocated for `MAX_TASKS` and this only
/// pops one entry then pushes one back. It only takes the scheduler lock
/// (with interrupts masked), so lock- and heap-wise it is safe to call from an
/// interrupt handler. It does not itself switch; the caller drops everything
/// and calls `switch`.
///
/// Once it returns, `current` already names the next task while the CPU is
/// still on the old one's stack, and the old task's `saved_rsp` isn't written
/// until `switch` stores it. If the timer ISR selected again in that window
/// it would requeue a task that was never saved, and the next resume would
/// jump through a stale `saved_rsp`. The window is closed by the callers, not
/// here: the ISR has IF=0 from its gate, and `yield_now`/`exit` hold
/// interrupts off from before the selection through `switch`. Any new caller
/// must do the same.
pub(crate) fn select_next() -> Option<SwitchArgs> {
    with_scheduler(|s| s.rotate(Outgoing::Requeue))
}

/// Moves the caller to the back of the queue and runs the front task. Returns
/// (later) once every other task ahead of it has yielded, been preempted or
/// finished; returns at once if nothing else is ready.
///
/// Selection and `switch` run inside one `without_interrupts`: a timer tick
/// between them would select again before this task's rsp was saved (see
/// `select_next`). That wrapper is also what makes IF per-task here: the IF
/// to restore sits in the wrapper's frame on *this task's* stack, so whenever
/// this task is resumed (by a later yield or by a preemption's `switch` back
/// into it) it re-enables or leaves masked exactly what its own caller had.
/// Hence it is also legal, if pointless, to call this with interrupts off.
///
/// Reaps on the way out even when nothing else was ready: this is mainline
/// code, and a task that was preempted away from (rather than yielded away
/// from) never reaps on resume, since the ISR path must not free.
pub fn yield_now() {
    // IF=0 is no longer a reliable "we're in an ISR" signal (yielding inside
    // `without_interrupts` is legal now), so check the depth directly. From a
    // handler this would switch with no EOI and no `ISR_DEPTH` hand-off.
    debug_assert!(
        !crate::interrupts::in_interrupt(),
        "yield_now called from interrupt context"
    );
    without_interrupts(|| {
        if let Some((old_rsp, new_rsp)) = select_next() {
            // Safety: `old_rsp` points into the heap-allocated `Task` now in
            // `ready` (alive, boxed, never freed while queued). `new_rsp`
            // came from a `Ready` task's `init_stack`, an earlier `switch`
            // or a preemption's `switch_from_isr`. No lock is held.
            unsafe { switch(old_rsp, new_rsp) };
            // Only reached once someone switches back to us, on our own stack.
        }
    });
    reap();
}

/// Top of the running task's kernel stack (`None` for the boot task). Exists
/// so tests can compare it with `gdt::rsp0()`.
pub fn current_stack_top() -> Option<VirtAddr> {
    with_scheduler(|s| s.current.stack_top())
}

/// Number of exited tasks whose stacks are still waiting to be freed. Exists
/// so tests can observe that `reap` ran promptly; not useful for scheduling.
pub fn finished_count() -> usize {
    with_scheduler(|s| s.finished.len())
}

/// Ends the calling task for good, from any call depth. Also what a task's
/// function returning ends up calling, via `task_returned`.
///
/// Destructors of live locals in the abandoned frames do NOT run (the stack is
/// discarded, not unwound), so anything owning a heap allocation or a lock
/// guard in those frames leaks or stays locked. Drop such values before
/// calling this.
///
/// Must not be called on the boot task: it runs on the bootloader's stack and
/// is the scheduler's fallback when the queue would otherwise be empty.
///
/// As in `yield_now`, rotation and `switch` share one `without_interrupts`.
/// Here that matters doubly: once the task is in `finished`, a tick that
/// preempted it would requeue-or-resume a task nothing may ever resume. The
/// wrapper never returns (its restore-IF code is abandoned with the stack);
/// the next task sets its own IF.
pub fn exit() -> ! {
    // Same reasoning as in `yield_now`.
    debug_assert!(
        !crate::interrupts::in_interrupt(),
        "exit called from interrupt context"
    );
    without_interrupts(|| {
        let (old_rsp, new_rsp) = with_scheduler(|s| {
            assert!(s.current.stack_top().is_some(), "the boot task cannot exit");
            // The boot task never exits, so it is always queued while another
            // task runs; an empty queue means the boot task was lost.
            s.rotate(Outgoing::Retire)
                .expect("last runnable task finished; no boot task to return to")
        });
        // Safety: as in `yield_now`; `old_rsp` stays valid because the task
        // sits in `finished` until the next task's `reap`, which runs after
        // this switch. The saved frame is never resumed: nothing queues a
        // `Finished` task.
        unsafe { switch(old_rsp, new_rsp) };
    });
    unreachable!("a finished task was switched back into");
}

/// The `exit` for exception handlers: ends the faulting task and switches to
/// the next. Same shape as `exit` (rotate with `Retire`, drop the lock, then
/// switch), but the hand-off goes through `switch_from_isr` so `ISR_DEPTH`
/// stays per-task.
///
/// Depth reasoning: the caller's `IsrGuard` made the depth 1 and lives in a
/// frame on the dying stack. `switch_from_isr` parks that 1 in a local there
/// and sets the depth to 0, and since the dead task never resumes, the parked
/// value is never restored, which is exactly right. Whoever runs next
/// then has the correct depth: a fresh task starts at 0 (plain mainline); a
/// yielder was never in an ISR, so 0 is its own depth; a task preempted
/// inside the timer ISR restores its own saved depth right after its
/// `switch` returns. The abandoned `IsrGuard` and fault frame are plain stack
/// memory freed with the stack; no destructor needs to run because the guard
/// only touched the counter, which the swap already accounted for.
///
/// Frees nothing: the dead task's stack goes to `finished` and is reaped by
/// mainline code (`reap`), since an ISR must not touch the heap. No EOI is
/// needed: exceptions don't go through the PIC.
///
/// Callable only for a task with its own heap stack (not the boot task, which
/// the scheduler needs as its fallback); callers check `current_stack_top`.
///
/// # Safety
/// Must be called from an exception handler on the faulting task's own
/// kernel stack, holding no locks, whose task will never be resumed.
pub(crate) unsafe fn exit_from_isr() -> ! {
    debug_assert!(
        crate::interrupts::in_interrupt(),
        "exit_from_isr called outside interrupt context"
    );
    // An interrupt gate cleared IF; a tick between rotation and `switch`
    // would select again before the outgoing rsp is saved (see `select_next`).
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "exit_from_isr with interrupts enabled"
    );
    let (old_rsp, new_rsp) = with_scheduler(|s| {
        assert!(s.current.stack_top().is_some(), "the boot task cannot exit");
        s.rotate(Outgoing::Retire)
            .expect("last runnable task finished; no boot task to return to")
    });
    // Safety: as in `exit` (`old_rsp` stays valid in `finished` until a later
    // `reap`, the frame is never resumed), plus this is a handler on the
    // dying task's own stack with no lock held and nothing to EOI.
    unsafe { crate::interrupts::switch_from_isr(old_rsp, new_rsp) };
    unreachable!("a finished task was switched back into");
}

/// Id of the running task, for fault reports.
pub fn current_id() -> TaskId {
    with_scheduler(|s| s.current.id())
}

/// Frees stacks of finished tasks. Runs on the resumed task, so `rsp` is
/// guaranteed to be off the stacks being freed.
///
/// Only ever called from mainline task context: at the end of `yield_now`,
/// at the start of `spawn`, and from `context::task_started` before a fresh
/// task's `entry`. Freeing takes the heap lock, which an ISR must never do (it may
/// have interrupted the allocator holding that lock, a deadlock;
/// `allocator::GuardedHeap` panics if it tries), so reaping must stay out of
/// interrupt handlers. The timer ISR therefore only picks and switches; a
/// task resumed by the ISR path doesn't reap, and its stacks wait for the next
/// `yield_now` or fresh-task start.
pub(crate) fn reap() {
    // One at a time, each dropped outside the lock: freeing takes the heap
    // lock and needn't nest. Popping (not `mem::take`) keeps `finished`'s
    // preallocated capacity, so `exit` never has to grow it.
    while let Some(dead) = with_scheduler(|s| s.finished.pop()) {
        drop(dead);
    }
}
