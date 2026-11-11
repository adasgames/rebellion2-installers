use crate::launch_settings::GraphicsApi;
use std::{io, path::Path, process::Command};

#[cfg(target_os = "linux")]
const APPIMAGE_PATH_VARIABLES: [&str; 8] = [
    "LD_LIBRARY_PATH",
    "PATH",
    "PYTHONPATH",
    "XDG_DATA_DIRS",
    "PERLLIB",
    "QT_PLUGIN_PATH",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SYSTEM_PATH_1_0",
];

#[cfg(target_os = "linux")]
const APPIMAGE_RUNTIME_VARIABLES: [&str; 14] = [
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "OWD",
    "PYTHONHOME",
    "GSETTINGS_SCHEMA_DIR",
    "GI_TYPELIB_PATH",
    "GIO_MODULE_DIR",
    "GTK_DATA_PREFIX",
    "GTK_THEME",
    "GTK_EXE_PREFIX",
    "GTK_PATH",
    "GTK_IM_MODULE_FILE",
    "GDK_PIXBUF_MODULE_FILE",
];

/// Starts an installed Unity player with its selected API and existing content location.
pub(crate) fn launch(executable: &Path, base: &Path, api: GraphicsApi) -> io::Result<()> {
    let mut command = player_command(executable, base, api);
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
            launch_from_explorer(executable, base, api)?;
        }
    }
    #[cfg(not(target_os = "windows"))]
    command.spawn()?;
    Ok(())
}

/// Builds process arguments without shell parsing or combining executable paths with flags.
fn player_command(executable: &Path, base: &Path, api: GraphicsApi) -> Command {
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
    #[cfg(target_os = "linux")]
    sanitize_appimage_environment(&mut command);
    command
}

/// Removes launcher-owned AppImage paths before starting the installed Linux player.
#[cfg(target_os = "linux")]
fn sanitize_appimage_environment(command: &mut Command) {
    let Some(appdir) = std::env::var_os("APPDIR") else {
        return;
    };
    sanitize_appimage_environment_from(command, Path::new(&appdir), |name| std::env::var_os(name));
}

/// Applies AppImage environment overrides using values supplied by the launcher process.
#[cfg(target_os = "linux")]
fn sanitize_appimage_environment_from(
    command: &mut Command,
    appdir: &Path,
    environment: impl Fn(&str) -> Option<std::ffi::OsString>,
) {
    for variable in APPIMAGE_PATH_VARIABLES {
        let Some(value) = environment(variable) else {
            continue;
        };
        match paths_outside_appimage(&value, appdir) {
            Some(value) => {
                command.env(variable, value);
            }
            None => {
                command.env_remove(variable);
            }
        }
    }

    for variable in APPIMAGE_RUNTIME_VARIABLES {
        command.env_remove(variable);
    }
}

/// Returns a path list without entries supplied by the launcher's AppImage.
#[cfg(target_os = "linux")]
fn paths_outside_appimage(value: &std::ffi::OsStr, appdir: &Path) -> Option<std::ffi::OsString> {
    let paths = std::env::split_paths(value)
        .filter(|path| !path.starts_with(appdir))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    std::env::join_paths(paths).ok()
}

/// Asks the desktop's Explorer process to launch the player outside the launcher's job.
/// Its shell automation API carries arguments as well as the executable and working directory.
#[cfg(target_os = "windows")]
fn launch_from_explorer(executable: &Path, base: &Path, api: GraphicsApi) -> io::Result<()> {
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
            shell.ShellExecute(
                &file,
                &VARIANT::from(api.argument().unwrap_or_default()),
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
        let command = player_command(Path::new("game"), Path::new("install"), GraphicsApi::Auto);
        assert!(!command
            .get_args()
            .any(|argument| argument.to_string_lossy().starts_with("-force-")));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn paths_outside_appimage_preserves_only_host_paths() {
        let appdir = Path::new("/tmp/.mount_rebellion2");
        let value = std::env::join_paths([
            appdir.join("usr/lib"),
            Path::new("/usr/lib").to_path_buf(),
            appdir.join("lib64"),
            Path::new("/opt/vendor/lib").to_path_buf(),
        ])
        .unwrap();

        let filtered = paths_outside_appimage(&value, appdir).unwrap();

        assert_eq!(
            std::env::split_paths(&filtered).collect::<Vec<_>>(),
            [Path::new("/usr/lib"), Path::new("/opt/vendor/lib")]
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn paths_outside_appimage_removes_an_appimage_only_list() {
        let appdir = Path::new("/tmp/.mount_rebellion2");
        let value = std::env::join_paths([appdir.join("usr/lib"), appdir.join("lib64")]).unwrap();

        assert!(paths_outside_appimage(&value, appdir).is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sanitize_appimage_environment_from_clears_runtime_and_filters_path_variables() {
        use std::collections::HashMap;

        let appdir = Path::new("/tmp/.mount_rebellion2");
        let values = HashMap::from([
            (
                "LD_LIBRARY_PATH",
                std::env::join_paths([appdir.join("usr/lib"), Path::new("/usr/lib").into()])
                    .unwrap(),
            ),
            (
                "GST_PLUGIN_SYSTEM_PATH_1_0",
                std::env::join_paths([appdir.join("usr/lib/gstreamer-1.0")]).unwrap(),
            ),
        ]);
        let mut command = Command::new("game");

        sanitize_appimage_environment_from(&mut command, appdir, |name| values.get(name).cloned());

        let overrides = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(ToOwned::to_owned)))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("LD_LIBRARY_PATH")),
            Some(&Some(std::ffi::OsString::from("/usr/lib")))
        );
        assert_eq!(
            overrides.get(std::ffi::OsStr::new("GST_PLUGIN_SYSTEM_PATH_1_0")),
            Some(&None)
        );
        for variable in APPIMAGE_RUNTIME_VARIABLES {
            assert_eq!(
                overrides.get(std::ffi::OsStr::new(variable)),
                Some(&None),
                "{variable} should not reach the player"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_preserves_the_content_location_before_the_rendering_argument() {
        let command = player_command(
            Path::new("Game.app"),
            Path::new("install"),
            GraphicsApi::Metal,
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
}
