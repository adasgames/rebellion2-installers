use rebellion2_update_core::{replace_file_atomically, sha256_hex};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const MAX_ATTEMPTS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// One authenticated or public HTTP download that can continue from a partial file.
pub(crate) struct DownloadSpec<'a> {
    pub(crate) url: &'a str,
    pub(crate) authorization: Option<&'a str>,
    pub(crate) destination: &'a Path,
    pub(crate) expected_size: Option<u64>,
    pub(crate) expected_sha256: Option<&'a str>,
}

/// A failed HTTP download with enough information for callers to choose recovery UI.
#[derive(Debug)]
pub(crate) enum DownloadError {
    HttpStatus(u16),
    Io(io::Error),
    InvalidData(String),
}

impl DownloadError {
    /// Returns the HTTP response status when the server rejected the request.
    pub(crate) fn status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus(status) => Some(*status),
            Self::Io(_) | Self::InvalidData(_) => None,
        }
    }

    /// Converts the download failure to an I/O error for the patch engine.
    pub(crate) fn into_io_error(self) -> io::Error {
        match self {
            Self::Io(error) => error,
            Self::HttpStatus(status) => {
                io::Error::other(format!("download returned HTTP {status}"))
            }
            Self::InvalidData(message) => io::Error::new(io::ErrorKind::InvalidData, message),
        }
    }
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(status) => write!(formatter, "HTTP {status}"),
            Self::Io(error) => error.fmt(formatter),
            Self::InvalidData(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::HttpStatus(_) | Self::InvalidData(_) => None,
        }
    }
}

impl From<io::Error> for DownloadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Downloads a file with bounded retries, stalled-read timeouts, and HTTP Range resume.
pub(crate) fn download_to_path(
    spec: &DownloadSpec<'_>,
    progress: impl FnMut(u64),
    retrying: impl FnMut(usize, Duration, &DownloadError),
) -> Result<(), DownloadError> {
    download_to_path_with_sleep(spec, progress, retrying, thread::sleep)
}

