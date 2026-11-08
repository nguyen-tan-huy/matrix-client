use matrix_sdk::encryption::verification::{Emoji, SasVerification};
use matrix_sdk::Client;
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex as AsyncMutex;

use crate::event::Event;

/// The SAS verification currently in flight, if any. `confirm_verification`
/// and `cancel_verification` act on whatever's here — this app only ever
/// drives one verification flow at a time.
static CURRENT_SAS: OnceLock<AsyncMutex<Option<SasVerification>>> = OnceLock::new();

fn sas_store() -> &'static AsyncMutex<Option<SasVerification>> {
    CURRENT_SAS.get_or_init(|| AsyncMutex::new(None))
}

/// NOTE: same caveat as the Tauri version — the verification API surface
/// (`SasVerification`, `SasState`, `get_verification_sas`) is one of the
/// more volatile corners of matrix-rust-sdk across versions. This mirrors
/// the SDK's own `emoji_verification` example; check that example for the
/// version you're building against if names have moved.

/// Registers the handler that reacts when ANOTHER of your devices starts a
/// verification request against this one. Call once, right after login.
pub fn register_verification_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent,
              client: Client| {
            let tx = tx.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                else {
                    return;
                };

                if request.accept().await.is_err() {
                    return;
                }

                let Ok(Some(sas)) = request.start_sas().await else {
                    return;
                };

                watch_sas(sas, tx).await;
            }
        },
    );
}

/// Starts a *self*-verification request from this device, to be accepted on
/// one of your other already-signed-in devices.
pub async fn start_self_verification(
    client: &Client,
    tx: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let user_identity = client
        .encryption()
        .get_user_identity(
            client
                .user_id()
                .ok_or_else(|| anyhow::anyhow!("no user id"))?,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("no cross-signing identity yet — verify from another device first")
        })?;

    let request = user_identity.request_verification().await?;

    let sas = loop {
        if let Some(sas) = request.start_sas().await? {
            break sas;
        }
    };

    watch_sas(sas, tx).await;
    Ok(())
}

/// Drives one SAS verification session to completion: waits for both sides
/// to reach the "show emoji" state, sends the emoji list to the UI thread,
/// and waits for the user to confirm/cancel via the functions below.
async fn watch_sas(sas: SasVerification, tx: UnboundedSender<Event>) {
    use futures_util::StreamExt;

    if sas.accept().await.is_err() {
        return;
    }

    *sas_store().lock().await = Some(sas.clone());

    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            matrix_sdk::encryption::verification::SasState::KeysExchanged { emojis, .. } => {
                if let Some(emojis) = emojis {
                    let labels: Vec<(String, String)> = emojis
                        .emojis
                        .iter()
                        .map(
                            |Emoji {
                                 symbol,
                                 description,
                             }| {
                                (symbol.to_string(), description.to_string())
                            },
                        )
                        .collect();
                    tx.send(Event::VerificationEmojis(labels)).ok();
                }
            }
            matrix_sdk::encryption::verification::SasState::Done { .. } => {
                *sas_store().lock().await = None;
                tx.send(Event::VerificationDone).ok();
                break;
            }
            matrix_sdk::encryption::verification::SasState::Cancelled(info) => {
                *sas_store().lock().await = None;
                tx.send(Event::VerificationCancelled(info.reason().to_string()))
                    .ok();
                break;
            }
            _ => {}
        }
    }
}

pub async fn confirm_verification(_client: &Client) -> anyhow::Result<()> {
    if let Some(sas) = sas_store().lock().await.clone() {
        sas.confirm().await?;
    }
    Ok(())
}

pub async fn cancel_verification(_client: &Client) -> anyhow::Result<()> {
    if let Some(sas) = sas_store().lock().await.take() {
        let _ = sas.cancel().await;
    }
    Ok(())
}

/// Restores the room-key backup using the recovery key from Secure Backup
/// (Element: Settings → Security & Privacy). Unlocks decrypting message
/// *history* from before this device existed — SAS verification alone only
/// covers live/future messages.
pub async fn recover_with_key(client: &Client, recovery_key: &str) -> anyhow::Result<()> {
    client.encryption().recovery().recover(recovery_key).await?;
    Ok(())
}

/// A new security (recovery) key. With no secret storage yet this turns it (and the key
/// backup) on; otherwise the existing storage's key is rotated and the secrets this device holds
/// are re-uploaded under it, so the key the user lost stops mattering.
pub async fn create_recovery_key(client: &Client) -> Result<String, CreateKeyError> {
    use matrix_sdk::encryption::recovery::RecoveryState;
    let recovery = client.encryption().recovery();
    if recovery.state() != RecoveryState::Disabled {
        return Ok(recovery.reset_key().await.map_err(anyhow::Error::from)?);
    }
    match recovery.enable().await {
        Ok(key) => Ok(key),
        // a backup this device can't open (its key is lost) is in the way
        Err(e) if e.to_string().contains("backup already exists") => Err(CreateKeyError::BackupExists),
        Err(e) => Err(anyhow::Error::from(e).into()),
    }
}

