//! Shaping of raw command output for the model (`plan/research/shell-tool.md` #9-#11),
//! and how bash output artifacts are named.

use axo::schemes::artifact::bash_tail;
use axo::tools::bash::shape_output;

/// 1000 unique 100-byte lines (100 000 bytes), so every line's offset is `index * 100`.
fn numbered_lines() -> String {
    (0..1000)
        .map(|index| format!("line {index:05} {}\n", "x".repeat(88)))
        .collect()
}

fn line(index: usize) -> String {
    format!("line {index:05} ")
}

#[test]
fn short_output_is_unchanged() {
    let shaped = shape_output(b"hello\nworld\n");
    assert!(!shaped.truncated);
    assert_eq!(shaped.text.trim_end(), "hello\nworld");
}

#[test]
fn long_output_keeps_head_and_tail() {
    let raw = numbered_lines();
    let shaped = shape_output(raw.as_bytes());
    assert!(shaped.truncated);

    // First 20 KB: lines 0..=203 start inside it.
    for index in [0, 1, 100, 190] {
        assert!(
            shaped.text.contains(&line(index)),
            "head line {index} missing"
        );
    }
    // Last 30 KB: bytes 70 000.. = lines 700..=999.
    for index in [710, 850, 998, 999] {
        assert!(
            shaped.text.contains(&line(index)),
            "tail line {index} missing"
        );
    }
    // The middle is elided.
    for index in [250, 400, 500, 650] {
        assert!(
            !shaped.text.contains(&line(index)),
            "middle line {index} kept"
        );
    }
    // The head comes before the tail.
    let head = shaped.text.find(&line(100)).expect("head line");
    let tail = shaped.text.find(&line(850)).expect("tail line");
    assert!(head < tail);
    // The inline text stays within the 50 KB budget plus a short notice.
    assert!(
        shaped.text.len() <= 52 * 1024,
        "{} bytes",
        shaped.text.len()
    );

    // A truncation notice that is not command output is present.
    assert!(
        shaped
            .text
            .lines()
            .any(|text| !text.trim().is_empty() && !raw.contains(text.trim())),
        "no truncation notice in output"
    );
}

#[test]
fn many_short_lines_are_cut_to_the_line_limit() {
    let raw: String = (0..5000).map(|index| format!("{index}\n")).collect();
    let shaped = shape_output(raw.as_bytes());
    assert!(shaped.truncated);
    assert!(
        shaped.text.lines().count() <= 2000 + 10,
        "{} lines",
        shaped.text.lines().count()
    );
    assert!(shaped.text.lines().any(|text| text == "0"));
    assert!(shaped.text.lines().any(|text| text == "4999"));
}

#[test]
fn long_lines_are_cut_at_768_bytes() {
    let raw = format!("{}\nafter\n", "a".repeat(5000));
    let shaped = shape_output(raw.as_bytes());
    let long = shaped
        .text
        .lines()
        .find(|text| text.starts_with('a'))
        .expect("cut line kept");
    assert!(long.starts_with(&"a".repeat(700)));
    assert!(long.len() < 900, "line is {} bytes", long.len());
    assert!(!shaped.text.contains(&"a".repeat(769)));
    assert!(shaped.text.lines().any(|text| text == "after"));
    // The full line survives only in the spool, so the result must say it was cut.
    assert!(shaped.truncated);
}

#[test]
fn line_cut_respects_utf8_boundaries() {
    let raw = format!("{}\n", "é".repeat(1000));
    let shaped = shape_output(raw.as_bytes());
    let cut = shaped.text.lines().next().expect("line");
    assert!(cut.starts_with(&"é".repeat(300)));
    assert!(cut.len() < 900);
}

#[test]
fn carriage_returns_fold_to_the_last_segment() {
    let shaped = shape_output(b"start\n10%\r50%\r100%\ndone\n");
    assert_eq!(
        shaped.text.lines().collect::<Vec<_>>(),
        ["start", "100%", "done"]
    );
    assert!(!shaped.text.contains('\r'));
    assert!(!shaped.text.contains("50%"));
}

#[test]
fn ansi_escapes_are_stripped() {
    let shaped =
        shape_output(b"\x1b[1;31mred\x1b[0m plain\n\x1b]0;title\x07after\n\x1b[2Kcleared\n");
    assert!(!shaped.text.contains('\x1b'));
    assert!(!shaped.text.contains("[1;31m"));
    assert_eq!(
        shaped.text.lines().collect::<Vec<_>>(),
        ["red plain", "after", "cleared"]
    );
    assert!(!shaped.truncated);
}

#[test]
fn bash_artifact_tails_escape_unsafe_call_ids() {
    assert_eq!(bash_tail("a|b"), "bash.a%7Cb.log");
    assert_eq!(bash_tail("toolu_01Ab"), "bash.toolu_01Ab.log");
    for id in ["../../etc/passwd", "/tmp/x"] {
        assert!(!bash_tail(id).contains('/'), "{id}");
    }
}
