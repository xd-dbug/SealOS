# SealOS

A hobby x86_64 kernel written in Rust, built as pre-capstone / capstone (PRO390) coursework. Boots via UEFI in QEMU, with a security-conscious design target (privilege separation, syscalls, and — as stretch goals — capability-based access control and ASLR).

## Status

**Working right now:**
- Boots via UEFI (OVMF firmware) in QEMU to a graphical framebuffer
- Text output to the framebuffer (`println!`/`print!`) via software-rendered glyphs — not legacy VGA text mode, which isn't available under UEFI/GOP boot (see `docs/superpowers/specs/2026-07-09-vga-text-mode-design.md` for why)
- Serial output over COM1 (`serial_println!`/`serial_print!`) via `uart_16550`
- GDT with a dedicated Interrupt Stack Table (IST) entry for double faults
- IDT with breakpoint, double-fault, and page-fault handlers, plus timer (IRQ0, PIT at 100 Hz) and keyboard (IRQ1) handlers behind remapped PICs
- Physical memory management (bump + free-list physical frame allocator), paging (`OffsetPageTable` virtual memory mapper), and a kernel heap allocator (`linked_list_allocator`, exposing `Box`/`Vec`/`String` via `extern crate alloc`) — Layers 2–3 complete
- Higher-half kernel: the kernel image is linked at `0xffff_ffff_8000_0000` (PML4 entry 511), all physical memory is mapped at `0xffff_8000_0000_0000`, and the heap lives at `0xffff_c000_0000_0000`, leaving the whole lower half free for future user address spaces
- Kernel threads (Layer 4, Weeks 1-2): heap-backed task stacks, a naked-asm context switch, and a round-robin scheduler with a capped (`MAX_TASKS = 8`), allocation-free run queue; tasks can yield cooperatively, exit, or be preempted by the 10 ms timer tick. Heap use inside an interrupt handler is a checked panic (`allocator::GuardedHeap`)
- A custom `#[no_std]` test harness: unit tests inside `kernel/src/`, plus integration tests (`basic_boot`, `should_panic`, `stack_overflow`, `page_fault`, `heap_allocation`, `higher_half`, `task_stacks`, `context_switch`, `cooperative_yield`, `run_queue`, `task_exit`, `preemption`, `isr_heap_safety`, `general_protection`) that boot a real kernel image in QEMU and report pass/fail over the `isa-debug-exit` device
- `cargo run` and `cargo test` both work end-to-end, cross-compiling the kernel and launching QEMU automatically

**Not implemented yet:**
- A `#[test_case]` proving the keyboard handler decodes scancodes correctly (the handler itself works interactively; see the "Done" table below)
- Clearing the bootloader's leftover lower-half identity mapping (PML4[0]) before per-process page tables (see the "Done" table below)
- Per-task address spaces, privilege separation (Ring 3), syscalls, and a shell (capstone-proper deliverables). Tasks are kernel threads on one shared page table, and their stacks have no guard page (an overflow is only caught by a canary check at switch time)

If something in the code looks unfinished or stubbed, it probably is — this project is under active, weekly development. Check the commit history rather than assuming this list is current by the time you read it.

 **Done**

Every subsystem in `kernel/src/` checked line-by-line against the actual repo state and its passing tests, since "done" is easy to over-claim from memory. Organized by the pre-capstone Layer 1–3 scope (bootloader, framebuffer/serial, GDT/IDT, hardware interrupts, physical memory, paging, heap). Each row cites the code/test that backs it — recheck it yourself if it matters for something you're relying on, since this decays the same way the bullets above do. Layer 4+ (kernel-thread scheduling, Ring 3, syscalls, a shell) started 2026-10-05; only the kernel-thread scheduling groundwork (Weeks 1-2: the bullet above and the "Testing" list below) exists so far and isn't evaluated in these tables.

Current test suite backing all the "proven" claims below: **51 tests across 15 test binaries** (`cargo test` from `kernel/`), all passing as of this check.

**1. Bootloader / boot handoff** — done:

| Item | Status | Evidence |
|---|---|---|
| UEFI boot via `bootloader`/OVMF | Done | `bootloader = { version = "0.11.16", features = ["uefi"] }`; `entry_point!(kernel_main, config = &BOOTLOADER_CONFIG)` in `main.rs` |
| `BootInfo` handoff (framebuffer, physical-memory offset, memory map) | Done | Consumed in `main.rs::kernel_main` and every `kernel/tests/*.rs` entry point |
| Boot ordering enforced (`gdt` → IDT → PIC → PIT → frame allocator → mapper → heap → scheduler → `sti`) | Done | `kernel::init()` in `lib.rs`; order is convention-enforced only, not type-enforced (see `docs/design-review-2026-07-21.md` item 3 — local file, not in git) |
| Kernel loaded in the higher half of the address space | Done | See item 6 below; the target spec links the kernel at `0xffff_ffff_8000_0000` |

