use std::env;
use std::path::PathBuf;

/// Passes the linker script by absolute path: the linker's working directory
/// isn't guaranteed, and a `rustflags` entry would be silently ignored if the
/// parent build exports `CARGO_ENCODED_RUSTFLAGS`.
fn main() {
    let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let script = dir.join("linker.ld");
    println!("cargo:rerun-if-changed={}", script.display());
    println!("cargo:rustc-link-arg=-T{}", script.display());
}
