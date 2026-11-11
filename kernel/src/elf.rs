//! ELF64 validation for user programs. The file is treated as untrusted input:
//! today it comes from the kernel image, but the same parser will later read
//! programs from somewhere a user controls. So a malformed file is an error
//! value, never a panic or an out-of-bounds read.
//!
//! Everything a loader needs is checked here, up front, so `user::load_elf`
//! can map an `Elf` without re-validating anything and a rejected file never
//! touches the page tables.
//!
//! Allocation-free (segments go in a fixed array) so it can run anywhere and
//! its malformed-input cases can be plain `#[test_case]`s in the lib binary.

use crate::user::{USER_CODE_START, USER_SPACE_END, USER_STACK_START, USER_STACK_TOP};
use x86_64::VirtAddr;

/// Most `PT_LOAD`s accepted. The linker script makes three (R-X, R--, RW-);
/// the cap bounds `Elf` and the loader's record of what it mapped.
pub const MAX_LOAD_SEGMENTS: usize = 4;

/// Most pages an image may map (1 MiB). `memsz` is attacker-chosen, and each
/// page costs a physical frame (and the loader a loop iteration), so a file
/// of a few bytes must not be able to ask for gigabytes.
pub const MAX_IMAGE_PAGES: u64 = 256;

const PAGE_SIZE: u64 = 4096;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Why a file was rejected. One variant per check, so tests can tell the
/// checks apart rather than just "it failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Shorter than the 64-byte ELF header.
    TooShort,
    BadMagic,
    /// Not `ELFCLASS64`.
    WrongClass,
    /// Not little-endian.
    WrongEndianness,
    /// `e_ident[EI_VERSION]` isn't `EV_CURRENT`.
    BadVersion,
    /// Not `ET_EXEC`. A PIE (`ET_DYN`) would need relocating.
    NotExecutable,
    /// Not `EM_X86_64`.
    WrongMachine,
    /// `e_phentsize` isn't the ELF64 program-header size.
    BadProgramHeaderSize,
    /// The program-header table doesn't lie wholly inside the file.
    ProgramHeadersOutOfBounds,
    /// `PT_INTERP`/`PT_DYNAMIC`: needs a dynamic linker this kernel lacks.
    NeedsDynamicLinking,
    /// A segment claims more file bytes than memory.
    FileSizeExceedsMemSize,
    /// `p_offset + p_filesz` runs past the end of the file (or overflows).
    SegmentOutOfFile,
    /// `p_vaddr` and `p_offset` differ modulo the page size, so file pages
    /// can't be mapped to memory pages one-to-one.
    Misaligned,
    /// `p_vaddr + p_memsz` overflows: the range would wrap, and a wrapped
    /// range passes a naive `end <= limit` check.
    AddressOverflow,
    /// Below PML4[1] (PML4[0] is the bootloader's identity map) or above
    /// the lower half (kernel addresses or non-canonical).
    OutsideUserRegion,
    /// Overlaps the fixed user stack pages.
    OverlapsStack,
    /// Both writable and executable: W^X is the point of the split segments.
    WritableAndExecutable,
    /// Shares a page with, or comes before, the previous `PT_LOAD`. The ELF
    /// spec requires ascending `p_vaddr`, and each page gets one set of
    /// permissions, so two segments can't share one.
    SegmentsOverlap,
    /// More than `MAX_LOAD_SEGMENTS`.
    TooManySegments,
    /// More than `MAX_IMAGE_PAGES` in total.
    ImageTooLarge,
    /// No non-empty `PT_LOAD`: nothing to run.
    NoLoadableSegments,
    /// `e_entry` isn't inside an executable segment.
    EntryNotExecutable,
}

/// One validated `PT_LOAD`: map `memsz` bytes at `vaddr`, the first
/// `data.len()` from the file and the rest zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSegment<'a> {
    pub vaddr: u64,
    /// Never 0 (empty segments are skipped) and `>= data.len()`.
    pub memsz: u64,
    pub data: &'a [u8],
    pub writable: bool,
    pub executable: bool,
}

