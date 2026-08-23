package com.example.matrixtauriclient

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat
import org.json.JSONObject

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // See ForegroundSyncService's doc comment — this is what keeps the
    // matrix sync loop (and therefore notifications) running in real time
    // while the app isn't in the foreground.
    ContextCompat.startForegroundService(this, Intent(this, ForegroundSyncService::class.java))
    handleNotificationTap(intent)
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
   * deliver it, this just re-fires it as this app's own
   * `matrixtauriclient://notification?...` deep link — Android redispatches
   * that straight back into this same activity via the deep-link plugin's
   * intent-filter, landing on the exact same `on_open_url` Rust handler
   * (`lib.rs`) already proven reliable for the OAuth redirect.
   */
  private fun handleNotificationTap(intent: Intent?) {
    val json = intent?.getStringExtra("LocalNotficationObject") ?: return
    try {
      val extra = JSONObject(json).optJSONObject("extra") ?: return
      val roomId = extra.optString("room_id", "")
      if (roomId.isEmpty()) return
      val threadId = extra.optString("thread_id", "")

      val uriBuilder = Uri.parse("matrixtauriclient://notification").buildUpon()
        .appendQueryParameter("room_id", roomId)
      if (threadId.isNotEmpty()) {
        uriBuilder.appendQueryParameter("thread_id", threadId)
      }
      startActivity(Intent(Intent.ACTION_VIEW, uriBuilder.build()).setPackage(packageName))
    } catch (e: Exception) {
      // Malformed/unexpected notification payload — nothing to route to.
    }
  }
}