**2. Framebuffer + serial output** — fully done:

| Item | Status | Evidence |
|---|---|---|
| UEFI GOP framebuffer text writer | Done | `framebuffer.rs`: software glyph blitting via `noto-sans-mono-bitmap`, not legacy VGA text mode (unavailable under UEFI/GOP — see `docs/superpowers/specs/2026-07-09-vga-text-mode-design.md`) |
| `print!`/`println!` macros | Done | `framebuffer.rs`, guarded with `without_interrupts` so the timer ISR's own `print!` can't deadlock against a held lock |
| Serial output over COM1 | Done | `serial.rs` via `uart_16550`; `serial_print!`/`serial_println!` macros, same `without_interrupts` guard |
| Proven working | Done | `test_println_simple`, `test_println_many` (`framebuffer.rs`); serial output additionally exercised implicitly by every integration test's `[ok]`/`[failed]` report |

**3. GDT / IDT / CPU exceptions** — done, with one known gap:

| Item | Status | Evidence |
|---|---|---|
| GDT with kernel code segment + TSS | Done | `gdt.rs` |
| Dedicated IST stack for double faults | Done | `gdt.rs`'s `DOUBLE_FAULT_IST_INDEX`, wired to `interrupts.rs`'s `double_fault` handler |
| Breakpoint (`#BP`) handler | Done | `interrupts.rs`; proven by `test_breakpoint_exception` |
| Double fault (`#DF`) handler | Done | Proven by `kernel/tests/stack_overflow.rs` (deliberate stack overflow, confirmed caught rather than triple-faulting) |
| Page fault (`#PF`) handler | Done | Proven by `kernel/tests/page_fault.rs` (deliberate unmapped-address access, confirmed caught rather than triple-faulting/hanging) |
| General-protection fault (`#GP`) handler | Done | `interrupts.rs`; reports the selector error code and whether ring 0 or ring 3 faulted, then panics. Proven by `kernel/tests/general_protection.rs` (non-canonical access, confirmed caught by the real handler rather than escalating to `#DF`) |

**4. Hardware interrupts (PIC / timer / keyboard)** — implemented, keyboard unproven by automated test:

| Item | Status | Evidence |
|---|---|---|
| PIC remapping (vectors 32/40, off the colliding BIOS defaults) | Done | `interrupts.rs`'s `PIC_1_OFFSET`/`PIC_2_OFFSET`, `PICS.lock().initialize()` in `kernel::init()` |
| Timer (IRQ0) handler + EOI | Done | `timer_interrupt_handler`, `AtomicU64` tick counter, PIT programmed to 100 Hz by `init_pit`; proven by `test_timer_ticks_increase` (and, for preemption, `kernel/tests/preemption.rs`) |
| Keyboard (IRQ1) handler + EOI | Implemented, not test-proven | `keyboard_interrupt_handler` decodes PS/2 scancodes via `pc-keyboard` and prints the result — works interactively (`cargo run`), but no `#[test_case]` injects a scancode and asserts on the decoded output, unlike every other subsystem here |

**5. Physical memory manager** — fully done:

| Item | Status | Evidence |
|---|---|---|
| Bump allocator over `Usable` `BootInfo` regions | Done | `memory.rs`'s `BootInfoFrameAllocator` |
| Free-list reclamation (`FrameDeallocator`) | Done | Same type; freed frames store their own next-pointer in their backing memory |
| Proven working | Done | `test_allocate_frames_distinct_and_aligned`, `test_allocate_across_region_boundary`, `test_deallocate_frame_is_reused_before_bump_cursor_advances` |

**6. Paging / virtual memory** — done (higher-half kernel placement added since the earlier audit), with one known gap (PML4[0]). Two things get called "paging"; both are now proven:

