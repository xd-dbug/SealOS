#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

//! #50: a validated ELF becomes the user image. Leaf flags follow `p_flags`,
//! file bytes land where the segment says, `.bss` and the rest of every page
//! read zero, a reload hands back exactly the previous image's frames, and a
//! file that fails validation leaves the current image alone.

extern crate alloc;

use alloc::vec::Vec;
use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::elf::{self, LoadSegment};
use kernel::memory::MAPPER;
use kernel::programs::HELLO;
use kernel::user::{self, SpawnUserError, USER_CODE_START, USER_STACK_PAGES, USER_STACK_START};
use kernel::BOOTLOADER_CONFIG;
use x86_64::structures::paging::{Mapper, Page, PageSize, PageTable, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("BOOTLOADER_CONFIG enables physical memory mapping");
    let memory_regions = &boot_info.memory_regions;
    if let Some(framebuffer) = boot_info.framebuffer.as_mut() {
        kernel::framebuffer::init(framebuffer);
    }
    kernel::init(physical_memory_offset, memory_regions);
    test_main();
    loop {}
}

bootloader_api::entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

const PAGE: u64 = Size4KiB::SIZE;

/// The four entries the CPU walks for `addr` (PML4 first) and the leaf's
/// frame, or `None` if any level is absent.
fn walk(addr: u64) -> Option<([PageTableFlags; 4], PhysFrame)> {
    let mapper = MAPPER.get().expect("MAPPER not initialized").lock();
    let offset = mapper.phys_offset();
    let va = VirtAddr::new(addr);
    let mut table: &PageTable = mapper.level_4_table();
    let indices = [va.p4_index(), va.p3_index(), va.p2_index(), va.p1_index()];
    let mut flags = [PageTableFlags::empty(); 4];
    for (level, idx) in indices.iter().enumerate() {
        let entry = &table[*idx];
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        flags[level] = entry.flags();
        if level == 3 {
            return Some((flags, PhysFrame::containing_address(entry.addr())));
        }
        assert!(!entry.flags().contains(PageTableFlags::HUGE_PAGE));
        let ptr: *const PageTable = (offset + entry.addr().as_u64()).as_ptr();
        // Safety: a present non-huge entry points at a page table, reachable
        // through the physical-memory offset mapping.
        table = unsafe { &*ptr };
    }
    unreachable!()
}

fn is_mapped(addr: u64) -> bool {
    let mapper = MAPPER.get().unwrap().lock();
    mapper.translate_page(Page::<Size4KiB>::containing_address(VirtAddr::new(addr))).is_ok()
}

fn segment_page_addrs(s: &LoadSegment) -> impl Iterator<Item = u64> {
    let first = s.vaddr & !(PAGE - 1);
    (first..s.end()).step_by(PAGE as usize)
}

fn stack_page_addrs() -> impl Iterator<Item = u64> {
    (0..USER_STACK_PAGES).map(|i| USER_STACK_START + i * PAGE)
}

/// Every leaf frame of the image `elf` describes, sorted, so two loads can be
/// compared as sets.
fn image_frames(elf: &elf::Elf) -> Vec<u64> {
    let mut frames: Vec<u64> = elf
        .segments()
        .iter()
        .flat_map(segment_page_addrs)
        .chain(stack_page_addrs())
        .map(|a| walk(a).expect("image page unmapped").1.start_address().as_u64())
        .collect();
    frames.sort_unstable();
    frames
}

fn hello() -> elf::Elf<'static> {
    elf::parse(HELLO).expect("embedded ELF rejected")
}

/// What Ring 3 would read at `addr..addr + len`. SMAP is off and the user
/// image is in the one shared page table, so the kernel can read it directly.
fn user_bytes(addr: u64, len: u64) -> &'static [u8] {
    // Safety: callers pass ranges inside pages `load_elf` just mapped.
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

#[test_case]
fn test_entry_comes_from_e_entry() {
    let elf = hello();
    let image = user::load_elf(&elf).expect("load failed");
    assert_eq!(image.entry, elf.entry());
    assert_ne!(image.entry.as_u64(), USER_CODE_START, "not the raw-program entry");
}

