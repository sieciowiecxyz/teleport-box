use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_teleport-box"))
        .args(args)
        .output()
        .expect("failed to run teleport-box test binary")
}

#[test]
fn top_level_help_lists_core_subcommands() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("doctor"));
    assert!(stdout.contains("shell"));
    assert!(stdout.contains("exec"));
    assert!(stdout.contains("codex"));
}

#[test]
fn exec_help_mentions_shared_bridge() {
    let output = run(&["exec", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--shared-dir"));
    assert!(stdout.contains("TARGET"));
}
