#![cfg(unix)]

use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

struct Session {
    child: Child,
    directory: TempDir,
}

impl Session {
    fn start(options: &[&str], command: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("watched")).unwrap();
        fs::write(directory.path().join("watched/input"), "fail").unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_stalker"))
            .current_dir(directory.path())
            .args(["--watch", "watched", "--debounce", "20ms"])
            .args(options)
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(File::create(directory.path().join("stdout")).unwrap())
            .stderr(File::create(directory.path().join("stderr")).unwrap())
            .spawn()
            .unwrap();
        Self { child, directory }
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn output(&self, stream: &str) -> String {
        fs::read_to_string(self.path().join(stream)).unwrap()
    }

    fn wait_for_text(&self, stream: &str, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.output(stream).contains(text) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {text:?}; stdout: {:?}; stderr: {:?}",
                self.output("stdout"),
                self.output("stderr"),
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "stalker did not exit; stdout: {:?}; stderr: {:?}",
                self.output("stdout"),
                self.output("stderr"),
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        // Let Stalker stop its child process group, including on test failure.
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn initial_success_exits_and_preserves_output_in_each_mode() {
    for options in [
        vec!["--until-success"],
        vec!["--until-success", "--markers"],
        vec!["--until-success", "--raw-output"],
    ] {
        let mut session = Session::start(&options, &["sh", "-c", "printf hello"]);
        assert!(session.wait_for_exit().success());
        if options.contains(&"--markers") {
            assert_eq!(
                session.output("stdout"),
                "@@stalker:start command=default run=1 trigger=initial@@\n\
                 hello\n@@stalker:end command=default run=1 exit=0@@\n"
            );
        } else {
            assert_eq!(session.output("stdout"), "hello");
        }
        assert!(session.output("stderr").is_empty());
    }
}

#[test]
fn failed_runs_wait_for_changes_then_exit_on_success() {
    for failure in ["exit 7", "kill -TERM $$"] {
        let script = format!("test \"$(cat watched/input)\" = pass && exit 0; {failure}");
        let mut session = Session::start(&["--until-success", "--markers"], &["sh", "-c", &script]);
        let failure_marker = if failure == "exit 7" {
            "run=1 exit=7@@"
        } else {
            "run=1 signal=SIGTERM@@"
        };
        session.wait_for_text("stdout", failure_marker);
        thread::sleep(Duration::from_millis(100));
        assert!(session.child.try_wait().unwrap().is_none());
        assert_eq!(
            session.output("stdout").matches("@@stalker:start").count(),
            1
        );

        fs::write(session.path().join("watched/input"), "pass").unwrap();
        assert!(session.wait_for_exit().success());
        let output = session.output("stdout");
        assert!(output.contains("run=2 trigger=filesystem@@"));
        assert!(output.ends_with("run=2 exit=0@@\n"));
        assert_eq!(output.matches("@@stalker:start").count(), 2);
    }
}

#[test]
fn no_initial_run_waits_for_a_change_before_succeeding() {
    let mut session = Session::start(
        &["--until-success", "--no-initial-run", "--markers"],
        &["sh", "-c", "exit 0"],
    );
    thread::sleep(Duration::from_millis(150));
    assert!(session.child.try_wait().unwrap().is_none());
    assert!(session.output("stdout").is_empty());

    // Repeat the change until the watcher is ready, even on a slow test host.
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.output("stdout").is_empty() {
        assert!(Instant::now() < deadline, "watcher did not start a run");
        fs::write(session.path().join("watched/input"), "pass").unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    assert!(session.wait_for_exit().success());
    let output = session.output("stdout");
    assert!(output.contains("run=1 trigger=filesystem@@"));
    assert!(output.ends_with("run=1 exit=0@@\n"));
    assert_eq!(output.matches("@@stalker:start").count(), 1);
}

#[test]
fn success_discards_a_queued_rerun() {
    let mut session = Session::start(
        &["--until-success", "--markers", "--print-events"],
        &[
            "sh",
            "-c",
            "while test ! -f release; do sleep 0.01; done; printf done",
        ],
    );
    session.wait_for_text("stdout", "run=1 trigger=initial@@");
    fs::write(session.path().join("watched/input"), "changed").unwrap();
    session.wait_for_text("stderr", "accepted filesystem event");
    fs::write(session.path().join("release"), "").unwrap();

    assert!(session.wait_for_exit().success());
    let output = session.output("stdout");
    assert_eq!(output.matches("@@stalker:start").count(), 1);
    assert!(output.ends_with("done\n@@stalker:end command=default run=1 exit=0@@\n"));
}

#[test]
fn spawn_failure_keeps_watching_until_the_command_is_available() {
    let mut session = Session::start(&["--until-success", "--markers"], &["./command"]);
    session.wait_for_text("stderr", "failed to spawn");
    assert!(session.child.try_wait().unwrap().is_none());

    let command = session.path().join("command");
    fs::write(&command, "#!/bin/sh\nprintf recovered\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(session.path().join("watched/input"), "changed").unwrap();

    assert!(session.wait_for_exit().success());
    assert!(session.output("stdout").contains("recovered\n"));
    assert!(session.output("stdout").ends_with("run=2 exit=0@@\n"));
}

#[test]
fn success_without_the_flag_keeps_watching() {
    let mut session = Session::start(&["--markers"], &["sh", "-c", "exit 0"]);
    session.wait_for_text("stdout", "run=1 exit=0@@");
    fs::write(session.path().join("watched/input"), "changed").unwrap();
    session.wait_for_text("stdout", "run=2 exit=0@@");
    assert!(session.child.try_wait().unwrap().is_none());
}
