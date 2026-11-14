//! Lower-half user mappings for the one shared page table (per-process
//! address spaces come later). Everything here is `USER_ACCESSIBLE`; every
//! kernel mapping stays supervisor-only.

use crate::elf::{Elf, LoadSegment, MAX_LOAD_SEGMENTS};
use crate::memory::{BootInfoFrameAllocator, FRAME_ALLOCATOR, MAPPER};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::mapper::{MapToError, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageSize, PageTable,
    PageTableFlags, PhysFrame, Size4KiB,
};

/// One past the highest user address: the lower canonical half ends here and
/// `crate::KERNEL_HALF_START` begins the upper one. Everything in between is
/// non-canonical.
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// Base of the user program's code page: PML4 index 1
/// (`0x0000_0080_0000_0000 >> 39 == 1`). Deliberately not PML4[0]: the
/// bootloader keeps its identity-mapped trampoline/GDT there, and mapping a
/// user page beneath an existing entry ORs `USER_ACCESSIBLE` into it, which
/// would expose that whole 512 GiB slot to Ring 3.
pub const USER_CODE_START: u64 = 0x0000_0080_0000_0000;

/// Pages in the user stack. A few is plenty for the W3 test programs.
pub const USER_STACK_PAGES: u64 = 4;

/// First stack page: PML4 index 1 as well, with an unmapped gap page after
/// the code (and below the stack) so a runaway stack or jump faults instead
/// of silently walking into the neighbouring region.
pub const USER_STACK_START: u64 = USER_CODE_START + 2 * Size4KiB::SIZE;

/// One past the last stack byte; the initial user RSP (pushes go below it).
pub const USER_STACK_TOP: u64 = USER_STACK_START + USER_STACK_PAGES * Size4KiB::SIZE;

/// Where `user/linker.ld` links ELF programs: 4 MiB into PML4[1], well clear
/// of the raw code page and the stack above it (`USER_CODE_START + 0..6`
/// pages). The stack stays at its fixed address so the raw-asm test programs
/// keep working unchanged; `elf::parse` rejects any segment that would
/// overlap it. Must match the linker script.
pub const USER_ELF_BASE: u64 = USER_CODE_START + 0x40_0000;
const _: () = assert!(USER_ELF_BASE > USER_STACK_TOP, "ELF base inside the stack");

/// Parent-table flags. The CPU ANDs U/S (and R/W) across the walk, so every
/// level must carry `USER_ACCESSIBLE`; `WRITABLE` here is harmless because the
/// leaf entry alone decides whether a page is writable.
const TABLE_FLAGS: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::USER_ACCESSIBLE);

const STACK_FLAGS: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::USER_ACCESSIBLE)
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::NO_EXECUTE);

/// Where a mapped user program starts and where its stack ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserImage {
    pub entry: VirtAddr,
    pub stack_top: VirtAddr,
}

#[derive(Debug)]
pub enum MapError {
    /// The program doesn't fit in the single code page.
    CodeTooLarge,
    /// No physical frame left for a page or a page table.
    OutOfFrames,
    Map(MapToError<Size4KiB>),
    Unmap(UnmapError),
}

/// Makes sure EFER.NXE is set. A page with `NO_EXECUTE` while NXE is off is a
/// reserved-bit #PF, so check rather than assume. The bootloader enables it
/// for its own mappings, but this is idempotent and costs one MSR read.
pub fn ensure_nx_enabled() {
    let efer = Efer::read();
    if !efer.contains(EferFlags::NO_EXECUTE_ENABLE) {
        // Safety: setting NXE only changes how bit 63 of page-table entries
        // is read, and with NXE off no present entry may have it set, so no
        // existing mapping changes meaning. Read-modify-write keeps LME, LMA
        // and SCE as they were.
        unsafe { Efer::write(efer | EferFlags::NO_EXECUTE_ENABLE) };
    }
}

