package com.example.matrixtauriclient

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat
import org.json.JSONObject

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    // Cold start (app was fully killed) from a notification tap: rewrite
    // the launch intent into the equivalent
    // `matrixtauriclient://notification?...` deep link *before*
    // `super.onCreate()` — that call is what boots the WebView and, as
    // part of it, the deep-link plugin's own `load()`, which is what
    // actually reads `activity.intent` for the cold-start catch
    // (`get_current()` on the Rust side, see `lib.rs`'s comment on it).
    // The original approach — leaving this rewrite to `handleNotificationTap`
    // below, called *after* `super.onCreate()` — fired a second, separate
    // `startActivity()` call that raced the deep-link plugin's own
    // asynchronous Rust-side setup: confirmed the hard way against a real
    // device, tapping a notification while the app was fully killed
    // reliably opened the app but never actually routed to the room. Doing
    // the rewrite first means `super.onCreate()`'s own WebView boot sees
    // the deep link as if the app had simply been *launched* with it —
    // no second intent, no race to lose.
    rewriteNotificationLaunchIntent()
    super.onCreate(savedInstanceState)
    // See ForegroundSyncService's doc comment — this is what keeps the
    // matrix sync loop (and therefore notifications) running in real time
    // while the app isn't in the foreground.
    ContextCompat.startForegroundService(this, Intent(this, ForegroundSyncService::class.java))
    requestBatteryOptimizationExemption()
    handleNotificationTap(intent)
  }

  /** See `onCreate`'s comment for why this exists as a separate,
   * pre-`super.onCreate()` step rather than just relying on
   * `handleNotificationTap`. Only ever changes anything when this
   * activity's launch intent actually came from tapping a notification
   * (i.e. it carries tauri-plugin-notification's own "LocalNotficationObject"
   * extra) — a no-op otherwise, including the deep-link-triggered launch
   * case (OAuth redirect, or the notification path itself on a *warm*
   * app), which already arrives as the real deep link with nothing to
   * rewrite. */
  private fun rewriteNotificationLaunchIntent() {
    val uri = notificationTapDeepLinkUri(intent) ?: return
    intent = Intent(Intent.ACTION_VIEW, uri).setPackage(packageName)
  }

  /**
   * Without this exemption, stock Android's Doze/App Standby can still
   * throttle or freeze the sync worker's network access in the background
   * even with the foreground service running. This shows the system's own
   * "allow unrestricted background activity" prompt — the user has to
   * accept it, it can't be granted silently.
   *
   * Note this only covers *standard* Android battery management. MIUI/
   * HyperOS (and other OEM skins) layer their own separate restrictions on
   * top — e.g. MIUI's per-app "No restrictions" toggle under Battery &
   * performance, and Autostart — that this call does not touch and no API
   * exists to request programmatically. See ForegroundSyncService's doc
   * comment for why.
   */
  private fun requestBatteryOptimizationExemption() {
    val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
    if (!powerManager.isIgnoringBatteryOptimizations(packageName)) {
      try {
        startActivity(
          Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:$packageName")
          )
        )
      } catch (e: Exception) {
        // Some OEM builds (again, MIUI included) don't implement this
        // system dialog at all and throw ActivityNotFoundException —
        // nothing to fall back to besides the manual settings path.
      }
    }
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    handleNotificationTap(intent)
  }

  /**
   * Tapping a notification (built in `platform.rs`'s `show_notification`,
   * with `room_id`/`thread_id` attached as its `extra` payload) launches
   * this activity with tauri-plugin-notification's own extras attached —
   * one of them ("LocalNotficationObject", the plugin's own key, typo and
   * all) is the notification's full JSON, including that `extra` data.
   *
   * Rather than build a second native-to-Rust bridge from scratch to
   * deliver it, this re-fires it as this app's own
   * `matrixtauriclient://notification?...` deep link — Android redispatches
   * that straight back into this same activity via the deep-link plugin's
   * intent-filter, landing on the exact same `on_open_url` Rust handler
   * (`lib.rs`) already proven reliable for the OAuth redirect. Only reached
   * for the *warm* case now (app already running, tapped from
   * `onNewIntent`) — a cold start's launch intent is rewritten by
   * `rewriteNotificationLaunchIntent` instead (see `onCreate`'s comment),
   * so by the time this runs there `intent` no longer carries the
   * notification extra at all, and this is a harmless no-op.
   */
  private fun handleNotificationTap(intent: Intent?) {
    val uri = notificationTapDeepLinkUri(intent) ?: return
    startActivity(Intent(Intent.ACTION_VIEW, uri).setPackage(packageName))
  }

  /** Shared by `rewriteNotificationLaunchIntent` (cold start) and
   * `handleNotificationTap` (warm) — parses tauri-plugin-notification's
   * "LocalNotficationObject" extra (see `handleNotificationTap`'s comment)
   * into the `matrixtauriclient://notification?room_id=...&thread_id=...`
   * URI both paths route through. `null` for any intent that isn't a
   * notification tap at all, or one whose payload doesn't parse. */
  private fun notificationTapDeepLinkUri(intent: Intent?): Uri? {
    val json = intent?.getStringExtra("LocalNotficationObject") ?: return null
    return try {
      val extra = JSONObject(json).optJSONObject("extra") ?: return null
      val roomId = extra.optString("room_id", "")
      if (roomId.isEmpty()) return null
      val threadId = extra.optString("thread_id", "")

      val uriBuilder = Uri.parse("matrixtauriclient://notification").buildUpon()
        .appendQueryParameter("room_id", roomId)
      if (threadId.isNotEmpty()) {
        uriBuilder.appendQueryParameter("thread_id", threadId)
      }
      uriBuilder.build()
    } catch (e: Exception) {
      // Malformed/unexpected notification payload — nothing to route to.
      null
    }
  }
}
