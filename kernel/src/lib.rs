#![no_std]
#![cfg_attr(test, no_main)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner)]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)] // `Box::try_new`: fallible task allocation in `scheduler::spawn`
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

#[cfg(test)]
use bootloader_api::BootInfo;
use bootloader_api::{config::Mapping, info::MemoryRegions, BootloaderConfig};
use core::panic::PanicInfo;
use x86_64::VirtAddr;
use spin::Mutex;

pub mod allocator;
pub mod framebuffer;
pub mod interrupts;
pub mod serial;
pub mod gdt;
pub mod memory;
pub mod task;
pub mod context;
pub mod scheduler;
pub mod user;
pub mod syscall;

/// Start of the upper (kernel) half of the canonical address space. Everything
/// the kernel owns lives at or above this so the whole lower half
/// (`0..0x0000_8000_0000_0000`) stays free for per-process user address
/// spaces, and the kernel's PML4 entries (256..512) can be shared into each.
pub const KERNEL_HALF_START: u64 = 0xffff_8000_0000_0000;

/// Where the bootloader maps all physical memory. PML4 entry 256, the very
/// first upper-half slot; a fixed value (not `Dynamic`) so the layout is
/// reproducible and the bootloader reserves exactly this entry.
/// (PML4 ranges below are half-open, `start..end` excluding `end`.)
pub const PHYSICAL_MEMORY_OFFSET: u64 = KERNEL_HALF_START;

/// Opts into the bootloader mapping all physical memory at
/// `PHYSICAL_MEMORY_OFFSET`, which the frame allocator and page-table walker
/// need to turn physical addresses into dereferenceable pointers, and pins
/// every other bootloader mapping (kernel stack, boot info, framebuffer) into
/// the dynamic range. That range nominally spans PML4 entries 256..384
/// (half-open: 256 through 383), but the physical-memory map already claims
/// 256 and the bootloader hands out whole entries, so in practice the dynamic
/// mappings land in 257..384. Entries 384..511 (384 through 510) are left for the kernel's own use, e.g.
/// the heap at `allocator::HEAP_START`; entry 511 holds the kernel image (linked at
/// `0xffff_ffff_8000_0000`). Both real and test entry points must pass this
/// explicitly via `entry_point!`.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::FixedAddress(PHYSICAL_MEMORY_OFFSET));
    config.mappings.dynamic_range_start = Some(KERNEL_HALF_START);
    config.mappings.dynamic_range_end = Some(0xffff_bfff_ffff_f000);
    config
};

/// Anything that can report itself as a named, pass/fail test over serial.
pub trait Testable {
    fn run(&self) -> ();
}