fn stack_pages() -> impl Iterator<Item = Page<Size4KiB>> {
    (0..USER_STACK_PAGES)
        .map(|i| Page::containing_address(VirtAddr::new(USER_STACK_START + i * Size4KiB::SIZE)))
}

/// First and last page of a segment, inclusive. Stored instead of a
/// `PageRangeInclusive` so the record below is a plain `Copy` array.
type SegmentPages = (Page<Size4KiB>, Page<Size4KiB>);

fn segment_pages(segment: &LoadSegment) -> SegmentPages {
    let first = Page::containing_address(VirtAddr::new(segment.vaddr));
    // `memsz > 0` (`elf::parse` skips empty segments), so `end - 1` is the
    // segment's last byte.
    let last = Page::containing_address(VirtAddr::new(segment.end() - 1));
    (first, last)
}

/// The segment pages the current image maps (the stack is always the same
/// `stack_pages`). The previous image is no longer one fixed code page, so
/// unmapping needs this to free exactly what the last load mapped. Only ever
/// locked while `MAPPER` is held, which is what actually serialises it.
static LOADED: Mutex<[Option<SegmentPages>; MAX_LOAD_SEGMENTS]> =
    Mutex::new([None; MAX_LOAD_SEGMENTS]);

/// Entry point of the current image, for `user_task_entry`, which takes no
/// arguments (`scheduler::spawn` takes a plain `extern "C" fn()`). One slot is
/// enough because there is one image (design-review items 24/25): it is only
/// rewritten by the next load, which must not happen while a task of this one
/// is still alive.
static ENTRY: AtomicU64 = AtomicU64::new(USER_CODE_START);

/// Maps `code` into the user code page and a fresh zeroed stack, returning
/// the entry point and initial stack top. Single user program at a time:
/// calling it (or `load_elf`) again first unmaps and frees the previous
/// program's pages, so repeat calls (several tests, a second program) are
/// safe. The raw-bytes path the asm test programs use; real programs go
/// through `load_elf`.
pub fn map_user_program(code: &[u8]) -> Result<UserImage, MapError> {
    if code.len() as u64 > Size4KiB::SIZE {
        return Err(MapError::CodeTooLarge);
    }
    // One R-X page, the rest of it zero: the same thing `load_elf` builds for
    // a one-segment ELF.
    let segment = LoadSegment {
        vaddr: USER_CODE_START,
        memsz: Size4KiB::SIZE,
        data: code,
        writable: false,
        executable: true,
    };
    map_image(&[segment], VirtAddr::new(USER_CODE_START))
}

/// Maps a validated ELF's segments (flags from `p_flags`) and a fresh stack,
/// replacing whatever image was mapped before. Taking an `Elf` rather than
/// bytes means a file that fails validation never reaches the page tables:
/// the previous image stays mapped, untouched.
///
/// Same single-image rule as `map_user_program`: must not run while a task of
/// the previous image could still be scheduled, since that image's frames
/// are freed here.
pub fn load_elf(elf: &Elf) -> Result<UserImage, MapError> {
    map_image(elf.segments(), elf.entry())
}

/// The one mapping path behind both loaders. Each page is filled before it
/// is mapped (bytes copied through the physical-memory-offset mapping, the
/// rest zeroed), so Ring 3 can never see a previous owner's data, not even
/// briefly, and a read-only page never has to be writable.
fn map_image(segments: &[LoadSegment], entry: VirtAddr) -> Result<UserImage, MapError> {
    assert!(segments.len() <= MAX_LOAD_SEGMENTS, "elf::parse caps the segment count");
    ensure_nx_enabled();

    // Same lock order as `allocator::init_heap`: MAPPER, then FRAME_ALLOCATOR.
    let mut mapper = MAPPER.get().expect("MAPPER not initialized").lock();
    let mut frames = FRAME_ALLOCATOR
        .get()
        .expect("FRAME_ALLOCATOR not initialized")
        .lock();
    let mut loaded = LOADED.lock();

    unmap_image(&mut mapper, &mut frames, &mut loaded)?;

    // Recorded before mapping, so a failure part-way can find and undo the
    // pages it did map: roll back to nothing mapped before reporting it.
    for (slot, segment) in loaded.iter_mut().zip(segments) {
        *slot = Some(segment_pages(segment));
    }
    if let Err(e) = map_segments(&mut mapper, &mut frames, segments) {
        unmap_image(&mut mapper, &mut frames, &mut loaded)?;
        return Err(e);
    }

    ENTRY.store(entry.as_u64(), Ordering::Relaxed);
    Ok(UserImage { entry, stack_top: VirtAddr::new(USER_STACK_TOP) })
}

