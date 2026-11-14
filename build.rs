use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Cross-compiles the `kernel` crate (a separate workspace with its own
/// target/linker config) as part of the outer crate's build, then exposes the
/// resulting ELF path to `src/main.rs` via the `KERNEL_ELF` env var.
fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let kernel_dir = manifest_dir.join("kernel");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap();

    println!("cargo:rerun-if-changed={}", kernel_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", kernel_dir.join("build.rs").display());
    // The kernel embeds the user programs (`kernel/build.rs`), so they are
    // kernel inputs too. Cargo only reruns this script for paths listed
    // here, never for the kernel's own `rerun-if-changed` list. Inputs only,
    // not `user/target`, which changes on every build.
    let user_dir = manifest_dir.join("user");
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
    cmd.current_dir(&kernel_dir);
    cmd.arg("build");
    if profile == "release" {
        cmd.arg("--release");
    }
    let status = cmd.status().expect("failed to run cargo to build kernel");
    if !status.success() {
        panic!("kernel build failed");
    }

    let kernel_target_dir = kernel_dir.join("target/x86_64-seal_os").join(&profile);
    let kernel_elf = kernel_target_dir.join("kernel");
    assert!(
        kernel_elf.exists(),
        "expected kernel binary at {}",
        kernel_elf.display()
    );

    println!("cargo:rustc-env=KERNEL_ELF={}", kernel_elf.display());
    println!("cargo:rustc-env=RUNNER_OUT_DIR={}", out_dir.display());
}