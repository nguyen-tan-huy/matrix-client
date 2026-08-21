//! Minimal loopback HTTP listener used to catch the browser redirect during
//! SSO/OAuth login. Identical to the Tauri version's oidc_callback.rs — this
//! part never depended on Tauri, it's plain tokio + std networking.

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
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
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
