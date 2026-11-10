//! The seal_os shell (W6): prompt, read a line, run a built-in, repeat.
//!
//! Everything it knows about the world comes through syscalls: keys arrive
//! from `read`, and all output (including the echo of what is typed, which
//! the kernel no longer does) goes out through `write`. The editing and
//! parsing rules live in `seal_user::line`, which is tested on the host; this
//! file is the part that needs a kernel underneath.
//!
//! No `run <program>` yet: there is one shared user image, so loading another
//! program would unmap this one while it runs (design-review items 24, 25,
//! 36).

#![no_std]
#![no_main]

use seal_user::line::{Action, Command, LineEditor, Parsed, parse};
use seal_user::{print, println, read, uptime_ms, write};

seal_user::entry!(main);

pub const PROMPT: &str = "seal> ";

/// Back one cell, blank it, back again: the console's backspace only moves
/// the cursor, so erasing is spelled out here.
const RUB_OUT: &[u8] = b"\x08 \x08";

/// Exit code if the console itself stops working (a `read` error).
const EXIT_NO_INPUT: u64 = 2;

struct Builtin {
    name: &'static str,
    usage: &'static str,
    about: &'static str,
    run: fn(&Command),
}

/// One table drives both dispatch and `help`, so a command can't exist
/// without being listed.
const BUILTINS: &[Builtin] = &[
    Builtin { name: "help", usage: "help", about: "list the built-in commands", run: help },
    Builtin { name: "echo", usage: "echo [args]", about: "print the arguments", run: echo },
    Builtin { name: "uptime", usage: "uptime", about: "time since boot", run: uptime },
    Builtin { name: "exit", usage: "exit [code]", about: "leave the shell (default code 0)", run: exit },
];

fn help(_: &Command) {
    println!("built-in commands:");
    for builtin in BUILTINS {
        println!("  {:<12} {}", builtin.usage, builtin.about);
    }
}

/// Arguments separated by single spaces, however they were spaced when typed
/// (the parser already dropped the original whitespace).
fn echo(command: &Command) {
    for (i, arg) in command.args().iter().enumerate() {
        if i > 0 {
            print!(" ");
        }
        print!("{}", arg);
    }
    println!();
}

/// The one built-in that asks the kernel something (`SYS_UPTIME`).
fn uptime(_: &Command) {
    let ms = uptime_ms();
    println!("up {}.{:02} s", ms / 1000, ms % 1000 / 10);
}

/// `exit` with no argument exits 0. A code that isn't a decimal number is
/// reported and the shell stays: exiting with a made-up code would hide the
/// typo.
fn exit(command: &Command) {
    let code = match command.args() {
        [] => 0,
        [code] => match code.parse::<u64>() {
            Ok(code) => code,
            Err(_) => {
                println!("exit: not a number: {}", code);
                return;
            }
        },
        _ => {
            println!("exit: too many arguments");
            return;
        }
    };
    seal_user::exit(code)
}

fn run(line: &str) {
    match parse(line) {
        Parsed::Empty => {}
        Parsed::TooManyArgs => println!("too many arguments"),
        Parsed::Command(command) => match BUILTINS.iter().find(|b| b.name == command.name) {
            Some(builtin) => (builtin.run)(&command),
            None => println!("{}: command not found", command.name),
        },
    }
}

fn main() -> u64 {
    let mut editor = LineEditor::new();
    let mut input = [0u8; 16];
    print!("{}", PROMPT);
    loop {
        // Blocks until at least one key is waiting; may return several.
        let Ok(count) = read(&mut input) else {
            return EXIT_NO_INPUT;
        };
        // Every byte goes through the editor, including any that follow an
        // Enter in the same read: type-ahead belongs to the next line.
        for &byte in &input[..count as usize] {
            match editor.feed(byte) {
                Action::Echo(byte) => {
                    let _ = write(&[byte]);
                }
                Action::Erase => {
                    let _ = write(RUB_OUT);
                }
                Action::Submit => {
                    println!();
                    run(editor.line());
                    editor.clear();
                    print!("{}", PROMPT);
                }
                Action::Ignore => {}
            }
        }
    }
}
