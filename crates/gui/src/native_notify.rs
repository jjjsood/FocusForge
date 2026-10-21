// SPDX-License-Identifier: GPL-3.0-or-later

//! Delivery of notification banners through the operating system's
//! notification service.

use std::fmt::Display;

use crate::notifications::Banner;

/// The identity the banners are sent under: the macOS bundle identifier and
/// the Windows AppUserModelID.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const APP_ID: &str = "org.focusforge.FocusForge";

/// Calls `send` once per banner. A failure is logged and not retried.
pub fn deliver<E: Display>(banners: Vec<Banner>, send: impl Fn(&Banner) -> Result<(), E>) {
    for banner in &banners {
        if let Err(error) = send(banner) {
            tracing::warn!(%error, title = %banner.title, "native notification not delivered");
        }
    }
}

/// Sends `banners` from a worker thread: showing one waits on the system's
/// notification service.
pub fn spawn(banners: Vec<Banner>) {
    let worker = std::thread::Builder::new()
        .name("native-notify".into())
        .spawn(move || deliver(banners, show));
    if let Err(error) = worker {
        tracing::warn!(%error, "native notifications not sent");
    }
}

fn show(banner: &Banner) -> Result<(), notify_rust::error::Error> {
    #[cfg(target_os = "macos")]
    {
        static IDENTITY: std::sync::Once = std::sync::Once::new();
        IDENTITY.call_once(|| {
            if let Err(error) = notify_rust::set_application(APP_ID) {
                tracing::warn!(%error, "native notification identity not set");
            }
        });
    }
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("FocusForge")
        .summary(&banner.title)
        .body(&banner.body);
    #[cfg(all(unix, not(target_os = "macos")))]
    notification.hint(notify_rust::Hint::DesktopEntry("focusforge".into()));
    #[cfg(target_os = "windows")]
    notification.app_id(APP_ID);
    notification.show().map(|_| ())
}
