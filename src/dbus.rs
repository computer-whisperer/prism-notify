//! `org.freedesktop.Notifications` server (spec 1.2).
//!
//! zbus in blocking mode: the connection runs its own executor thread,
//! so method calls land there — handlers decode the wire format
//! (`notification.rs`) and push [`Event`]s into the main calloop via a
//! calloop channel. IDs are assigned here, atomically, so `Notify` can
//! return without a round-trip to the main thread. The main thread
//! emits `NotificationClosed` / `ActionInvoked` back through the
//! (thread-safe) connection via [`Dbus::emit_closed`] / [`Dbus::emit_action`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use damascene_core::image::Image;

use crate::notification::{self, CloseReason, Notification, Urgency};

const PATH: &str = "/org/freedesktop/Notifications";
const NAME: &str = "org.freedesktop.Notifications";

/// D-Bus → main loop.
pub enum Event {
    /// New or replacing notification (`replaces_id` reuses the id).
    Notify(Notification),
    /// `CloseNotification` call; close with [`CloseReason::CloseCall`].
    Close(u32),
}

struct Interface {
    sender: Sender<Event>,
    next_id: AtomicU32,
}

impl Interface {
    fn send(&self, event: Event) {
        if self.sender.send(event).is_err() {
            // Main loop gone; the process is exiting anyway.
            tracing::warn!("main loop channel closed; dropping bus event");
        }
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Interface {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };
        let (actions, has_default_action) = notification::parse_actions(&actions);
        let notification = Notification {
            id,
            app_name,
            summary,
            body: notification::strip_markup(&body),
            urgency: urgency_hint(&hints),
            actions,
            has_default_action,
            image: image_hint(&hints, &app_icon),
            expire_timeout,
        };
        tracing::debug!(
            id,
            app = %notification.app_name,
            summary = %notification.summary,
            "notify"
        );
        self.send(Event::Notify(notification));
        id
    }

    fn close_notification(&self, id: u32) {
        self.send(Event::Close(id));
    }

    fn get_capabilities(&self) -> Vec<String> {
        // No "body-markup": we render plain text (markup is stripped),
        // so well-behaved clients won't send tags in the first place.
        ["body", "actions", "icon-static"]
            .map(String::from)
            .to_vec()
    }

    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("prism-notify", "prism", env!("CARGO_PKG_VERSION"), "1.2")
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

/// Handle for the main thread: keeps the connection (and its executor
/// thread) alive and emits the spec's signals.
pub struct Dbus {
    conn: zbus::blocking::Connection,
}

impl Dbus {
    /// Connect to the session bus, claim the well-known name and serve
    /// the interface. Fails if another notification daemon holds the
    /// name.
    pub fn serve(sender: Sender<Event>) -> Result<Self> {
        let iface = Interface {
            sender,
            next_id: AtomicU32::new(1),
        };
        let conn = zbus::blocking::connection::Builder::session()
            .context("session bus")?
            .name(NAME)
            .context("claim org.freedesktop.Notifications (is another daemon running?)")?
            .serve_at(PATH, iface)
            .context("serve interface")?
            .build()
            .context("connect to session bus")?;
        tracing::info!("serving {NAME}");
        Ok(Self { conn })
    }

    pub fn emit_closed(&self, id: u32, reason: CloseReason) {
        self.emit(|e| zbus::block_on(Interface::notification_closed(e, id, reason as u32)));
    }

    pub fn emit_action(&self, id: u32, key: &str) {
        self.emit(|e| zbus::block_on(Interface::action_invoked(e, id, key)));
    }

    fn emit(&self, f: impl FnOnce(&SignalEmitter<'_>) -> zbus::Result<()>) {
        let result = self
            .conn
            .object_server()
            .interface::<_, Interface>(PATH)
            .and_then(|iface| f(iface.signal_emitter()));
        if let Err(err) = result {
            tracing::error!(%err, "signal emission failed");
        }
    }
}

fn urgency_hint(hints: &HashMap<String, OwnedValue>) -> Urgency {
    match hints.get("urgency").and_then(|v| v.downcast_ref::<u8>().ok()) {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
    }
}

/// Image resolution order per spec §Icons and Images: `image-data`
/// hint, `image-path` hint, `app_icon` parameter, legacy `icon_data`.
fn image_hint(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<Image> {
    let data = |key| hints.get(key).and_then(image_from_value);
    let path = |key: &str| {
        hints
            .get(key)
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .and_then(notification::image_from_path)
    };
    data("image-data")
        .or_else(|| data("image_data")) // pre-1.1 spelling
        .or_else(|| path("image-path"))
        .or_else(|| path("image_path"))
        .or_else(|| notification::image_from_path(app_icon))
        .or_else(|| data("icon_data"))
}

/// Decode the `(iiibiiay)` image structure from a hint value.
fn image_from_value(value: &OwnedValue) -> Option<Image> {
    let s = value.downcast_ref::<&zbus::zvariant::Structure>().ok()?;
    let f = s.fields();
    if f.len() != 7 {
        return None;
    }
    let width: i32 = f[0].downcast_ref().ok()?;
    let height: i32 = f[1].downcast_ref().ok()?;
    let rowstride: i32 = f[2].downcast_ref().ok()?;
    let has_alpha: bool = f[3].downcast_ref().ok()?;
    let bits_per_sample: i32 = f[4].downcast_ref().ok()?;
    let channels: i32 = f[5].downcast_ref().ok()?;
    let data: Vec<u8> = f[6].try_clone().ok()?.try_into().ok()?;
    notification::image_from_data(
        width,
        height,
        rowstride,
        has_alpha,
        bits_per_sample,
        channels,
        &data,
    )
}
