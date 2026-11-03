use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::post,
    Form, Router,
};
use serde::Deserialize;
use std::{io, net::TcpListener, sync::Arc, sync::Mutex, time::Duration};
use subtle::ConstantTimeEq;
use tauri::Url;
use tokio::sync::{oneshot, watch};

/// One browser attempt whose listener is allocated before the browser opens.
pub struct Attempt {
    listener: TcpListener,
    secret: [u8; 32],
    origin: String,
    cancellation: watch::Receiver<bool>,
}

/// Allows the launcher to reopen or cancel exactly one pending browser attempt.
#[derive(Clone)]
pub struct Control {
    pub url: String,
    pub id: String,
    cancellation: watch::Sender<bool>,
}

/// The gate's existing ownership result, delivered in a POST body rather than a URL.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    state: String,
    pub ok: String,
    pub msg: Option<String>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub reason: Option<String>,
    pub profile_visibility: Option<String>,
}

/// Distinguishes a completed ownership check from local cancellation or transport failure.
#[derive(Debug)]
pub enum Outcome {
    Completed(Completion),
    Cancelled,
    TimedOut,
    Unavailable,
}

struct CallbackState {
    secret: [u8; 32],
    origin: String,
    host: String,
    completion: Mutex<Option<oneshot::Sender<Completion>>>,
}

impl Attempt {
    /// Binds exclusively to IPv4 loopback and creates an unguessable per-attempt secret.
    pub fn prepare(base: &str, version: Option<&str>) -> io::Result<(Self, Control)> {
        let base = Url::parse(base).map_err(io::Error::other)?;
        if (base.scheme() != "https"
            && !(cfg!(debug_assertions)
                && base.scheme() == "http"
                && base.host_str() == Some("127.0.0.1")))
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(io::Error::other("Invalid sign-in service address"));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(|error| io::Error::other(error.to_string()))?;
        let id = hex::encode(secret);
        let mut url = base.join("start").map_err(io::Error::other)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("provider", "steam");
            if let Some(version) = version {
                query.append_pair("v", version);
            }
            query.append_pair("browser_port", &listener.local_addr()?.port().to_string());
            query.append_pair("browser_state", &id);
        }
        let (sender, cancellation) = watch::channel(false);
        Ok((
            Self {
                listener,
                secret,
                origin: base.origin().ascii_serialization(),
                cancellation,
            },
            Control {
                url: url.into(),
                id,
                cancellation: sender,
            },
        ))
    }

    /// Accepts one matching gate result and closes the listener on every terminal outcome.
    pub async fn wait(mut self, timeout: Duration) -> Outcome {
        if *self.cancellation.borrow() {
            return Outcome::Cancelled;
        }
        let host = match self.listener.local_addr() {
            Ok(address) => address.to_string(),
            Err(_) => return Outcome::Unavailable,
        };
        let listener = match tokio::net::TcpListener::from_std(self.listener) {
            Ok(listener) => listener,
            Err(_) => return Outcome::Unavailable,
        };
        let (complete, mut received) = oneshot::channel();
        let state = Arc::new(CallbackState {
            secret: self.secret,
            origin: self.origin,
            host,
            completion: Mutex::new(Some(complete)),
        });
        let router = Router::new()
            .route("/steam-signin", post(receive))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .with_state(state);
        let (shutdown, stopped) = oneshot::channel();
        let mut server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        let outcome = tokio::select! {
            result = &mut received => match result {
                Ok(result) => Outcome::Completed(result),
                Err(_) => Outcome::Unavailable,
            },
            _ = self.cancellation.changed() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            _ = &mut server => return Outcome::Unavailable,
        };
        let _ = shutdown.send(());
        if tokio::time::timeout(Duration::from_secs(1), &mut server)
            .await
            .is_err()
        {
            server.abort();
            let _ = server.await;
        }
        outcome
    }
}

impl Control {
    /// Detects gate support before opening a browser that an older service cannot return from.
    pub fn is_supported(&self) -> bool {
        ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(10))
            .build()
            .head(&self.url)
            .call()
            .is_ok_and(|response| {
                response.status() == 302
                    && response.header("rebellion2-browser-signin") == Some("1")
            })
    }

    /// Reports whether the launcher has already left this attempt.
    pub fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow()
    }

    /// Invalidates this attempt even after its callback or timeout has finished.
    pub fn cancel(&self) {
        self.cancellation.send_replace(true);
    }
}

