use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor};
use x86_64::structures::gdt::SegmentSelector;
use core::cell::UnsafeCell;
use lazy_static::lazy_static;

/// Which TSS IST slot holds the double-fault stack. Shared with
/// `interrupts.rs` and `tests/stack_overflow.rs`, whose IDTs must point their
/// `#DF` entries at the same slot this module populates.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// `UnsafeCell` rather than a `static mut` (project convention) or a `Mutex`:
/// the CPU reads the TSS straight from memory on a ring 3 -> 0 transition and
/// knows nothing of our lock, and the cell tells the compiler those bytes can
/// change behind a shared reference, so it can't cache or elide the writes.
struct Tss(UnsafeCell<TaskStateSegment>);

// Safety: single core. The kernel writes the TSS only from ring 0 with IF=0
// (see `set_rsp0`), so the only other reader, the CPU taking an interrupt from
// ring 3, can't run mid-write. Nothing ever forms a `&TaskStateSegment`; all
// access goes through the raw pointer from `UnsafeCell::get`.
unsafe impl Sync for Tss {}

// Task State Segment: on x86_64 its only real job here is to hold the
// Interrupt Stack Table, an alternate stack the CPU switches to for chosen
// exceptions (see DOUBLE_FAULT_IST_INDEX) regardless of the current stack's state.
lazy_static! {
    static ref TSS: Tss = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

            // Stacks grow downward, so the top of the guard stack is the *end* of the array.
            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            let stack_end = stack_start + STACK_SIZE as u64;
            stack_end
        };
        Tss(UnsafeCell::new(tss))
    };
}

/// Sets the stack the CPU switches to on a ring 3 -> 0 interrupt. Callers
/// must have IF=0: RSP0 has to change in the same uninterrupted step as the
/// scheduler's choice of the next task, or a tick in between would leave it
/// pointing at the wrong task's stack.
pub fn set_rsp0(top: VirtAddr) {
    debug_assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "set_rsp0 with interrupts enabled"
    );
    // Safety: the `Sync` invariant on `Tss` holds here (IF=0, one core). Raw
    // place assignment, so no reference to the TSS exists alongside the write
    // (and the packed field is stored unaligned-safely by the compiler).
    unsafe { (*TSS.0.get()).privilege_stack_table[0] = top; }
}

/// Current RSP0, for tests to check that the scheduler keeps it on the
/// running task's kernel stack.
pub fn rsp0() -> VirtAddr {
    // Safety: a plain copy out through the raw pointer; no reference is formed.
    unsafe { (*TSS.0.get()).privilege_stack_table[0] }
}

// Global Descriptor Table: legacy x86 segmentation structure that long mode
// still requires a minimal instance of. Built alongside its selectors so both
// stay in sync (the selectors are meaningless without this exact GDT loaded).
//
// Order is fixed by `syscall`/`sysret` (W4): STAR gives one base for kernel
// (CS = base, SS = base+8) and one for user (SS = base+8, CS = base+16, in
// 64-bit `sysret`), hence kernel code, kernel data, user data, user code.
lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        // Safety: `TSS` is a `'static` that is never moved or dropped, so the
        // base address the descriptor records stays valid for the CPU's reads.
        let tss = gdt.append(unsafe { Descriptor::tss_segment_unchecked(TSS.0.get()) });
        (gdt, Selectors { kernel_code, kernel_data, user_data, user_code, tss })
    };
}

struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
    user_data: SegmentSelector,
    user_code: SegmentSelector,
    tss: SegmentSelector,
}

/// Kernel code selector (RPL 0).
pub fn kernel_code_selector() -> SegmentSelector {
    GDT.1.kernel_code
}

/// Kernel data selector (RPL 0).
pub fn kernel_data_selector() -> SegmentSelector {
    GDT.1.kernel_data
}

/// User data selector (RPL 3), for the SS/DS an `iretq` into ring 3 loads.
pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data
}

/// User code selector (RPL 3), for the CS an `iretq` into ring 3 loads.
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code
}

/// Loads the GDT and points the segment registers and TSS at its selectors.
/// Must run before `interrupts::init_idt`, which relies on the TSS's IST
/// already being live.
pub fn init() {
    use x86_64::instructions::tables::load_tss;
    use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        load_tss(GDT.1.tss);

        // GDT.0.load() only updates GDTR; segment registers keep whatever
        // selector value they already held from the bootloader's own GDT.
        // Those old values now alias whatever sits at the same index in *our*
        // GDT (possibly a user or TSS descriptor) instead of a kernel data
        // segment. SS isn't validated until it's next reloaded, which `iretq`
        // does on every interrupt return, so a stale value would #GP on the
        // first one. NULL would be legal for SS at CPL0 in long mode (the CPU
        // loads it itself on a ring 3 -> 0 interrupt), but a real kernel data
        // descriptor matches the selector `syscall` will load from STAR. DS/ES
        // get the same so no stale index lingers.
        SS::set_reg(GDT.1.kernel_data);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
    }
}