/// Brings up CPU-level state needed before anything else can run safely:
/// segment/TSS descriptors, then the interrupt handlers that depend on them,
/// the `syscall`/`sysret` MSRs (built from the same selectors), then the PICs
/// (remapped to vectors 32+ so they can't collide with CPU exception vectors
/// 0-31) and the PIT that drives IRQ0, then the frame
/// allocator, page-table mapper, and heap (in that order, each needing the
/// one before it), the scheduler (needs the heap; turns timer preemption on),
/// and finally
/// interrupts themselves — enabling them any earlier would let a hardware IRQ
/// arrive before its handler or the remapped PIC vectors are in place.
///
/// Takes the specific `BootInfo` fields the memory subsystem setup (the
/// frame allocator and page-table mapper built below) needs, rather than
/// `&'static BootInfo` itself: each entry point also reborrows
/// `boot_info.framebuffer` as `&'static mut` for `framebuffer::init`, and
/// the borrow checker cannot prove that borrow is disjoint from a
/// *whole-struct* reference — only from direct projections of other
/// individual fields.
pub fn init(physical_memory_offset: u64, memory_regions: &'static MemoryRegions) {
    gdt::init();
    interrupts::init_idt();
    // STAR is built from the GDT's selectors, so after `gdt::init`.
    syscall::init();
    unsafe { interrupts::PICS.lock().initialize() };
    // Unmask only IRQ0 (timer, bit 0) and IRQ1 (keyboard, bit 1) on the
    // master PIC; the slave PIC's cascade line (IRQ2) and everything else
    // stays masked since no other device is wired up yet.
    unsafe { interrupts::PICS.lock().write_masks(0xFC, 0xFF) }
    // Before `sti` so the very first tick already arrives at `TIMER_HZ`.
    interrupts::init_pit();
    let allocator = unsafe { memory::BootInfoFrameAllocator::init(memory_regions, VirtAddr::new(physical_memory_offset)) };
    memory::FRAME_ALLOCATOR.init_once(|| Mutex::new(allocator));
    memory::MAPPER.init_once(|| {
        // Safety: `BOOTLOADER_CONFIG` opts into
        // `Mapping::FixedAddress(PHYSICAL_MEMORY_OFFSET)`, so the entire physical address space really is mapped at this offset.
        // `OnceCell::init_once` only ever runs this closure once, which is
        // what makes it sound to call `memory::init` here rather than
        // outside the closure — a second call would alias the `&mut
        // PageTable` `memory::init` builds internally.
        let mapper = unsafe { memory::init(VirtAddr::new(physical_memory_offset)) };
        Mutex::new(mapper)
    });
    // Needs both globals live (the mapper to create the heap's page-table
    // entries, the frame allocator to back them), so this can't move any
    // earlier.
    allocator::init_heap(
        &mut *memory::MAPPER.get().expect("MAPPER not initialized").lock(),
        &mut *memory::FRAME_ALLOCATOR
            .get()
            .expect("FRAME_ALLOCATOR not initialized")
            .lock(),
    )
    .expect("heap initialization failed");
    // Allocates the boot task's `Box`, so it must follow `init_heap`, and
    // must precede `sti`: it also switches timer preemption on, and the
    // timer ISR must never be the one to lazily initialize the scheduler.
    scheduler::init();
    x86_64::instructions::interrupts::enable();
}

// Blanket impl: any zero-arg fn (i.e. every `#[test_case]`) is Testable for free.
impl<T> Testable for T
where
    T: Fn(),
{
    fn run(&self) {
        serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        serial_println!("[ok]");
    }
}

/// Custom `#[test_runner]`: runs every collected test, then shuts QEMU down
/// with a success exit code (there's no OS underneath to return control to).
pub fn test_runner(tests: &[&dyn Testable]) {
    serial_println!("Running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

/// Shared `#[panic_handler]` for test builds: reports the failure over serial
/// and exits QEMU with a failure code so `cargo test` sees a non-zero result.
pub fn test_panic_handler(info: &PanicInfo) -> ! {
    serial_println!("[failed]\n");
    serial_println!("Error: {}\n", info);
    exit_qemu(QemuExitCode::Failed);
    loop {}
}

/// Values written to `isa-debug-exit`, which turns `v` into process exit code
/// `(v << 1) | 1`. `Success` therefore becomes 33, which `src/main.rs`'s
/// `QEMU_TEST_SUCCESS` must stay in sync with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    Success = 0x10,
    Failed = 0x11,
}

/// Writes to QEMU's `isa-debug-exit` I/O port to terminate the VM with a
/// specific exit code, standing in for a real "shutdown" syscall in tests.
pub fn exit_qemu(exit_code: QemuExitCode) {
    use x86_64::instructions::port::Port;

    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
}

/// Entry point for `cargo test` against this library itself.
#[cfg(test)]
fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("BOOTLOADER_CONFIG enables physical memory mapping");
    let memory_regions = &boot_info.memory_regions;

    if let Some(framebuffer) = boot_info.framebuffer.as_mut() {
        framebuffer::init(framebuffer);
    }
    init(physical_memory_offset, memory_regions);
    test_main();
    loop {}
}

#[cfg(test)]
bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    test_panic_handler(info)
}