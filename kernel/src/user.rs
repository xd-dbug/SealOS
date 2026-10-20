//! Lower-half user mappings for the one shared page table (per-process
//! address spaces come later). Everything here is `USER_ACCESSIBLE`; every
//! kernel mapping stays supervisor-only.

use crate::memory::{BootInfoFrameAllocator, FRAME_ALLOCATOR, MAPPER};
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::mapper::{MapToError, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageSize, PageTableFlags,
    PhysFrame, Size4KiB,
};

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

/// Parent-table flags. The CPU ANDs U/S (and R/W) across the walk, so every
/// level must carry `USER_ACCESSIBLE`; `WRITABLE` here is harmless because the
/// leaf entry alone decides whether a page is writable.
const TABLE_FLAGS: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::USER_ACCESSIBLE);

const CODE_FLAGS: PageTableFlags = PageTableFlags::PRESENT.union(PageTableFlags::USER_ACCESSIBLE);

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

fn code_page() -> Page<Size4KiB> {
    Page::containing_address(VirtAddr::new(USER_CODE_START))
}

fn stack_pages() -> impl Iterator<Item = Page<Size4KiB>> {
    (0..USER_STACK_PAGES)
        .map(|i| Page::containing_address(VirtAddr::new(USER_STACK_START + i * Size4KiB::SIZE)))
}

/// Maps `code` into the user code page and a fresh zeroed stack, returning
/// the entry point and initial stack top. Single user program at a time:
/// calling it again first unmaps and frees the previous program's pages, so
/// repeat calls (several tests, a second program) are safe.
///
/// The bytes are written through the physical-memory-offset mapping of the
/// backing frame, so the user code page never has to be writable, not even
/// briefly.
pub fn map_user_program(code: &[u8]) -> Result<UserImage, MapError> {
    if code.len() as u64 > Size4KiB::SIZE {
        return Err(MapError::CodeTooLarge);
    }
    ensure_nx_enabled();

    // Same lock order as `allocator::init_heap`: MAPPER, then FRAME_ALLOCATOR.
    let mut mapper = MAPPER.get().expect("MAPPER not initialized").lock();
    let mut frames = FRAME_ALLOCATOR
        .get()
        .expect("FRAME_ALLOCATOR not initialized")
        .lock();
    let offset = mapper.phys_offset();

    unmap_program(&mut mapper, &mut frames)?;

    // A failure part-way would otherwise leave a half-mapped image behind, so
    // roll back to nothing mapped before reporting it.
    let code_frame = match map_program_pages(&mut mapper, &mut frames) {
        Ok(frame) => frame,
        Err(e) => {
            unmap_program(&mut mapper, &mut frames)?;
            return Err(e);
        }
    };

    let dst: *mut u8 = (offset + code_frame.start_address().as_u64()).as_mut_ptr();
    // Safety: `dst` is the code frame's offset mapping, `code` fits in it
    // (checked above), and the regions can't overlap (kernel vs. fresh frame).
    unsafe { core::ptr::copy_nonoverlapping(code.as_ptr(), dst, code.len()) };

    Ok(UserImage {
        entry: VirtAddr::new(USER_CODE_START),
        stack_top: VirtAddr::new(USER_STACK_TOP),
    })
}

/// Unmaps and frees whatever user program pages are currently mapped; pages
/// that aren't mapped are skipped. Parent tables stay allocated for reuse.
fn unmap_program(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
) -> Result<(), MapError> {
    for page in core::iter::once(code_page()).chain(stack_pages()) {
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
    Ok(())
}

/// Maps the code page and the stack, returning the code page's frame so the
/// caller can copy the program into it.
fn map_program_pages(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
) -> Result<PhysFrame, MapError> {
    let code_frame = map_one(mapper, frames, code_page(), CODE_FLAGS)?;
    for page in stack_pages() {
        map_one(mapper, frames, page, STACK_FLAGS)?;
    }
    Ok(code_frame)
}

fn map_one(
    mapper: &mut OffsetPageTable<'static>,
    frames: &mut BootInfoFrameAllocator,
    page: Page<Size4KiB>,
    flags: PageTableFlags,
) -> Result<PhysFrame, MapError> {
    let frame = frames.allocate_frame().ok_or(MapError::OutOfFrames)?;
    // Zero before the page becomes reachable from Ring 3.
    let ptr: *mut u8 = (mapper.phys_offset() + frame.start_address().as_u64()).as_mut_ptr();
    // Safety: `frame` was just allocated, so nothing else uses it.
    unsafe { ptr.write_bytes(0, Size4KiB::SIZE as usize) };
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

user_program!(pub placeholder_program, "2: pause\n jmp 2b");

#[test_case]
fn test_placeholder_bytes_nonempty() {
    let bytes = placeholder_program();
    assert_eq!(bytes, &[0xf3, 0x90, 0xeb, 0xfc]);
}
