//! Integration proof that protobuf Exec commands reach the OS confinement supervisor.
//!
//! The test calls the same command runner used by join and observes helper output captured by
//! `spawn_confined`; Linux hosts without systemd-run/bwrap skip because the sandbox cannot start.

use tagteam_node::{CommandJournal, CommandRunner};
use tagteam_sandbox::CancellationToken;

fn sandbox_runtime_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        if !Path::new("/usr/bin/systemd-run").exists()
            || !Path::new("/usr/bin/bwrap").exists()
            || std::env::var_os("XDG_RUNTIME_DIR").is_none()
            || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        {
            return false;
        }
        std::process::Command::new("/usr/bin/systemd-run")
            .args(["--user", "--scope", "--quiet", "--", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[tokio::test]
async fn exec_command_runs_through_spawn_confined() {
    if !sandbox_runtime_available() {
        let message = "a working Linux user systemd manager, /usr/bin/systemd-run, and /usr/bin/bwrap are required; skipping confined Exec test";
        if std::env::var_os("TAGTEAM_REQUIRE_LINUX_SANDBOX").is_some() {
            panic!("{message}");
        }
        eprintln!("{message}");
        return;
    }

    let directory = tempfile::tempdir().expect("temporary node directory");
    let journal =
        CommandJournal::open(directory.path().join("commands.json")).expect("journal opens");
    let runner = CommandRunner::new(directory.path(), journal, tagteam_node::NoBundleSource)
        .with_executable(env!("CARGO_BIN_EXE_tagteam-node"));
    let command = tagteam_proto::v1::Command {
        command_id: "exec-smoke".into(),
        subtask_id: "subtask-smoke".into(),
        workspace: "workspace-smoke".into(),
        op: Some(tagteam_proto::v1::command::Op::Exec(
            tagteam_proto::v1::Exec {
                argv: vec!["/usr/bin/printf".into(), "confined-node-exec".into()],
                cwd: String::new(),
                env: Default::default(),
                stdin: Vec::new(),
                timeout_ms: 5_000,
                output_cap: 1024,
            },
        )),
    };
    let (_, token) = CancellationToken::new();
    let result = runner
        .execute_command(&command, token)
        .await
        .expect("Exec completes");
    let Some(tagteam_proto::v1::command_result::Result::Exec(result)) = result.result else {
        panic!("Exec result variant returned");
    };
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout_tail, b"confined-node-exec");
}
