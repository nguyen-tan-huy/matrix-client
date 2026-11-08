//! Catches the browser redirect during SSO/OAuth login.
//!
//! Desktop: a minimal loopback HTTP listener (plain tokio + std
//! networking, no Tauri dependency) — the browser redirects to
//! `http://127.0.0.1:<port>/callback?loginToken=...` and this reads it
//! straight off the socket.
//!
//! Mobile: that only works as long as this process stays alive for the
//! whole time the user is away in the browser, which Android in
//! particular does not guarantee (confirmed on a MIUI/HyperOS device —
//! backgrounding this app to let the browser run can get the process
//! killed at any point, silently losing the pending redirect forever).
//! Mobile instead registers this app as the handler for a custom URL
//! scheme (`matrixtauriclient://oauth-callback`, see `tauri.conf.json`'s
//! `plugins.deep-link` config) — the OS delivers that as an Intent/Activity
//! launch that resumes this app regardless of whether the process is
//! still running, the standard mobile OAuth redirect mechanism (RFC 8252).

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use std::sync::OnceLock;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    static LISTENER: OnceLock<Mutex<Option<TcpListener>>> = OnceLock::new();

    pub async fn listen_for_redirect() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind loopback port for OIDC callback");
        let port = listener.local_addr().unwrap().port();

        LISTENER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .await
            .replace(listener);

        format!("http://127.0.0.1:{port}/callback")
    }

    pub async fn wait_for_token() -> anyhow::Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::time::{timeout, Duration};

        let listener_slot = LISTENER.get().expect("listen_for_redirect not called");
        let mut guard = listener_slot.lock().await;
        let listener = guard
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pending OIDC listener"))?;
        drop(guard);

        // Browsers sometimes open extra loopback connections around the real
        // redirect (e.g. an HTTPS-first probe that gets nothing but garbage/TLS
        // bytes back, or a speculative preconnect) before the actual
        // `GET /callback?loginToken=...` request arrives. Bailing out on the
        // first connection that isn't the real one meant the genuine redirect —
        // which showed up moments later on a separate connection — was missed
        // entirely. So loop over connections until one actually carries a
        // loginToken, ignoring/closing the rest, bounded by an overall timeout.
        let overall = timeout(Duration::from_secs(300), async {
            loop {
                let (mut stream, _) = listener.accept().await?;
                let mut buf = [0u8; 4096];
                let n = match timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
                    Ok(Ok(n)) => n,
                    _ => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);

                // Request line looks like: "GET /callback?loginToken=XYZ HTTP/1.1"
                let first_line = request.lines().next().unwrap_or_default();
                let path = first_line.split_whitespace().nth(1).unwrap_or_default();
                let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();

                let token = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("loginToken="))
                    .map(|t| t.to_string());

                let Some(token) = token else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    continue;
                };

                let body = "<html><body>Signed in — you can close this tab.</body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;

                return Ok::<String, anyhow::Error>(token);
            }
        })
        .await;

        overall.map_err(|_| anyhow::anyhow!("timed out waiting for OIDC redirect"))?
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use desktop::{listen_for_redirect, wait_for_token};

#[cfg(any(target_os = "android", target_os = "ios"))]
mod mobile {
    use std::sync::OnceLock;
    use tokio::sync::{oneshot, Mutex};

    /// Holds the sender half while a login is in flight — set by
    /// `wait_for_token`, consumed by `handle_redirect_url` when the deep
    /// link fires. `Mutex` rather than raw `OnceLock<Sender>` since a
    /// second login attempt needs to replace it, not just fail to set it.
    static PENDING: OnceLock<Mutex<Option<oneshot::Sender<String>>>> = OnceLock::new();

    /// Same scheme registered in `tauri.conf.json`'s `plugins.deep-link`
    /// config and (generated from that) the Android manifest's
    /// intent-filter — no host needed, this is a plain custom URI scheme,
    /// not an https App Link, so there's no domain/assetlinks.json to
    /// stand up.
    const REDIRECT_URL: &str = "matrixtauriclient://oauth-callback";

    pub async fn listen_for_redirect() -> String {
        REDIRECT_URL.to_string()
    }

    pub async fn wait_for_token() -> anyhow::Result<String> {
        let (tx, rx) = oneshot::channel();
        PENDING.get_or_init(|| Mutex::new(None)).lock().await.replace(tx);

        tokio::time::timeout(std::time::Duration::from_secs(300), rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for OIDC redirect"))?
            .map_err(|_| anyhow::anyhow!("OIDC redirect channel closed"))
    }

    /// Called from `lib.rs`'s `deep_link().on_open_url(...)` handler
    /// whenever the OS delivers a `matrixtauriclient://...` open — which
    /// happens whether this app was already running (most of the time)
    /// or the OS just cold-started it fresh for this Intent, though in
    /// the latter case there's no in-flight `wait_for_token` future left
    /// to deliver to (that state died with the old process) and the
    /// token is simply dropped; the user just retries from the login
    /// screen, same as any other login failure.
    pub fn handle_redirect_url(url: &str) {
        let Some(query) = url.split_once('?').map(|(_, q)| q) else {
            return;
        };
        let Some(token) = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("loginToken="))
        else {
            return;
        };
        let token = token.to_string();

        if let Some(mutex) = PENDING.get() {
            if let Ok(mut guard) = mutex.try_lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(token);
                }
            }
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub use mobile::{handle_redirect_url, listen_for_redirect, wait_for_token};
