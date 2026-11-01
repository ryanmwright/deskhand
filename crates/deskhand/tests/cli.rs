use assert_cmd::Command;

#[test]
fn prints_version() {
    let out = Command::cargo_bin("deskhand")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains(env!("CARGO_PKG_VERSION")));
}