impl LoadSegment<'_> {
    const EMPTY: LoadSegment<'static> = LoadSegment {
        vaddr: 0,
        memsz: 0,
        data: &[],
        writable: false,
        executable: false,
    };

    /// One past the last byte. Can't overflow: `parse` checked it.
    pub fn end(&self) -> u64 {
        self.vaddr + self.memsz
    }
}

/// A file that passed every check in `parse`.
#[derive(Debug)]
pub struct Elf<'a> {
    entry: u64,
    segments: [LoadSegment<'a>; MAX_LOAD_SEGMENTS],
    count: usize,
}

impl<'a> Elf<'a> {
    pub fn entry(&self) -> VirtAddr {
        VirtAddr::new(self.entry)
    }

    /// In ascending address order, with no two sharing a page.
    pub fn segments(&self) -> &[LoadSegment<'a>] {
        &self.segments[..self.count]
    }
}

fn read_bytes<const N: usize>(file: &[u8], at: usize) -> Option<[u8; N]> {
    file.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn read_u16(file: &[u8], at: usize) -> Option<u16> {
    read_bytes(file, at).map(u16::from_le_bytes)
}

fn read_u32(file: &[u8], at: usize) -> Option<u32> {
    read_bytes(file, at).map(u32::from_le_bytes)
}

fn read_u64(file: &[u8], at: usize) -> Option<u64> {
    read_bytes(file, at).map(u64::from_le_bytes)
}

fn page_floor(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

/// Rounds up to a page boundary. Only called on ends at or below
/// `USER_SPACE_END`, which is page-aligned, so it can't overflow.
fn page_ceil(addr: u64) -> u64 {
    page_floor(addr + (PAGE_SIZE - 1))
}

/// Validates `file` as a static x86_64 executable loadable into the user
/// half. On success every segment is in bounds, page-congruent, inside
/// PML4[1].., clear of the stack, not W+X, sorted and non-overlapping, and
/// the entry point is executable.
pub fn parse(file: &[u8]) -> Result<Elf<'_>, ElfError> {
    let ident: [u8; 16] = read_bytes(file, 0).ok_or(ElfError::TooShort)?;
    if file.len() < EHDR_SIZE {
        return Err(ElfError::TooShort);
    }
    if ident[..4] != *b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if ident[4] != ELFCLASS64 {
        return Err(ElfError::WrongClass);
    }
    if ident[5] != ELFDATA2LSB {
        return Err(ElfError::WrongEndianness);
    }
    if ident[6] != EV_CURRENT {
        return Err(ElfError::BadVersion);
    }
    // The header is 64 bytes and `file` is at least that long, so these reads
    // can't fail. `TooShort` rather than a panic all the same.
    let field16 = |at| read_u16(file, at).ok_or(ElfError::TooShort);
    if field16(16)? != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if field16(18)? != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }
    let entry = read_u64(file, 24).ok_or(ElfError::TooShort)?;
    let phoff = read_u64(file, 32).ok_or(ElfError::TooShort)?;
    if usize::from(field16(54)?) != PHDR_SIZE {
        return Err(ElfError::BadProgramHeaderSize);
    }
    let phnum = usize::from(field16(56)?);

    // The whole table must fit, checked once with overflow-checked math, so
    // every per-entry read below is in bounds.
    let phoff = usize::try_from(phoff).map_err(|_| ElfError::ProgramHeadersOutOfBounds)?;
    let table_end = phnum
        .checked_mul(PHDR_SIZE)
        .and_then(|len| phoff.checked_add(len))
        .ok_or(ElfError::ProgramHeadersOutOfBounds)?;
    if table_end > file.len() {
        return Err(ElfError::ProgramHeadersOutOfBounds);
    }

    let mut elf = Elf { entry, segments: [LoadSegment::EMPTY; MAX_LOAD_SEGMENTS], count: 0 };
    let mut total_pages = 0u64;
    for i in 0..phnum {
        let base = phoff + i * PHDR_SIZE;
        let field32 = |at| read_u32(file, base + at).ok_or(ElfError::ProgramHeadersOutOfBounds);
        let field64 = |at| read_u64(file, base + at).ok_or(ElfError::ProgramHeadersOutOfBounds);
        match field32(0)? {
            PT_LOAD => {}
            PT_INTERP | PT_DYNAMIC => return Err(ElfError::NeedsDynamicLinking),
            // PT_GNU_STACK, PT_NOTE and friends describe the file; nothing
            // to map.
            _ => continue,
        }
        let flags = field32(4)?;
        let offset = field64(8)?;
        let vaddr = field64(16)?;
        let filesz = field64(32)?;
        let memsz = field64(40)?;

        if filesz > memsz {
            return Err(ElfError::FileSizeExceedsMemSize);
        }
        if memsz == 0 {
            continue;
        }
        let data = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(filesz).ok())
            .and_then(|(start, len)| file.get(start..start.checked_add(len)?))
            .ok_or(ElfError::SegmentOutOfFile)?;
        if vaddr % PAGE_SIZE != offset % PAGE_SIZE {
            return Err(ElfError::Misaligned);
        }
        let end = vaddr.checked_add(memsz).ok_or(ElfError::AddressOverflow)?;
        // `USER_CODE_START` is the first byte of PML4[1].
        if vaddr < USER_CODE_START || end > USER_SPACE_END {
            return Err(ElfError::OutsideUserRegion);
        }
        let (first_page, end_page) = (page_floor(vaddr), page_ceil(end));
        if first_page < USER_STACK_TOP && USER_STACK_START < end_page {
            return Err(ElfError::OverlapsStack);
        }
        let writable = flags & PF_W != 0;
        let executable = flags & PF_X != 0;
        if writable && executable {
            return Err(ElfError::WritableAndExecutable);
        }
        if let Some(prev) = elf.segments().last()
            && first_page < page_ceil(prev.end())
        {
            return Err(ElfError::SegmentsOverlap);
        }
        if elf.count == MAX_LOAD_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        total_pages += (end_page - first_page) / PAGE_SIZE;
        if total_pages > MAX_IMAGE_PAGES {
            return Err(ElfError::ImageTooLarge);
        }
        elf.segments[elf.count] = LoadSegment { vaddr, memsz, data, writable, executable };
        elf.count += 1;
    }

    if elf.count == 0 {
        return Err(ElfError::NoLoadableSegments);
    }
    let entry_ok = elf
        .segments()
        .iter()
        .any(|s| s.executable && (s.vaddr..s.end()).contains(&entry));
    if !entry_ok {
        return Err(ElfError::EntryNotExecutable);
    }
    Ok(elf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::USER_ELF_BASE;

    // A minimal valid file: header, two program headers (R-X text, RW-
    // data with .bss), then their bytes. `p_offset`s are not page-aligned,
    // just page-congruent with `p_vaddr`, which is all the format requires.
    const PHOFF: usize = 64;
    const TEXT_OFF: u64 = 0xb0;
    const DATA_OFF: u64 = 0xc0;
    const TEXT_VADDR: u64 = USER_ELF_BASE + TEXT_OFF;
    const DATA_VADDR: u64 = USER_ELF_BASE + PAGE_SIZE + DATA_OFF;

    fn put16(buf: &mut [u8], at: usize, v: u16) {
        buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(buf: &mut [u8], at: usize, v: u64) {
        buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Byte offset of field `at` in program header `i`.
    fn ph(i: usize, at: usize) -> usize {
        PHOFF + i * PHDR_SIZE + at
    }

    fn sample() -> [u8; 0x100] {
        let mut f = [0u8; 0x100];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = ELFCLASS64;
        f[5] = ELFDATA2LSB;
        f[6] = EV_CURRENT;
        put16(&mut f, 16, ET_EXEC);
        put16(&mut f, 18, EM_X86_64);
        put64(&mut f, 24, TEXT_VADDR);
        put64(&mut f, 32, PHOFF as u64);
        put16(&mut f, 54, PHDR_SIZE as u16);
        put16(&mut f, 56, 2);
        for (i, (flags, off, vaddr, filesz, memsz)) in [
            (5, TEXT_OFF, TEXT_VADDR, 0x10, 0x10),
            (6, DATA_OFF, DATA_VADDR, 0x10, 0x2000),
        ]
        .into_iter()
        .enumerate()
        {
            put32(&mut f, ph(i, 0), PT_LOAD);
            put32(&mut f, ph(i, 4), flags);
            put64(&mut f, ph(i, 8), off);
            put64(&mut f, ph(i, 16), vaddr);
            put64(&mut f, ph(i, 32), filesz);
            put64(&mut f, ph(i, 40), memsz);
        }
        f
    }

    fn rejects(f: &[u8], expected: ElfError) {
        assert_eq!(parse(f).map(|_| ()), Err(expected));
    }

    #[test_case]
    fn test_embedded_hello_parses() {
        let elf = parse(crate::programs::HELLO).expect("embedded ELF rejected");
        let segs = elf.segments();
        assert!(segs.len() >= 2);
        assert!(segs.iter().any(|s| s.executable && !s.writable));
        assert!(segs.iter().any(|s| s.writable && !s.executable && s.memsz > s.data.len() as u64));
        assert!(segs.iter().all(|s| s.vaddr >= USER_ELF_BASE && s.end() <= USER_SPACE_END));
        assert_eq!(elf.entry().as_u64(), USER_ELF_BASE, "_start opens .text");
    }

    #[test_case]
    fn test_sample_parses() {
        let f = sample();
        let elf = parse(&f).expect("sample rejected");
        assert_eq!(elf.segments().len(), 2);
        let data = elf.segments()[1];
        assert_eq!((data.vaddr, data.memsz, data.data.len()), (DATA_VADDR, 0x2000, 0x10));
        assert_eq!(data.data.as_ptr(), f[DATA_OFF as usize..].as_ptr());
    }

    #[test_case]
    fn test_header_checks() {
        rejects(&sample()[..10], ElfError::TooShort);
        rejects(&sample()[..63], ElfError::TooShort);
        let cases: [(usize, u8, ElfError); 4] = [
            (0, 0x7e, ElfError::BadMagic),
            (4, 1, ElfError::WrongClass),
            (5, 2, ElfError::WrongEndianness),
            (6, 0, ElfError::BadVersion),
        ];
        for (at, byte, err) in cases {
            let mut f = sample();
            f[at] = byte;
            rejects(&f, err);
        }
        let mut f = sample();
        put16(&mut f, 16, 3); // ET_DYN
        rejects(&f, ElfError::NotExecutable);
        let mut f = sample();
        put16(&mut f, 18, 3); // EM_386
        rejects(&f, ElfError::WrongMachine);
        let mut f = sample();
        put16(&mut f, 54, 32); // ELF32's phentsize
        rejects(&f, ElfError::BadProgramHeaderSize);
    }

    #[test_case]
    fn test_program_header_table_bounds() {
        // Truncated: the table needs 64 + 2 * 56 = 176 bytes.
        rejects(&sample()[..175], ElfError::ProgramHeadersOutOfBounds);
        let mut f = sample();
        put64(&mut f, 32, u64::MAX - 8); // phoff + table length overflows
        rejects(&f, ElfError::ProgramHeadersOutOfBounds);
        let mut f = sample();
        put16(&mut f, 56, u16::MAX);
        rejects(&f, ElfError::ProgramHeadersOutOfBounds);
    }

    #[test_case]
    fn test_segment_file_checks() {
        let mut f = sample();
        put64(&mut f, ph(1, 32), 0x2001); // filesz > memsz
        rejects(&f, ElfError::FileSizeExceedsMemSize);
        let mut f = sample();
        put64(&mut f, ph(1, 32), 0x41); // 0xc0 + 0x41 = one past the file
        rejects(&f, ElfError::SegmentOutOfFile);
        let mut f = sample();
        put64(&mut f, ph(1, 8), u64::MAX); // offset + filesz overflows
        rejects(&f, ElfError::SegmentOutOfFile);
        let mut f = sample();
        put64(&mut f, ph(1, 16), DATA_VADDR + 8); // no longer congruent
        rejects(&f, ElfError::Misaligned);
    }

    #[test_case]
    fn test_segment_address_checks() {
        // vaddr + memsz wraps past 2^64 (page-congruent so only overflow trips).
        let mut f = sample();
        put64(&mut f, ph(1, 16), 0xffff_ffff_ffff_f000 + DATA_OFF);
        rejects(&f, ElfError::AddressOverflow);
        // A kernel-half segment: no overflow, but not a user address.
        let mut f = sample();
        put64(&mut f, ph(1, 16), 0xffff_ffff_8000_0000 + DATA_OFF);
        rejects(&f, ElfError::OutsideUserRegion);
        // Ends one byte past the lower half.
        let mut f = sample();
        put64(&mut f, ph(1, 16), USER_SPACE_END - 0x2000 + DATA_OFF);
        put64(&mut f, ph(1, 40), 0x2000 - DATA_OFF + 1);
        rejects(&f, ElfError::OutsideUserRegion);
        // PML4[0], the bootloader's identity map.
        let mut f = sample();
        put64(&mut f, ph(1, 16), 0x40_0000 + DATA_OFF);
        rejects(&f, ElfError::OutsideUserRegion);
        // The last stack page.
        let mut f = sample();
        put64(&mut f, ph(1, 16), USER_STACK_TOP - PAGE_SIZE + DATA_OFF);
        rejects(&f, ElfError::OverlapsStack);
        // Starting below the stack and running into its first page.
        let mut f = sample();
        put64(&mut f, ph(1, 16), USER_STACK_START - PAGE_SIZE + DATA_OFF);
        rejects(&f, ElfError::OverlapsStack);
    }

    #[test_case]
    fn test_permission_and_layout_checks() {
        let mut f = sample();
        put32(&mut f, ph(1, 4), 7); // RWX
        rejects(&f, ElfError::WritableAndExecutable);
        // Data moved onto the text page.
        let mut f = sample();
        put64(&mut f, ph(1, 16), USER_ELF_BASE + DATA_OFF);
        rejects(&f, ElfError::SegmentsOverlap);
        // Data moved below text: not ascending.
        let mut f = sample();
        put64(&mut f, ph(1, 16), USER_ELF_BASE - PAGE_SIZE + DATA_OFF);
        rejects(&f, ElfError::SegmentsOverlap);
        // memsz far beyond the page budget.
        let mut f = sample();
        put64(&mut f, ph(1, 40), (MAX_IMAGE_PAGES + 1) * PAGE_SIZE);
        rejects(&f, ElfError::ImageTooLarge);
        // Interpreter request.
        let mut f = sample();
        put32(&mut f, ph(0, 0), PT_INTERP);
        rejects(&f, ElfError::NeedsDynamicLinking);
    }

    #[test_case]
    fn test_segment_count_checks() {
        // Both headers turned into non-loads, or emptied: nothing to run.
        let mut f = sample();
        put32(&mut f, ph(0, 0), 0x6474_e551); // PT_GNU_STACK
        put32(&mut f, ph(1, 0), 0x6474_e551);
        rejects(&f, ElfError::NoLoadableSegments);
        let mut f = sample();
        for i in 0..2 {
            put64(&mut f, ph(i, 32), 0);
            put64(&mut f, ph(i, 40), 0);
        }
        rejects(&f, ElfError::NoLoadableSegments);
        // Five one-page loads (the table grows into the segment bytes, which
        // is fine: nothing says they can't overlap in the file).
        let mut f = [0u8; 0x200];
        f[..0x100].copy_from_slice(&sample());
        put16(&mut f, 56, 5);
        for i in 0..5 {
            put32(&mut f, ph(i, 0), PT_LOAD);
            put32(&mut f, ph(i, 4), 5);
            put64(&mut f, ph(i, 8), 0);
            put64(&mut f, ph(i, 16), USER_ELF_BASE + i as u64 * PAGE_SIZE);
            put64(&mut f, ph(i, 32), 1);
            put64(&mut f, ph(i, 40), 1);
        }
        put64(&mut f, 24, USER_ELF_BASE);
        rejects(&f, ElfError::TooManySegments);
    }

    #[test_case]
    fn test_entry_checks() {
        // In the data segment: mapped, but not executable.
        let mut f = sample();
        put64(&mut f, 24, DATA_VADDR);
        rejects(&f, ElfError::EntryNotExecutable);
        // One past the end of text.
        let mut f = sample();
        put64(&mut f, 24, TEXT_VADDR + 0x10);
        rejects(&f, ElfError::EntryNotExecutable);
        // Unmapped.
        let mut f = sample();
        put64(&mut f, 24, 0);
        rejects(&f, ElfError::EntryNotExecutable);
    }
}
