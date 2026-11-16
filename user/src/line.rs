//! The shell's line editor and command parser: bytes in, actions and tokens
//! out. No syscalls and nothing but `core`, on purpose: the user crate has no
//! test harness (its only target is seal_os itself), so the root crate's
//! `tests/shell_line.rs` includes this file with `#[path]` and tests it on
//! the host. Keep it free of `crate::` paths or that stops compiling.
//!
//! No allocator either, so the line and the argument list are fixed arrays.

/// Longest line the editor holds. Short enough that prompt + line fits on
/// one console row at any plausible resolution (the framebuffer console can't
/// back up across a wrapped line), long enough for any built-in.
pub const LINE_CAPACITY: usize = 100;

/// Most arguments one command can take (the name not counted).
pub const MAX_ARGS: usize = 16;

/// What the caller should do on screen after feeding the editor one byte.
/// The editor decides, the caller draws: that split is what keeps this
/// testable without a console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The byte joined the line; show it.
    Echo(u8),
    /// The last byte left the line; rub it out.
    Erase,
    /// Enter: the line is complete. Read it with `line`, then `clear`.
    Submit,
    /// Nothing changed and nothing should be shown.
    Ignore,
}

/// One line being typed. ASCII only for W6: the keyboard path can deliver
/// multi-byte UTF-8, and a byte-wise backspace would cut such a character in
/// half and leave an invalid sequence, so non-ASCII bytes never enter the
/// line at all. One byte is then always one character and one screen cell.
pub struct LineEditor {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl LineEditor {
    pub const fn new() -> Self {
        LineEditor { bytes: [0; LINE_CAPACITY], len: 0 }
    }

    /// Applies one input byte.
    /// - Enter (`\n`, or `\r` from a terminal that sends it) submits.
    /// - Backspace (`0x08`) or DEL (`0x7f`) removes the last byte. On an
    ///   empty line it is ignored, so the caller never erases its prompt.
    /// - Printable ASCII is appended. When the line is full it is ignored
    ///   and the line kept: the user sees typing stop, and can still submit
    ///   or backspace. (Silently wrapping or submitting early would run a
    ///   command nobody typed.)
    /// - Everything else (control bytes, non-ASCII) is ignored.
    pub fn feed(&mut self, byte: u8) -> Action {
        match byte {
            b'\n' | b'\r' => Action::Submit,
            0x08 | 0x7f => {
                if self.len == 0 {
                    return Action::Ignore;
                }
                self.len -= 1;
                Action::Erase
            }
            b' '..=b'~' => {
                if self.len == LINE_CAPACITY {
                    return Action::Ignore;
                }
                self.bytes[self.len] = byte;
                self.len += 1;
                Action::Echo(byte)
            }
            _ => Action::Ignore,
        }
    }

    /// The line so far. Always valid UTF-8, since only ASCII gets in; the
    /// fallback keeps this free of `unsafe` rather than covering a real case.
    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// Starts a new line.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

/// A command name and its arguments, borrowed from the line they were typed
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command<'a> {
    pub name: &'a str,
    args: [&'a str; MAX_ARGS],
    argc: usize,
}

impl<'a> Command<'a> {
    pub fn args(&self) -> &[&'a str] {
        &self.args[..self.argc]
    }
}

/// The outcome of splitting one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// Empty or all whitespace: nothing to run.
    Empty,
    Command(Command<'a>),
    /// More than `MAX_ARGS` arguments. An error rather than a silent cut:
    /// running `echo` with some of its words missing would be a wrong answer.
    TooManyArgs,
}

/// Splits `line` on ASCII whitespace into a name and arguments. Leading,
/// trailing and repeated whitespace never produce an empty token. No quoting
/// or escapes yet.
pub fn parse(line: &str) -> Parsed<'_> {
    let mut tokens = line.split_ascii_whitespace();
    let Some(name) = tokens.next() else {
        return Parsed::Empty;
    };
    let mut args = [""; MAX_ARGS];
    let mut argc = 0;
    for token in tokens {
        if argc == MAX_ARGS {
            return Parsed::TooManyArgs;
        }
        args[argc] = token;
        argc += 1;
    }
    Parsed::Command(Command { name, args, argc })
}
