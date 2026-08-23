// Prevents an extra console window from opening on Windows in release
// builds — the mobile entry point in `lib.rs` doesn't use this binary at
// all, so it's harmless there.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    matrix_tauri_client_lib::run();
}
