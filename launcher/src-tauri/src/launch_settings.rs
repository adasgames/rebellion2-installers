use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// Selects an API included in the desktop player build.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GraphicsApi {
    #[default]
    Auto,
    D3d11,
    D3d12,
    Vulkan,
    Metal,
}

impl GraphicsApi {
    /// Lists only APIs included in this platform's player.
    pub(crate) fn supported_on(platform: &str) -> &'static [Self] {
        match platform {
            "windows" => &[Self::Auto, Self::D3d12, Self::D3d11],
            "linux" => &[Self::Auto, Self::Vulkan],
            "macos" => &[Self::Auto, Self::Metal],
            _ => &[Self::Auto],
        }
    }

    /// Parses a selection without accepting arbitrary player arguments.
    pub(crate) fn from_selection(value: &str, platform: &str) -> io::Result<Self> {
        Self::supported_on(platform)
            .iter()
            .copied()
            .find(|api| api.value() == value)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Choose a supported graphics API.",
                )
            })
    }

    /// Returns the persisted and form-submitted identifier.
    pub(crate) fn value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::D3d11 => "d3d11",
            Self::D3d12 => "d3d12",
            Self::Vulkan => "vulkan",
            Self::Metal => "metal",
        }
    }

    /// Returns the user-facing API name.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic (recommended)",
            Self::D3d11 => "DirectX 11",
            Self::D3d12 => "DirectX 12",
            Self::Vulkan => "Vulkan",
            Self::Metal => "Metal",
        }
    }

    /// Preserves normal startup for Automatic and otherwise supplies one fixed Unity flag.
    pub(crate) fn argument(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::D3d11 => Some("-force-d3d11"),
            Self::D3d12 => Some("-force-d3d12"),
            Self::Vulkan => Some("-force-vulkan"),
            Self::Metal => Some("-force-metal"),
        }
    }
}

/// Stores launcher preferences independently of installed game and content files.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct LauncherSettings {
    pub(crate) graphics_api: GraphicsApi,
}

impl LauncherSettings {
    /// Loads preferences, using defaults only when no preference file exists.
    pub(crate) fn load(path: &Path, platform: &str) -> io::Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let settings: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        GraphicsApi::from_selection(settings.graphics_api.value(), platform)?;
        Ok(settings)
    }

    /// Atomically replaces the user preference file after creating its parent directory.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::write_file_atomically(path.to_path_buf(), &serde_json::to_vec_pretty(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_apis_match_each_desktop_player() {
        assert_eq!(
            GraphicsApi::supported_on("windows"),
            &[GraphicsApi::Auto, GraphicsApi::D3d12, GraphicsApi::D3d11]
        );
        assert_eq!(
            GraphicsApi::supported_on("linux"),
            &[GraphicsApi::Auto, GraphicsApi::Vulkan]
        );
        assert_eq!(
            GraphicsApi::supported_on("macos"),
            &[GraphicsApi::Auto, GraphicsApi::Metal]
        );
    }

    #[test]
    fn selections_reject_unsupported_apis_and_extra_arguments() {
        for (platform, value) in [
            ("windows", "vulkan"),
            ("windows", "metal"),
            ("macos", "d3d12"),
            ("linux", "d3d11"),
            ("windows", "d3d11 -logFile other"),
            ("windows", ""),
        ] {
            assert!(GraphicsApi::from_selection(value, platform).is_err());
        }
    }

    #[test]
    fn automatic_keeps_startup_arguments_unchanged() {
        assert_eq!(GraphicsApi::Auto.argument(), None);
    }

    #[test]
    fn explicit_apis_supply_their_unity_startup_flag() {
        for (api, expected) in [
            (GraphicsApi::D3d11, "-force-d3d11"),
            (GraphicsApi::D3d12, "-force-d3d12"),
            (GraphicsApi::Vulkan, "-force-vulkan"),
            (GraphicsApi::Metal, "-force-metal"),
        ] {
            assert_eq!(api.argument(), Some(expected));
        }
    }

    #[test]
    fn missing_settings_use_automatic_without_creating_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        assert_eq!(
            LauncherSettings::load(&path, "windows").unwrap(),
            LauncherSettings::default()
        );
        assert!(!path.exists());
    }

    #[test]
    fn saving_and_replacing_settings_survives_a_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config/settings.json");
        for api in [GraphicsApi::D3d11, GraphicsApi::D3d12, GraphicsApi::Auto] {
            let settings = LauncherSettings { graphics_api: api };
            settings.save(&path).unwrap();
            assert_eq!(LauncherSettings::load(&path, "windows").unwrap(), settings);
        }
    }

    #[test]
    fn damaged_or_foreign_platform_settings_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        for contents in [
            "{",
            r#"{"graphics_api":"unknown"}"#,
            r#"{"graphics_api":"metal"}"#,
        ] {
            fs::write(&path, contents).unwrap();
            assert!(LauncherSettings::load(&path, "windows").is_err());
        }
    }
}
