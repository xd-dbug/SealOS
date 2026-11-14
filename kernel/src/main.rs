#![no_std] // don't link the Rust standard library
#![no_main] // disable all Rust-level entry points
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::BootInfo;
use core::panic::PanicInfo;
use kernel::{println, BOOTLOADER_CONFIG};

/// Kernel entry point invoked by the bootloader once long mode is set up.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let physical_memory_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("BOOTLOADER_CONFIG enables physical memory mapping");
    let memory_regions = &boot_info.memory_regions;

    if let Some(framebuffer) = boot_info.framebuffer.as_mut() {
        kernel::framebuffer::init(framebuffer);
        println!("Hello World{}", "!");

        kernel::init(physical_memory_offset, memory_regions);

        // fn stack_overflow() {
        //     stack_overflow(); // for each recursion, the return address is pushed
        // }
        //
        // // trigger a stack overflow
        // stack_overflow();

        #[cfg(test)]
        test_main();

        #[cfg(not(test))]
        run_hello();

        println!("It did not crash!");
    }

    loop {}
}

bootloader_api::entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// The W5 demo: load the embedded `hello` ELF, run it to completion at
/// Ring 3, and report its exit code on screen (its own output goes to
/// serial, through `SYS_WRITE`).
#[cfg(not(test))]
fn run_hello() {
    use kernel::scheduler::{ready_count, yield_now};
    match kernel::user::spawn_user_elf(kernel::programs::HELLO) {
        Ok(_) => {
            while ready_count() > 0 {
                yield_now();
            }
            println!("hello exited with {}", kernel::syscall::last_exit_code());
        }
        Err(e) => println!("hello failed to load: {:?}", e),
    }
}

#[test_case]
fn trivial_assertion() {
    assert_eq!(1, 1);
}

/// This function is called on panic.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("KERNEL PANIC: {}", info);
    loop {}
}

// In test builds, route panics through the shared test harness instead so a
// failing test reports over serial and exits QEMU rather than hanging.
#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}