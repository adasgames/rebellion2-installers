use rebellion2_update_core::InstallationLock;
use std::fs;
use std::io;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn installation_lock_excludes_other_processes_and_survives_owner_termination() {
    if let Some(directory) = std::env::var_os("REB2_LOCK_FIXTURE_DIRECTORY") {
        let directory = std::path::PathBuf::from(directory);
        let _owner = InstallationLock::try_acquire(&directory).unwrap();
        fs::write(directory.join("owner-ready"), b"ready").unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "installation_lock_excludes_other_processes_and_survives_owner_termination",
        ])
        .env("REB2_LOCK_FIXTURE_DIRECTORY", directory.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = TestChild(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !directory.path().join("owner-ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "lock owner exited before acquiring its lock"
        );
        assert!(Instant::now() < deadline, "lock owner never became ready");
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        InstallationLock::try_acquire(directory.path())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let _replacement_owner = InstallationLock::try_acquire(directory.path()).unwrap();
}