| Item | Status | Evidence |
|---|---|---|
| Physical-memory offset mapping | Done | `BOOTLOADER_CONFIG` opts into `Mapping::FixedAddress(PHYSICAL_MEMORY_OFFSET)` (`0xffff_8000_0000_0000`); `memory::init` builds an `OffsetPageTable` over it |
| Frame allocator backing new mappings | Done | `BootInfoFrameAllocator`, exercised by 3 passing tests above plus `test_map_unused_page_and_translate_write`'s live `map_to`/`unmap` round-trip |
| Every `map_to`/`unmap` call site flushes correctly | Done | Audited this session (`memory.rs`, `allocator.rs`) — all three call sites flush appropriately for how the mapping is used afterward |
| Recursive page tables | N/A by design | This project uses `OffsetPageTable` via the bootloader's fixed-address physical-memory mapping instead — a second, unused implementation was deliberately skipped, not a gap |
| Kernel itself loaded in the higher half | Done | `kernel/x86_64-seal_os.json` sets `code-model: kernel`, `relocation-model: static`, and `--image-base=0xffffffff80000000` (static ET_EXEC), so the kernel loads at the top 2 GiB. `BOOTLOADER_CONFIG` pins the physical-memory map to `0xffff_8000_0000_0000` and the bootloader's dynamic mappings (stack, boot info, framebuffer) into PML4 entries 257..384 (half-open). Proven by `kernel/tests/higher_half.rs` |
| Address-space layout | Done | Lower half (PML4 0..256, half-open) reserved for user space; phys map at 256; bootloader dynamic mappings 257..384; heap (`HEAP_START = 0xffff_c000_0000_0000`) at 384; kernel image at 511 |
| Bootloader's PML4[0] identity mapping | Known gap | The bootloader's identity mapping of its `context_switch` code and GDT frame is still present in the lower half; safe to clear after `gdt::init()`, but must be dealt with before Ring 3 per-process page tables |

**7. Kernel heap allocator** — fully done:

| Item | Status | Evidence |
|---|---|---|
| Heap region reserved | Done | `HEAP_START`/`HEAP_SIZE` constants in `kernel/src/allocator.rs` |
| Heap actually mapped | Done | `init_heap`'s per-page `Mapper::map_to` loop, backed by `FRAME_ALLOCATOR`, called once from `kernel::init()` |
| Allocator implementation | Done | `linked_list_allocator::LockedHeap` (the linked-list option of the three Oppermann covers) |
| Registered as global | Done | `#[global_allocator] static ALLOCATOR: GuardedHeap`, a `LockedHeap` wrapper that panics on heap use from interrupt context |
| `alloc` crate usable | Done | `extern crate alloc;` in `lib.rs`; no explicit `#[alloc_error_handler]` — this toolchain's default OOM-abort behavior (stable since ~Rust 1.68) covers it, confirmed by a clean `cargo build` with no missing-lang-item error |
| Proven working | Done | 7 `#[test_case]`s in `kernel/src/allocator.rs` (`Box`/`Vec` round-trips, a 10,000-cycle stress test, an alignment audit, a fragmentation check) plus 3 more in the dedicated-boot `kernel/tests/heap_allocation.rs` (`Box`, `Vec`, `String`) |

**Not evaluated (out of scope, Layer 4+):** the Week 1-2 kernel-thread scheduler (`task.rs`, `context.rs`, `scheduler.rs`) is covered only by its integration tests, not by the tables above; Ring 3/privilege separation, syscalls and a shell don't exist in the repo yet — not a gap in this checklist, just outside what pre-capstone is claiming.

## Architecture

Two independent Cargo projects live in this repo:

```
.
├── Cargo.toml          # "runner" — a std host binary
├── build.rs             # cross-compiles kernel/ as part of the runner's own build
├── src/main.rs           # builds a UEFI disk image and launches QEMU
└── kernel/
    ├── Cargo.toml       # "kernel" — the actual no_std, no_main OS
    ├── .cargo/config.toml   # points kernel's `cargo test` runner back at ../target/debug/runner
    ├── x86_64-seal_os.json  # custom target spec (softfloat, no SSE/MMX, no red zone, kernel code model, linked at `0xffff_ffff_8000_0000`)
    └── src/
        ├── main.rs       # kernel entry point
        ├── lib.rs        # shared init, panic handling, test harness
        ├── framebuffer.rs
        ├── serial.rs
        ├── gdt.rs
        ├── interrupts.rs
        ├── memory.rs     # physical frame allocator + OffsetPageTable mapper
        ├── allocator.rs  # kernel heap (#[global_allocator])
        ├── task.rs       # Task/TaskId, heap-backed stacks
        ├── context.rs    # naked context switch, new-task trampoline
        └── scheduler.rs  # round-robin run queue, spawn/yield/exit, timer preemption hook
```

**Why two crates:** `kernel` needs its own target spec, its own `build-std` configuration, and `no_std`. Keeping it a fully separate Cargo project (its own `target/` directory, its own `.cargo/config.toml`) avoids a workspace deadlock where a nested `cargo build` inside `build.rs` blocks on the same build lock as the outer build.

