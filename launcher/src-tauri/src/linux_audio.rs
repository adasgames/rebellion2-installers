//! Applies the saved audio selection, including an automatic PipeWire ALSA fallback.
//! Native checks run in a disposable process so a missing or stalled audio service cannot block play.

use crate::launch_settings::AudioBackend;
use libloading::Library;
use std::{
    ffi::{c_char, c_int, c_void, OsStr, OsString},
    fs, io,
    path::Path,
    process::{Command, Stdio},
    ptr, thread,
    time::{Duration, Instant},
};

const PROBE_ARGUMENT: &str = "--probe-linux-audio";
const PIPEWIRE_AVAILABLE: i32 = 10;
// Bound native service discovery, including PulseAudio autospawn, independently of library timeouts.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIG_FILE: &str = ".rebellion2-pipewire.conf";
const CONFIG: &str = "</usr/share/alsa/alsa.conf>\npcm.!default {\n    type pipewire\n}\nctl.!default {\n    type pipewire\n}\n";

/// Recognizes the private preflight invocation and its supported probe mode.
pub(crate) fn probe_selection(
    arguments: impl IntoIterator<Item = OsString>,
) -> Option<AudioBackend> {
    let mut arguments = arguments.into_iter();
    arguments.next()?;
    if arguments.next().as_deref() != Some(OsStr::new(PROBE_ARGUMENT)) {
        return None;
    }
    let backend = match arguments.next().as_deref().and_then(OsStr::to_str) {
        Some("auto") => AudioBackend::Auto,
        Some("pipewire") => AudioBackend::Pipewire,
        _ => return None,
    };
    arguments.next().is_none().then_some(backend)
}

/// Applies the saved route to the player alone, preserving system settings in automatic mode.
pub(crate) fn configure(
    player: &mut Command,
    base: &Path,
    backend: AudioBackend,
) -> io::Result<bool> {
    if backend == AudioBackend::System {
        return Ok(false);
    }
    if backend == AudioBackend::Auto
        && ["ALSA_CONFIG_PATH", "ALSA_CONFIG_DIR", "ALSA_PLUGIN_DIR"]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
    {
        return Ok(false);
    }
    let mut probe = Command::new(std::env::current_exe()?);
    probe
        .arg(PROBE_ARGUMENT)
        .arg(backend.value())
        .current_dir(base);
    if !run_probe(&mut probe, PROBE_TIMEOUT)? {
        if backend == AudioBackend::Pipewire {
            return Err(io::Error::new(io::ErrorKind::NotFound,
                "PipeWire audio is unavailable. Choose Automatic or System default in Options, or install PipeWire with ALSA support."));
        }
        return Ok(false);
    }
    let config = base.join(CONFIG_FILE);
    if fs::read(&config).ok().as_deref() != Some(CONFIG.as_bytes()) {
        crate::write_file_atomically(config.clone(), CONFIG.as_bytes())?;
    }
    player.env("ALSA_CONFIG_PATH", config);
    if backend == AudioBackend::Pipewire {
        // Unity tries native PulseAudio before ALSA. A non-socket endpoint bypasses it for this
        // player, ensuring the explicitly selected PipeWire route is used even when Pulse works.
        player.env("PULSE_SERVER", "unix:/dev/null");
    }
    Ok(true)
}

/// Reaps the preflight, killing it on timeout and accepting only a verified PipeWire route.
fn run_probe(command: &mut Command, timeout: Duration) -> io::Result<bool> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.code() == Some(PIPEWIRE_AVAILABLE)),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return match result {
                    Err(error) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "audio preflight timed out",
                    )),
                };
            }
        }
    }
}

/// Verifies the explicit PipeWire route, or the automatic fallback after a failed Pulse connection.
pub(crate) fn probe_exit_code(backend: AudioBackend) -> i32 {
    let needed = match backend {
        AudioBackend::Auto => matches!(pulse_ready(), Ok(false)),
        AudioBackend::Pipewire => true,
        AudioBackend::System => false,
    };
    if needed && pipewire_available().unwrap_or(false) {
        PIPEWIRE_AVAILABLE
    } else {
        0
    }
}