/// Unmaps and frees every page the current image maps (the recorded segment
/// pages and the stack) and clears the record. Pages that aren't mapped are
/// skipped, which is what lets this undo a half-finished load. Parent
/// tables stay allocated for reuse.
fn unmap_image(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
    loaded: &mut [Option<SegmentPages>; MAX_LOAD_SEGMENTS],
) -> Result<(), MapError> {
    let segments = loaded
        .iter()
        .flatten()
        .flat_map(|&(first, last)| Page::range_inclusive(first, last));
    for page in segments.chain(stack_pages()) {
        match mapper.unmap(page) {
            Ok((frame, flush)) => {
                flush.flush();
                // Safety: the page was just unmapped, and this module is the
                // only owner of these frames.
                unsafe { frames.deallocate_frame(frame) };
            }
            Err(UnmapError::PageNotMapped) => {}
            Err(e) => return Err(MapError::Unmap(e)),
        }
    }
    *loaded = [None; MAX_LOAD_SEGMENTS];
    Ok(())
}

/// Leaf flags for a segment, straight from `p_flags`. Always `USER_ACCESSIBLE`
/// (and readable: x86 has no write- or execute-only pages).
fn segment_flags(segment: &LoadSegment) -> PageTableFlags {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if segment.writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if !segment.executable {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn map_segments(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
    segments: &[LoadSegment],
) -> Result<(), MapError> {
    for segment in segments {
        let (first, last) = segment_pages(segment);
        let flags = segment_flags(segment);
        for page in Page::range_inclusive(first, last) {
            map_one(mapper, frames, page, flags, |bytes| copy_segment_bytes(segment, page, bytes))?;
        }
    }
    for page in stack_pages() {
        map_one(mapper, frames, page, STACK_FLAGS, |_| {})?;
    }
    Ok(())
}

/// Copies the part of `segment`'s file bytes that falls in `page` into
/// `bytes` (that page's already-zeroed frame). Whatever isn't copied stays
/// zero: `.bss` (`memsz - filesz`), and any bytes of a first or last page
/// outside the segment.
fn copy_segment_bytes(segment: &LoadSegment, page: Page<Size4KiB>, bytes: &mut [u8]) {
    let page_start = page.start_address().as_u64();
    let page_end = page_start + Size4KiB::SIZE;
    let data_end = segment.vaddr + segment.data.len() as u64;
    let lo = segment.vaddr.max(page_start);
    let hi = data_end.min(page_end);
    if lo >= hi {
        return;
    }
    let src = &segment.data[(lo - segment.vaddr) as usize..(hi - segment.vaddr) as usize];
    bytes[(lo - page_start) as usize..(hi - page_start) as usize].copy_from_slice(src);
}

/// Allocates a frame, zeroes it, lets `fill` initialise it, and only then
/// maps it: the page is complete before it becomes reachable from Ring 3.
fn map_one(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
    page: Page<Size4KiB>,
    flags: PageTableFlags,
    fill: impl FnOnce(&mut [u8]),
) -> Result<PhysFrame, MapError> {
    let frame = frames.allocate_frame().ok_or(MapError::OutOfFrames)?;
    let ptr: *mut u8 = (mapper.phys_offset() + frame.start_address().as_u64()).as_mut_ptr();
    // Safety: `frame` was just allocated, so nothing else uses it, and its
    // offset mapping covers the whole 4 KiB.
    let bytes = unsafe { core::slice::from_raw_parts_mut(ptr, Size4KiB::SIZE as usize) };
    bytes.fill(0);
    fill(bytes);
    // Safety: the user range is unused by anything else in the kernel, and
    // `frame` is fresh.
    match unsafe { mapper.map_to_with_table_flags(page, frame, flags, TABLE_FLAGS, frames) } {
        Ok(flush) => {
            flush.flush();
            Ok(frame)
        }
        Err(e) => {
            // Never mapped, so it's still ours alone; hand it back rather
            // than leak it.
            unsafe { frames.deallocate_frame(frame) };
            Err(MapError::Map(e))
        }
    }
}

/// `ptr..ptr + len` was not wholly inside mapped, user-accessible memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadUserPointer;

/// Runs `f` on the user bytes at `ptr..ptr + len`, but only after checking
/// that Ring 3 itself could read every one of them, so a syscall can never
/// be tricked into reading kernel memory or faulting at CPL 0:
/// - `ptr + len` must not overflow (else the range wraps around to kernel
///   space and the end check below would pass);
/// - the range must end at or below `USER_SPACE_END` (lower half, so also
///   canonical);
/// - every page it touches, not just the first, must be present and
///   `USER_ACCESSIBLE` at all four levels, as the CPU checks for Ring 3.
///
/// The `MAPPER` lock is held while `f` runs, so the pages can't be unmapped
/// between the check and the read. `f` must therefore not take `MAPPER` or
/// `FRAME_ALLOCATOR` itself, nor yield (another task could spin on the lock).
///
/// An empty range is accepted without looking at `ptr`: zero bytes are read.
///
/// Precondition: nothing but the calling task may run code in the user image
/// while `f` runs. `f` gets a `&[u8]` straight over user memory, which is only
/// sound if nobody writes it meanwhile. True today (one shared image, its one
/// task blocked in this syscall), but a second live user task, or SMP, needs
/// a copy into a kernel buffer instead (design-review item 31).
pub fn with_user_bytes<R>(
    ptr: u64,
    len: u64,
    f: impl FnOnce(&[u8]) -> R,
) -> Result<R, BadUserPointer> {
    if len == 0 {
        return Ok(f(&[]));
    }
    let end = ptr.checked_add(len).ok_or(BadUserPointer)?;
    if end > USER_SPACE_END {
        return Err(BadUserPointer);
    }
    let mapper = MAPPER.get().expect("MAPPER not initialized").lock();
    let first = Page::<Size4KiB>::containing_address(VirtAddr::new(ptr));
    let last = Page::<Size4KiB>::containing_address(VirtAddr::new(end - 1));
    if !Page::range_inclusive(first, last).all(|page| user_readable(&mapper, page.start_address())) {
        return Err(BadUserPointer);
    }
    // Safety: every page of the range is mapped user-readable (checked above
    // under the lock still held), so the read can't fault. SMAP is off, so
    // CPL 0 may read it directly; with SMAP on, this is where `stac`/`clac`
    // would bracket `f`. The bytes belong to the calling task, which is
    // blocked in this syscall, and the one shared user image is never
    // written by kernel code, so nothing mutates them while `f` reads.
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    Ok(f(bytes))
}

/// Whether a CPL 3 read of `addr` would succeed: present and
/// `USER_ACCESSIBLE` at every level, since the CPU ANDs U/S across the walk
/// (see `TABLE_FLAGS`). A huge page ends the walk early, but only in a PDPT
/// (1 GiB) or PD (2 MiB) entry: the bit is reserved in a PML4 entry.
fn user_readable(mapper: &OffsetPageTable<'static>, addr: VirtAddr) -> bool {
    let required = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    let offset = mapper.phys_offset();
    let mut table: &PageTable = mapper.level_4_table();
    let indices = [addr.p4_index(), addr.p3_index(), addr.p2_index(), addr.p1_index()];
    for (level, index) in indices.into_iter().enumerate() {
        let entry = &table[index];
        if !entry.flags().contains(required) {
            return false;
        }
        if level == 3 {
            return true;
        }
        if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            return level != 0;
        }
        let next: *const PageTable = (offset + entry.addr().as_u64()).as_ptr();
        // Safety: a present, non-huge entry points at a page table, reachable
        // through the physical-memory offset mapping; the `MAPPER` lock the
        // caller holds keeps it from changing.
        table = unsafe { &*next };
    }
    unreachable!("the level-1 entry always ends the walk")
}

/// Defines `pub fn $name() -> &'static [u8]` returning the machine code of
/// the given position-independent asm. Start and end labels are emitted
/// around the body, so the length is exact rather than guessed from
/// neighbouring symbols. The asm must not reference absolute addresses or
/// kernel symbols: it runs at `USER_CODE_START`, not where it was linked.
#[macro_export]
macro_rules! user_program {
    ($vis:vis $name:ident, $asm:literal) => {
        core::arch::global_asm!(
            ".pushsection .text.user_program,\"ax\"",
            concat!(".Luser_", stringify!($name), "_start:"),
            $asm,
            concat!(".Luser_", stringify!($name), "_end:"),
            ".popsection",
            // Table of [start, end). Global so the extern static below
            // resolves across codegen units; prefixed so it can't collide
            // with an unrelated symbol of the same name.
            ".pushsection .rodata,\"a\"",
            ".balign 8",
            concat!(".global __user_prog_", stringify!($name)),
            concat!("__user_prog_", stringify!($name), ":"),
            concat!(".quad .Luser_", stringify!($name), "_start"),
            concat!(".quad .Luser_", stringify!($name), "_end"),
            ".popsection",
        );
        $vis fn $name() -> &'static [u8] {
            unsafe extern "C" {
                #[link_name = concat!("__user_prog_", stringify!($name))]
                static $name: [usize; 2];
            }
            // Safety: the table was emitted by the asm above and bounds
            // exactly the bytes between its two labels.
            unsafe {
                let [start, end] = $name;
                core::slice::from_raw_parts(start as *const u8, end - start)
            }
        }
    };
}

