use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = ".player-session.json";

/// A game-requested store sign-in that does not run installation or updates.
pub struct SignIn {
    pub provider: String,
    pub session_file: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    access_token: String,
}

/// Parses the explicit player-authentication mode and its required output path.
pub fn sign_in_arguments(arguments: &[String]) -> Result<Option<SignIn>, &'static str> {
    if !arguments.iter().any(|argument| argument == "--auth-only") {
        return Ok(None);
    }
    let value = |key: &str| {
        arguments
            .iter()
            .position(|argument| argument == key)
            .and_then(|index| arguments.get(index + 1))
    };
    let provider = value("--provider").ok_or("Missing store provider")?;
    if !["steam", "gog"].contains(&provider.as_str()) {
        return Err("Unsupported store provider");
    }
    let session_file = PathBuf::from(value("--session-file").ok_or("Missing session output path")?);
    if !session_file.is_absolute() {
        return Err("Session output path must be absolute");
    }
    Ok(Some(SignIn {
        provider: provider.clone(),
        session_file,
    }))
}

/// Supplies paths as separate process arguments, preserving spaces without shell interpolation.
pub fn launch_arguments(base: &Path, launcher: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "-playerSessionFile".into(),
        base.join(FILE_NAME).into_os_string(),
        "-playerLauncher".into(),
        launcher.as_os_str().to_owned(),
    ]
}

/// Writes a separate player credential; the server remains responsible for signature validation.
pub fn save(path: &Path, token: &str) -> io::Result<()> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 2 || parts[1].is_empty() || super::token_expired(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid player session",
        ));
    }
    let payload = token.split('.').next().unwrap_or_default();
    let bytes = super::base64url_decode(payload)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid player session"))?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)?;
    let provider = claims["provider"].as_str().unwrap_or_default();
    let subject = claims["sub"].as_str().unwrap_or_default();
    if claims["aud"] != "rebellion2-multiplayer"
        || !["steam", "gog"].contains(&provider)
        || !subject.starts_with(&format!("{provider}:"))
        || claims["name"]
            .as_str()
            .is_none_or(|name| name.trim().is_empty())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid player identity",
        ));
    }
    fs_write_private(
        path,
        &serde_json::to_vec(&Session {
            access_token: token.to_owned(),
        })?,
    )
}

/// Restricts cached player credentials to the current user on Unix platforms.
fn fs_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_arguments_requires_supported_provider_and_absolute_output() {
        for arguments in [
            vec!["--auth-only"],
            vec![
                "--auth-only",
                "--provider",
                "epic",
                "--session-file",
                "relative.json",
            ],
            vec![
                "--auth-only",
                "--provider",
                "steam",
                "--session-file",
                "relative.json",
            ],
        ] {
            assert!(sign_in_arguments(
                &arguments.into_iter().map(String::from).collect::<Vec<_>>()
            )
            .is_err());
        }
    }

    #[test]
    fn sign_in_arguments_preserves_session_path_with_spaces() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("player session.json");
        let args = vec![
            "--auth-only".into(),
            "--provider".into(),
            "gog".into(),
            "--session-file".into(),
            output.to_string_lossy().into_owned(),
        ];
        let request = sign_in_arguments(&args).unwrap().unwrap();
        assert_eq!(request.provider, "gog");
        assert_eq!(request.session_file, output);
    }

    #[test]
    fn save_rejects_legacy_download_token_without_replacing_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        std::fs::write(&path, b"existing").unwrap();
        let payload = "eyJzdWIiOiJzdGVhbSIsImV4cCI6NDEwMjQ0NDgwMH0";
        assert!(save(&path, &format!("{payload}.signature")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    }
    #[test]
    fn sign_in_arguments_ordinary_launcher_arguments_keep_normal_startup() {
        assert!(sign_in_arguments(&["launcher".into(), "--repair".into()])
            .unwrap()
            .is_none());
    }

    #[test]
    fn launch_arguments_preserves_paths_with_spaces() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("Game folder");
        let launcher = base.join("launcher executable");
        assert_eq!(
            launch_arguments(&base, &launcher),
            vec![
                std::ffi::OsString::from("-playerSessionFile"),
                base.join(FILE_NAME).into_os_string(),
                std::ffi::OsString::from("-playerLauncher"),
                launcher.into_os_string(),
            ]
        );
    }

    #[test]
    fn save_writes_supported_player_metadata_without_reformatting_token() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let token = "eyJhdWQiOiJyZWJlbGxpb24yLW11bHRpcGxheWVyIiwic3ViIjoic3RlYW06NDIiLCJuYW1lIjoiVGVzdCBwbGF5ZXIiLCJwcm92aWRlciI6InN0ZWFtIiwiZXhwIjo0MTAyNDQ0ODAwfQ.signature";
        save(&path, token).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["accessToken"], token);
    }

    #[test]
    fn save_malformed_token_preserves_existing_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        for token in ["eyJhdWQiOiJyZWJlbGxpb24yLW11bHRpcGxheWVyIiwic3ViIjoic3RlYW06NDIiLCJuYW1lIjoiVGVzdCBwbGF5ZXIiLCJwcm92aWRlciI6InN0ZWFtIiwiZXhwIjo0MTAyNDQ0ODAwfQ.", "eyJhdWQiOiJyZWJlbGxpb24yLW11bHRpcGxheWVyIiwic3ViIjoic3RlYW06NDIiLCJuYW1lIjoiVGVzdCBwbGF5ZXIiLCJwcm92aWRlciI6InN0ZWFtIiwiZXhwIjo0MTAyNDQ0ODAwfQ.signature.extra", "malformed.signature"] {
            std::fs::write(&path, b"existing").unwrap();
            assert!(save(&path, token).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        }
    }
}