/// Connects through the standard PulseAudio client configuration, allowing normal daemon autospawn.
fn pulse_ready() -> Result<bool, libloading::Error> {
    // SAFETY: Signatures and enum values follow libpulse's public C ABI. Opaque handles are used
    // only by that library and released before it unloads. No callbacks or handles escape.
    unsafe {
        let library = Library::new("libpulse.so.0")?;
        let new = library.get::<unsafe extern "C" fn() -> *mut c_void>(b"pa_mainloop_new\0")?;
        let api = library
            .get::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(b"pa_mainloop_get_api\0")?;
        let free = library.get::<unsafe extern "C" fn(*mut c_void)>(b"pa_mainloop_free\0")?;
        let iterate = library
            .get::<unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int>(
                b"pa_mainloop_iterate\0",
            )?;
        let create = library
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>(
                b"pa_context_new\0",
            )?;
        let connect = library.get::<unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            c_int,
            *const c_void,
        ) -> c_int>(b"pa_context_connect\0")?;
        let state = library
            .get::<unsafe extern "C" fn(*const c_void) -> c_int>(b"pa_context_get_state\0")?;
        let unref = library.get::<unsafe extern "C" fn(*mut c_void)>(b"pa_context_unref\0")?;
        let mainloop = new();
        if mainloop.is_null() {
            return Ok(true); // Allocation failure is inconclusive; preserve system audio.
        }
        let context = create(api(mainloop), c"Rebellion 2 audio preflight".as_ptr());
        if context.is_null() {
            free(mainloop);
            return Ok(true);
        }
        let ready = if connect(context, ptr::null(), 0, ptr::null()) < 0 {
            false
        } else {
            loop {
                match state(context) {
                    4 => break true,      // PA_CONTEXT_READY
                    5 | 6 => break false, // PA_CONTEXT_FAILED / PA_CONTEXT_TERMINATED
                    _ => {}
                }
                if iterate(mainloop, 0, ptr::null_mut()) < 0 {
                    break true; // Main-loop errors do not establish a failed connection.
                }
                thread::sleep(Duration::from_millis(10));
            }
        };
        unref(context);
        free(mainloop);
        Ok(ready)
    }
}

/// Opens and closes the candidate's default PCM using the installed ALSA plugin and running server.
fn pipewire_available() -> Result<bool, libloading::Error> {
    // SAFETY: These are public libasound C ABI functions. Configuration and PCM handles remain
    // local, are checked before use, and are released while the library is still loaded.
    unsafe {
        let library = Library::new("libasound.so.2")?;
        let load = library
            .get::<unsafe extern "C" fn(*mut *mut c_void, *const c_char, usize) -> c_int>(
                b"snd_config_load_string\0",
            )?;
        let delete =
            library.get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"snd_config_delete\0")?;
        let open = library.get::<unsafe extern "C" fn(
            *mut *mut c_void,
            *const c_char,
            c_int,
            c_int,
            *mut c_void,
        ) -> c_int>(b"snd_pcm_open_lconf\0")?;
        let close =
            library.get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"snd_pcm_close\0")?;
        let mut config = ptr::null_mut();
        if load(&mut config, CONFIG.as_ptr().cast(), CONFIG.len()) < 0 {
            return Ok(false);
        }
        let mut pcm = ptr::null_mut();
        // SND_PCM_STREAM_PLAYBACK = 0; SND_PCM_NONBLOCK = 1. No samples are played.
        let opened = open(&mut pcm, c"default".as_ptr(), 0, 1, config) >= 0;
        if opened {
            close(pcm);
        }
        delete(config);
        Ok(opened)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_selection_only_accepts_supported_probe_modes() {
        for (arguments, expected) in [
            (
                vec!["launcher", PROBE_ARGUMENT, "auto"],
                Some(AudioBackend::Auto),
            ),
            (
                vec!["launcher", PROBE_ARGUMENT, "pipewire"],
                Some(AudioBackend::Pipewire),
            ),
            (vec!["launcher", PROBE_ARGUMENT], None),
            (vec!["launcher"], None),
            (vec!["launcher", "--player-sign-in"], None),
            (vec!["launcher", PROBE_ARGUMENT, "system"], None),
            (vec!["launcher", PROBE_ARGUMENT, "pipewire", "extra"], None),
        ] {
            assert_eq!(
                probe_selection(arguments.into_iter().map(OsString::from)),
                expected
            );
        }
    }

    #[test]
    fn configure_system_default_does_not_change_the_player_environment() {
        let mut player = Command::new("game");
        assert!(!configure(
            &mut player,
            Path::new("/nonexistent-install"),
            AudioBackend::System
        )
        .unwrap());
        assert_eq!(player.get_envs().count(), 0);
    }

    #[test]
    fn run_probe_only_accepts_the_verified_route_exit_code() {
        for (code, expected) in [(0, false), (1, false), (10, true), (127, false)] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            assert_eq!(
                run_probe(&mut command, Duration::from_secs(1)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn run_probe_crashed_process_preserves_system_audio() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "kill -KILL $$"]);
        assert!(!run_probe(&mut command, Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn run_probe_timeout_kills_and_reaps_the_child() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        assert_eq!(
            run_probe(&mut command, Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn run_probe_missing_executable_reports_the_error() {
        let mut command = Command::new("/nonexistent-rebellion2-audio-probe");
        assert_eq!(
            run_probe(&mut command, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
