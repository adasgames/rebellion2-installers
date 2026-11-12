use crate::launch_settings::{GraphicsApi, LauncherSettings};
use std::{ffi::OsString, io, path::Path, process::Command};

/// Starts an installed Unity player with its selected API and existing content location.
pub(crate) fn launch(
    executable: &Path,
    base: &Path,
    settings: &LauncherSettings,
    extra_arguments: &[OsString],
) -> io::Result<()> {
    let api = settings.graphics_api;
    let mut command = player_command(executable, base, api, extra_arguments);
    #[cfg(target_os = "linux")]
    match crate::linux_audio::configure(&mut command, base, settings.audio_backend) {
        Ok(true) => {
            crate::log_line("[launcher] using the verified PipeWire ALSA route for this player.")
        }
        Ok(false) => {}
        Err(error) if settings.audio_backend == crate::launch_settings::AudioBackend::Pipewire => {
            return Err(error)
        }
        Err(error) => crate::log_line(&format!(
            "[launcher] audio preflight unavailable ({error}); preserving system audio settings."
        )),
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        if let Err(error) = command.spawn() {
            crate::log_line(&format!(
                "[launcher] breakaway spawn failed ({error}); handing off to Explorer."
            ));
            launch_from_explorer(executable, base, api, extra_arguments)?;
        }
    }
    #[cfg(not(target_os = "windows"))]
    command.spawn()?;
    Ok(())
}

/// Builds process arguments without shell parsing or combining executable paths with flags.
fn player_command(
    executable: &Path,
    base: &Path,
    api: GraphicsApi,
    extra_arguments: &[OsString],
) -> Command {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/open");
        command
            .arg(executable)
            .arg("--args")
            .arg("-contentPath")
            .arg(base.join("Content"));
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new(executable);
    command.current_dir(base);
    if let Some(argument) = api.argument() {
        command.arg(argument);
    }
    command.args(extra_arguments);
    command
}

/// Asks the desktop's Explorer process to launch the player outside the launcher's job.
/// Its shell automation API carries arguments as well as the executable and working directory.
#[cfg(target_os = "windows")]
fn launch_from_explorer(
    executable: &Path,
    base: &Path,
    api: GraphicsApi,
    extra_arguments: &[OsString],
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{Interface, BSTR},
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            System::{
                Com::{
                    CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, IServiceProvider,
                    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
                },
                Variant::VARIANT,
            },
            UI::Shell::{
                IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
                SID_STopLevelBrowser, ShellWindows, SVGIO_BACKGROUND, SWC_DESKTOP,
                SWFO_NEEDDISPATCH,
            },
        },
    };

    // SAFETY: COM is initialized for this thread, all interface references remain local,
    // and every borrowed string or variant outlives its synchronous shell call.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
            return Err(io::Error::other(windows::core::Error::from_hresult(
                initialized,
            )));
        }
        let result = (|| -> windows::core::Result<()> {
            let shell_windows: IShellWindows =
                CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
            let mut desktop_window = 0;
            let desktop = shell_windows.FindWindowSW(
                &VARIANT::default(),
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut desktop_window,
                SWFO_NEEDDISPATCH,
            )?;
            let provider: IServiceProvider = desktop.cast()?;
            let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
            let view = browser.QueryActiveShellView()?;
            let background: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
            let folder: IShellFolderViewDual = background.cast()?;
            let shell: IShellDispatch2 = folder.Application()?.cast()?;
            let file = BSTR::from_wide(&executable.as_os_str().encode_wide().collect::<Vec<_>>());
            let directory = BSTR::from_wide(&base.as_os_str().encode_wide().collect::<Vec<_>>());
            let arguments = BSTR::from_wide(&explorer_arguments(api, extra_arguments));
            shell.ShellExecute(
                &file,
                &VARIANT::from(arguments),
                &VARIANT::from(directory),
                &VARIANT::from("open"),
                &VARIANT::from(1_i32),
            )
        })();
        if initialized.is_ok() {
            CoUninitialize();
        }
        result.map_err(io::Error::other)
    }
}

/// Quotes each UTF-16 argument for the Windows process parser, preserving path separators and quotes.
#[cfg(target_os = "windows")]
fn explorer_arguments(api: GraphicsApi, extra_arguments: &[OsString]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut arguments = Vec::new();
    let rendering = api.argument().map(OsString::from);
    for argument in rendering.iter().chain(extra_arguments) {
        if !arguments.is_empty() {
            arguments.push(b' ' as u16);
        }
        arguments.push(b'"' as u16);
        let mut slashes = 0;
        for character in argument.encode_wide() {
            if character == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            let count = if character == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            };
            arguments.extend(std::iter::repeat_n(b'\\' as u16, count));
            arguments.push(character);
            slashes = 0;
        }
        arguments.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        arguments.push(b'"' as u16);
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn explicit_api_is_passed_as_its_own_argument() {
        let api = GraphicsApi::supported_on(std::env::consts::OS)[1];
        let command = player_command(
            Path::new("game with spaces"),
            Path::new("install with spaces"),
            api,
            &[],
        );
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("install with spaces"))
        );
        assert_eq!(
            command.get_args().last(),
            Some(OsStr::new(api.argument().unwrap()))
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(command.get_program(), OsStr::new("game with spaces"));
    }

    #[test]
    fn automatic_does_not_force_a_graphics_api() {
        let command = player_command(
            Path::new("game"),
            Path::new("install"),
            GraphicsApi::Auto,
            &[],
        );
        assert!(!command
            .get_args()
            .any(|argument| argument.to_string_lossy().starts_with("-force-")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_preserves_the_content_location_before_the_rendering_argument() {
        let command = player_command(
            Path::new("Game.app"),
            Path::new("install"),
            GraphicsApi::Metal,
            &[],
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "Game.app",
                "--args",
                "-contentPath",
                "install/Content",
                "-force-metal"
            ]
        );
    }

    #[test]
    fn player_session_paths_are_forwarded_as_separate_arguments() {
        let extra = crate::player_session::launch_arguments(
            Path::new("install with spaces"),
            Path::new("launcher with spaces"),
        );
        let command = player_command(
            Path::new("game"),
            Path::new("install with spaces"),
            GraphicsApi::Auto,
            &extra,
        );
        let arguments = command.get_args().collect::<Vec<_>>();
        assert!(arguments.ends_with(&extra.iter().map(OsString::as_os_str).collect::<Vec<_>>()));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn explorer_handoff_preserves_spaces_quotes_and_trailing_slashes() {
        let extra = [
            OsString::from("plain"),
            OsString::from("a path\\"),
            OsString::from("quoted\"value"),
        ];
        let arguments = String::from_utf16(&explorer_arguments(GraphicsApi::Auto, &extra)).unwrap();
        assert_eq!(arguments, "\"plain\" \"a path\\\\\" \"quoted\\\"value\"");
    }
}
