package com.example.matrixtauriclient

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Keeps this app's process alive and off Android's background CPU/network
 * throttling while it's not in the foreground — without this, the matrix
 * sync loop (a long-lived connection in the worker thread, same as the
 * desktop build) gets frozen within seconds to minutes of backgrounding on
 * stock Android, and far more aggressively on MIUI/HyperOS specifically,
 * which is why notifications only ever showed up once the app was
 * reopened instead of arriving as messages actually came in.
 *
 * Android requires an ongoing, user-visible notification for any
 * foreground service — there's no way to hide it, that's the tradeoff for
 * the background network/CPU access this grants. Given `IMPORTANCE_MIN`
 * and no sound/vibration, it sits quietly at the bottom of the
 * notification shade rather than interrupting anything.
 */
class ForegroundSyncService : Service() {
    companion object {
        // A channel's importance can't be changed by re-creating it with
        // the same ID once Android has already seen it — only the user
        // can change that, in system settings, or a fresh ID gets a fresh
        // channel. This one changed from MIN to LOW (see below), so it
        // needs a new ID to actually take effect on a device that already
        // had the old channel from a previous install.
        const val CHANNEL_ID = "matrix_sync_channel_v2"
        const val NOTIFICATION_ID = 1
    }

    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            // `IMPORTANCE_MIN` hides the icon from the status bar entirely
            // (it only shows up if you pull the notification shade down)
            // — `IMPORTANCE_LOW` is the lowest level that still keeps a
            // persistent status-bar icon, like the always-visible Wi-Fi/
            // battery ones, while still making no sound and not popping
            // up/interrupting anything (that only starts at `DEFAULT`).
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Matrix sync",
                NotificationManager.IMPORTANCE_LOW
            )
            channel.description = "Keeps Matrix connected so messages and notifications arrive instantly"
            channel.setShowBadge(false)
            getSystemService(NotificationManager::class.java)?.createNotificationChannel(channel)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("Matrix")
            .setContentText("Connected — syncing messages")
            // Not `R.mipmap.ic_launcher` — Android tints a status-bar small
            // icon down to just its alpha channel (ignoring color info
            // entirely), so a full-color launcher icon there either shows
            // up as a solid, illegible blob or gets silently swapped for a
            // generic system icon on some OEMs. `ic_notification` is a
            // proper white-silhouette-on-transparent asset generated
            // specifically for this from the same app icon.
            .setSmallIcon(R.drawable.ic_notification)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    // Deliberately *not* overriding `onTaskRemoved()` to call `stopSelf()`
    // — leaving it as a no-op is what makes this service (and the process
    // hosting the matrix sync worker) survive the user swiping the app
    // away in Recents, the same way a music player's playback service
    // keeps running after being swiped away. That's the stock-Android
    // contract for a plain `startForegroundService`-launched service like
    // this one.
    //
    // It doesn't help against MIUI/HyperOS's own separate cleanup pass,
    // though — confirmed on a real device (logged literally as
    // `ActivityManager: Killing ... SwipeUpClean`), MIUI kills this
    // process on swipe regardless of the foreground service, unless the
    // app is explicitly exempted from its battery restrictions. No code
    // on our side can prevent that kill or usefully recover from it: even
    // scheduling a restart would only relaunch this bare Kotlin service,
    // not the Rust sync worker (which only ever starts from
    // `MainActivity`) — a real fix needs either the user disabling
    // MIUI's restrictions for this app (Settings → Battery & performance
    // → App battery saver → Matrix → No restrictions), or push
    // notifications (FCM), which — unlike this — really does survive a
    // full kill, at the cost of needing a push gateway configured on the
    // homeserver side.
}
