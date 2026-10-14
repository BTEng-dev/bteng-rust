//! `bteng validate --strict` and `structure` output, exercised through the real binary.

use std::path::PathBuf;
use std::process::{Command, Output};

const GOOD: &str =
    r#"<root main_tree_to_execute="X"><BehaviorTree ID="X"><AlwaysSuccess/></BehaviorTree></root>"#;
const UNKNOWN_TYPE: &str =
    r#"<root main_tree_to_execute="X"><BehaviorTree ID="X"><Bogus/></BehaviorTree></root>"#;
const WITH_STRING_PORT: &str = r#"<root main_tree_to_execute="X"><BehaviorTree ID="X"><Region hz="10" overrun="skip"><AlwaysSuccess/></Region></BehaviorTree></root>"#;

fn fixture(name: &str, xml: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, xml).unwrap();
    path
}

fn bteng(args: &[&str], file: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bteng"))
        .args(args)
        .arg(file)
        .output()
        .unwrap()
}

#[test]
fn validate_tolerates_unknown_node_types_by_default() {
    let out = bteng(&["validate"], &fixture("unknown_default.xml", UNKNOWN_TYPE));
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Bogus"));
}

#[test]
fn validate_strict_fails_on_unknown_node_types() {
    let file = fixture("unknown_strict.xml", UNKNOWN_TYPE);
    let out = Command::new(env!("CARGO_BIN_EXE_bteng"))
        .args(["validate"])
        .arg(&file)
        .arg("--strict")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not built in"));
}

#[test]
fn validate_strict_passes_a_fully_known_tree() {
    let file = fixture("good_strict.xml", GOOD);
    let out = Command::new(env!("CARGO_BIN_EXE_bteng"))
        .args(["validate"])
        .arg(&file)
        .arg("--strict")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn structure_reports_port_types_without_module_paths() {
    let out = bteng(
        &["structure"],
        &fixture("string_port.xml", WITH_STRING_PORT),
    );
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains(r#""type":"String""#), "{json}");
    assert!(!json.contains("alloc::"), "module path leaked: {json}");
}