/// Why `spawn_user`/`spawn_user_elf` refused.
#[derive(Debug)]
pub enum SpawnUserError {
    /// The file failed validation. Nothing was mapped or unmapped.
    Elf(crate::elf::ElfError),
    Map(MapError),
    Spawn(crate::scheduler::SpawnError),
}

/// Maps `code` and queues a kernel task that drops into it at Ring 3. The
/// task is an ordinary scheduled task (its heap stack is what RSP0 points at
/// while it runs), so it is preempted and resumed like any other.
///
/// Maps first, spawns second: a failed spawn leaves a mapped but unused
/// program, which the next load frees. Never call this while an earlier
/// user task might still be running, since remapping frees the frames it
/// executes on.
pub fn spawn_user(code: &[u8]) -> Result<crate::task::TaskId, SpawnUserError> {
    map_user_program(code).map_err(SpawnUserError::Map)?;
    crate::scheduler::spawn(user_task_entry).map_err(SpawnUserError::Spawn)
}

/// `spawn_user` for an ELF file: validate, load, spawn at `e_entry`. The
/// same single-image rule applies.
pub fn spawn_user_elf(file: &[u8]) -> Result<crate::task::TaskId, SpawnUserError> {
    let elf = crate::elf::parse(file).map_err(SpawnUserError::Elf)?;
    load_elf(&elf).map_err(SpawnUserError::Map)?;
    crate::scheduler::spawn(user_task_entry).map_err(SpawnUserError::Spawn)
}

