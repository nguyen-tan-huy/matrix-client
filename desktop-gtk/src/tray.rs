//! Tray icon (StatusNotifierItem via `ksni`) and the restart-on-resume
//! watch — both on one background thread with its own small tokio runtime,
//! talking to the GTK side through a channel.
//!
//! Restart on resume: after suspend the sync loop can sit on a dead pooled
//! connection for minutes (see tauri-app's suspend_watch.rs); relaunching is
//! the reliable cure, and the sqlite store makes it cheap.

use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, Status, Tray, TrayMethods};
use tokio::sync::mpsc::UnboundedSender;
use zbus::{proxy, Connection};

#[derive(Debug)]
pub enum TrayAction {
    Toggle,
    Resumed,
}

pub fn restart() -> ! {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(err) = std::process::Command::new(exe).spawn() {
                tracing::error!(error = %err, "restart: relaunch failed, quitting anyway");
            }
        }
        Err(err) => tracing::error!(error = %err, "restart: no current_exe, quitting anyway"),
    }
    std::process::exit(0);
}

pub fn spawn(tx: UnboundedSender<TrayAction>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tray runtime");
        rt.block_on(async move {
            let resume_tx = tx.clone();
            match (ChoSuaTray { tx }).spawn().await {
                Ok(handle) => {
                    tracing::info!("tray icon registered");
                    std::mem::forget(handle);
                }
                Err(err) => tracing::warn!(error = %err, "no tray icon"),
            }
            watch_resume(resume_tx).await;
        });
    });
}

fn icon() -> Icon {
    let img = image::load_from_memory(include_bytes!("../../tauri-app/src-tauri/icons/tray-icon.png"))
        .expect("bundled tray icon")
        .into_rgba8();
    let (width, height) = img.dimensions();
    // SNI wants ARGB32, network byte order
    let data = img.pixels().flat_map(|p| [p.0[3], p.0[0], p.0[1], p.0[2]]).collect();
    Icon { width: width as i32, height: height as i32, data }
}

struct ChoSuaTray {
    tx: UnboundedSender<TrayAction>,
}

impl Tray for ChoSuaTray {
    fn id(&self) -> String {
        "chosua".into()
    }
    fn title(&self) -> String {
        "ChoSua".into()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![icon()]
    }
    fn status(&self) -> Status {
        Status::Active
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayAction::Toggle);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Hiện/ẩn cửa sổ".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayAction::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Khởi động lại".into(),
                activate: Box::new(|_: &mut Self| restart()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Thoát".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager"
)]
trait Login1Manager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

async fn watch_resume(tx: UnboundedSender<TrayAction>) {
    loop {
        if let Err(err) = watch_resume_once(&tx).await {
            tracing::warn!(error = %err, "resume watch: system bus lost, retrying in 30 s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn watch_resume_once(tx: &UnboundedSender<TrayAction>) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let manager = Login1ManagerProxy::new(&conn).await?;
    let mut signals = manager.receive_prepare_for_sleep().await?;
    while let Some(signal) = futures_util::StreamExt::next(&mut signals).await {
        if !signal.args()?.start {
            tracing::info!("resumed from suspend: restarting sync");
            let _ = tx.send(TrayAction::Resumed);
        }
    }
    Ok(())
}
