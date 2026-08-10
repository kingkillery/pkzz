#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;
use uuid::Uuid;

const FIXTURE: &[u8] = include_bytes!("fixtures/windows/ompk-acp-shim.cmd");
const FIXTURE_ENV_VALUE: &str = "layered environment value";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);

struct TempFixtureDir(PathBuf);

impl TempFixtureDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("buzz acp windows batch fixture {}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create spaced fixture directory");
        Self(path)
    }

    fn copy_as(&self, file_name: &str) -> PathBuf {
        let path = self.0.join(file_name);
        std::fs::write(&path, FIXTURE).expect("copy Windows batch fixture");
        path
    }

    fn marker(&self, file_name: &str) -> PathBuf {
        self.0.join(file_name)
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run_models(script: &Path, agent_args: &str, marker: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_buzz-acp"));
    command
        .arg("models")
        .arg("--agent-command")
        .arg(script)
        .arg("--agent-args")
        .arg(agent_args)
        .arg("--json")
        .env("BUZZ_ACP_WINDOWS_FIXTURE_MARKER", marker)
        .env("BUZZ_ACP_WINDOWS_FIXTURE_ENV", FIXTURE_ENV_VALUE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().expect("spawn buzz-acp models CLI");
    tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .expect("buzz-acp models CLI timed out; batch process was not shut down")
        .expect("wait for buzz-acp models CLI")
}

#[tokio::test]
async fn windows_batch_shim_cmd_and_bat_execute_through_models_cli() {
    let fixtures = TempFixtureDir::new();
    assert!(
        fixtures.0.to_string_lossy().contains(' '),
        "fixture path must exercise executable-path quoting"
    );

    for extension in ["cmd", "bat"] {
        let script = fixtures.copy_as(&format!("ompk acp shim.{extension}"));
        let marker = fixtures.marker(&format!("{extension}.entered"));
        let output = run_models(&script, "acp,argument with spaces", &marker).await;

        assert!(
            output.status.success(),
            ".{extension} fixture failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists(), ".{extension} fixture did not start");

        let response: Value =
            serde_json::from_slice(&output.stdout).expect("models CLI emitted valid JSON");
        assert_eq!(
            response.pointer("/agent/name"),
            Some(&Value::from("ompk-shim"))
        );
        assert_eq!(
            response.pointer("/agent/version"),
            Some(&Value::from("1.0.0"))
        );
        assert_eq!(
            response.pointer("/stable/configOptions/0/currentValue"),
            Some(&Value::from("fixture/model"))
        );
    }
}

#[tokio::test]
async fn windows_batch_shim_rejects_crlf_before_fixture_entry() {
    let fixtures = TempFixtureDir::new();
    let script = fixtures.copy_as("ompk acp shim.cmd");
    let marker = fixtures.marker("invalid-argument.entered");

    let output = run_models(&script, "acp,line one\r\nline two", &marker).await;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "CR/LF batch argument was accepted"
    );
    assert!(
        stderr.contains("batch file arguments are invalid"),
        "expected Rust's InvalidInput batch-argument rejection, got: {stderr}"
    );
    assert!(
        !marker.exists(),
        "batch fixture ran before the unsafe argument was rejected"
    );
}
