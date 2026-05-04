use std::process::{Command, Stdio};

use json_repair_rs::{loads, repair_json};
use serde_json::json;

#[test]
fn repairs_upstream_representative_cases() {
    assert_eq!(
        repair_json(r#"{"key": """#).expect("repair"),
        r#"{"key": ""}"#
    );
    assert_eq!(
        repair_json(r#"{"employees":["John", "Anna","#).expect("repair"),
        r#"{"employees": ["John", "Anna"]}"#
    );
    assert_eq!(
        repair_json(r#"{"key1": {"key2": [1, 2, 3"#).expect("repair"),
        r#"{"key1": {"key2": [1, 2, 3]}}"#
    );
    assert_eq!(
        repair_json("{key:value,key2:value2}").expect("repair"),
        r#"{"key": "value", "key2": "value2"}"#
    );
    assert_eq!(
        repair_json("{'item1', 'item2', 'item3'}").expect("repair"),
        r#"["item1", "item2", "item3"]"#
    );
}

#[test]
fn repairs_markdown_and_prose_wrappers() {
    let raw = r#"
    **Decision**: bla, bla (some clarification):

    ```json
    {
      "key": "value"
    }
    ```
    "#;
    assert_eq!(repair_json(raw).expect("repair"), r#"{"key": "value"}"#);
}

#[test]
fn loads_repaired_values_for_programmatic_use() {
    assert_eq!(
        loads(r#"[{"key":"value"},{"key2": False,}]"#).expect("loads"),
        json!([
            {"key": "value"},
            {"key2": false}
        ])
    );
}

#[test]
fn cli_repairs_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_json-repair-rs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(b"{name: 'Ada', ok: tru}").expect("write");
    }
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success(), "status: {:?}", output.status);
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8"),
        "{\"name\": \"Ada\", \"ok\": true}\n"
    );
}

#[test]
fn cli_repairs_file_input() {
    let path = std::env::temp_dir().join(format!(
        "json_repair_rs_{}_file_input.json",
        std::process::id()
    ));
    std::fs::write(&path, "{name: 'Ada', skills: ['math' 'logic']}").expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_json-repair-rs"))
        .arg(&path)
        .output()
        .expect("run cli");
    std::fs::remove_file(&path).expect("remove fixture");

    assert!(output.status.success(), "status: {:?}", output.status);
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8"),
        "{\"name\": \"Ada\", \"skills\": [\"math\", \"logic\"]}\n"
    );
}

#[test]
fn cli_object_pretty_prints_repaired_json() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_json-repair-rs"))
        .arg("--object")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(b"{name: 'Ada'}").expect("write");
    }
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success(), "status: {:?}", output.status);
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8"),
        "{\n  \"name\": \"Ada\"\n}\n"
    );
}

#[test]
fn cli_errors_when_input_has_no_recoverable_json() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_json-repair-rs"))
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cli");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(b"not json").expect("write");
    }
    let output = child.wait_with_output().expect("output");
    assert!(!output.status.success(), "status: {:?}", output.status);
    let stderr = String::from_utf8(output.stderr).expect("utf8");
    assert!(
        stderr.contains("failed to repair JSON"),
        "stderr did not include context: {stderr}"
    );
    assert!(
        stderr.contains("input did not contain a recoverable JSON value"),
        "stderr did not include root cause: {stderr}"
    );
}
