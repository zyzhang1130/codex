#![cfg(target_os = "linux")]
use codex_core::config_types::ShellEnvironmentPolicy;
use codex_core::error::CodexErr;
use codex_core::error::SandboxErr;
use codex_core::exec::ExecParams;
use codex_core::exec::SandboxType;
use codex_core::exec::process_exec_tool_call;
use codex_core::exec_env::create_env;
use codex_core::protocol::SandboxPolicy;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::NamedTempFile;

// At least on GitHub CI, the arm64 tests appear to need longer timeouts.

#[cfg(not(target_arch = "aarch64"))]
const SHORT_TIMEOUT_MS: u64 = 200;
#[cfg(target_arch = "aarch64")]
const SHORT_TIMEOUT_MS: u64 = 5_000;

#[cfg(not(target_arch = "aarch64"))]
const LONG_TIMEOUT_MS: u64 = 1_000;
#[cfg(target_arch = "aarch64")]
const LONG_TIMEOUT_MS: u64 = 5_000;

#[cfg(not(target_arch = "aarch64"))]
const NETWORK_TIMEOUT_MS: u64 = 2_000;
#[cfg(target_arch = "aarch64")]
const NETWORK_TIMEOUT_MS: u64 = 10_000;

fn create_env_from_core_vars() -> HashMap<String, String> {
    let policy = ShellEnvironmentPolicy::default();
    create_env(&policy)
}

#[expect(clippy::print_stdout, clippy::expect_used, clippy::unwrap_used)]
async fn run_cmd(cmd: &[&str], writable_roots: &[PathBuf], timeout_ms: u64) {
    let params = ExecParams {
        command: cmd.iter().map(|elm| elm.to_string()).collect(),
        cwd: std::env::current_dir().expect("cwd should exist"),
        timeout_ms: Some(timeout_ms),
        env: create_env_from_core_vars(),
        with_escalated_permissions: None,
        justification: None,
    };

    let sandbox_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: writable_roots.to_vec(),
        network_access: false,
        // Exclude tmp-related folders from writable roots because we need a
        // folder that is writable by tests but that we intentionally disallow
        // writing to in the sandbox.
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
        read_blocklist: Vec::new(),
    };
    let sandbox_program = env!("CARGO_BIN_EXE_codex-linux-sandbox");
    let codex_linux_sandbox_exe = Some(PathBuf::from(sandbox_program));
    let res = process_exec_tool_call(
        params,
        SandboxType::LinuxSeccomp,
        &sandbox_policy,
        &codex_linux_sandbox_exe,
        None,
    )
    .await
    .unwrap();

    if res.exit_code != 0 {
        println!("stdout:\n{}", res.stdout.text);
        println!("stderr:\n{}", res.stderr.text);
        panic!("exit code: {}", res.exit_code);
    }
}

#[tokio::test]
async fn test_root_read() {
    run_cmd(&["ls", "-l", "/bin"], &[], SHORT_TIMEOUT_MS).await;
}

#[tokio::test]
#[should_panic]
async fn test_root_write() {
    let tmpfile = NamedTempFile::new().unwrap();
    let tmpfile_path = tmpfile.path().to_string_lossy();
    run_cmd(
        &["bash", "-lc", &format!("echo blah > {tmpfile_path}")],
        &[],
        SHORT_TIMEOUT_MS,
    )
    .await;
}

#[tokio::test]
async fn test_dev_null_write() {
    run_cmd(
        &["bash", "-lc", "echo blah > /dev/null"],
        &[],
        // We have seen timeouts when running this test in CI on GitHub,
        // so we are using a generous timeout until we can diagnose further.
        LONG_TIMEOUT_MS,
    )
    .await;
}

#[tokio::test]
async fn test_writable_root() {
    let tmpdir = tempfile::tempdir().unwrap();
    let file_path = tmpdir.path().join("test");
    run_cmd(
        &[
            "bash",
            "-lc",
            &format!("echo blah > {}", file_path.to_string_lossy()),
        ],
        &[tmpdir.path().to_path_buf()],
        // We have seen timeouts when running this test in CI on GitHub,
        // so we are using a generous timeout until we can diagnose further.
        LONG_TIMEOUT_MS,
    )
    .await;
}

#[tokio::test]
#[should_panic(expected = "Sandbox(Timeout)")]
async fn test_timeout() {
    run_cmd(&["sleep", "2"], &[], 50).await;
}

