package com.example.matrixtauriclient

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import org.json.JSONObject
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage
import java.io.File

/**
 * The other half of what `ForegroundSyncService` couldn't do on its own:
 * that service only keeps this app's *own* process alive for as long as
 * Android (and, on MIUI/HyperOS, the OEM's own separate cleanup pass) lets
 * it — once the process is actually killed, there's nothing left running to
 * receive anything, foreground service or not. Real push doesn't have that
 * problem: the *homeserver* wakes this app up from the outside, through
 * Android's own push plumbing, the same way FCM does for Element — except
 * this uses UnifiedPush instead of Firebase, so it works with any
 * self-hosted distributor (e.g. the `ntfy` Android app) and needs no
 * Google Play Services or Firebase project of our own.
 *
 * The full round trip:
 *  1. [onNewEndpoint] gets an endpoint URL from whichever UnifiedPush
 *     distributor the user has installed — that URL already encodes a
 *     private "topic" unique to this install.
 *  2. It's written to a plain file `lib.rs`'s `.setup()` reads back on
 *     every app launch and turns into `Command::SetPushEndpoint` — see that
 *     command's own doc comment for why a file handoff instead of the
 *     `matrixtauriclient://...` deep link the rest of this app's native↔
 *     Rust bridge otherwise uses everywhere else (this runs from a bare
 *     background `Service` with no `MainActivity`/WebView necessarily
 *     alive to receive one).
 *  3. `Command::SetPushEndpoint`'s handler registers a Matrix `http` pusher
 *     whose gateway URL is that same distributor's own `/_matrix/push/v1/
 *     notify` endpoint (ntfy, and any other UnifiedPush-compatible push
 *     server, doubles as a Matrix Push Gateway at that fixed path) — so
 *     from the homeserver's perspective this is just an ordinary pusher,
 *     no different from Element's FCM one.
 *  4. When a message arrives, Synapse POSTs to that gateway, which
 *     forwards it through UnifiedPush to the distributor, which delivers
 *     it to [onMessage] here — starting this app's process fresh if it had
 *     been fully killed, exactly the wake-up `ForegroundSyncService` alone
 *     could never provide.
 *
 * [onMessage] deliberately does *not* try to decrypt/sync/show real
 * message content itself — this app's whole matrix-sdk client (session,
 * crypto, timeline cache) only exists once `MainActivity`'s WebView has
 * booted Tauri's Rust core (see `platform.rs`), which a bare `Service`
 * dispatched from a killed process has no way to do safely or quickly.
 * Registering the pusher with `format: event_id_only` (see the Rust
 * handler) already keeps real content out of this payload anyway, same
 * privacy tradeoff Element itself makes for UnifiedPush. Showing a plain
 * "new message" notification that launches the app on tap — which then
 * does a real sync and shows the *actual* notifications with content, same
 * as `ForegroundSyncService`'s live path already does — is the honest
 * version of "notified even when fully killed" this can deliver without
 * that larger rework.
 */
