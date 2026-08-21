/// Desktop notifications via the freedesktop.org Notifications D-Bus spec.
/// Works with mako (sway), dunst, or any other compliant daemon — nothing
/// mako-specific here, `notify-rust` just talks the standard protocol.
///
/// Sends happen on a spawned OS thread since the D-Bus round-trip can block
/// briefly and we don't want to stall the egui frame.
pub fn notify(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let result = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .appname("Matrix")
            .timeout(notify_rust::Timeout::Milliseconds(6000))
            .show();

        if let Err(err) = result {
            eprintln!("desktop notification failed: {err}");
        }
    });
}
