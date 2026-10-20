#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::memory::MAPPER;
use kernel::user::{self, USER_CODE_START, USER_STACK_PAGES, USER_STACK_START, USER_STACK_TOP};
use kernel::{serial_println, BOOTLOADER_CONFIG};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::mapper::TranslateResult;
use x86_64::structures::paging::{
    Mapper, Page, PageSize, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::VirtAddr;

/// Full `kernel::init()` so the mapper and frame allocator exist; each test
/// maps the placeholder program itself (mapping is idempotent).
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

/// The four entries the CPU walks for `addr` (PML4 first), plus the leaf's
/// frame, found by hand through the physical-memory offset.
fn walk(addr: u64) -> ([PageTableFlags; 4], PhysFrame) {
    let mapper = MAPPER.get().expect("MAPPER not initialized").lock();
    let offset = mapper.phys_offset();
    let va = VirtAddr::new(addr);
    let mut table: &PageTable = mapper.level_4_table();
    let indices = [va.p4_index(), va.p3_index(), va.p2_index(), va.p1_index()];
    let mut flags = [PageTableFlags::empty(); 4];
    for (level, idx) in indices.iter().enumerate() {
        let entry = &table[*idx];
        assert!(entry.flags().contains(PageTableFlags::PRESENT), "level {} absent", 4 - level);
        flags[level] = entry.flags();
        if level == 3 {
            return (flags, PhysFrame::containing_address(entry.addr()));
        }
        assert!(!entry.flags().contains(PageTableFlags::HUGE_PAGE));
        let ptr: *const PageTable = (offset + entry.addr().as_u64()).as_ptr();
        // Safety: a present non-huge entry points at a page table, reachable
        // through the physical-memory offset mapping.
        table = unsafe { &*ptr };
    }
    unreachable!()
}

fn stack_page_addrs() -> impl Iterator<Item = u64> {
    (0..USER_STACK_PAGES).map(|i| USER_STACK_START + i * Size4KiB::SIZE)
}

fn frame_bytes(frame: PhysFrame) -> &'static [u8] {
    let offset = MAPPER.get().unwrap().lock().phys_offset();
    let p: *const u8 = (offset + frame.start_address().as_u64()).as_ptr();
    // Safety: the frame is mapped through the physical-memory offset.
    unsafe { core::slice::from_raw_parts(p, Size4KiB::SIZE as usize) }
}

#[test_case]
fn test_nxe_enabled_and_layout() {
    let image = user::map_user_program(user::placeholder_program()).expect("map failed");
    serial_println!("EFER.NXE = {}", Efer::read().contains(EferFlags::NO_EXECUTE_ENABLE));
    assert!(Efer::read().contains(EferFlags::NO_EXECUTE_ENABLE));
    assert_eq!(image.entry.as_u64(), USER_CODE_START);
    assert_eq!(image.stack_top.as_u64(), USER_STACK_TOP);
    let code_p4 = VirtAddr::new(USER_CODE_START).p4_index();
    assert_eq!(code_p4, VirtAddr::new(USER_STACK_TOP - 1).p4_index());
    assert_ne!(usize::from(code_p4), 0);
    assert!(USER_STACK_TOP < KERNEL_HALF);
}

const KERNEL_HALF: u64 = kernel::KERNEL_HALF_START;

#[test_case]
fn test_translate_matches_walk() {
    user::map_user_program(user::placeholder_program()).expect("map failed");
    for addr in core::iter::once(USER_CODE_START).chain(stack_page_addrs()) {
        let (_, frame) = walk(addr);
        let mapper = MAPPER.get().unwrap().lock();
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
        assert_eq!(mapper.translate_page(page).ok(), Some(frame));
        match mapper.translate(VirtAddr::new(addr + 5)) {
            TranslateResult::Mapped { frame: f, offset, .. } => {
                assert_eq!(offset, 5);
                assert_eq!(f.start_address(), frame.start_address());
            }
            other => panic!("unexpected translation {:?}", other),
        }
    }
}

#[test_case]
fn test_user_accessible_at_every_level() {
    user::map_user_program(user::placeholder_program()).expect("map failed");
    for addr in core::iter::once(USER_CODE_START).chain(stack_page_addrs()) {
        let (flags, _) = walk(addr);
        for (i, f) in flags.iter().enumerate() {
            assert!(f.contains(PageTableFlags::USER_ACCESSIBLE), "{:#x}: level {} lacks U/S", addr, 4 - i);
        }
    }
}

#[test_case]
fn test_leaf_permissions() {
    user::map_user_program(user::placeholder_program()).expect("map failed");
    let (code, _) = walk(USER_CODE_START);
    assert!(!code[3].contains(PageTableFlags::WRITABLE), "code page writable");
    assert!(!code[3].contains(PageTableFlags::NO_EXECUTE), "code page NX");
    for addr in stack_page_addrs() {
        let (s, _) = walk(addr);
        assert!(s[3].contains(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE));
    }
    // The gap page after the code stays unmapped.
    let mapper = MAPPER.get().unwrap().lock();
    let gap = Page::<Size4KiB>::containing_address(VirtAddr::new(USER_CODE_START + Size4KiB::SIZE));
    assert!(mapper.translate_page(gap).is_err());
}

#[test_case]
fn test_code_bytes_match() {
    let code = user::placeholder_program();
    user::map_user_program(code).expect("map failed");
    let (_, frame) = walk(USER_CODE_START);
    let page = frame_bytes(frame);
    assert_eq!(&page[..code.len()], code);
    assert!(page[code.len()..].iter().all(|b| *b == 0), "tail not zeroed");
}

#[test_case]
fn test_remap_replaces_program() {
    let a = user::map_user_program(user::placeholder_program()).expect("first map");
    let b = user::map_user_program(&[0x90, 0xeb, 0xfd]).expect("second map");
    assert_eq!(a, b);
    let (_, frame) = walk(USER_CODE_START);
    assert_eq!(&frame_bytes(frame)[..3], &[0x90, 0xeb, 0xfd]);
    assert!(user::map_user_program(&[0u8; 4097]).is_err());
}

#[test_case]
fn test_only_user_pml4_entry_is_user_accessible() {
    user::map_user_program(user::placeholder_program()).expect("map failed");
    let user_idx = usize::from(VirtAddr::new(USER_CODE_START).p4_index());
    let mapper = MAPPER.get().unwrap().lock();
    let p4 = mapper.level_4_table();
    for (i, entry) in p4.iter().enumerate() {
        let u = entry.flags().contains(PageTableFlags::USER_ACCESSIBLE);
        if i == user_idx {
            assert!(u);
        } else {
            assert!(!u, "PML4[{}] is user-accessible", i);
        }
    }
    assert!(!p4[0].flags().contains(PageTableFlags::USER_ACCESSIBLE));
}
