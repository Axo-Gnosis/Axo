//! `/name args` parsing and substitution (pi `core/prompt-templates.ts`).

use axo::context::{parse_command_args, substitute_args};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn parse_splits_on_whitespace_and_honors_quotes() {
    assert_eq!(
        parse_command_args(r#"  a "b c"   'd e' f"g h"i "#),
        args(&["a", "b c", "d e", "fg hi"])
    );
    assert_eq!(
        parse_command_args(r#""it's" 'say "hi"'"#),
        args(&["it's", r#"say "hi""#])
    );
    assert!(parse_command_args(" \t\n ").is_empty());
}

#[test]
fn parse_keeps_quoted_empty_args() {
    assert_eq!(parse_command_args(r#"x "" y"#), args(&["x", "", "y"]));
    assert_eq!(parse_command_args("''"), args(&[""]));
    assert_eq!(parse_command_args(r#""" ''"#), args(&["", ""]));
}

#[test]
fn all_args() {
    let list = args(&["a", "b c"]);
    assert_eq!(substitute_args("[$ARGUMENTS]", &list), "[a b c]");
    assert_eq!(substitute_args("[$@]", &list), "[a b c]");
    assert_eq!(substitute_args("[$ARGUMENTS]", &[]), "[]");
    assert_eq!(
        substitute_args("${@:-none}|${ARGUMENTS:-none}", &[]),
        "none|none"
    );
    assert_eq!(substitute_args("${@:-none}", &list), "a b c");
}

#[test]
fn positional_args() {
    let list = args(&["a", "b"]);
    assert_eq!(substitute_args("$1 and $2", &list), "a and b");
    assert_eq!(substitute_args("[$3]", &list), "[]");
    assert_eq!(substitute_args("[$0]", &list), "[]");
    let ten = args(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "ten"]);
    assert_eq!(substitute_args("$10", &ten), "ten");
}

#[test]
fn positional_default() {
    assert_eq!(substitute_args("${1:-default}", &[]), "default");
    assert_eq!(substitute_args("${1:-default}", &args(&[""])), "default");
    assert_eq!(substitute_args("${1:-default}", &args(&["x"])), "x");
    assert_eq!(substitute_args("${2:-}", &args(&["x"])), "");
}

#[test]
fn slices() {
    let list = args(&["a", "b", "c", "d"]);
    assert_eq!(substitute_args("${@:2}", &list), "b c d");
    assert_eq!(substitute_args("${@:2:1}", &list), "b");
    assert_eq!(substitute_args("${@:2:2}", &list), "b c");
    assert_eq!(substitute_args("${@:0}", &list), "a b c d");
    assert_eq!(substitute_args("[${@:9}]", &list), "[]");
    assert_eq!(substitute_args("${@:3:10}", &list), "c d");
}

#[test]
fn quoted_empty_args_keep_their_position() {
    let list = parse_command_args(r#""src/main.rs" "" tail"#);
    assert_eq!(
        substitute_args("Fix $1|$2|$3", &list),
        "Fix src/main.rs||tail"
    );
    assert_eq!(substitute_args("${2:-none}", &list), "none");
    assert_eq!(substitute_args("[${@:2}]", &list), "[ tail]");
}

#[test]
fn substitution_is_single_pass() {
    assert_eq!(substitute_args("$1", &args(&["$2", "x"])), "$2");
    assert_eq!(
        substitute_args("$ARGUMENTS", &args(&["$1", "${@:1}"])),
        "$1 ${@:1}"
    );
    assert_eq!(substitute_args("${2:-$1}", &args(&["a"])), "$1");
    assert_eq!(
        substitute_args("${@:2}", &args(&["a", "$ARGUMENTS"])),
        "$ARGUMENTS"
    );
}

#[test]
fn text_without_placeholders_is_unchanged() {
    let text = "Cost is $ 5, see {braces} and $ alone; ${unknown} stays.";
    assert_eq!(substitute_args(text, &args(&["x"])), text);
}
