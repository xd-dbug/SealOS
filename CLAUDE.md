# seal_os

A capstone project: a bare-metal `no_std` x86_64 kernel, built by following the
["Writing an OS in Rust"](https://os.phil-opp.com) tutorial series, then extended past it for the
capstone (Layer 4+, see "Current phase" below).

Note: `.claude/` (the subagent and skill definitions) is gitignored and local-only, so on a fresh
clone the agents in "Available subagents" below won't exist.

## Repo layout

Two crates, not a Cargo workspace (both `edition = "2024"`):

- **Root (`Cargo.toml`, name `runner`)** — a normal `std` host binary. Its `build.rs`
  cross-compiles `kernel/` to the custom bare-metal target; its `src/main.rs` builds a UEFI disk
  image (via the `bootloader` crate) and launches `qemu-system-x86_64` with OVMF firmware. This
  same binary is also wired up (via `kernel/.cargo/config.toml`'s `runner = ...`) as the
  `cargo run`/`cargo test` runner *for the kernel crate itself*.
- **`kernel/`** — the actual `#![no_std]`, `#![no_main]` kernel crate. Custom target spec
  `kernel/x86_64-seal_os.json` (bare metal, softfloat, no redzone, `panic-strategy: abort`),
  built with `build-std = ["core", "compiler_builtins", "alloc"]` and
  `build-std-features = ["compiler-builtins-mem"]`. `alloc` is what makes `Box`/`Vec`/`String`
  usable on top of the kernel heap.

## Build / run / test

- `cargo run` (from repo root) — builds the kernel, boots it in QEMU with a display.
- `cd kernel && cargo test` — runs the QEMU-based test suite headless. **Requires the runner to
  already be built**: `kernel/.cargo/config.toml` points at the prebuilt `../target/debug/runner`,
  so on a clean checkout run `cargo build` (or `cargo run`) from the repo root first. Each test
  binary boots in its own QEMU instance; results are reported over a virtual serial port and via
  QEMU's `isa-debug-exit` device, translated back into a normal 0/1 process exit code for cargo.
  For the full exit-code plumbing and how to tell a real test failure apart from a triple fault
  or hang, see the `qemu-test-diagnostician` agent below.
- Requires a pinned nightly toolchain (`rust-toolchain.toml`: `nightly-2026-07-19`, includes
  `rust-src`) and OVMF UEFI firmware. The firmware paths are **hardcoded** in `src/main.rs`
  (`OVMF_CODE`/`OVMF_VARS`) to the Arch `edk2-ovmf` layout,
  `/usr/share/OVMF/x64/OVMF_{CODE,VARS}.4m.fd` — there's no env-var override, so other distros
  need those constants edited.

## Architecture notes

- **Boot flow**: bootloader → `entry_point!`-registered fn (`kernel_main` in `kernel/src/main.rs`,
  `test_kernel_main` in `lib.rs`) → `framebuffer::init` →
  `kernel::init(physical_memory_offset, memory_regions)`, which runs, in order:
  1. `gdt::init()`
  2. `interrupts::init_idt()`
  3. PIC init + unmask (IRQ0 timer, IRQ1 keyboard only)
  4. `interrupts::init_pit()` (PIT channel 0 at `TIMER_HZ` = 100 Hz, i.e. 10 ms slices)
  5. frame allocator → `memory::FRAME_ALLOCATOR`
  6. page-table mapper → `memory::MAPPER`
  7. `allocator::init_heap` (needs both globals above)
  8. `scheduler::init()` (wraps the running context as the boot task and turns timer preemption
     on; allocates, so after the heap)
  9. `sti` (interrupts enabled last)

  **This ordering is required but only convention-enforced, not compiler-enforced** —
  `gdt::init()` must run first because the double-fault IDT entry references the TSS's IST, which
  must already be loaded; the heap needs the mapper and frame allocator live; the scheduler
  needs the heap; `sti` must come
  after every handler, the remapped PIC vectors and the PIT rate are in place, and after
  `scheduler::init()` so the timer ISR never lazily initializes (allocates) the scheduler. `init` takes individual
  `BootInfo` fields rather than `&BootInfo` so callers can still reborrow `boot_info.framebuffer`
  as `&'static mut` (see the doc comment on `kernel::init`).
- **`BOOTLOADER_CONFIG` gotcha**: any entry point that calls `kernel::init()` must register with
  `entry_point!(fn, config = &BOOTLOADER_CONFIG)`. That config maps all physical memory at
  `Mapping::FixedAddress(PHYSICAL_MEMORY_OFFSET)` (`0xffff_8000_0000_0000`, the start of the
  higher half) and pins the bootloader's dynamic mappings into the upper half; without it
  `physical_memory_offset` is absent and the `expect` in the entry point panics at boot. The kernel
  image itself is linked at `0xffff_ffff_8000_0000`, leaving the whole lower half free for future
  user address spaces (see the doc comments in `lib.rs`).
- **Memory**: `kernel/src/memory.rs` — `BootInfoFrameAllocator` over the bootloader's memory
  regions, plus an `OffsetPageTable` mapper built from the physical-memory offset; both live in
  `OnceCell<Mutex<_>>` globals (`FRAME_ALLOCATOR`, `MAPPER`). `kernel/src/allocator.rs` — the
  `#[global_allocator]` is `GuardedHeap`, a `linked_list_allocator::LockedHeap` wrapper (see
  "Interrupt context" below), at a fixed `HEAP_START = 0xffff_c000_0000_0000`,
  `HEAP_SIZE = 100 KiB`. Task stacks come out of this heap too (10 KiB each);
  `scheduler::MAX_TASKS = 8` encodes that budget, so `HEAP_SIZE`, `STACK_SIZE` and `MAX_TASKS`
  change together.
- **Tasks and scheduling (Layer 4)**: kernel threads on a shared page table, switched
  cooperatively (`yield_now`) or preempted by the IRQ0 tick.
  - `kernel/src/task.rs` — `Task`/`TaskId`/`TaskState`; heap-backed `STACK_SIZE` (10 KiB) stacks
    with a canary at the bottom (no guard page possible on heap memory); fallible
    `Task::try_with_stack`/`Task::spawn(entry)`.
  - `kernel/src/context.rs` — naked `switch(old_rsp, new_rsp)` saving only callee-saved registers
    (rbx, rbp, r12–r15) on the outgoing stack; `init_stack` builds a new task's first frame so the
    first `switch` `ret`s into `task_trampoline`: `sti` → `task_started` (`reap`) → `entry` → a
    return routes to `scheduler::exit`.
  - `kernel/src/scheduler.rs` — round-robin over a `lazy_static` `Mutex<Scheduler>` with
    `ready`/`finished` preallocated to `MAX_TASKS` (pushes never allocate). `spawn` returns
    `Result<TaskId, SpawnError>`; `exit()` ends a task from any call depth; `Scheduler::rotate` is
    the one selection step, shared by `exit` and by `select_next` (used by `yield_now` and the
    timer ISR);
    `set_preemption(bool)` toggles preemption. Invariants:
    - the lock is **always dropped before `switch`** (the resumed task would deadlock on it);
    - `yield_now`/`exit` keep IF=0 from selection through `switch` (a tick in that window would
      re-select before `saved_rsp` is written);
    - IF is per-task state: a preempted task gets it back via `iretq`, a yielder via its own
      `without_interrupts`, a fresh task via the trampoline's `sti`;
    - a finished task's stack is freed by *another* task (`reap`: end of `yield_now`, start of
      `spawn`, `task_started`), never by code running on it and never in an ISR.
- **Interrupt context**: every handler creates `interrupts::IsrGuard` as its first statement
  (bumps `ISR_DEPTH`). `GuardedHeap` panics on alloc/free while `in_interrupt()`, and takes the
  heap lock only under `without_interrupts`. `ISR_DEPTH` describes the running context, so an
  ISR-driven task switch goes through `interrupts::switch_from_isr` (swap to 0 → `switch` →
  restore), never a plain `switch` or an early guard drop. An ISR may take only locks mainline
  holds with IF=0 (`SCHEDULER`, and `PICS`, so any mainline `PICS.lock()` after `sti` needs
  `without_interrupts`), and sends EOI before switching.
- **Text output**: the kernel renders text by software-blitting glyphs onto a UEFI GOP pixel
  framebuffer (`kernel/src/framebuffer.rs`, via `noto-sans-mono-bitmap`), not the legacy VGA text
  buffer at `0xb8000` — that address doesn't exist under a UEFI-only `bootloader` 0.11+ setup. See
  `docs/superpowers/specs/2026-07-09-vga-text-mode-design.md` for the full rationale (this one
  file *is* tracked in git despite `/docs` being gitignored — it was committed before the ignore
  rule). Serial output (`kernel/src/serial.rs`, `uart_16550`) is what tests report through.
- **Testing**: unit tests (`#[test_case]`) in the `lib.rs` and `main.rs` test binaries, plus fourteen
  integration test binaries under `kernel/tests/`, each with different semantics. Five
  (`should_panic`, `stack_overflow`, `page_fault`, `isr_heap_safety`, `general_protection`) set `harness = false` in
  `kernel/Cargo.toml`; the rest use the normal custom test harness. Tests that assert an exact
  interleaving (`cooperative_yield`, `run_queue`, `task_exit`) call
  `scheduler::set_preemption(false)` after `kernel::init()`; a tick would otherwise reorder them. **Heap-allocating tests must go in an integration
  binary, not the lib unit-test binary**: the `memory.rs` frame-allocator tests corrupt the lib
  binary's heap for any test that runs after them (design-review item 15).
  - `basic_boot.rs` — smoke test; framebuffer init only, does **not** call `kernel::init()`
    (and doesn't pass `BOOTLOADER_CONFIG`, so it couldn't without also adding that).
  - `heap_allocation.rs` — full `kernel::init()` (heap included, so it passes
    `BOOTLOADER_CONFIG`), then exercises `Box`/`Vec`/`String` end-to-end.
  - `should_panic.rs` — inverted: reaching its panic handler *is* success.
  - `stack_overflow.rs` — calls only `gdt::init()` + a test-local `TEST_IDT` to verify the IST
    double-fault mechanism catches a blown stack instead of triple-faulting.
  - `page_fault.rs` — calls only `gdt::init()` + a test-local `TEST_IDT` whose `#PF` handler exits
    QEMU with success, verifying a write to an unmapped address is caught.
  - `higher_half.rs` — full `kernel::init()`; asserts kernel code, stack, heap, framebuffer and the
    physical-memory map all live in the upper half and the lower-half PML4 entries are free.
  - `task_stacks.rs` — full `kernel::init()`; per-task stacks are distinct, 16-aligned, in bounds,
    canary-intact, and freed/reused across create-drop cycles.
  - `context_switch.rs` — full `kernel::init()`; round-trips `switch` into an `init_stack` frame
    and back, checking callee-saved registers survive and the task's stack is aligned.
  - `cooperative_yield.rs` — full `kernel::init()`; the W1 deliverable proof: two/three threads
    log and `yield_now` in an exact interleaving (`A B A B A B`), including early-return tasks.
  - `run_queue.rs` — full init; exact `1234` order with four tasks; `spawn` past the cap and on
    heap exhaustion errors, then recovers.
  - `task_exit.rs` — full init; `exit()` explicit and nested, a fresh task sees no pending finished
    tasks, heap returns exactly to baseline (`allocator::used_bytes`) after spawn/exit cycles.
  - `isr_heap_safety.rs` — full init, then a test-local `int3` handler allocates under an
    `IsrGuard`; reaching the panic handler with the interrupt-context message is success.
  - `general_protection.rs` — full init, keeping the kernel's real IDT; a non-canonical read
    must reach the panic handler with the `#GP` handler's kernel-mode message (not a `#DF`).
  - `preemption.rs` — full init, preemption on; the W2 deliverable proof: CPU-bound
    never-yielding tasks hit exact totals within a tick budget, all GPRs survive preemption, heap
    churn, fresh-task IF, preemption-off never switches, and cooperative + preempted tasks mixed.

## Known design debt

`docs/design-review-2026-07-21.md` is a self-authored punch list of known rough edges (duplicated
boot-sequence logic across `main.rs`/`lib.rs`/`tests/*`, the implicit `kernel::init` ordering
dependency, duplicated IDT-building boilerplate in the test binaries, memory/heap items, a few
others). Treat it as a living document — the `kernel-docs-sync` agent keeps it in sync as
findings get addressed. Items 15–23 cover Layer 4 debt: W1 15–19 (lib-test heap corruption, no stack
guard page, stack size, `Task` getter visibility; 18, lazy reaping, is addressed) and W2 20–23
(preempted-only tasks don't reap, Ring 3 needs a per-task RSP0, preemption test gaps). Item 6's
heap-in-ISR hazard is addressed. Unlike the spec above, this file is untracked (`/docs` is gitignored), so
it's a local reference only and won't exist on a fresh clone.

## Conventions

- Doc comments are sparse and rationale-first — explain *why*, not *what* (well-named identifiers
  already say what). Compare `kernel/src/gdt.rs`: "Must run before `interrupts::init_idt`, which
  relies on the TSS's IST already being live." This project intentionally comments more than
  typical defaults elsewhere, since it's a learning-focused capstone — don't strip comments down
  to nothing.
- Shared/static state uses `lazy_static!` + `spin::Mutex`/`conquer_once::spin::OnceCell`, not bare
  `static mut`. Other sanctioned forms: atomics (`TICKS`, `ISR_DEPTH`, `PREEMPTION`), the
  `#[global_allocator]` `GuardedHeap` (its `LockedHeap` has its own internal lock), and test-local `lazy_static!` IDTs in integration tests. The one `static mut` exception,
  `gdt.rs`'s IST guard stack, is accessed only via `&raw const`/`&raw mut`, never a reference —
  see `kernel-safety-reviewer` for the full rule.

## Current phase

As of 2026-10-05 the capstone has started: active work is **Layer 4+**, built on top of the
finished tutorial layers.

- **Layer 1-3 (foundation, still fair game to fix/refactor):** bootloader/boot handoff,
  framebuffer + serial output, GDT/IDT + CPU exceptions, hardware interrupts
  (PIC/keyboard/timer), physical memory manager, paging/virtual memory, kernel heap allocator.
- **Layer 4+ (current focus):** process control blocks, context switching, scheduling,
  Ring 3 / privilege separation, syscalls, shell.

Completion status is tracked on the project board (below) and in the burndown sheet and proposal
doc (outside this repo) — not in this file. If status matters for a task, check the board or ask
the user rather than assuming.

## Working style

- **Project board**: [Capstone Project Board](https://github.com/users/xd-dbug/projects/2)
  (user project #2, owner `xd-dbug`; issues live in the `xd-dbug/Capstone` repo, which is this
  repo's `origin`). When asked to start working on the project, check the board first for the
  current week's tasks — items are grouped by the `Week` iteration field, with weekly parent
  issues titled `W<n>: ...` and their sub-issues. Via CLI:
  `gh project item-list 2 --owner xd-dbug`.
- **Teaching mode is the default.** Unless explicitly asked to write the code, the task is to help
  with learning and understanding: don't default to giving answers — guide toward the correct
  answer, use pseudocode to build understanding, and only show real code once the user
  understands the approach. This also governs subagents: don't delegate to
  `kernel-feature-implementer` (which writes code) unless the user has explicitly asked for the
  code to be written; `kernel-concept-explainer` fits teaching mode.

## Available subagents

| Agent | Use for |
|---|---|
| `kernel-feature-implementer` | Writing new kernel features/subsystems that match existing conventions (only when code was explicitly requested — see "Working style") |
| `kernel-safety-reviewer` | Reviewing unsafe Rust / interrupt / GDT / PIC correctness (auto-suggests itself on risky diffs) |
| `kernel-concept-explainer` | Learning how/why a kernel subsystem works |
| `kernel-docs-sync` | Updating doc comments and reconciling `docs/design-review-2026-07-21.md` |
| `qemu-test-diagnostician` | Diagnosing a failed or hung `cargo test` run (auto-suggests itself on test failure) |
