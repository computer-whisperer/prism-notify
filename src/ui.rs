//! The daemon's damascene `App` — notification stack → tree projection.
//!
//! The host pushes the visible stack in via [`NotifyApp::set_state`]
//! before each build; clicks come back out through
//! [`NotifyApp::take_actions`]. The same build path serves the
//! offscreen measuring pass (`main.rs` lays the tree out at the
//! configured width to size the layer surface) and the real render.

use std::rc::Rc;

use damascene_core::prelude::*;

use crate::config::{Config, Corner};
use crate::notification::{Notification, Urgency};

/// Key of the stack column, read back by the host's measuring pass.
pub const STACK_KEY: &str = "stack";

/// Paint gutter around the stack, logical px. `card()` draws its
/// `SHADOW_MD` drop shadow outside the layout rect; the wl_surface is
/// sized to the keyed stack (gutter included), so without this the
/// shadow would hard-clip at the surface edge. The host adds it to the
/// surface width; the measured stack height already contains it.
pub const SHADOW_GUTTER: u32 = 16;

/// Side of the square image slot, logical px.
const IMAGE_SIZE: f32 = 48.0;

/// What the user did to a notification, drained by the host.
#[derive(Debug)]
pub enum UserAction {
    /// Close (the card's × or a body click without a default action).
    Dismiss(u32),
    /// An action was invoked; emit `ActionInvoked` then close.
    Invoke(u32, String),
}

pub struct NotifyApp {
    /// Visible stack, newest first (host order).
    items: Vec<Rc<Notification>>,
    /// Total stack size including items hidden by `max_visible`.
    total: usize,
    corner: Corner,
    gap: u32,
    max_visible: usize,
    /// Card fill translucency from config (1.0 = opaque). The strip
    /// composites over the already-translucent card fill, so its
    /// region reads a touch more opaque — which suits a chrome strip.
    opacity: f32,
    pending: Vec<UserAction>,
}

impl NotifyApp {
    pub fn new(config: &Config) -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            corner: config.corner,
            gap: config.gap,
            max_visible: config.max_visible,
            opacity: config.opacity,
            pending: Vec::new(),
        }
    }

    /// Host-side state push, called before each build. `items` is
    /// newest-first.
    pub fn set_state(&mut self, items: Vec<Rc<Notification>>) {
        self.total = items.len();
        self.items = items;
        self.items.truncate(self.max_visible);
    }

    /// Drain the user actions from the last event batch.
    pub fn take_actions(&mut self) -> Vec<UserAction> {
        std::mem::take(&mut self.pending)
    }

    /// Stock card anatomy: a tinted `card_header` strip carries the app
    /// name and dismiss button, `card_content` the message (optional
    /// image beside summary + body), `card_footer` the action buttons.
    /// Only the strip's vertical padding is tightened from the shadcn
    /// recipe — full `SPACE_6` reads as a banner, not a chrome strip.
    fn card(&self, n: &Notification, palette: &Palette) -> El {
        // Critical urgency tints the strip destructive — a 1px
        // destructive stroke alone is invisible on the dark theme.
        let strip_fill = match n.urgency {
            Urgency::Critical => tokens::DESTRUCTIVE,
            _ => tokens::MUTED,
        }
        .with_alpha(self.opacity);
        let strip = card_header([row([
            text(n.app_name.clone()).caption().muted(),
            spacer(),
            icon_button(IconName::X)
                .ghost()
                .small()
                .key(format!("n-{}-close", n.id)),
        ])
        .align(Align::Center)])
        .fill(strip_fill)
        .py(tokens::SPACE_2);

        let mut lines: Vec<El> = Vec::new();
        if !n.summary.is_empty() {
            lines.push(card_title(n.summary.clone()).wrap_text().max_lines(2));
        }
        if !n.body.is_empty() {
            lines.push(card_description(n.body.clone()).max_lines(6));
        }
        let mut message: Vec<El> = Vec::new();
        if let Some(img) = &n.image {
            message.push(
                image(img.clone())
                    .image_fit(ImageFit::Cover)
                    .width(Size::Fixed(IMAGE_SIZE))
                    .height(Size::Fixed(IMAGE_SIZE))
                    .radius(6.0),
            );
        }
        message.push(column(lines).gap(tokens::SPACE_2).fill_width());

        // The filled strip supplies its own separation, so the content
        // takes a real top padding instead of the recipe's `pt-0`.
        let content = card_content([row(message).gap(tokens::SPACE_3).align(Align::Start)])
            .pt(tokens::SPACE_4);

        let mut slots = vec![strip, content];
        if !n.actions.is_empty() {
            slots.push(card_footer([row(n
                .actions
                .iter()
                .map(|a| {
                    button(a.label.clone())
                        .secondary()
                        .small()
                        .key(format!("n-{}-act-{}", n.id, a.key))
                })
                .collect::<Vec<_>>())
            .gap(tokens::SPACE_2)]));
        }

        let card = card(slots)
            .key(format!("n-{}", n.id))
            .fill(tokens::CARD.with_alpha(self.opacity));
        match n.urgency {
            Urgency::Critical => card.stroke(palette.destructive),
            Urgency::Low => card.opacity(0.85),
            Urgency::Normal => card,
        }
    }
}

impl App for NotifyApp {
    fn build(&self, cx: &BuildCx) -> El {
        let palette = cx.palette();

        let mut cards: Vec<El> = self.items.iter().map(|n| self.card(n, palette)).collect();
        let hidden = self.total.saturating_sub(self.items.len());
        if hidden > 0 {
            cards.push(
                row([text(format!("+{hidden} more")).caption().muted()])
                    .fill_width()
                    .justify(Justify::Center),
            );
        }
        // Bottom corners: newest card sits nearest the corner, i.e.
        // last in a top-to-bottom column.
        if self.corner.is_bottom() {
            cards.reverse();
        }

        // The wl_surface is sized to exactly fit this keyed column (the
        // host measures it at the surface width before committing), so
        // the wrapper contributes nothing but the root viewport rect.
        // The gutter padding keeps card shadows inside the surface.
        column([column(cards)
            .key(STACK_KEY)
            .gap(self.gap as f32)
            .padding(Sides::all(SHADOW_GUTTER as f32))
            .fill_width()])
        .fill_width()
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
        for n in &self.items {
            if event.is_click_or_activate(&format!("n-{}-close", n.id)) {
                self.pending.push(UserAction::Dismiss(n.id));
                return;
            }
            for a in &n.actions {
                if event.is_click_or_activate(&format!("n-{}-act-{}", n.id, a.key)) {
                    self.pending.push(UserAction::Invoke(n.id, a.key.clone()));
                    return;
                }
            }
            if event.is_click_or_activate(&format!("n-{}", n.id)) {
                self.pending.push(if n.has_default_action {
                    UserAction::Invoke(n.id, "default".into())
                } else {
                    UserAction::Dismiss(n.id)
                });
                return;
            }
        }
    }
}