/// Starts over: the old backup and secret storage go, the cross-signing identity is made anew
/// (this device signs itself, so it is verified again) and a new security key is returned.
/// `status` is told when the homeserver wants the user to approve the reset in a browser.
pub async fn reset_identity(client: &Client, password: &str, status: impl Fn(String)) -> anyhow::Result<String> {
    use matrix_sdk::encryption::CrossSigningResetAuthType;
    use matrix_sdk::ruma::api::client::uiaa::{AuthData, MatrixUserIdentifier, Password, UserIdentifier};
    let recovery = client.encryption().recovery();
    if let Some(handle) = recovery.reset_identity().await? {
        match handle.auth_type() {
            CrossSigningResetAuthType::Uiaa(info) => {
                use matrix_sdk::ruma::api::client::uiaa::{AuthType, FallbackAcknowledgement};
                let session = info.session.clone().ok_or_else(|| anyhow::anyhow!("máy chủ không gửi phiên xác thực"))?;
                let offers = |t: AuthType| info.flows.iter().any(|f| f.stages.contains(&t));
                if offers(AuthType::Password) && !password.is_empty() {
                    let user = client.user_id().ok_or_else(|| anyhow::anyhow!("not logged in"))?.to_string();
                    let mut auth = Password::new(UserIdentifier::Matrix(MatrixUserIdentifier::new(user)), password.to_string());
                    auth.session = Some(session);
                    handle.reset(Some(AuthData::Password(auth))).await?;
                } else if offers(AuthType::Sso) {
                    // SSO/OAuth account, no password: approve in the browser (the homeserver's fallback page),
                    // then the reset is retried until the homeserver accepts it
                    let url = format!(
                        "{}_matrix/client/v3/auth/m.login.sso/fallback/web?session={session}",
                        client.homeserver().as_str().trim_end_matches('/').to_string() + "/"
                    );
                    if crate::platform::open_url(&url).is_ok() {
                        status("đã mở trình duyệt: hãy đăng nhập/duyệt ở đó, app sẽ tự tiếp tục".to_string());
                    } else {
                        status(format!("mở liên kết này để duyệt việc đặt lại: {url}"));
                    }
                    let ack = AuthData::FallbackAcknowledgement(FallbackAcknowledgement::new(session));
                    let mut last = None;
                    for _ in 0..90 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        match handle.reset(Some(ack.clone())).await {
                            Ok(()) => {
                                last = None;
                                break;
                            }
                            Err(e) => last = Some(e),
                        }
                    }
                    if let Some(e) = last {
                        return Err(anyhow::anyhow!("chưa nhận được xác nhận từ trình duyệt: {e}"));
                    }
                } else {
                    let kinds: Vec<String> = info.flows.iter().map(|f| f.stages.iter().map(|s| s.as_ref().to_string()).collect::<Vec<_>>().join("+")).collect();
                    return Err(anyhow::anyhow!("máy chủ yêu cầu cách xác thực chưa hỗ trợ ({}); nếu tài khoản có mật khẩu hãy nhập mật khẩu", kinds.join(" | ")));
                }
            }
            CrossSigningResetAuthType::OAuth(info) => {
                // OAuth/SSO accounts have no password: the homeserver wants the reset approved in a browser
                let url = info.approval_url.to_string();
                if crate::platform::open_url(&url).is_ok() {
                    status("đã mở trình duyệt: hãy duyệt việc đặt lại ở đó, app sẽ tự tiếp tục".to_string());
                } else {
                    status(format!("mở liên kết này để duyệt việc đặt lại: {url}"));
                }
                handle.reset(None).await?;
            }
        }
    }
    Ok(match recovery.state() {
        matrix_sdk::encryption::recovery::RecoveryState::Disabled => recovery.enable().await?,
        _ => recovery.reset_key().await?,
    })
}

pub enum CreateKeyError {
    /// A key backup exists on the server that this device can't open.
    BackupExists,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CreateKeyError {
    fn from(e: anyhow::Error) -> Self {
        CreateKeyError::Other(e)
    }
}

/// Imports E2EE room keys from an Element key-export file (Settings →
/// Security & Privacy → Export keys). `import_room_keys` only takes a
/// filesystem path, so the bytes the frontend sent over the command
/// channel are written to a throwaway temp file first, then cleaned up
/// regardless of whether the import itself succeeds.
pub async fn import_room_keys(
    client: &Client,
    bytes: &[u8],
    passphrase: &str,
) -> anyhow::Result<(usize, usize)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("matrix-key-import-{nanos}.txt"));
    tokio::fs::write(&path, bytes).await?;

    let result = client.encryption().import_room_keys(path.clone(), passphrase).await;
    let _ = tokio::fs::remove_file(&path).await;

    let result = result?;
    Ok((result.imported_count, result.total_count))
}

/// Every room key this device holds, as an encrypted Element-compatible export file.
pub async fn export_room_keys(client: &Client, passphrase: &str) -> anyhow::Result<Vec<u8>> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("matrix-key-export-{nanos}.txt"));

    let result = client.encryption().export_room_keys(path.clone(), passphrase, |_| true).await;
    let bytes = match result {
        Ok(()) => tokio::fs::read(&path).await.map_err(anyhow::Error::from),
        Err(e) => Err(e.into()),
    };
    let _ = tokio::fs::remove_file(&path).await;
    bytes
}