fn download_to_path_with_sleep(
    spec: &DownloadSpec<'_>,
    mut progress: impl FnMut(u64),
    mut retrying: impl FnMut(usize, Duration, &DownloadError),
    mut sleep: impl FnMut(Duration),
) -> Result<(), DownloadError> {
    if let Some(parent) = spec.destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let partial = partial_path(spec.destination);
    let etag_path = metadata_path(spec.destination, "etag");
    let size_path = metadata_path(spec.destination, "size");
    let mut known_size = spec.expected_size.or_else(|| read_known_size(&size_path));

    if promote_if_complete(spec, &partial, known_size, &mut progress)? {
        return Ok(());
    }
    if file_is_complete(spec.destination, known_size, spec.expected_sha256)? {
        progress(fs::metadata(spec.destination)?.len());
        return Ok(());
    }
    remove_file_if_present(spec.destination)?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build();
    let mut last_error = None;

    for attempt in 0..MAX_ATTEMPTS {
        let mut offset = partial_length(&partial)?;
        if known_size.is_some_and(|size| offset > size) {
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
            offset = 0;
        }
        progress(offset);

        let mut request = agent.get(spec.url);
        if let Some(authorization) = spec.authorization {
            request = request.set("Authorization", authorization);
        }
        if offset > 0 {
            request = request.set("Range", &format!("bytes={offset}-"));
            if let Ok(etag) = fs::read_to_string(&etag_path) {
                let etag = etag.trim();
                if !etag.is_empty() {
                    request = request.set("If-Range", etag);
                }
            }
        }

        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                if status == 416 && offset > 0 {
                    reset_partial(&partial, &etag_path, &size_path)?;
                    known_size = spec.expected_size;
                    let error = DownloadError::HttpStatus(status);
                    if schedule_retry(
                        attempt,
                        retry_after(&response),
                        error,
                        &mut last_error,
                        &mut retrying,
                        &mut sleep,
                    ) {
                        continue;
                    }
                    break;
                }
                let error = DownloadError::HttpStatus(status);
                if !retryable_status(status) {
                    last_error = Some(error);
                    break;
                }
                if !schedule_retry(
                    attempt,
                    retry_after(&response),
                    error,
                    &mut last_error,
                    &mut retrying,
                    &mut sleep,
                ) {
                    break;
                }
                continue;
            }
            Err(ureq::Error::Transport(error)) => {
                if !schedule_retry(
                    attempt,
                    None,
                    DownloadError::Io(io::Error::other(error.to_string())),
                    &mut last_error,
                    &mut retrying,
                    &mut sleep,
                ) {
                    break;
                }
                continue;
            }
        };

        let response_status = response.status();
        let content_range = response
            .header("Content-Range")
            .and_then(parse_content_range);
        let valid_partial_response =
            response_status != 206 || content_range.is_some_and(|range| range.start == offset);
        if !valid_partial_response {
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
            let error = DownloadError::InvalidData(
                "download server returned an invalid resume range".to_string(),
            );
            if !schedule_retry(
                attempt,
                None,
                error,
                &mut last_error,
                &mut retrying,
                &mut sleep,
            ) {
                break;
            }
            continue;
        }
        let resumed = offset > 0 && response_status == 206;
        if offset > 0 && !resumed {
            offset = 0;
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
        }

        let response_total = content_range.and_then(|range| range.total).or_else(|| {
            (response_status != 206)
                .then(|| response.header("Content-Length")?.parse().ok())
                .flatten()
        });
        if response_status == 206
            && known_size.is_none()
            && response_total.is_none()
            && spec.expected_sha256.is_none()
        {
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
            let error = DownloadError::InvalidData(
                "download server did not report the total size of the resumed file".to_string(),
            );
            if !schedule_retry(
                attempt,
                None,
                error,
                &mut last_error,
                &mut retrying,
                &mut sleep,
            ) {
                break;
            }
            continue;
        }
        if resumed
            && known_size.is_some()
            && response_total.is_some()
            && known_size != response_total
        {
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
            let error = DownloadError::InvalidData(
                "download size changed while resuming; restarting from the beginning".to_string(),
            );
            if !schedule_retry(
                attempt,
                None,
                error,
                &mut last_error,
                &mut retrying,
                &mut sleep,
            ) {
                break;
            }
            continue;
        }
        if let Some(total) = response_total {
            if spec.expected_size.is_some_and(|expected| expected != total) {
                let error = DownloadError::InvalidData(format!(
                    "download size changed: expected {}, server reported {total}",
                    spec.expected_size.unwrap_or_default()
                ));
                reset_partial(&partial, &etag_path, &size_path)?;
                if !schedule_retry(
                    attempt,
                    None,
                    error,
                    &mut last_error,
                    &mut retrying,
                    &mut sleep,
                ) {
                    break;
                }
                continue;
            }
            known_size = Some(total);
            fs::write(&size_path, total.to_string())?;
        }
        if let Some(etag) = response.header("ETag") {
            fs::write(&etag_path, etag)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&partial)?;
        let mut reader = response.into_reader();
        let mut buffer = [0_u8; 64 * 1024];
        let mut downloaded = offset;
        let read_result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(count) => {
                    if let Err(error) = file.write_all(&buffer[..count]) {
                        break Err(error);
                    }
                    downloaded = downloaded.saturating_add(count as u64);
                    progress(downloaded);
                }
                Err(error) => break Err(error),
            }
        };
        let read_error = read_result.err();
        let sync_error = file.sync_all().err();
        drop(file);

        let expected_length_received = known_size.is_some_and(|expected| downloaded == expected);
        let response_completed = read_error.is_none() || expected_length_received;
        let can_validate = sync_error.is_none()
            && response_completed
            && known_size.is_none_or(|expected| downloaded == expected);
        if can_validate {
            if file_matches_hash(&partial, spec.expected_sha256)? {
                replace_file_atomically(&partial, spec.destination)?;
                return Ok(());
            }
            reset_partial(&partial, &etag_path, &size_path)?;
            known_size = spec.expected_size;
            last_error = Some(DownloadError::InvalidData(
                "download did not match its signed hash".to_string(),
            ));
        }

        if last_error.is_none() {
            if let Some(error) = sync_error.or(read_error) {
                last_error = Some(DownloadError::Io(error));
            } else if known_size.is_some_and(|expected| downloaded != expected) {
                last_error = Some(DownloadError::InvalidData(format!(
                    "download ended at {downloaded} bytes; expected {}",
                    known_size.unwrap_or_default()
                )));
            }
        }

        let error = last_error.take().unwrap_or_else(|| {
            DownloadError::InvalidData("download failed without an error".to_string())
        });
        if !schedule_retry(
            attempt,
            None,
            error,
            &mut last_error,
            &mut retrying,
            &mut sleep,
        ) {
            break;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        DownloadError::InvalidData("download failed without an error".to_string())
    }))
}

