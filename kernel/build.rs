use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds the user programs in `../user` and hands their paths to the kernel
/// (`env!("USER_HELLO_ELF")`, read by `include_bytes!` in `programs.rs`).
///
/// This lives in the kernel crate, not the root `build.rs`, because
/// `include_bytes!` needs the file before the kernel compiles, and
/// `cd kernel && cargo test` never runs the root build script.
fn main() {
    let kernel_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let user_dir = kernel_dir.parent().unwrap().join("user");
    // Its own target dir, never the kernel's: the outer cargo holds a lock on
    // the kernel's target dir for the whole build, so a nested cargo pointed
    // at the same one would wait on it forever. Explicit rather than the
    // default `user/target`, so a `CARGO_TARGET_DIR` in the environment can't
    // redirect both builds to one directory.
    let target_dir = user_dir.join("target");

    // Inputs only. Not the whole `user/` dir: `user/target` changes on every
    // build, which would make this script (and the kernel) rebuild every time.
    for input in [
        "src",
        "Cargo.toml",
        "build.rs",
        "linker.ld",
        "x86_64-seal_os-user.json",
        ".cargo/config.toml",
    ] {
        println!("cargo:rerun-if-changed={}", user_dir.join(input).display());
    }

    let mut cmd = Command::new(env::var_os("CARGO").unwrap());
    // `current_dir` so the user crate's own `.cargo/config.toml` (target,
    // build-std) is the one cargo finds, not the kernel's.
    cmd.current_dir(&user_dir)
        .args(["build", "--release", "--bins", "--target-dir"])
        .arg(&target_dir)
        // Cargo exports the kernel's rustflags to build scripts; a nested
        // cargo would read them back and apply them to the user build.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS");
    let status = cmd.status().expect("failed to run cargo to build user programs");
    assert!(status.success(), "user program build failed");

    // Always release: small, and independent of the kernel's own profile.
    let bin_dir = target_dir.join("x86_64-seal_os-user").join("release");
    embed(&bin_dir, "hello", "USER_HELLO_ELF");
}

/// Exports the path and length of one built user program. The length lets a
/// test check the embedded slice is the whole file.
fn embed(bin_dir: &Path, name: &str, var: &str) {
    let elf = bin_dir.join(name);
    let len = std::fs::metadata(&elf)
        .unwrap_or_else(|e| panic!("expected user program at {}: {e}", elf.display()))
        .len();
    println!("cargo:rustc-env={var}={}", elf.display());
    println!("cargo:rustc-env={var}_LEN={len}");
}
