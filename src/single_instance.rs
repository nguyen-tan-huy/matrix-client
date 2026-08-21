//! Since closing the window now hides it instead of exiting (see main.rs's
//! close-request handling), launching the app a second time — e.g. from a
//! launcher keybinding — would otherwise spawn a second process with its
//! own tray icon. This uses a plain flock on a file in `$XDG_RUNTIME_DIR`
//! to make the second launch a no-op instead.
//!
//! The lock file also carries the running instance's PID, so `--quit` (see
//! `try_quit_running_instance`) can find and stop it — e.g. after
//! installing a rebuilt binary, without having to hunt down the PID by
//! hand or rely on the tray icon still being reachable.

use fs4::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

fn lock_path() -> std::path::PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("matrix-egui-client.lock")
}

/// Call once at the very top of `main()`. Returns a `File` you must keep
/// bound to a variable for the lifetime of the process (e.g.
/// `let _lock = single_instance::acquire_or_exit();`) — the lock is held
/// as long as the file descriptor stays open, and released automatically
/// on drop or process exit.
///
/// If another instance already holds the lock, prints a message and exits
/// immediately (status 0) rather than opening a second window/tray icon.
pub fn acquire_or_exit() -> File {
    let path = lock_path();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("failed to open single-instance lock file");

    if file.try_lock_exclusive().is_err() {
        eprintln!(
            "matrix-egui-client is already running — check the tray icon, or run with --quit to stop it."
        );
        std::process::exit(0);
    }

    // Best-effort: a stale PID left over from a previous run isn't a
    // correctness problem, just makes `--quit` a no-op via `kill` failing
    // on a PID that no longer exists.
    let _ = file.set_len(0);
    let _ = file.write_all(std::process::id().to_string().as_bytes());
    let _ = file.flush();

    file
}

/// Handles `--quit`: reads the running instance's PID from the lock file
/// and sends it `SIGTERM`, then exits. Returns without doing anything if
/// no instance appears to be running (nothing holds the lock).
pub fn try_quit_running_instance() {
    let path = lock_path();
    let Ok(mut file) = OpenOptions::new().read(true).open(&path) else {
        eprintln!("matrix-egui-client is not running.");
        std::process::exit(0);
    };

    // If we can take the lock ourselves, nothing else is holding it.
    if file.try_lock_exclusive().is_ok() {
        eprintln!("matrix-egui-client is not running.");
        std::process::exit(0);
    }

    let mut pid_str = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.read_to_string(&mut pid_str);
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        eprintln!("couldn't read the running instance's PID from the lock file.");
        std::process::exit(1);
    };

    match std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
    {
        Ok(status) if status.success() => {
            eprintln!("sent quit signal to matrix-egui-client (pid {pid}).");
            std::process::exit(0);
        }
        _ => {
            eprintln!("failed to signal pid {pid} — it may have already exited.");
            std::process::exit(1);
        }
    }
}
