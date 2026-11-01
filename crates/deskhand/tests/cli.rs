use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::Value;

fn deskhand() -> Command {
    let mut cmd = Command::cargo_bin("deskhand").unwrap();
    cmd.env_remove("DESKHAND_CONFIG").env_remove("RUST_LOG");
    cmd
}

/// Writes a config into a temp dir. The broker is 127.0.0.1:1, where nothing listens.
fn config(extra: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = format!(
        r#"
[device]
name = "CLI Test"
node_id = "clitest"

[mqtt]
host = "127.0.0.1"
port = 1

[integrations.metrics]
enabled = false

{extra}
"#
    );
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

#[test]
fn prints_version() {
    let out = deskhand().arg("--version").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn doctor_json_reports_broker_failure() {
    let (_dir, path) = config("[[script]]\nid = \"hello\"\ncommand = [\"true\"]\nbogus = 1");
    let out = deskhand()
        .arg("--config")
        .arg(&path)
        .args(["doctor", "--json"])
        .assert()
        .code(1);
    let report: Value = serde_json::from_slice(&out.get_output().stdout).expect("JSON report");
    assert_eq!(report["ok"], false);
    assert_eq!(report["config"]["ok"], true);
    assert_eq!(report["device"]["node_id"], "clitest");
    assert_eq!(report["mqtt"]["connected"], false);
    assert!(
        report["mqtt"]["error"]
            .as_str()
            .unwrap()
            .contains("refused"),
        "{report}"
    );
    let warnings = report["config"]["warnings"].to_string();
    assert!(warnings.contains("script"), "{warnings}");
    let caps: Vec<&Value> = report["integrations"].as_array().unwrap().iter().collect();
    let cap = |name: &str| {
        caps.iter()
            .find(|c| c["capability"] == name)
            .unwrap_or_else(|| panic!("no {name} in {report}"))
    };
    assert_eq!(cap("scripts")["selected"], "scripts");
    assert_eq!(cap("metrics")["enabled"], "false");
    assert_eq!(cap("metrics")["selected"], Value::Null);
    assert_eq!(cap("command_sensors")["selected"], Value::Null);
}

#[test]
fn doctor_reports_config_errors() {
    let (_dir, path) = config("[[script]]\nid = \"Bad Id\"\ncommand = [\"true\"]");
    let out = deskhand()
        .arg("--config")
        .arg(&path)
        .args(["doctor", "--json"])
        .assert()
        .code(1);
    let report: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(report["config"]["ok"], false);
    assert!(
        report["config"]["error"]
            .as_str()
            .unwrap()
            .contains("Bad Id")
    );
}

#[test]
fn print_discovery_lists_configured_entities() {
    let (_dir, path) = config(
        r#"
[[script]]
id = "hello"
name = "Say hello"
command = ["true"]

[[sensor]]
id = "answer"
command = ["echo", "42"]
"#,
    );
    let out = deskhand()
        .arg("--config")
        .arg(&path)
        .args(["print-discovery", "--all"])
        .assert()
        .success();
    let all: Value = serde_json::from_slice(&out.get_output().stdout).expect("JSON");
    let doc = &all["homeassistant/device/deskhand_clitest/config"];
    assert_eq!(doc["device"]["name"], "CLI Test");
    let components = doc["components"].as_object().unwrap();
    assert_eq!(components["script_hello"]["platform"], "button");
    assert_eq!(components["script_hello"]["name"], "Say hello");
    assert_eq!(components["answer"]["platform"], "sensor");
    assert!(!components.contains_key("cpu_percent"), "metrics disabled");
    assert_eq!(all["deskhand/clitest/answer/state"], "42");
}

#[test]
fn missing_config_is_a_clear_error() {
    let out = deskhand()
        .args(["--config", "/nonexistent/deskhand.toml", "print-discovery"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("could not load the config file"),
        "{stderr}"
    );
}

#[test]
fn uninstall_without_broker_fails() {
    let (_dir, path) = config("");
    let out = deskhand()
        .arg("--config")
        .arg(&path)
        .args(["uninstall", "--yes"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(
        stderr.contains("cannot connect to the MQTT broker"),
        "{stderr}"
    );
}
