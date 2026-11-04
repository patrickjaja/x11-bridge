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

#[test]
fn doctor_has_atspi_key_and_never_fails() {
    let out = run(&["doctor"]);
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("one JSON value");
    let atspi = v.get("atspi").expect("atspi key");
    assert!(atspi["bus"].is_boolean());
}

#[test]
fn ax_action_rejects_text_in_argv() {
    for extra in [["--text", "secret"], ["--text-stdin", "x"]] {
        let out = run(&[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-text",
            extra[0],
            extra[1],
        ]);
        assert_eq!(out.status.code(), Some(2), "{extra:?}");
    }
}

#[test]
fn ax_usage_errors_exit_2() {
    assert_eq!(run(&["ax-find"]).status.code(), Some(2));
    assert_eq!(run(&["ax-enable"]).status.code(), Some(2));
    assert_eq!(
        run(&[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "bogus"
        ])
        .status
        .code(),
        Some(2)
    );
}

#[test]
fn ax_set_text_invalid_utf8_stdin_exits_2() {
    use std::io::Write;
    use std::process::Stdio;
    let home = std::env::temp_dir().join(format!("x11-bridge-utf8-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_x11-bridge"))
        .args([
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-text",
        ])
        .env_clear()
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn x11-bridge");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[0xff, 0xfe])
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn doctor_reports_screenshot_mask_capability() {
    let out = run(&["doctor"]);
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("one JSON value");
    assert_eq!(v["contract"], 1);
    assert_eq!(v["capabilities"]["screenshot_mask"], true);
}

#[test]
fn mask_flags_parse_and_conflict() {
    let ok: &[&[&str]] = &[
        &["screenshot", "--keep-window", "12583107", "--help"],
        &["screenshot", "--mask-all", "--help"],
        &[
            "zoom",
            "--x",
            "0",
            "--y",
            "0",
            "--w",
            "1",
            "--h",
            "1",
            "--keep-window",
            "0xC00003",
            "--keep-window",
            "42",
            "--help",
        ],
    ];
    for args in ok {
        assert_eq!(run(args).status.code(), Some(0), "{args:?}");
    }
    let usage: &[&[&str]] = &[
        &["screenshot", "--keep-window", "1", "--mask-all"],
        &["screenshot", "--keep-window", "notanid"],
        &[
            "zoom",
            "--x",
            "0",
            "--y",
            "0",
            "--w",
            "1",
            "--h",
            "1",
            "--keep-window",
        ],
    ];
    for args in usage {
        assert_eq!(run(args).status.code(), Some(2), "{args:?}");
    }
    // Mask mode without a display: runtime error, never an image.
    let out = run(&["screenshot", "--mask-all"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}
