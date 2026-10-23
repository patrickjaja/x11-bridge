//! Computer Use bridge CLI contract v1, checked against the built binary with
//! no X display and a scrubbed environment.

use std::process::{Command, Output};

use serde_json::Value;

fn run(args: &[&str]) -> Output {
    let home = std::env::temp_dir().join(format!("x11-bridge-contract-{}", std::process::id()));
    let state = home.join("state");
    let runtime = home.join("run");
    Command::new(env!("CARGO_BIN_EXE_x11-bridge"))
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .expect("spawn x11-bridge")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
}

#[test]
fn version_format() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out).trim(),
        format!("x11-bridge {}", env!("CARGO_PKG_VERSION"))
    );
    let ver = stdout(&out).trim().split(' ').nth(1).unwrap().to_owned();
    let parts: Vec<&str> = ver.split('.').collect();
    assert_eq!(parts.len(), 3, "semver: {ver}");
    assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()));
}

#[test]
fn doctor_without_display_exits_zero_with_required_keys() {
    let out = run(&["doctor"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {:?}", out.stderr);
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("one JSON value");
    assert_eq!(v["contract"], 1);
    assert_eq!(v["bridge"], "x11-bridge");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["backend"], "x11");
    assert_eq!(v["session_match"], false);
    assert_eq!(v["backend_reachable"], false);
    assert!(v["backend_error"].is_string(), "{v}");
    assert_eq!(v["session_supported"], false);
    assert_eq!(v["restore_token_present"], false);
    assert!(v["restore_token_path"].is_null());
    assert_eq!(v["daemon_running"], false);
    assert!(v["session_socket"].is_null());
}

#[test]
fn session_end_and_forget_session_outputs() {
    let out = run(&["session-end"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out).trim(),
        r#"{"ok":true,"ended":false,"daemon":"none"}"#
    );

    let out = run(&["forget-session"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out).trim(),
        r#"{"ok":true,"ended":false,"daemon":"none","token_removed":false,"token_path":null}"#
    );

    let out = run(&["session-start"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out).trim(), r#"{"ok":true,"session":"noop"}"#);
}

#[test]
fn forget_session_has_no_side_effects() {
    let home = std::env::temp_dir().join(format!("x11-bridge-contract-{}", std::process::id()));
    let _ = run(&["forget-session"]);
    assert!(!home.join("state").exists());
    assert!(!home.join("run").exists());
}

#[test]
fn help_exits_zero() {
    for sub in ["screens", "windows", "session-start"] {
        let out = run(&[sub, "--help"]);
        assert_eq!(out.status.code(), Some(0), "{sub} --help");
    }
}

#[test]
fn negative_coordinates_parse() {
    let cases: &[&[&str]] = &[
        &["pointer-move", "--x", "-5", "--y", "-5", "--help"],
        &["pointer-click", "--x", "-5", "--y", "-5", "--help"],
        &[
            "pointer-scroll",
            "--x",
            "-5",
            "--y",
            "-5",
            "--dx",
            "-1",
            "--dy",
            "-2",
            "--help",
        ],
        &[
            "pointer-drag",
            "--from-x",
            "-5",
            "--from-y",
            "-6",
            "--to-x",
            "-7",
            "--to-y",
            "-8",
            "--help",
        ],
        &[
            "zoom", "--x", "-5", "--y", "-5", "--w", "10", "--h", "10", "--help",
        ],
        &["app-under-point", "--x", "-5", "--y", "-5", "--help"],
    ];
    for args in cases {
        // --help short-circuits after parsing, so exit 0 proves the values parsed.
        let out = run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
    }
    // Without --help the parse must still pass: the failure is the missing X
    // display (exit 1), never a usage error (exit 2).
    let out = run(&["pointer-move", "--x", "-5", "--y", "-5"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn bogus_subcommand_exits_two() {
    let out = run(&["definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(2));
}