class UnifiedPushServiceImpl : PushService() {
    companion object {
        const val CHANNEL_ID = "matrix_push_channel"
        const val ENDPOINT_FILE = "unifiedpush_endpoint.txt"

        /** Kicks off distributor selection + registration — call once
         * from `MainActivity.onCreate`. A no-op if already registered
         * (the library tracks that itself); harmless to call on every
         * launch. */
        fun register(context: Context) {
            UnifiedPush.tryUseCurrentOrDefaultDistributor(context) { usable ->
                if (usable) {
                    UnifiedPush.register(context, messageForDistributor = "Matrix notifications")
                }
            }
        }
    }

    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        // `dataDir`, not `filesDir` — has to match `platform::data_dir()`
        // on the Rust side exactly (see `Command::SetPushEndpoint`'s doc
        // comment), which resolves to `Context.getDataDir()`, the app's
        // root data directory, not its `files/` subdirectory.
        try {
            File(applicationContext.dataDir, ENDPOINT_FILE).writeText(endpoint.url)
        } catch (e: Exception) {
            // Nothing useful to do about a write failure here — the next
            // `onNewEndpoint` (or app launch re-registering) gets another
            // chance regardless.
        }
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        // No VAPID handling — unlike the example app, this one has no UI
        // running (or even a process alive) most of the time to react to
        // this from, and ntfy's own Matrix gateway doesn't require it.
        // Nothing to clean up: no endpoint was ever written for this
        // registration attempt, so the previous one (if any) simply stays
        // in effect until the next successful registration replaces it.
    }

    override fun onUnregistered(instance: String) {
        try {
            File(applicationContext.dataDir, ENDPOINT_FILE).delete()
        } catch (e: Exception) {
            // Not fatal — an unregistered endpoint the homeserver still
            // has on file just means we stop actually receiving pushes to
            // it, not an error state as far as this app is concerned.
        }
    }

    override fun onMessage(message: PushMessage, instance: String) {
        showWakeUpNotification(extractRoomId(message))
        // Give the sync worker a real chance to catch up and show the
        // actual per-message notification(s) with content — same
        // mechanism `MainActivity.onCreate` already starts on every
        // normal launch, just triggered here without an Activity. If the
        // process was already alive (app merely backgrounded, not fully
        // killed) this is a harmless no-op restart of an already-running
        // service.
        try {
            androidx.core.content.ContextCompat.startForegroundService(
                applicationContext,
                Intent(applicationContext, ForegroundSyncService::class.java)
            )
        } catch (e: Exception) {
            // Best-effort — the plain notification above already told the
            // user something arrived even if this fails (e.g. background
            // start restrictions on some OEM skins).
        }
    }

    /** A generic "something happened" notification — deliberately not
     * routed through `platform.rs`'s `show_notification` (that needs
     * Tauri's `AppHandle`, which doesn't exist yet from a cold, Rust-less
     * process) and deliberately generic rather than guessing at real
     * content (see this class's own doc comment for why). `room_id` is
     * plaintext metadata even in an encrypted room (routing info, not
     * message content), so when it's present the tap can still go
     * straight to the right room via the same `matrixtauriclient://
     * notification?room_id=...` deep link `MainActivity`'s
     * `handleNotificationTap`/`rewriteNotificationLaunchIntent` already
     * handle for `platform.rs`'s own notifications — this just reuses
     * that existing path instead of inventing a second one. */
    private fun showWakeUpNotification(roomId: String?) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Matrix messages",
                NotificationManager.IMPORTANCE_HIGH
            )
            channel.description = "New messages arriving while the app isn't running"
            getSystemService(NotificationManager::class.java)?.createNotificationChannel(channel)
        }

        val launchIntent = if (roomId != null) {
            Intent(
                Intent.ACTION_VIEW,
                Uri.parse("matrixtauriclient://notification").buildUpon()
                    .appendQueryParameter("room_id", roomId)
                    .build()
            ).setPackage(applicationContext.packageName)
        } else {
            Intent(applicationContext, MainActivity::class.java)
        }
        val pendingIntent = PendingIntent.getActivity(
            applicationContext,
            0,
            launchIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val notification = NotificationCompat.Builder(applicationContext, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText("New message")
            .setSmallIcon(R.drawable.ic_notification)
            .setLargeIcon(
                android.graphics.BitmapFactory.decodeResource(resources, R.drawable.ic_notification_large)
            )
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setAutoCancel(true)
            .setContentIntent(pendingIntent)
            .build()

        NotificationManagerCompat.from(applicationContext).notify(
            System.currentTimeMillis().toInt(),
            notification
        )
    }

    /** Best-effort peek at `room_id` from the push payload — present with
     * the `event_id_only` pusher format this app registers (see
     * `Command::SetPushEndpoint`'s Rust handler), `null` if the payload
     * doesn't parse as expected (a different/older gateway, say). */
    private fun extractRoomId(message: PushMessage): String? {
        val content = message.content
        return try {
            val json = JSONObject(String(content, Charsets.UTF_8))
            json.optJSONObject("notification")?.optString("room_id")?.takeIf { it.isNotEmpty() }
        } catch (e: Exception) {
            null
        }
    }
}