#[derive(Clone, Copy)]
struct ContentRange {
    start: u64,
    total: Option<u64>,
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let total = (total != "*").then(|| total.parse().ok()).flatten();
    (start <= end && total.is_none_or(|total| end < total)).then_some(ContentRange { start, total })
}

fn schedule_retry(
    attempt: usize,
    retry_after: Option<Duration>,
    error: DownloadError,
    last_error: &mut Option<DownloadError>,
    retrying: &mut impl FnMut(usize, Duration, &DownloadError),
    sleep: &mut impl FnMut(Duration),
) -> bool {
    *last_error = Some(error);
    if attempt + 1 >= MAX_ATTEMPTS {
        return false;
    }
    let delay = retry_after
        .unwrap_or_else(|| Duration::from_secs(1 << attempt))
        .min(MAX_RETRY_DELAY);
    let next_attempt = attempt + 2;
    retrying(
        next_attempt,
        delay,
        last_error.as_ref().expect("download error was stored"),
    );
    sleep(delay);
    *last_error = None;
    true
}

fn retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

fn retry_after(response: &ureq::Response) -> Option<Duration> {
    response
        .header("Retry-After")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn promote_if_complete(
    spec: &DownloadSpec<'_>,
    partial: &Path,
    known_size: Option<u64>,
    progress: &mut impl FnMut(u64),
) -> Result<bool, DownloadError> {
    if !file_is_complete(partial, known_size, spec.expected_sha256)? {
        return Ok(false);
    }
    let length = fs::metadata(partial)?.len();
    replace_file_atomically(partial, spec.destination)?;
    progress(length);
    Ok(true)
}

fn file_is_complete(
    path: &Path,
    expected_size: Option<u64>,
    expected_sha256: Option<&str>,
) -> Result<bool, DownloadError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if expected_size.is_none() && expected_sha256.is_none() {
        return Ok(false);
    }
    if expected_size.is_some_and(|expected| metadata.len() != expected) {
        return Ok(false);
    }
    file_matches_hash(path, expected_sha256)
}

fn file_matches_hash(path: &Path, expected_sha256: Option<&str>) -> Result<bool, DownloadError> {
    let Some(expected_sha256) = expected_sha256 else {
        return Ok(true);
    };
    let bytes = fs::read(path)?;
    Ok(sha256_hex(&bytes) == expected_sha256)
}

