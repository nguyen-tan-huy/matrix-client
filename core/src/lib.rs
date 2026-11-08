//! ChoSua's UI-agnostic core: run `matrix::worker::run` on a tokio runtime,
//! feed it `Command`s, render the `Event`s it sends back. Both front ends
//! (the Tauri webview and the native GTK desktop app) are just that loop.

pub mod command;
#[cfg(target_os = "linux")]
pub mod desktop_notify;
pub mod event;
pub mod matrix;
pub mod models;
pub mod platform;
#[cfg(target_os = "linux")]
pub mod sway_glass;

pub use command::Command;
pub use event::Event;
