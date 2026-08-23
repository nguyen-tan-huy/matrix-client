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
        const val CHANNEL_ID = "matrix_sync_channel"
        const val NOTIFICATION_ID = 1
    }

    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Matrix sync",
                NotificationManager.IMPORTANCE_MIN
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
            .setSmallIcon(R.mipmap.ic_launcher)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_MIN)
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
