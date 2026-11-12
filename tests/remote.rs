#![cfg(unix)]

use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn corrupted_and_truncated_uploads_are_never_executed_and_are_cleaned_up() {
    for truncated in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("executed");
        let payload = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
        let script = include_str!("../src/remote-bootstrap.sh").replace(
            "/tmp/stalker.",
            &format!("{}/stalker.", temp.path().display()),
        );
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let digest = if truncated {
            format!("{:x}", Sha256::digest(payload.as_bytes()))
        } else {
            "0".repeat(64)
        };
        writeln!(input, "{}\n{digest}", payload.len()).unwrap();
        input
            .write_all(if truncated {
                &payload.as_bytes()[..3]
            } else {
                payload.as_bytes()
            })
            .unwrap();
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 verification failed"));
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

// `cargo xtask test` embeds a native helper before compiling these tests.
#[cfg(embedded_helpers)]
mod embedded {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, ExitStatus},
        thread,
        time::{Duration, Instant},
    };

    struct Session {
        temp: tempfile::TempDir,
        child: Child,
    }

    impl Session {
        fn start(options: &[&str], command: &[&str]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            for directory in ["local", "remote/project/.git", "bin", "uploads"] {
                fs::create_dir_all(temp.path().join(directory)).unwrap();
            }
            fs::write(temp.path().join("remote/project/input.rs"), "initial").unwrap();
            fs::write(
                temp.path().join("remote/project/.gitignore"),
                "ignored.rs\n",
            )
            .unwrap();
            // Execute the real bootstrap/helper while replacing only SSH's
            // transport. All argv, remote HOME/cwd, upload and protocol stay real.
            let ssh = temp.path().join("bin/ssh");
            fs::write(
                &ssh,
                r#"#!/bin/sh
printf '%s\n' "$@" > "$STALKER_TEST_ROOT/ssh-args"
for argument do last=$argument; done
cd "$STALKER_TEST_ROOT/remote" || exit
printf '%s' "$$" > "$STALKER_TEST_ROOT/remote-shell-pid"
HOME=$PWD
export HOME
# Give each test its own deployment location so cleanup is observable.
last=$(printf '%s' "$last" | sed "s|/tmp/stalker\.|$STALKER_TEST_ROOT/uploads/stalker.|g")
exec sh -c "$last"
"#,
            )
            .unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
            let path = env_path(&temp.path().join("bin"));
            let child = Command::new(env!("CARGO_BIN_EXE_stalker"))
                .current_dir(temp.path().join("local"))
                .env("PATH", path)
                .env("STALKER_TEST_ROOT", temp.path())
                .args([
                    "--remote",
                    "ssh://test@example.invalid:2222",
                    "--remote-cwd",
                    "~/project",
                    "--watch",
                    ".",
                    "--debounce",
                    "20ms",
                    "--markers",
                ])
                .args(options)
                .arg("--")
                .args(command)
                .stdin(Stdio::null())
                .stdout(fs::File::create(temp.path().join("stdout")).unwrap())
                .stderr(fs::File::create(temp.path().join("stderr")).unwrap())
                .spawn()
                .unwrap();
            Self { temp, child }
        }

        fn root(&self) -> &Path {
            self.temp.path()
        }
        fn text(&self, name: &str) -> String {
            fs::read_to_string(self.root().join(name)).unwrap()
        }
        fn wait_text(&mut self, name: &str, needle: &str) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !self.text(name).contains(needle) {
                assert!(
                    Instant::now() < deadline && self.child.try_wait().unwrap().is_none(),
                    "waiting for {needle:?}; stdout: {}; stderr: {}",
                    self.text("stdout"),
                    self.text("stderr")
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        fn wait_exit(&mut self) -> ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "did not exit; stderr: {}",
                    self.text("stderr")
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        fn change(&self, name: &str) {
            fs::write(self.root().join("remote/project").join(name), "changed").unwrap();
        }
        fn stop(&mut self) {
            if self.child.try_wait().unwrap().is_none() {
                assert!(
                    Command::new("kill")
                        .args(["-TERM", &self.child.id().to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        fn assert_clean(&self) {
            assert_eq!(
                fs::read_dir(self.root().join("uploads")).unwrap().count(),
                0
            );
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.stop();
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                if self.child.try_wait().unwrap().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn env_path(prefix: &Path) -> std::ffi::OsString {
        let mut paths = vec![PathBuf::from(prefix)];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        std::env::join_paths(paths).unwrap()
    }

    #[test]
    fn remote_filtering_local_command_and_private_directory_cleanup() {
        let mut session = Session::start(
            &[
                "--include",
                "*.rs",
                "--ssh-option",
                "IdentityFile=/tmp/key with spaces",
            ],
            &["sh", "-c", "pwd"],
        );
        session.wait_text("stdout", "run=1 exit=0@@");
        assert!(
            session.text("stdout").contains(
                &session
                    .root()
                    .join("local")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string()
            )
        );
        let arguments = session.text("ssh-args");
        assert!(arguments.contains("IdentityFile=/tmp/key with spaces\n"));
        assert!(arguments.contains("ssh://test@example.invalid:2222\n"));
        let deployment = fs::read_dir(session.root().join("uploads"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::metadata(&deployment).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(deployment.join("agent"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o500
        );
        session.change("ignored.rs");
        session.change("unmatched.txt");
        thread::sleep(Duration::from_millis(250));
        assert_eq!(session.text("stdout").matches("@@stalker:start").count(), 1);
        session.change("input.rs");
        session.wait_text("stdout", "run=2 exit=0@@");
        session.stop();
        assert!(session.wait_exit().success());
        session.assert_clean();
    }

    #[test]
    fn initial_success_waits_for_remote_ready_and_cleans_up() {
        let mut session = Session::start(&["--until-success"], &["true"]);
        assert!(session.wait_exit().success(), "{}", session.text("stderr"));
        assert!(session.text("stdout").contains("run=1 exit=0@@"));
        session.assert_clean();
    }

    #[test]
    fn no_initial_run_waits_then_until_success_exits() {
        let mut session = Session::start(&["--no-initial-run", "--until-success"], &["true"]);
        // Wait for helper extraction, then keep changing until registration.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "{}", session.text("stderr"));
            if fs::read_dir(session.root().join("uploads"))
                .unwrap()
                .next()
                .is_some()
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.text("stdout").is_empty());
        while session.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "{}", session.text("stderr"));
            session.change("input.rs");
            thread::sleep(Duration::from_millis(100));
        }
        assert!(session.wait_exit().success());
        assert!(session.text("stdout").contains("trigger=filesystem@@"));
        session.assert_clean();
    }

    #[test]
    fn remote_startup_failure_does_not_run_local_command() {
        let mut session = Session::start(
            &["--watch", "missing"],
            &["sh", "-c", "touch should-not-run"],
        );
        assert!(!session.wait_exit().success());
        assert!(!session.root().join("local/should-not-run").exists());
        assert!(
            session
                .text("stderr")
                .contains("could not resolve watch path")
        );
        session.assert_clean();
    }

    #[test]
    fn losing_the_remote_watcher_stops_the_local_process_group() {
        let mut session = Session::start(
            &[],
            &[
                "sh",
                "-c",
                "trap 'printf stopped > stopped; exit 0' TERM; printf running; while :; do sleep 0.05; done",
            ],
        );
        session.wait_text("stdout", "running");
        let pid = session.text("remote-shell-pid");
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        assert!(!session.wait_exit().success());
        assert!(
            session
                .text("stderr")
                .contains("SSH connection to remote watcher closed")
        );
        assert!(session.root().join("local/stopped").exists());
        session.assert_clean();
    }

    #[test]
    fn remote_paths_are_data_and_preserve_non_utf8_names() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let name = OsString::from_vec(b"file\xff\n'$(touch injected)".to_vec());
        let mut session = Session::start(&["--print-events"], &["true"]);
        session.wait_text("stdout", "run=1 exit=0@@");
        fs::write(session.root().join("remote/project").join(name), "changed").unwrap();
        session.wait_text("stdout", "run=2 exit=0@@");
        assert!(!session.root().join("remote/injected").exists());
        session.stop();
        assert!(session.wait_exit().success());
        session.assert_clean();
    }
}
