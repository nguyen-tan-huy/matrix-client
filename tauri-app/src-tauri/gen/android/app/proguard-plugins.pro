# Hand-written — upstream `tauri-plugin-opener` and `tauri-plugin-notification`
# (2.5.4 / 2.3.3) ship no `android/consumer-rules.pro` of their own (confirmed:
# Gradle warns "Supplied consumer proguard configuration does not exist" for
# both, every release build). Their Kotlin plugin classes are only ever
# reached via Tauri's reflection-based plugin loader, never referenced
# directly from any other Java/Kotlin code — so R8 has no static reference to
# keep them alive, and strips/renames them in a minified (release) build.
# That's why SSO/OAuth login (routes through `OpenerPlugin.open`) and
# notifications (`NotificationPlugin`) silently do nothing in release while
# working fine in an unminified debug build.
-keep class app.tauri.opener.** { *; }
-keep class app.tauri.notification.** { *; }
