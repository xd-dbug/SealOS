//! #55: the shell's line editor and parser, tested on the host. The user
//! crate builds only for seal_os and has no test harness, so this includes
//! the module's source directly; it compiles here because `line.rs` uses
//! nothing but `core`.

#[path = "../user/src/line.rs"]
#[allow(dead_code)]
mod line;

use line::{Action, LINE_CAPACITY, LineEditor, MAX_ARGS, Parsed, parse};

const BS: u8 = 0x08;

/// Feeds `input` and returns the action for each byte.
fn feed(editor: &mut LineEditor, input: &[u8]) -> Vec<Action> {
    input.iter().map(|&byte| editor.feed(byte)).collect()
}

#[test]
fn typing_echoes_and_enter_submits() {
    let mut editor = LineEditor::new();
    let actions = feed(&mut editor, b"hi\n");
    assert_eq!(actions, [Action::Echo(b'h'), Action::Echo(b'i'), Action::Submit]);
    assert_eq!(editor.line(), "hi");
}

#[test]
fn backspace_removes_the_last_byte() {
    let mut editor = LineEditor::new();
    let actions = feed(&mut editor, &[b'a', b'b', BS, b'c', b'\n']);
    assert_eq!(
        actions,
        [Action::Echo(b'a'), Action::Echo(b'b'), Action::Erase, Action::Echo(b'c'), Action::Submit]
    );
    assert_eq!(editor.line(), "ac");
}

#[test]
fn backspace_on_an_empty_line_does_nothing() {
    let mut editor = LineEditor::new();
    assert_eq!(editor.feed(BS), Action::Ignore);
    assert_eq!(editor.line(), "");
    // Also once a line has been typed and fully erased: no underflow.
    assert_eq!(feed(&mut editor, &[b'x', BS, BS, BS]), [
        Action::Echo(b'x'),
        Action::Erase,
        Action::Ignore,
        Action::Ignore
    ]);
    assert_eq!(editor.feed(b'y'), Action::Echo(b'y'));
    assert_eq!(editor.line(), "y");
}

#[test]
fn del_erases_like_backspace() {
    let mut editor = LineEditor::new();
    assert_eq!(feed(&mut editor, &[b'a', 0x7f]), [Action::Echo(b'a'), Action::Erase]);
    assert_eq!(editor.line(), "");
}

#[test]
fn carriage_return_submits_too() {
    let mut editor = LineEditor::new();
    assert_eq!(feed(&mut editor, b"ok\r"), [Action::Echo(b'o'), Action::Echo(b'k'), Action::Submit]);
}

#[test]
fn a_full_line_stops_accepting_and_keeps_what_it_has() {
    let mut editor = LineEditor::new();
    for i in 0..LINE_CAPACITY {
        assert_eq!(editor.feed(b'a'), Action::Echo(b'a'), "byte {i} refused before full");
    }
    assert_eq!(editor.feed(b'b'), Action::Ignore);
    assert_eq!(editor.feed(b'c'), Action::Ignore);
    assert_eq!(editor.line().len(), LINE_CAPACITY);
    assert!(editor.line().bytes().all(|b| b == b'a'), "an overflow byte got in");
    // Still editable and still submittable.
    assert_eq!(editor.feed(BS), Action::Erase);
    assert_eq!(editor.feed(b'z'), Action::Echo(b'z'));
    assert_eq!(editor.feed(b'\n'), Action::Submit);
    assert!(editor.line().ends_with("az"));
}

#[test]
fn non_ascii_and_control_bytes_never_enter_the_line() {
    let mut editor = LineEditor::new();
    // "é" as UTF-8, a NUL, an escape, a tab.
    for byte in [0xc3, 0xa9, 0x00, 0x1b, b'\t'] {
        assert_eq!(editor.feed(byte), Action::Ignore, "byte {byte:#x}");
    }
    assert_eq!(editor.line(), "");
    // So a backspace can never cut a character in half: there is nothing
    // multi-byte to cut.
    assert_eq!(feed(&mut editor, &[b'a', 0xc3, 0xa9, BS]), [
        Action::Echo(b'a'),
        Action::Ignore,
        Action::Ignore,
        Action::Erase
    ]);
    assert_eq!(editor.line(), "");
}

#[test]
fn clear_starts_a_new_line() {
    let mut editor = LineEditor::new();
    feed(&mut editor, b"first\n");
    editor.clear();
    assert_eq!(editor.line(), "");
    feed(&mut editor, b"second");
    assert_eq!(editor.line(), "second");
}

fn command(line: &str) -> (&str, Vec<&str>) {
    match parse(line) {
        Parsed::Command(command) => (command.name, command.args().to_vec()),
        other => panic!("{line:?} parsed as {other:?}"),
    }
}

#[test]
fn splits_into_name_and_arguments() {
    assert_eq!(command("echo hi there"), ("echo", vec!["hi", "there"]));
    assert_eq!(command("help"), ("help", vec![]));
}

#[test]
fn extra_whitespace_never_makes_an_empty_argument() {
    assert_eq!(command("  echo  hi  there "), ("echo", vec!["hi", "there"]));
    assert_eq!(command("\techo\t\thi "), ("echo", vec!["hi"]));
}

#[test]
fn empty_and_blank_lines_are_no_command() {
    assert_eq!(parse(""), Parsed::Empty);
    assert_eq!(parse("     "), Parsed::Empty);
    assert_eq!(parse(" \t "), Parsed::Empty);
}

#[test]
fn too_many_arguments_is_an_error_not_a_truncation() {
    let exactly = format!("cmd{}", " a".repeat(MAX_ARGS));
    assert_eq!(command(&exactly).1.len(), MAX_ARGS);
    let over = format!("cmd{}", " a".repeat(MAX_ARGS + 1));
    assert_eq!(parse(&over), Parsed::TooManyArgs);
}

/// The editor's output is the parser's input: the issue's own example, end
/// to end.
#[test]
fn an_edited_line_parses_as_typed() {
    let mut editor = LineEditor::new();
    feed(&mut editor, &[b'e', b'c', b'h', b'x', BS, b'o', b' ', b' ', b'h', b'i', b'\n']);
    assert_eq!(command(editor.line()), ("echo", vec!["hi"]));
}