**The `runner` crate does double duty:**
1. Run directly (`cargo run` at the repo root) — `build.rs` cross-compiles the kernel, and `runner` boots the resulting ELF in QEMU.
2. Run indirectly, as the `runner` configured in `kernel/.cargo/config.toml` — when you `cargo test` inside `kernel/`, Cargo invokes the prebuilt `../target/debug/runner` binary with the test ELF's path as an argument. `runner` detects test binaries (they live under a `deps/` directory) and wires up the `isa-debug-exit` QEMU device plus headless serial output, translating the VM's exit code back into a `cargo test` pass/fail.

**UEFI only.** BIOS boot is not supported — `bootloader = "0.11"` is configured with `default-features = false, features = ["uefi"]`, matching the framebuffer-only design above.

## Requirements

- **Rust nightly**, pinned via `rust-toolchain.toml` (currently `nightly-2026-07-19`) with the `rust-src` component — `rustup` will fetch this automatically the first time you build
- **QEMU** (`qemu-system-x86_64` on your `PATH`)
- **OVMF UEFI firmware** — `src/main.rs` currently expects it at `/usr/share/OVMF/x64/OVMF_CODE.4m.fd` and `/usr/share/OVMF/x64/OVMF_VARS.4m.fd` (the Arch `edk2-ovmf` package layout; other distros, e.g. Debian/Ubuntu's `ovmf`, lay the files out differently). If your firmware lives elsewhere, you'll need to edit those constants directly for now — there's no environment-variable override yet.

## Build & run

```sh
# Boot the kernel in QEMU (builds kernel/ first via build.rs)
cargo run

# Run the kernel's unit + integration tests (also boots QEMU, headless,
# and reports results via the isa-debug-exit device)
cd kernel && cargo test
```

Both commands cross-compile against the custom `x86_64-seal_os.json` target using `-Z build-std`, which is why the toolchain must be nightly with `rust-src` installed.

## Testing

Tests run inside the actual kernel environment rather than on the host, since most of this code can't run under a normal OS. Two kinds exist:

- **Unit tests** (`#[test_case]` functions inside `kernel/src/`, e.g. `framebuffer.rs`, `interrupts.rs`, `allocator.rs`, `memory.rs`) — compiled into the kernel binary itself under `cfg(test)`
- **Integration tests** (`kernel/tests/*.rs`) — each is its own tiny kernel image with its own entry point, boots independently in QEMU:
  - `basic_boot.rs` — smoke test that the kernel reaches framebuffer init and can print
  - `should_panic.rs` — confirms a deliberately failing assertion is correctly detected as a failure
  - `stack_overflow.rs` — deliberately overflows the kernel stack and confirms it's caught as a double fault via the IST-backed handler, rather than triple-faulting the VM
  - `page_fault.rs` — deliberately dereferences an unmapped address and confirms it's caught as a page fault rather than triple-faulting/hanging the VM
  - `higher_half.rs` — asserts the kernel stack, boot info, framebuffer, heap, and physical-memory map all live in the upper half, and that PML4 entries 256/384/511 are populated
  - `heap_allocation.rs` — runs a full `kernel::init()` (heap included) and exercises `Box`/`Vec`/`String` end-to-end through the real global allocator
  - `task_stacks.rs` — per-task stacks are distinct, 16-aligned, in bounds, canary-intact, and freed/reused across create-drop cycles
  - `context_switch.rs` — round-trips `switch` into a fresh task frame and back, checking callee-saved registers survive
  - `cooperative_yield.rs` — two/three threads yield in an exact interleaving, including early-return tasks
  - `run_queue.rs` — exact round-robin order, and `spawn` erroring (then recovering) past `MAX_TASKS` and on heap exhaustion
  - `task_exit.rs` — explicit `exit()` (also from a nested call), prompt reaping, and heap usage returning exactly to baseline after repeated spawn/exit
  - `preemption.rs` — CPU-bound tasks that never yield still all progress under the timer tick, with all registers surviving preemption, heap churn, and cooperative and preempted tasks mixed
  - `isr_heap_safety.rs` — confirms that allocating from interrupt context is detected and panics, while normal allocation still works

A test binary reports success or failure by writing an exit code to QEMU's `isa-debug-exit` I/O port; `runner` reads QEMU's process exit code and translates it back into something `cargo test` understands.

## Roadmap

Pre-capstone (Layers 1–3, done apart from the gaps listed under "Not implemented yet") → capstone (Weeks 1–10, now underway): process scheduling, Ring 3 privilege separation, syscalls, a basic shell. Capability-based access control, memory-safe syscalls, and ASLR are stretch goals. See the capstone proposal doc for the full schedule.

## License

MIT — see [LICENSE](LICENSE).