/// Helper that runs `cmd` under the Linux sandbox and asserts that the command
/// does NOT succeed (i.e. returns a non‑zero exit code) **unless** the binary
/// is missing in which case we silently treat it as an accepted skip so the
/// suite remains green on leaner CI images.
#[expect(clippy::expect_used)]
async fn assert_network_blocked(cmd: &[&str]) {
    let cwd = std::env::current_dir().expect("cwd should exist");
    let params = ExecParams {
        command: cmd.iter().map(|s| s.to_string()).collect(),
        cwd,
        // Give the tool a generous 2-second timeout so even slow DNS timeouts
        // do not stall the suite.
        timeout_ms: Some(NETWORK_TIMEOUT_MS),
        env: create_env_from_core_vars(),
        with_escalated_permissions: None,
        justification: None,
    };

    let sandbox_policy = SandboxPolicy::new_read_only_policy();
    let sandbox_program = env!("CARGO_BIN_EXE_codex-linux-sandbox");
    let codex_linux_sandbox_exe: Option<PathBuf> = Some(PathBuf::from(sandbox_program));
    let result = process_exec_tool_call(
        params,
        SandboxType::LinuxSeccomp,
        &sandbox_policy,
        &codex_linux_sandbox_exe,
        None,
    )
    .await;

    let (exit_code, stdout, stderr) = match result {
        Ok(output) => (output.exit_code, output.stdout.text, output.stderr.text),
        Err(CodexErr::Sandbox(SandboxErr::Denied(exit_code, stdout, stderr))) => {
            (exit_code, stdout, stderr)
        }
        _ => {
            panic!("expected sandbox denied error, got: {result:?}");
        }
    };

    dbg!(&stderr);
    dbg!(&stdout);
    dbg!(&exit_code);

    // A completely missing binary exits with 127.  Anything else should also
    // be non‑zero (EPERM from seccomp will usually bubble up as 1, 2, 13…)
    // If—*and only if*—the command exits 0 we consider the sandbox breached.

    if exit_code == 0 {
        panic!("Network sandbox FAILED - {cmd:?} exited 0\nstdout:\n{stdout}\nstderr:\n{stderr}",);
    }
}

#[tokio::test]
async fn sandbox_blocks_curl() {
    assert_network_blocked(&["curl", "-I", "http://openai.com"]).await;
}

#[tokio::test]
async fn sandbox_blocks_wget() {
    assert_network_blocked(&["wget", "-qO-", "http://openai.com"]).await;
}

#[tokio::test]
async fn sandbox_blocks_ping() {
    // ICMP requires raw socket – should be denied quickly with EPERM.
    assert_network_blocked(&["ping", "-c", "1", "8.8.8.8"]).await;
}

#[tokio::test]
async fn sandbox_blocks_nc() {
    // Zero‑length connection attempt to localhost.
    assert_network_blocked(&["nc", "-z", "127.0.0.1", "80"]).await;
}

#[tokio::test]
async fn sandbox_blocks_ssh() {
    // Force ssh to attempt a real TCP connection but fail quickly.  `BatchMode`
    // avoids password prompts, and `ConnectTimeout` keeps the hang time low.
    assert_network_blocked(&[
        "ssh",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=1",
        "github.com",
    ])
    .await;
}

#[tokio::test]
async fn sandbox_blocks_getent() {
    assert_network_blocked(&["getent", "ahosts", "openai.com"]).await;
}

#[tokio::test]
async fn sandbox_blocks_dev_tcp_redirection() {
    // This syntax is only supported by bash and zsh. We try bash first.
    // Fallback generic socket attempt using /bin/sh with bash‑style /dev/tcp.  Not
    // all images ship bash, so we guard against 127 as well.
    assert_network_blocked(&["bash", "-c", "echo hi > /dev/tcp/127.0.0.1/80"]).await;
}

#[tokio::test]
async fn read_blocklist_blocks_file() {
    use std::io::Write;

    let mut tmpfile = NamedTempFile::new().unwrap();
    writeln!(tmpfile, "secret").unwrap();
    let tmp_path = tmpfile.path().to_path_buf();

    let cwd = std::env::current_dir().expect("cwd should exist");
    let env = create_env_from_core_vars();

    let sandbox_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
        read_blocklist: vec![tmp_path.clone()],
    };
    let sandbox_program = env!("CARGO_BIN_EXE_codex-linux-sandbox");
    let codex_linux_sandbox_exe = Some(PathBuf::from(sandbox_program));

    // Reading unblocked path succeeds.
    let params_ok = ExecParams {
        command: vec!["cat".to_string(), "/etc/hosts".to_string()],
        cwd: cwd.clone(),
        timeout_ms: Some(SHORT_TIMEOUT_MS),
        env: env.clone(),
        with_escalated_permissions: None,
        justification: None,
    };
    let ok = process_exec_tool_call(
        params_ok,
        SandboxType::LinuxSeccomp,
        &sandbox_policy,
        &codex_linux_sandbox_exe,
        None,
    )
    .await
    .unwrap();
    assert_eq!(ok.exit_code, 0);

    // Reading blocked path fails.
    let params_block = ExecParams {
        command: vec![
            "cat".to_string(),
            tmp_path.to_string_lossy().to_string(),
        ],
        cwd,
        timeout_ms: Some(SHORT_TIMEOUT_MS),
        env,
        with_escalated_permissions: None,
        justification: None,
    };
    let blocked = process_exec_tool_call(
        params_block,
        SandboxType::LinuxSeccomp,
        &sandbox_policy,
        &codex_linux_sandbox_exe,
        None,
    )
    .await
    .unwrap();
    assert_ne!(blocked.exit_code, 0);
}
