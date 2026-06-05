//! prism-notify host: a wlr-layer-shell surface driven by the damascene
//! wgpu `Runner` (the custom-host path), fed by an
//! `org.freedesktop.Notifications` D-Bus server.
//!
//! Shape:
//!
//!   one Daemon (wayland conn, protocol state, wgpu device, config,
//!   notification stack)
//!     → zero-or-one NotifySurface (exists while the stack is non-empty)
//!       → layer surface + wgpu swapchain + damascene Runner
//!   zbus executor thread → calloop channel → stack mutation
//!   stack mutation → measure tree at config width → set_size → draw
//!   calloop: wayland socket + dbus channel + deadlines (expiry, anim)
//!   SCTK pointer events → runner.pointer_*() → UserActions → signals

mod config;
mod dbus;
mod notification;
mod ui;

use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel as calloop_channel;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};

use damascene_core::event::{Pointer, PointerButton};
use damascene_core::prelude::{App, Rect};
use damascene_core::{BuildCx, UiState};
use damascene_wgpu::{MsaaTarget, Runner, RunnerCaps};

use crate::config::{Config, Corner};
use crate::dbus::Dbus;
use crate::notification::{CloseReason, Notification};
use crate::ui::{NotifyApp, UserAction};

const MSAA_SAMPLES: u32 = 4;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_notify=info".into()),
        )
        .init();

    let config = Config::load()?;

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) = registry_queue_init::<Daemon>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let (dbus_send, dbus_recv) = calloop_channel::channel();
    let dbus = Dbus::serve(dbus_send)?;

    let app = NotifyApp::new(&config);
    let mut daemon = Daemon {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        conn: conn.clone(),
        config,
        dbus,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
        gpu: None,
        surface: None,
        pointer: None,
        app,
        measure_ui: UiState::new(),
        stack: Vec::new(),
        stack_changed: false,
        reload_at: None,
        exit: false,
    };

    let mut event_loop: EventLoop<Daemon> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;
    event_loop
        .handle()
        .insert_source(dbus_recv, |event, _, daemon: &mut Daemon| {
            if let calloop_channel::Event::Msg(event) = event {
                daemon.on_dbus_event(event);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert dbus channel: {e}"))?;
    watch_config(&mut event_loop)?;

    while !daemon.exit {
        // Sleep until the earliest deadline: a notification expiring,
        // damascene animation, or a debounced config reload. Wayland
        // and D-Bus events interrupt.
        let now = Instant::now();
        let mut timeout = Duration::from_secs(3600);
        for popup in &daemon.stack {
            if let Some(d) = popup.deadline {
                timeout = timeout.min(d.saturating_duration_since(now));
            }
        }
        if let Some(s) = &daemon.surface {
            if let Some(d) = s.anim_deadline {
                timeout = timeout.min(d.saturating_duration_since(now));
            }
        }
        if let Some(d) = daemon.reload_at {
            timeout = timeout.min(d.saturating_duration_since(now));
        }

        event_loop
            .dispatch(Some(timeout), &mut daemon)
            .context("event loop dispatch")?;

        // Debounced config reload (armed by the inotify source).
        if daemon.reload_at.is_some_and(|d| d <= Instant::now()) {
            daemon.reload_at = None;
            daemon.reload_config();
        }
        // Expire due notifications.
        let now = Instant::now();
        let expired: Vec<u32> = daemon
            .stack
            .iter()
            .filter(|p| p.deadline.is_some_and(|d| d <= now))
            .map(|p| p.notif.id)
            .collect();
        for id in expired {
            daemon.close(id, CloseReason::Expired);
        }
        // An animation deadline elapsing is a redraw reason.
        if let Some(s) = &mut daemon.surface {
            if s.anim_deadline.is_some_and(|d| d <= now) {
                s.anim_deadline = None;
                s.dirty = true;
            }
        }
        // Stack changes resize (or create/destroy) the surface.
        if daemon.stack_changed {
            daemon.stack_changed = false;
            daemon.sync_surface(&qh);
        }
        if daemon.surface.as_ref().is_some_and(|s| s.dirty) {
            daemon.draw();
        }
    }
    Ok(())
}

/// One queued notification with its resolved expiry.
struct Popup {
    notif: Rc<Notification>,
    /// `None` = sticky (critical, or timeout 0).
    deadline: Option<Instant>,
}

/// GPU objects, created lazily with the first surface (adapter
/// selection wants a compatible surface) and kept across surface
/// destroy/create cycles.
struct GpuShared {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Swapchain + renderer; created on the first layer-shell configure
/// (before that we don't know the size the compositor granted).
struct Swapchain {
    config: wgpu::SurfaceConfiguration,
    msaa: Option<MsaaTarget>,
    runner: Runner,
}

/// The notification stack's surface. Exists only while the stack is
/// non-empty.
struct NotifySurface {
    // Drop order: the wgpu surface (unsafely) borrows the wl_surface
    // kept alive by `layer`, so it must drop first.
    wgpu_surface: wgpu::Surface<'static>,
    swapchain: Option<Swapchain>,
    layer: LayerSurface,
    /// Logical (surface-coordinate) size.
    width: u32,
    height: u32,
    /// Integer buffer scale from the compositor.
    scale: i32,
    dirty: bool,
    /// When damascene wants the next animation frame.
    anim_deadline: Option<Instant>,
    /// Last pointer position in logical px (button events don't carry one).
    pointer_pos: (f64, f64),
}

struct Daemon {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    conn: Connection,
    config: Config,
    dbus: Dbus,
    instance: wgpu::Instance,
    gpu: Option<GpuShared>,
    surface: Option<NotifySurface>,
    pointer: Option<wl_pointer::WlPointer>,
    app: NotifyApp,
    /// Scratch layout state for the offscreen measuring pass.
    measure_ui: UiState,
    /// Notification stack, newest first.
    stack: Vec<Popup>,
    /// Arms a `sync_surface` on the next loop turn.
    stack_changed: bool,
    /// Debounced config-reload deadline (armed by the inotify source).
    reload_at: Option<Instant>,
    exit: bool,
}

impl Daemon {
    fn on_dbus_event(&mut self, event: dbus::Event) {
        match event {
            dbus::Event::Notify(n) => self.upsert(n),
            dbus::Event::Close(id) => self.close(id, CloseReason::CloseCall),
        }
    }

    /// Insert a new notification at the front, or replace an existing
    /// one in place (`replaces_id`), resetting its expiry either way.
    fn upsert(&mut self, n: Notification) {
        let deadline = n
            .timeout(self.config.default_timeout)
            .map(|d| Instant::now() + d);
        let popup = Popup {
            notif: Rc::new(n),
            deadline,
        };
        match self.stack.iter_mut().find(|p| p.notif.id == popup.notif.id) {
            Some(slot) => *slot = popup,
            None => self.stack.insert(0, popup),
        }
        self.stack_changed = true;
    }

    /// Remove `id` from the stack (if present) and emit
    /// `NotificationClosed`.
    fn close(&mut self, id: u32, reason: CloseReason) {
        let Some(pos) = self.stack.iter().position(|p| p.notif.id == id) else {
            return; // unknown/already-closed id: ignore (CloseNotification race)
        };
        self.stack.remove(pos);
        tracing::debug!(id, ?reason, "closed");
        self.dbus.emit_closed(id, reason);
        self.stack_changed = true;
    }

    /// Surface width: configured card width plus the app's shadow
    /// gutter on both sides.
    fn surface_width(&self) -> u32 {
        self.config.width + 2 * ui::SHADOW_GUTTER
    }

    /// Reconcile the surface with the stack: destroy when empty,
    /// create when needed, resize to the measured content height.
    fn sync_surface(&mut self, qh: &QueueHandle<Self>) {
        self.app
            .set_state(self.stack.iter().map(|p| p.notif.clone()).collect());

        if self.stack.is_empty() {
            if self.surface.take().is_some() {
                tracing::debug!("stack empty; surface destroyed");
            }
            return;
        }

        let height = self.measure_height().max(1);
        let width = self.surface_width();
        match &mut self.surface {
            None => self.create_surface(qh, height),
            Some(s) => {
                if s.height != height {
                    s.height = height;
                    s.layer.set_size(width, height);
                    s.layer.commit();
                    // The swapchain follows on the configure event.
                }
                s.dirty = true;
            }
        }
    }

    /// Lay the current tree out at the configured width and read back
    /// the stack's height — the layer surface is sized to content.
    fn measure_height(&mut self) -> u32 {
        let theme = self.app.theme();
        let viewport = Rect::new(0.0, 0.0, self.surface_width() as f32, 16384.0);
        let cx = BuildCx::new(&theme).with_viewport(viewport.w, viewport.h);
        let mut tree = self.app.build(&cx);
        damascene_core::layout::layout(&mut tree, &mut self.measure_ui, viewport);
        let h = self
            .measure_ui
            .rect_of_key(&tree, ui::STACK_KEY)
            .map(|r| r.h)
            .unwrap_or(0.0);
        h.ceil() as u32
    }

    fn anchor(&self) -> Anchor {
        match self.config.corner {
            Corner::TopRight => Anchor::TOP | Anchor::RIGHT,
            Corner::TopLeft => Anchor::TOP | Anchor::LEFT,
            Corner::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
            Corner::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        }
    }

    fn create_surface(&mut self, qh: &QueueHandle<Self>, height: u32) {
        // Configured output if present; else let the compositor pick
        // (normally the focused output).
        let output = self.config.output.as_deref().and_then(|want| {
            let found = self
                .output_state
                .outputs()
                .find(|o| self.output_state.info(o).and_then(|i| i.name).as_deref() == Some(want));
            if found.is_none() {
                tracing::warn!(output = want, "configured output absent; compositor picks");
            }
            found
        });

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("prism-notify"),
            output.as_ref(),
        );
        let m = self.config.margin;
        layer.set_anchor(self.anchor());
        layer.set_margin(m, m, m, m);
        layer.set_size(self.surface_width(), height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();

        // SAFETY: the wl_display and wl_surface pointers stay valid for
        // the life of this NotifySurface — `conn` is owned by `Daemon`,
        // the wl_surface by `layer`, and `wgpu_surface` drops first
        // (field order).
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).expect("display ptr"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).expect("surface ptr"),
        ));
        let wgpu_surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
        }
        .expect("create wgpu surface on layer surface");

        if self.gpu.is_none() {
            let adapter =
                pollster::block_on(self.instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::default(),
                    compatible_surface: Some(&wgpu_surface),
                    force_fallback_adapter: false,
                }))
                .expect("no compatible adapter");
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("prism-notify::device"),
                    ..Default::default()
                }))
                .expect("request device");
            tracing::info!(backend = ?adapter.get_info().backend, "gpu initialized");
            self.gpu = Some(GpuShared {
                adapter,
                device,
                queue,
            });
        }

        tracing::debug!(height, "surface created");
        self.surface = Some(NotifySurface {
            wgpu_surface,
            swapchain: None,
            layer,
            width: self.surface_width(),
            height,
            scale: 1,
            dirty: false,
            anim_deadline: None,
            pointer_pos: (0.0, 0.0),
        });
    }

    /// Reload the config file and apply it. A file that fails to load
    /// keeps the running config.
    fn reload_config(&mut self) {
        let new = match Config::load() {
            Ok(c) => c,
            Err(err) => {
                tracing::error!("config reload failed; keeping current config\n{err:#}");
                return;
            }
        };
        tracing::info!("config reloaded");
        self.config = new;
        self.app = NotifyApp::new(&self.config);
        // Geometry, anchor, even the output may have changed — drop the
        // surface; the next sync recreates it from scratch.
        self.surface = None;
        self.stack_changed = true;
    }

    /// Configure (or reconfigure) the swapchain from the surface's
    /// current logical size + scale.
    fn configure_swapchain(&mut self) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let Some(s) = self.surface.as_mut() else {
            return;
        };
        let scale = s.scale as u32;
        let (w, h) = ((s.width * scale).max(1), (s.height * scale).max(1));

        match &mut s.swapchain {
            Some(sc) => {
                if sc.config.width == w && sc.config.height == h {
                    return;
                }
                sc.config.width = w;
                sc.config.height = h;
                s.wgpu_surface.configure(&gpu.device, &sc.config);
                sc.runner.set_surface_size(w, h);
                let extent = wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                };
                if let Some(msaa) = sc.msaa.as_mut() {
                    if !msaa.matches(extent) {
                        *msaa =
                            MsaaTarget::new(&gpu.device, sc.config.format, extent, msaa.sample_count);
                    }
                }
            }
            None => {
                let caps = s.wgpu_surface.get_capabilities(&gpu.adapter);
                let format = caps
                    .formats
                    .iter()
                    .copied()
                    .find(|f| f.is_srgb())
                    .unwrap_or(caps.formats[0]);
                // Transparent background: damascene's blend states leave
                // correct premultiplied coverage over a transparent
                // clear, so PreMultiplied is the right composite mode.
                let alpha_mode = if caps
                    .alpha_modes
                    .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                {
                    wgpu::CompositeAlphaMode::PreMultiplied
                } else {
                    tracing::warn!(
                        modes = ?caps.alpha_modes,
                        "no premultiplied alpha; notifications will be opaque"
                    );
                    caps.alpha_modes[0]
                };
                let config = wgpu::SurfaceConfiguration {
                    // COPY_SRC matches the runner's backdrop-snapshot path.
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    format,
                    width: w,
                    height: h,
                    present_mode: wgpu::PresentMode::Fifo,
                    alpha_mode,
                    view_formats: vec![],
                    desired_maximum_frame_latency: 1,
                };
                s.wgpu_surface.configure(&gpu.device, &config);

                let mut runner = Runner::with_caps(
                    &gpu.device,
                    &gpu.queue,
                    format,
                    MSAA_SAMPLES,
                    RunnerCaps::from_adapter(&gpu.adapter),
                );
                runner.set_theme(self.app.theme());
                runner.set_surface_size(w, h);
                runner.warm_default_glyphs();

                let msaa = (MSAA_SAMPLES > 1).then(|| {
                    MsaaTarget::new(
                        &gpu.device,
                        format,
                        wgpu::Extent3d {
                            width: w,
                            height: h,
                            depth_or_array_layers: 1,
                        },
                        MSAA_SAMPLES,
                    )
                });
                tracing::debug!(?format, "swapchain configured");
                s.swapchain = Some(Swapchain {
                    config,
                    msaa,
                    runner,
                });
            }
        }
    }

    fn draw(&mut self) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let Some(s) = self.surface.as_mut() else {
            return;
        };
        s.dirty = false;
        let Some(sc) = s.swapchain.as_mut() else {
            return; // not configured yet; the configure will redraw
        };

        let scale = s.scale as f32;
        let viewport = Rect::new(0.0, 0.0, s.width as f32, s.height as f32);

        self.app.before_build();
        let theme = self.app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(sc.runner.ui_state())
                .with_viewport(viewport.w, viewport.h);
            self.app.build(&cx)
        };
        sc.runner.set_theme(theme);
        sc.runner.set_hotkeys(self.app.hotkeys());

        let prepare = sc
            .runner
            .prepare(&gpu.device, &gpu.queue, &mut tree, viewport, scale);

        let frame = match s.wgpu_surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                s.wgpu_surface.configure(&gpu.device, &sc.config);
                s.dirty = true; // try again next loop turn
                return;
            }
            other => {
                tracing::error!("surface unavailable: {other:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("prism-notify::encoder"),
            });
        sc.runner.render(
            &gpu.device,
            &mut encoder,
            &frame.texture,
            &view,
            sc.msaa.as_ref().map(|m| &m.view),
            // Transparent clear — the visible cards are rounded rects in
            // the tree; the compositor sees through the gaps.
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();

        s.anim_deadline = prepare.next_redraw_in.map(|d| Instant::now() + d);
        if prepare.needs_redraw && s.anim_deadline.is_none() {
            s.anim_deadline = Some(Instant::now());
        }
    }

    fn is_our_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.surface
            .as_ref()
            .is_some_and(|s| s.layer.wl_surface() == surface)
    }

    fn dispatch_ui_events(&mut self, events: Vec<damascene_core::UiEvent>) {
        for event in events {
            self.app.on_event(event);
        }
        // Side effects the app requested (it can't talk D-Bus itself).
        for action in self.app.take_actions() {
            match action {
                UserAction::Dismiss(id) => self.close(id, CloseReason::Dismissed),
                UserAction::Invoke(id, key) => {
                    self.dbus.emit_action(id, &key);
                    self.close(id, CloseReason::Dismissed);
                }
            }
        }
        if let Some(s) = &mut self.surface {
            s.dirty = true;
        }
    }
}

