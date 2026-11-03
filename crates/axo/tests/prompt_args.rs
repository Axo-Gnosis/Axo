//! `/name args` parsing and substitution (pi `core/prompt-templates.ts`).

use axo::context::skills::{Skill, expand_skill_command};
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
fn skill_args_fill_placeholders_or_follow_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let skill = |name: &str, body: &str| {
        let path = dir.path().join(format!("{name}.md"));
        std::fs::write(&path, format!("---\ndescription: d\n---\n{body}\n")).unwrap();
        Skill {
            name: name.into(),
            description: "d".into(),
            path,
            disable_model_invocation: false,
            argument_hint: None,
        }
    };
    let skills = [
        skill("greet", "Say hello to $1 in a ${2:-friendly} tone."),
        skill("plain", "Do the thing."),
    ];
    let block = |name: &str, body: &str| {
        format!(
            "<skill name=\"{name}\" location=\"skill://{name}\">\nReferences are relative to skill://{name}/.\n\n{body}\n</skill>"
        )
    };

    assert_eq!(
        expand_skill_command("/greet Alice cheerful", &skills),
        block("greet", "Say hello to Alice in a cheerful tone.")
    );
    assert_eq!(
        expand_skill_command("/plain Alice cheerful", &skills),
        format!("{}\n\nAlice cheerful", block("plain", "Do the thing."))
    );
    assert_eq!(
        expand_skill_command("/plain", &skills),
        block("plain", "Do the thing.")
    );
}

#[test]
fn text_without_placeholders_is_unchanged() {
    let text = "Cost is $ 5, see {braces} and $ alone; ${unknown} stays.";
    assert_eq!(substitute_args(text, &args(&["x"])), text);
}