/// Entry of every user task. Takes no arguments, so it reads the entry point
/// the last load stored in `ENTRY`; the stack is always at `USER_STACK_TOP`.
/// Per-process images (and an entry point per task) come with per-process
/// address spaces.
extern "C" fn user_task_entry() {
    let entry = VirtAddr::new(ENTRY.load(Ordering::Relaxed));
    // Safety: the load that stored `entry` mapped it executable, and the
    // stack, and nothing has replaced that image since (single-image rule).
    unsafe { enter_user(entry, VirtAddr::new(USER_STACK_TOP)) }
}

/// Drops to Ring 3 at `entry` with `rsp = stack_top` and never returns.
///
/// `iretq` is the only way down from Ring 0 that sets CS, SS, RSP, RIP and
/// RFLAGS atomically: it pops RIP, CS, RFLAGS, RSP, SS, and because the new
/// CS has a higher RPL it takes the privilege-change path. The frame is
/// built by hand with the user selectors (RPL 3, from the GDT) and RFLAGS
/// `0x202` = IF set (so the timer can preempt Ring 3; without it the task
/// could never be descheduled) plus the always-one bit 1, IOPL 0 (so `cli`,
/// `hlt` and port I/O fault).
///
/// DS and ES get the user data selector so Ring 3 does not start with
/// kernel-DPL descriptors cached. Long mode ignores their bases and limits,
/// so this is hygiene, not a security boundary. FS/GS are left alone: they
/// are null and nothing sets their bases yet.
///
/// Every GPR is zeroed after the pushes so no kernel pointer or value leaks
/// into user mode; the pushes use rax/rcx/rdx/rsi/rdi, which are pinned as
/// inputs and then cleared with the rest.
///
/// Kernel stack: this call's frames (and the task's trampoline frame) below
/// `stack_top` become dead the instant `iretq` runs, since the task never
/// returns here. A later Ring 3 -> 0 interrupt lands at RSP0 = `stack_top`
/// and overwrites them, which is fine. Nothing live sits above or at
/// `stack_top`, and the canary is at the bottom.
///
/// # Safety
/// `entry` and `stack_top` must be mapped user-accessible (code executable,
/// stack writable), the GDT/TSS must be loaded, and the caller must be a
/// scheduled task whose kernel stack `gdt::rsp0` already names.
pub unsafe fn enter_user(entry: VirtAddr, stack_top: VirtAddr) -> ! {
    use x86_64::registers::segmentation::{DS, ES, Segment};

    // IF=1 here (the trampoline's `sti`), deliberately: a tick before `iretq`
    // is an ordinary Ring 0 preemption on the current stack, and RSP0 is
    // reloaded for this task by `rotate` before it resumes.
    debug_assert_eq!(
        Some(crate::gdt::rsp0()),
        crate::scheduler::current_stack_top(),
        "enter_user from a task whose kernel stack RSP0 doesn't name"
    );
    let user_data = crate::gdt::user_data_selector();
    let user_code = crate::gdt::user_code_selector();
    // Safety: a valid GDT data descriptor, loaded from Ring 0.
    unsafe {
        DS::set_reg(user_data);
        ES::set_reg(user_data);
    }
    // Safety: the frame below is exactly what `iretq` expects for a
    // privilege-raising return (see the contract above). `noreturn`: control
    // never comes back, so nothing needs preserving or declaring clobbered.
    unsafe {
        core::arch::asm!(
            // Frame, pushed high to low: SS, RSP, RFLAGS, CS, RIP.
            "push rax",
            "push rcx",
            "push rdx",
            "push rsi",
            "push rdi",
            "xor eax, eax",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor ebx, ebx",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "iretq",
            in("rax") user_data.0 as u64,
            in("rcx") stack_top.as_u64(),
            in("rdx") 0x202u64,
            in("rsi") user_code.0 as u64,
            in("rdi") entry.as_u64(),
            options(noreturn),
        )
    }
}

user_program!(pub placeholder_program, "2: pause\n jmp 2b");

#[test_case]
fn test_placeholder_bytes_nonempty() {
    let bytes = placeholder_program();
    assert_eq!(bytes, &[0xf3, 0x90, 0xeb, 0xfc]);
}