/// Matches only Steam's start action on the configured gate origin and path.
pub fn is_steam_start(url: &Url, base: &str) -> bool {
    Url::parse(base)
        .and_then(|base| base.join("start"))
        .is_ok_and(|expected| {
            url.origin() == expected.origin()
                && url.path() == expected.path()
                && url
                    .query_pairs()
                    .filter(|(key, _)| key == "provider")
                    .map(|(_, value)| value.into_owned())
                    .collect::<Vec<_>>()
                    == ["steam"]
        })
}

/// Validates the origin, request host, attempt secret, and result before consuming the callback.
async fn receive(
    State(state): State<Arc<CallbackState>>,
    headers: HeaderMap,
    Form(result): Form<Completion>,
) -> Response {
    if headers.get_all(header::ORIGIN).iter().count() != 1
        || headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            != Some(&state.origin)
        || headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            != Some(&state.host)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut actual = [0; 32];
    if hex::decode_to_slice(&result.state, &mut actual).is_err()
        || !bool::from(actual.ct_eq(&state.secret))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !["0", "1"].contains(&result.ok.as_str())
        || (result.ok == "1"
            && (result.token.as_deref().is_none_or(str::is_empty)
                || result.url.as_deref().is_none_or(str::is_empty)))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(sender) = state.completion.lock().unwrap().take() else {
        return StatusCode::GONE.into_response();
    };
    if sender.send(result).is_err() {
        return StatusCode::GONE.into_response();
    }
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; frame-ancestors 'none'"),
        ],
        Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Rebellion 2</title><h1>Sign-in finished</h1><p>You can close this tab and return to the launcher.</p></html>"),
    ).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_steam_start_requires_the_configured_origin_path_and_provider() {
        let base = "https://gate.example/auth/";
        for (url, expected) in [
            ("https://gate.example/auth/start?provider=steam", true),
            ("https://gate.example/auth/start?provider=gog", false),
            (
                "https://gate.example/auth/start?provider=steam&provider=gog",
                false,
            ),
            (
                "https://gate.example/auth/start/other?provider=steam",
                false,
            ),
            ("https://other.example/auth/start?provider=steam", false),
            ("http://gate.example/auth/start?provider=steam", false),
        ] {
            assert_eq!(is_steam_start(&Url::parse(url).unwrap(), base), expected);
        }
    }

    #[test]
    fn prepare_creates_distinct_attempts_and_preserves_the_content_version() {
        let (_, first) = Attempt::prepare("https://gate.example/", Some("0.1.0")).unwrap();
        let (_, second) = Attempt::prepare("https://gate.example/", None).unwrap();
        let url = Url::parse(&first.url).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "v").unwrap().1,
            "0.1.0"
        );
        assert_ne!(first.id, second.id);
        assert_eq!(first.id.len(), 64);
    }

    #[test]
    fn is_supported_requires_the_gate_capability_on_a_redirect_response() {
        use std::io::{Read, Write};
        for (status, header, expected) in [
            ("302 Found", "rebellion2-browser-signin: 1\r\n", true),
            ("302 Found", "", false),
            ("200 OK", "rebellion2-browser-signin: 1\r\n", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut bytes = [0; 2048];
                let length = socket.read(&mut bytes).unwrap();
                assert!(String::from_utf8_lossy(&bytes[..length]).starts_with("HEAD /start?"));
                write!(socket, "HTTP/1.1 {status}\r\n{header}Location: https://steamcommunity.com/openid/login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            });
            let (_attempt, control) = Attempt::prepare(&base, None).unwrap();
            assert_eq!(control.is_supported(), expected);
            server.join().unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_accepts_the_matching_gate_result() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        let response = submit(&control, "https://gate.example", &control.id, "1", None);
        assert_eq!(response, 200);
        let Outcome::Completed(result) = pending.await.unwrap() else {
            panic!("No completion")
        };
        assert_eq!(result.token.as_deref(), Some("test-token"));
        assert_eq!(
            result.url.as_deref(),
            Some("https://content.example/archive.zip")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_rejects_wrong_origins_and_secrets_without_consuming_the_attempt() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        assert_eq!(
            submit(&control, "https://evil.example", &control.id, "1", None),
            403
        );
        assert_eq!(
            submit(&control, "https://gate.example", &"0".repeat(64), "1", None),
            403
        );
        assert_eq!(submit(&control, "null", &control.id, "1", None), 403);
        assert_eq!(
            submit(&control, "https://gate.example", &control.id, "1", None),
            200
        );
        assert!(matches!(pending.await.unwrap(), Outcome::Completed(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_preserves_ownership_failure_and_privacy_details() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        assert_eq!(
            submit(
                &control,
                "https://gate.example",
                &control.id,
                "0",
                Some("steam_library_unavailable")
            ),
            200
        );
        let Outcome::Completed(result) = pending.await.unwrap() else {
            panic!("No completion")
        };
        assert_eq!(result.ok, "0");
        assert_eq!(result.reason.as_deref(), Some("steam_library_unavailable"));
        assert_eq!(result.profile_visibility.as_deref(), Some("public"));
        assert!(result.token.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_rejects_oversized_and_incomplete_results_without_consuming_the_attempt() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        for (fields, expected) in [
            (
                vec![("state", control.id.clone()), ("ok", "1".to_string())],
                400,
            ),
            (
                vec![
                    ("state", control.id.clone()),
                    ("ok", "1".to_string()),
                    ("token", "x".repeat(20000)),
                ],
                413,
            ),
        ] {
            let borrowed: Vec<_> = fields
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            let result = ureq::post(&callback_url(&control))
                .set("Origin", "https://gate.example")
                .send_form(&borrowed);
            assert!(matches!(result, Err(ureq::Error::Status(status, _)) if status == expected));
        }
        assert_eq!(
            submit(&control, "https://gate.example", &control.id, "1", None),
            200
        );
        assert!(matches!(pending.await.unwrap(), Outcome::Completed(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_closes_after_one_completion_and_rejects_replay() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        assert_eq!(
            submit(&control, "https://gate.example", &control.id, "1", None),
            200
        );
        assert!(matches!(pending.await.unwrap(), Outcome::Completed(_)));
        let replay = ureq::post(&callback_url(&control))
            .set("Origin", "https://gate.example")
            .send_form(&[("state", &control.id), ("ok", "0")]);
        assert!(replay.is_err());
    }

    #[tokio::test]
    async fn wait_cancels_an_active_listener() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        let pending = tokio::spawn(attempt.wait(Duration::from_secs(5)));
        tokio::task::yield_now().await;
        control.cancel();
        assert!(matches!(pending.await.unwrap(), Outcome::Cancelled));
    }

    #[tokio::test]
    async fn wait_cancels_an_attempt_before_its_task_starts() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        control.cancel();
        assert!(matches!(
            attempt.wait(Duration::from_secs(5)).await,
            Outcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn wait_times_out_and_releases_the_listener() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        assert!(matches!(
            attempt.wait(Duration::from_millis(20)).await,
            Outcome::TimedOut
        ));
        let url = Url::parse(&callback_url(&control)).unwrap();
        assert!(std::net::TcpStream::connect(("127.0.0.1", url.port().unwrap())).is_err());
    }

    #[tokio::test]
    async fn cancel_after_steam_timeout_remains_visible() {
        let (attempt, control) = Attempt::prepare("https://gate.example/", None).unwrap();
        assert!(matches!(
            attempt.wait(Duration::from_millis(1)).await,
            Outcome::TimedOut
        ));
        control.cancel();
        assert!(control.is_cancelled());
    }

    fn callback_url(control: &Control) -> String {
        let url = Url::parse(&control.url).unwrap();
        let port = url
            .query_pairs()
            .find(|(key, _)| key == "browser_port")
            .unwrap()
            .1;
        format!("http://127.0.0.1:{port}/steam-signin")
    }

    fn submit(
        control: &Control,
        origin: &str,
        secret: &str,
        ok: &str,
        reason: Option<&str>,
    ) -> u16 {
        let mut fields = vec![("state", secret), ("ok", ok), ("msg", "Test result")];
        if ok == "1" {
            fields.extend([
                ("token", "test-token"),
                ("url", "https://content.example/archive.zip"),
            ]);
        }
        if let Some(reason) = reason {
            fields.extend([("reason", reason), ("profile_visibility", "public")]);
        }
        match ureq::post(&callback_url(control))
            .set("Origin", origin)
            .send_form(&fields)
        {
            Ok(response) => response.status(),
            Err(ureq::Error::Status(status, _)) => status,
            Err(error) => panic!("Callback failed: {error}"),
        }
    }
}
