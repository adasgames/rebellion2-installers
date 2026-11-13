//! Collects Windows output-device evidence without making successful diagnosis a launch requirement.

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Seek, SeekFrom},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "windows")]
#[path = "windows_audio_native.rs"]
mod native;

const PROBE_ARGUMENT: &str = "--probe-windows-audio";
// Match the existing Linux native-preflight bound; an unavailable driver must not prevent play.
#[cfg(target_os = "windows")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Recognizes only the private, argument-free diagnostic invocation.
pub(crate) fn is_probe(arguments: impl IntoIterator<Item = OsString>) -> bool {
    let mut arguments = arguments.into_iter();
    arguments.next().is_some()
        && arguments.next().as_deref() == Some(OsStr::new(PROBE_ARGUMENT))
        && arguments.next().is_none()
}

/// Runs before the installation lock or webview is initialized, writing only diagnostic output.
#[cfg(target_os = "windows")]
pub(crate) fn write_probe_report() -> io::Result<()> {
    use io::Write;
    io::stdout().write_all(native::collect().as_bytes())
}

/// Records the prelaunch report, preserving normal player startup after every diagnostic failure.
#[cfg(target_os = "windows")]
pub(crate) fn record() {
    let result = std::env::current_exe().and_then(|executable| {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        let mut command = Command::new(executable);
        command
            .arg(PROBE_ARGUMENT)
            .creation_flags(CREATE_NO_WINDOW.0);
        run_probe(&mut command, PROBE_TIMEOUT)
    });
    match result {
        Ok(report) => {
            for line in report.lines() {
                crate::log_line(&format!("[audio diagnostic] {line}"));
            }
        }
        Err(error) => crate::log_line(&format!(
            "[audio diagnostic] unavailable: {error}; continuing game launch."
        )),
    }
}

/// Captures a disposable helper through an anonymous temporary file and kills it on timeout.
/// File capture also permits reports larger than a pipe buffer without blocking the helper.
fn run_probe(command: &mut Command, timeout: Duration) -> io::Result<String> {
    let mut output = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "audio helper exited with {status}"
                    )));
                }
                output.seek(SeekFrom::Start(0))?;
                let mut report = String::new();
                output.read_to_string(&mut report)?;
                if report.trim().is_empty() {
                    return Err(io::Error::other("audio helper returned no report"));
                }
                return Ok(report);
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                // Windows does not require POSIX child reaping. Avoid waiting indefinitely for
                // a malfunctioning audio driver to finish shutdown after TerminateProcess.
                #[cfg(not(target_os = "windows"))]
                let _ = child.wait();
                return match result {
                    Err(error) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "audio helper timed out",
                    )),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_probe_requires_exact_arguments() {
        assert!(is_probe(["launcher", PROBE_ARGUMENT].map(OsString::from)));
        for arguments in [
            vec![],
            vec!["launcher"],
            vec!["launcher", "--auth-only"],
            vec!["launcher", PROBE_ARGUMENT, "unexpected"],
        ] {
            assert!(!is_probe(arguments.into_iter().map(OsString::from)));
        }
    }

    #[test]
    fn successful_probe_captures_the_complete_report() {
        let report = run_probe(&mut fixture_command("report"), Duration::from_secs(10)).unwrap();
        assert!(report.contains("native mix: 48000 Hz"));
    }

    #[test]
    fn large_probe_output_does_not_block_on_a_full_pipe() {
        let report = run_probe(&mut fixture_command("large"), Duration::from_secs(10)).unwrap();
        assert!(report.contains(&"x".repeat(200_000)));
    }

    #[test]
    fn failed_probe_returns_a_diagnostic_error() {
        let error = run_probe(&mut fixture_command("fail"), Duration::from_secs(10)).unwrap_err();
        assert!(error.to_string().contains("audio helper exited"));
    }

    #[test]
    fn stalled_probe_is_killed_and_reaped_within_the_deadline() {
        let started = Instant::now();
        let error =
            run_probe(&mut fixture_command("hang"), Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn missing_helper_returns_a_diagnostic_error() {
        let directory = tempfile::tempdir().unwrap();
        let error = run_probe(
            &mut Command::new(directory.path().join("missing-helper")),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn native_probe_reports_windows_results_without_requiring_an_audio_device() {
        let report = run_probe(&mut fixture_command("native"), Duration::from_secs(5)).unwrap();
        assert!(report.contains("Windows output diagnostic"));
        assert!(
            report.contains("Default multimedia:")
                || report.contains("COM initialization:")
                || report.contains("Endpoint enumeration:")
        );
        assert!(report.contains("0x") || report.contains("Native mix:"));
    }

    #[test]
    #[ignore = "Spawned by the subprocess tests with an explicit fixture mode."]
    fn audio_probe_child() {
        match std::env::var("REB2_AUDIO_TEST_MODE").unwrap().as_str() {
            "report" => println!("native mix: 48000 Hz"),
            "large" => println!("{}", "x".repeat(200_000)),
            "fail" => std::process::exit(13),
            #[cfg(target_os = "windows")]
            "native" => print!("{}", native::collect()),
            "hang" => loop {
                thread::sleep(Duration::from_secs(1));
            },
            _ => panic!("unknown subprocess fixture mode"),
        }
    }

    /// Uses this test executable as a deterministic helper, requiring no shell or audio hardware.
    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "windows_audio::tests::audio_probe_child",
                "--nocapture",
            ])
            .env("REB2_AUDIO_TEST_MODE", mode);
        command
    }
}
