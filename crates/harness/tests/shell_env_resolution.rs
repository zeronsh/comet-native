//! End-to-end: a CLI visible only through the login shell's PATH resolves.
//!
//! This file must stay a single test: it mutates process env (SHELL/PATH/HOME)
//! and warms the process-global login-shell snapshot cache, so it needs its
//! own test binary with no parallel siblings.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zeron_harness::{AcpHarness, Harness as _};

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn cli_on_login_shell_path_only_is_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    write_executable(&shell_bin.join("devin"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("hermes"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("pi-acp"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("claude"), "#!/bin/sh\nexit 0\n");

    // A $SHELL whose init shapes PATH — the shape resolution must survive.
    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    // A GUI/service-launch environment: minimal PATH, no CLIs reachable, HOME
    // pointed away from any real install dirs.
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", "/usr/bin:/bin");
        std::env::remove_var("DEVIN_EXECUTABLE");
        std::env::remove_var("HERMES_EXECUTABLE");
        std::env::remove_var("PI_ACP_EXECUTABLE");
        std::env::remove_var("CLAUDE_CODE_EXECUTABLE");
        std::env::remove_var("ZERON_NO_LOGIN_SHELL");
    }

    let snapshot = zeron_harness::shell_env::login_shell_path().expect("snapshot captured");
    let snapshot = snapshot.to_string_lossy();
    assert!(
        snapshot.starts_with(&format!("{}:", shell_bin.display())),
        "snapshot should carry the shell-shaped PATH, got: {snapshot}"
    );

    // The agent binaries are only reachable through the snapshot; the
    // launch program (not the npx fallback) must be the shell-PATH binary,
    // proving resolution consulted the login-shell snapshot.
    // Native drivers consult the same snapshot for the agent CLI itself.
    assert!(
        zeron_harness::ClaudeHarness::new().installed(),
        "claude resolves via login-shell PATH"
    );
    let devin = AcpHarness::devin()
        .launch_program()
        .expect("devin resolves via login-shell PATH");
    assert_eq!(devin, shell_bin.join("devin"), "{devin:?}");
    let hermes = AcpHarness::hermes()
        .launch_program()
        .expect("hermes resolves via login-shell PATH");
    assert_eq!(hermes, shell_bin.join("hermes"), "{hermes:?}");
    let pi = AcpHarness::pi()
        .launch_program()
        .expect("pi-acp resolves via login-shell PATH");
    assert_eq!(pi, shell_bin.join("pi-acp"), "{pi:?}");
}
