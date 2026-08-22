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