fn partial_length(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn read_known_size(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn reset_partial(partial: &Path, etag: &Path, size: &Path) -> io::Result<()> {
    remove_file_if_present(partial)?;
    remove_file_if_present(etag)?;
    remove_file_if_present(size)
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn partial_path(destination: &Path) -> PathBuf {
    path_with_suffix(destination, ".part")
}

fn metadata_path(destination: &Path, extension: &str) -> PathBuf {
    path_with_suffix(destination, &format!(".part.{extension}"))
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

/// Returns the server-reported total retained for a resumable download.
pub(crate) fn known_download_size(destination: &Path) -> Option<u64> {
    read_known_size(&metadata_path(destination, "size"))
}

/// Removes resume metadata after the caller has committed the downloaded file.
pub(crate) fn clear_download_metadata(destination: &Path) -> io::Result<()> {
    remove_file_if_present(&metadata_path(destination, "etag"))?;
    remove_file_if_present(&metadata_path(destination, "size"))
}

/// Removes a completed update layer's cache while retaining failed layers for retry.
pub(crate) fn clear_cache(directory: &Path) -> io::Result<()> {
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[test]
    fn download_to_path_existing_partial_resumes_from_range() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve(body, 1, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(partial_path(&destination), &body[..4]).unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        assert!(requests.lock().unwrap()[0].contains("Range: bytes=4-"));
    }

    #[test]
    fn download_to_path_interrupted_response_retries_download() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_interrupted_then_resumed(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn download_to_path_server_ignores_range_replaces_partial() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_without_range(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(partial_path(&destination), b"stale").unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        assert!(requests.lock().unwrap()[0].contains("Range: bytes=5-"));
    }

    #[test]
    fn download_to_path_changed_total_restarts_unhashed_download() {
        let body = b"abcdefghijkl";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_changed_total_then_full(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("content.zip");
        fs::write(partial_path(&destination), b"old!").unwrap();
        fs::write(metadata_path(&destination, "size"), b"10").unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: None,
                expected_sha256: None,
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("Range: bytes=4-"));
        assert!(!requests[1].contains("Range:"));
    }

    #[test]
    fn download_to_path_unknown_resumed_total_restarts_unhashed_download() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_unknown_total_then_full(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("content.zip");
        fs::write(partial_path(&destination), &body[..4]).unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: None,
                expected_sha256: None,
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("Range: bytes=4-"));
        assert!(!requests[1].contains("Range:"));
    }

    #[test]
    fn download_to_path_verified_cache_avoids_network_request() {
        let body = b"already downloaded";
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(&destination, body).unwrap();
        let mut reported = 0;

        download_without_waiting(
            &DownloadSpec {
                url: "http://127.0.0.1:1/unreachable",
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |downloaded| reported = downloaded,
        )
        .unwrap();

        assert_eq!(reported, body.len() as u64);
        assert_eq!(fs::read(destination).unwrap(), body);
    }

    #[test]
    fn download_to_path_completed_partial_atomically_replaces_stale_destination() {
        let body = b"completed replacement";
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(&destination, b"stale").unwrap();
        fs::write(partial_path(&destination), body).unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: "http://127.0.0.1:1/unreachable",
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), body);
        assert!(!partial_path(&destination).exists());
    }

    #[test]
    fn download_to_path_completed_unhashed_download_reuses_recorded_size() {
        let body = b"completed archive";
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("content.zip");
        fs::write(&destination, body).unwrap();
        fs::write(metadata_path(&destination, "size"), body.len().to_string()).unwrap();
        let mut reported = 0;

        download_without_waiting(
            &DownloadSpec {
                url: "http://127.0.0.1:1/unreachable",
                authorization: None,
                destination: &destination,
                expected_size: None,
                expected_sha256: None,
            },
            |downloaded| reported = downloaded,
        )
        .unwrap();

        assert_eq!(reported, body.len() as u64);
        assert_eq!(fs::read(destination).unwrap(), body);
    }

    #[test]
    fn download_to_path_nonretryable_status_preserves_status() {
        let (url, server) = serve_status(404);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");

        let error = download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(1),
                expected_sha256: None,
            },
            |_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.status(), Some(404));
    }

    #[test]
    fn download_to_path_hash_retry_reports_progress_reset() {
        let body = b"abcdefghij";
        let (url, server) = serve_bad_then_good(body);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        let mut progress = Vec::new();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |downloaded| progress.push(downloaded),
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let completed = progress
            .iter()
            .position(|value| *value == body.len() as u64)
            .unwrap();
        assert!(progress[completed + 1..].contains(&0));
    }

    #[test]
    fn download_to_path_retry_reports_each_attempts_actual_failure() {
        let body = b"abcdefghij";
        let (url, server) = serve_retryable_then_interrupted_then_not_found(body);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        let mut retry_errors = Vec::new();

        let error = download_to_path_with_sleep(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
            |_, _, error| retry_errors.push(error.to_string()),
            |_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.status(), Some(404));
        assert_eq!(retry_errors[0], "HTTP 500");
        assert_eq!(
            retry_errors[1],
            "response body closed before all bytes were read"
        );
    }

    #[test]
    fn download_to_path_range_not_satisfiable_resets_partial_and_retries_from_zero() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_range_not_satisfiable_then_full(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(partial_path(&destination), b"stale").unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("Range: bytes=5-"));
        assert!(!requests[1].contains("Range:"));
    }

    #[test]
    fn download_to_path_invalid_content_range_restarts_from_zero() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_invalid_range_then_full(body, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(partial_path(&destination), &body[..4]).unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(destination).unwrap(), body);
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("Range: bytes=4-"));
        assert!(!requests[1].contains("Range:"));
    }

    #[test]
    fn download_to_path_resume_sends_saved_etag_as_if_range() {
        let body = b"abcdefghij";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve(body, 1, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(partial_path(&destination), &body[..4]).unwrap();
        fs::write(metadata_path(&destination, "etag"), b"\"release-1\"").unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        let request = &requests.lock().unwrap()[0];
        assert!(request.contains("Range: bytes=4-"));
        assert!(request.contains("If-Range: \"release-1\""));
    }

    #[test]
    fn download_to_path_server_size_mismatch_never_promotes_file() {
        let body = b"short";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_full_repeated(body, MAX_ATTEMPTS, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");

        let error = download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(10),
                expected_sha256: None,
            },
            |_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert!(error.to_string().contains("expected 10"));
        assert_eq!(requests.lock().unwrap().len(), MAX_ATTEMPTS);
        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
    }

    #[test]
    fn download_to_path_retryable_status_stops_after_maximum_attempts() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_status_repeated(500, MAX_ATTEMPTS, requests.clone(), None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        let mut retry_numbers = Vec::new();

        let error = download_to_path_with_sleep(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(1),
                expected_sha256: None,
            },
            |_| {},
            |attempt, _, _| retry_numbers.push(attempt),
            |_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.status(), Some(500));
        assert_eq!(requests.lock().unwrap().len(), MAX_ATTEMPTS);
        assert_eq!(retry_numbers, vec![2, 3, 4]);
    }

    #[test]
    fn download_to_path_retry_after_is_capped_before_sleeping() {
        let body = b"ok";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_retry_after_then_full(body, requests);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        let mut reported_delays = Vec::new();
        let mut slept_delays = Vec::new();

        download_to_path_with_sleep(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
            |_, delay, _| reported_delays.push(delay),
            |delay| slept_delays.push(delay),
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(reported_delays, vec![MAX_RETRY_DELAY]);
        assert_eq!(slept_delays, vec![MAX_RETRY_DELAY]);
        assert_eq!(fs::read(destination).unwrap(), body);
    }

    #[test]
    fn download_to_path_unauthorized_status_is_not_retried() {
        let (url, server) = serve_status(401);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        let mut retries = 0;

        let error = download_to_path_with_sleep(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(1),
                expected_sha256: None,
            },
            |_| {},
            |_, _, _| retries += 1,
            |_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.status(), Some(401));
        assert_eq!(retries, 0);
    }

    #[test]
    fn download_to_path_corrupt_destination_is_replaced_from_network() {
        let body = b"replacement";
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (url, server) = serve_full_repeated(body, 1, requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(&destination, b"corrupt-data").unwrap();

        download_without_waiting(
            &DownloadSpec {
                url: &url,
                authorization: None,
                destination: &destination,
                expected_size: Some(body.len() as u64),
                expected_sha256: Some(&sha256_hex(body)),
            },
            |_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(fs::read(destination).unwrap(), body);
    }

    #[test]
    fn clear_download_metadata_and_cache_are_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("blob");
        fs::write(metadata_path(&destination, "etag"), b"etag").unwrap();
        fs::write(metadata_path(&destination, "size"), b"5").unwrap();
        let cache = directory.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("blob"), b"cached").unwrap();

        clear_download_metadata(&destination).unwrap();
        clear_download_metadata(&destination).unwrap();
        clear_cache(&cache).unwrap();
        clear_cache(&cache).unwrap();

        assert!(!metadata_path(&destination, "etag").exists());
        assert!(!metadata_path(&destination, "size").exists());
        assert!(!cache.exists());
    }

    #[test]
    fn parse_content_range_rejects_reversed_or_out_of_bounds_ranges() {
        assert!(parse_content_range("bytes 5-4/10").is_none());
        assert!(parse_content_range("bytes 0-10/10").is_none());
        assert!(parse_content_range("bytes 0-final/10").is_none());
        assert_eq!(parse_content_range("bytes 4-9/10").unwrap().start, 4);
    }

    fn download_without_waiting(
        spec: &DownloadSpec<'_>,
        progress: impl FnMut(u64),
    ) -> Result<(), DownloadError> {
        download_to_path_with_sleep(spec, progress, |_, _, _| {}, |_| {})
    }

    fn serve(
        body: &'static [u8],
        request_count: usize,
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&stream);
                let offset = requested_offset(&request).unwrap_or(0);
                requests.lock().unwrap().push(request);
                let response_body = &body[offset..];
                write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    response_body.len(),
                    offset,
                    body.len() - 1,
                    body.len()
                )
                .unwrap();
                stream.write_all(response_body).unwrap();
            }
        });
        (url, server)
    }

    fn serve_interrupted_then_resumed(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&first));
            write!(
                first,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            first.write_all(&body[..4]).unwrap();
            drop(first);

            let (mut second, _) = listener.accept().unwrap();
            let request = read_request(&second);
            let offset = requested_offset(&request).unwrap_or(0);
            requests.lock().unwrap().push(request);
            let response_body = &body[offset..];
            if offset > 0 {
                write!(
                    second,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    response_body.len(),
                    offset,
                    body.len() - 1,
                    body.len()
                )
                .unwrap();
            } else {
                write!(
                    second,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
            }
            second.write_all(response_body).unwrap();
        });
        (url, server)
    }

    fn serve_without_range(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&stream));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_changed_total_then_full(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let request = read_request(&first);
            let offset = requested_offset(&request).unwrap();
            requests.lock().unwrap().push(request);
            let response_body = &body[offset..];
            write!(
                first,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                response_body.len(),
                offset,
                body.len() - 1,
                body.len()
            )
            .unwrap();
            first.write_all(response_body).unwrap();

            let (mut second, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&second));
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_range_not_satisfiable_then_full(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&first));
            write!(
                first,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();

            let (mut second, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&second));
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_unknown_total_then_full(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let request = read_request(&first);
            let offset = requested_offset(&request).unwrap();
            requests.lock().unwrap().push(request);
            let response_body = &body[offset..];
            write!(
                first,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/*\r\nConnection: close\r\n\r\n",
                response_body.len(),
                offset,
                body.len() - 1
            )
            .unwrap();
            first.write_all(response_body).unwrap();

            let (mut second, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&second));
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_invalid_range_then_full(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&first));
            write!(
                first,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                body.len(),
                body.len() - 1,
                body.len()
            )
            .unwrap();
            first.write_all(body).unwrap();

            let (mut second, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&second));
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_full_repeated(
        body: &'static [u8],
        count: usize,
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                requests.lock().unwrap().push(read_request(&stream));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        (url, server)
    }

    fn serve_status_repeated(
        status: u16,
        count: usize,
        requests: Arc<Mutex<Vec<String>>>,
        retry_after: Option<u64>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                requests.lock().unwrap().push(read_request(&stream));
                let retry_after = retry_after
                    .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                    .unwrap_or_default();
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\n{retry_after}Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        (url, server)
    }

    fn serve_retry_after_then_full(
        body: &'static [u8],
        requests: Arc<Mutex<Vec<String>>>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&first));
            write!(
                first,
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 999\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();

            let (mut second, _) = listener.accept().unwrap();
            requests.lock().unwrap().push(read_request(&second));
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(body).unwrap();
        });
        (url, server)
    }

    fn serve_status(status: u16) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&stream);
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        (url, server)
    }

    fn serve_bad_then_good(body: &'static [u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for response_body in [b"xxxxxxxxxx".as_slice(), body] {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&stream);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                )
                .unwrap();
                stream.write_all(response_body).unwrap();
            }
        });
        (url, server)
    }

    fn serve_retryable_then_interrupted_then_not_found(
        body: &'static [u8],
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let _ = read_request(&first);
            write!(
                first,
                "HTTP/1.1 500 Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();

            let (mut second, _) = listener.accept().unwrap();
            let _ = read_request(&second);
            write!(
                second,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            second.write_all(&body[..4]).unwrap();
            drop(second);

            let (mut third, _) = listener.accept().unwrap();
            let _ = read_request(&third);
            write!(
                third,
                "HTTP/1.1 404 Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        (url, server)
    }

    fn read_request(stream: &TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            request.push_str(&line);
        }
        request
    }

    fn requested_offset(request: &str) -> Option<usize> {
        request.lines().find_map(|line| {
            line.strip_prefix("Range: bytes=")
                .or_else(|| line.strip_prefix("range: bytes="))?
                .strip_suffix('-')?
                .parse()
                .ok()
        })
    }
}
