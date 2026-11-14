//! #47: the built user program is the kind of file the kernel loader expects,
//! checked from the outside with `readelf` (binutils) rather than with the
//! kernel's own parser, so a bug in `elf.rs` can't hide a bad build.
//!
//! A host test: `cargo test` from the repo root. The root `build.rs` builds
//! the kernel, whose `build.rs` builds `user/`, so the ELF exists by now.

use std::path::PathBuf;
use std::process::Command;

/// Must match `USER_ELF_BASE` in `kernel/src/user.rs` and `user/linker.ld`.
const USER_ELF_BASE: u64 = 0x80_0040_0000;
const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

fn hello_elf() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("user/target/x86_64-seal_os-user/release/hello")
}

fn readelf(args: &[&str]) -> String {
    let out = Command::new("readelf")
        .args(args)
        .arg(hello_elf())
        .output()
        .expect("readelf not found (install binutils)");
    assert!(out.status.success(), "readelf failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn hex(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
}

/// `(vaddr, memsz, flags)` of every PT_LOAD, from `readelf -l -W`.
fn loads() -> Vec<(u64, u64, String)> {
    readelf(&["-l", "-W"])
        .lines()
        .filter(|l| l.trim_start().starts_with("LOAD"))
        .map(|l| {
            // LOAD offset vaddr paddr filesz memsz flags... align
            let f: Vec<&str> = l.split_whitespace().collect();
            let flags = f[6..f.len() - 1].concat();
            (hex(f[2]), hex(f[5]), flags)
        })
        .collect()
}

#[test]
fn header_is_static_x86_64_exec() {
    let h = readelf(&["-h"]);
    assert!(h.contains("ELF64"), "{h}");
    assert!(h.contains("little endian"), "{h}");
    assert!(h.contains("EXEC (Executable file)"), "not ET_EXEC:\n{h}");
    assert!(h.contains("Advanced Micro Devices X86-64"), "not EM_X86_64:\n{h}");
    let l = readelf(&["-l", "-W"]);
    assert!(!l.contains("INTERP") && !l.contains("DYNAMIC"), "needs a dynamic linker:\n{l}");
}

#[test]
fn loads_are_in_the_user_half_and_split_by_permission() {
    let loads = loads();
    assert!(loads.len() >= 2, "{loads:?}");
    for (vaddr, memsz, flags) in &loads {
        assert!(*vaddr >= USER_ELF_BASE && vaddr + memsz <= USER_SPACE_END, "{vaddr:#x}");
        assert_eq!(vaddr % 4096, 0, "segment at {vaddr:#x} not page-aligned");
        assert!(!(flags.contains('W') && flags.contains('E')), "W+X segment at {vaddr:#x}");
    }
    assert!(loads.iter().any(|(_, _, f)| f == "RE"), "no R-X segment: {loads:?}");
    assert!(loads.iter().any(|(_, _, f)| f == "RW"), "no RW- segment: {loads:?}");
    // Page-aligned starts and ascending order mean no two share a page.
    for pair in loads.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "segments overlap: {pair:?}");
    }
}

#[test]
fn entry_is_inside_the_code_segment() {
    let h = readelf(&["-h"]);
    let entry = h
        .lines()
        .find_map(|l| l.trim().strip_prefix("Entry point address:"))
        .map(|v| hex(v.trim()))
        .unwrap();
    let in_code = loads()
        .iter()
        .any(|(v, m, f)| f == "RE" && (*v..v + m).contains(&entry));
    assert!(in_code, "entry {entry:#x} not in the R-X segment");
}