#[test_case]
fn test_leaf_flags_follow_p_flags() {
    let elf = hello();
    user::load_elf(&elf).expect("load failed");
    let mut saw = [false; 3]; // text, rodata, data
    for s in elf.segments() {
        for addr in segment_page_addrs(s) {
            let (flags, _) = walk(addr).expect("segment page unmapped");
            for (i, f) in flags.iter().enumerate() {
                assert!(f.contains(PageTableFlags::USER_ACCESSIBLE), "{addr:#x}: level {} lacks U/S", 4 - i);
            }
            let leaf = flags[3];
            assert_eq!(leaf.contains(PageTableFlags::WRITABLE), s.writable, "{addr:#x} W");
            assert_eq!(!leaf.contains(PageTableFlags::NO_EXECUTE), s.executable, "{addr:#x} X");
        }
        match (s.writable, s.executable) {
            (false, true) => saw[0] = true,
            (false, false) => saw[1] = true,
            (true, false) => saw[2] = true,
            (true, true) => panic!("W+X segment survived parse"),
        }
    }
    assert_eq!(saw, [true; 3], "hello has text, rodata and data segments");
    // Code not writable and data not executable, spelled out for the W5 proof.
    let text = elf.segments().iter().find(|s| s.executable).unwrap();
    assert!(!walk(text.vaddr).unwrap().0[3].contains(PageTableFlags::WRITABLE));
    let data = elf.segments().iter().find(|s| s.writable).unwrap();
    assert!(walk(data.vaddr).unwrap().0[3].contains(PageTableFlags::NO_EXECUTE));
}

#[test_case]
fn test_file_bytes_copied_rest_zeroed() {
    let elf = hello();
    user::load_elf(&elf).expect("load failed");
    for s in elf.segments() {
        let filesz = s.data.len() as u64;
        assert_eq!(user_bytes(s.vaddr, filesz), s.data, "segment at {:#x}", s.vaddr);
        // `.bss`, then the rest of the last page.
        let page_end = (s.end() + PAGE - 1) & !(PAGE - 1);
        let tail = user_bytes(s.vaddr + filesz, page_end - (s.vaddr + filesz));
        assert!(tail.iter().all(|b| *b == 0), "segment at {:#x}: tail not zero", s.vaddr);
        // And anything before the segment on its first page.
        let first = s.vaddr & !(PAGE - 1);
        assert!(user_bytes(first, s.vaddr - first).iter().all(|b| *b == 0));
    }
    let data = elf.segments().iter().find(|s| s.writable).unwrap();
    assert!(data.end() - data.vaddr - data.data.len() as u64 > PAGE, ".bss spans pages");
}

#[test_case]
fn test_stale_bytes_never_reach_ring3() {
    // Dirty the image's frames through Ring 3's own view, then reload: the
    // second load gets the same frames back (see the next test) and must
    // still present clean `.bss`.
    let elf = hello();
    user::load_elf(&elf).expect("load failed");
    let data = *elf.segments().iter().find(|s| s.writable).unwrap();
    let bss = data.vaddr + data.data.len() as u64;
    // Safety: the data segment is mapped writable.
    unsafe { core::ptr::write_bytes(bss as *mut u8, 0xaa, (data.end() - bss) as usize) };
    user::load_elf(&elf).expect("reload failed");
    assert!(user_bytes(bss, data.end() - bss).iter().all(|b| *b == 0));
}

#[test_case]
fn test_reload_returns_previous_frames() {
    let elf = hello();
    user::load_elf(&elf).expect("first load");
    let first = image_frames(&elf);
    user::load_elf(&elf).expect("second load");
    assert_eq!(image_frames(&elf), first, "the second load should reuse exactly the freed frames");
}

#[test_case]
fn test_rejected_file_leaves_image_alone() {
    let elf = hello();
    user::load_elf(&elf).expect("load failed");
    let before = image_frames(&elf);
    // Make the first program header W+X.
    let mut bad: Vec<u8> = HELLO.into();
    let phoff = u64::from_le_bytes(bad[32..40].try_into().unwrap()) as usize;
    bad[phoff + 4..phoff + 8].copy_from_slice(&7u32.to_le_bytes());
    match user::spawn_user_elf(&bad) {
        Err(SpawnUserError::Elf(elf::ElfError::WritableAndExecutable)) => {}
        other => panic!("corrupted ELF not rejected by validation: {:?}", other),
    }
    assert_eq!(image_frames(&elf), before, "a rejected file touched the page tables");
}

#[test_case]
fn test_switching_loaders_unmaps_the_other_image() {
    let elf = hello();
    user::load_elf(&elf).expect("ELF load");
    user::map_user_program(user::placeholder_program()).expect("raw map");
    for s in elf.segments() {
        for addr in segment_page_addrs(s) {
            assert!(!is_mapped(addr), "ELF page {addr:#x} survived a raw load");
        }
    }
    assert!(is_mapped(USER_CODE_START));
    user::load_elf(&elf).expect("ELF reload");
    assert!(!is_mapped(USER_CODE_START), "raw code page survived an ELF load");
    assert!(stack_page_addrs().all(is_mapped));
}