impl LayerShellHandler for Daemon {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor dismissed the surface (e.g. its output going
        // away). Drop it; a non-empty stack recreates it next turn.
        if self.is_our_surface(layer.wl_surface()) {
            self.surface = None;
            self.stack_changed = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if !self.is_our_surface(layer.wl_surface()) {
            return;
        }
        let (w, h) = configure.new_size;
        {
            let s = self.surface.as_mut().expect("checked above");
            if w > 0 {
                s.width = w;
            }
            if h > 0 {
                s.height = h;
            }
        }
        self.configure_swapchain();
        self.surface.as_mut().expect("checked above").dirty = true;
    }
}

impl CompositorHandler for Daemon {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if !self.is_our_surface(surface) {
            return;
        }
        let s = self.surface.as_mut().expect("checked above");
        if s.scale != new_factor {
            s.scale = new_factor;
            surface.set_buffer_scale(new_factor);
            let configured = s.swapchain.is_some();
            if configured {
                self.configure_swapchain();
            }
            self.surface.as_mut().expect("checked above").dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Daemon {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // If the configured output just appeared and the stack is
        // showing on a fallback, migrate by recreating.
        if let (Some(want), Some(_)) = (&self.config.output, &self.surface) {
            let name = self.output_state.info(&output).and_then(|i| i.name);
            if name.as_deref() == Some(want.as_str()) {
                tracing::info!(output = %want, "configured output appeared; moving stack");
                self.surface = None;
                self.stack_changed = true;
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // If our surface lived there the compositor sends closed();
        // nothing to do here.
    }
}

impl SeatHandler for Daemon {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Daemon {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if !self.is_our_surface(&event.surface) {
                continue;
            }
            // SCTK positions are surface-local logical coordinates —
            // exactly what damascene's pointer methods take.
            let (x, y) = (event.position.0 as f32, event.position.1 as f32);
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let s = self.surface.as_mut().expect("checked above");
                    s.pointer_pos = event.position;
                    let Some(sc) = s.swapchain.as_mut() else {
                        continue;
                    };
                    let moved = sc.runner.pointer_moved(Pointer::moving(x, y));
                    let needs_redraw = moved.needs_redraw;
                    self.dispatch_ui_events(moved.events);
                    if needs_redraw {
                        if let Some(s) = &mut self.surface {
                            s.dirty = true;
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    let s = self.surface.as_mut().expect("checked above");
                    let Some(sc) = s.swapchain.as_mut() else {
                        continue;
                    };
                    let events = sc.runner.pointer_left();
                    self.dispatch_ui_events(events);
                }
                PointerEventKind::Press { button, .. }
                | PointerEventKind::Release { button, .. } => {
                    let Some(button) = linux_button(button) else {
                        continue;
                    };
                    let s = self.surface.as_mut().expect("checked above");
                    let (px, py) = (s.pointer_pos.0 as f32, s.pointer_pos.1 as f32);
                    let Some(sc) = s.swapchain.as_mut() else {
                        continue;
                    };
                    let p = Pointer::mouse(px, py, button);
                    let events = if matches!(event.kind, PointerEventKind::Press { .. }) {
                        sc.runner.pointer_down(p)
                    } else {
                        sc.runner.pointer_up(p)
                    };
                    self.dispatch_ui_events(events);
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

/// Watch the config file's parent directory for changes to the file
/// and arm `Daemon::reload_at` (debounced — editors emit event bursts,
/// and rename-replace saves never touch the watched fd of the file
/// itself, hence the directory watch). No config directory yet means
/// live reload stays inactive for this run.
fn watch_config(event_loop: &mut EventLoop<Daemon>) -> Result<()> {
    use rustix::fs::inotify;

    let Some(path) = Config::path() else {
        return Ok(());
    };
    let (Some(dir), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Ok(());
    };
    if !dir.is_dir() {
        tracing::info!("{} absent; live config reload inactive", dir.display());
        return Ok(());
    }
    let file_name = file_name.to_owned();

    let fd = inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
        .context("inotify init")?;
    inotify::add_watch(
        &fd,
        dir,
        inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::CREATE
            | inotify::WatchFlags::DELETE,
    )
    .context("inotify add_watch")?;
    tracing::debug!("watching {} for config changes", dir.display());

    event_loop
        .handle()
        .insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, fd, daemon: &mut Daemon| {
                let mut buf = [std::mem::MaybeUninit::uninit(); 1024];
                let mut reader = inotify::Reader::new(fd, &mut buf);
                while let Ok(event) = reader.next() {
                    let matches = event
                        .file_name()
                        .map(|n| n.to_bytes() == file_name.as_encoded_bytes())
                        .unwrap_or(false);
                    if matches {
                        daemon.reload_at = Some(Instant::now() + Duration::from_millis(150));
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert inotify source: {e}"))?;
    Ok(())
}

fn linux_button(code: u32) -> Option<PointerButton> {
    match code {
        0x110 => Some(PointerButton::Primary),   // BTN_LEFT
        0x111 => Some(PointerButton::Secondary), // BTN_RIGHT
        0x112 => Some(PointerButton::Middle),    // BTN_MIDDLE
        _ => None,
    }
}

impl ProvidesRegistryState for Daemon {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Daemon);
delegate_output!(Daemon);
delegate_layer!(Daemon);
delegate_seat!(Daemon);
delegate_pointer!(Daemon);
delegate_registry!(Daemon);
