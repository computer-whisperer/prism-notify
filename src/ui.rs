//! The daemon's damascene `App` — notification stack → tree projection.
//!
//! The host pushes the visible stack in via [`NotifyApp::set_state`]
//! before each build; clicks come back out through
//! [`NotifyApp::take_actions`]. The same build path serves the
//! offscreen measuring pass (`main.rs` lays the tree out at the
//! configured width to size the layer surface) and the real render.

use std::rc::Rc;

use damascene_core::prelude::*;

use crate::config::Corner;
use crate::notification::{Notification, Urgency};

/// Key of the stack column, read back by the host's measuring pass.
pub const STACK_KEY: &str = "stack";

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
    pending: Vec<UserAction>,
}

impl NotifyApp {
    pub fn new(corner: Corner, gap: u32, max_visible: usize) -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            corner,
            gap,
            max_visible,
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

    fn card(&self, n: &Notification, palette: &Palette) -> El {
        let header = row([
            text(n.app_name.clone()).caption().muted(),
            spacer(),
            icon_button(IconName::X)
                .ghost()
                .small()
                .key(format!("n-{}-close", n.id)),
        ])
        .fill_width()
        .align(Align::Center);

        let mut content: Vec<El> = Vec::new();
        if let Some(img) = &n.image {
            content.push(
                image(img.clone())
                    .image_fit(ImageFit::Cover)
                    .width(Size::Fixed(IMAGE_SIZE))
                    .height(Size::Fixed(IMAGE_SIZE))
                    .radius(6.0),
            );
        }
        let mut lines: Vec<El> = Vec::new();
        if !n.summary.is_empty() {
            lines.push(
                text(n.summary.clone())
                    .label()
                    .semibold()
                    .wrap_text()
                    .max_lines(2),
            );
        }
        if !n.body.is_empty() {
            lines.push(
                text(n.body.clone())
                    .caption()
                    .muted()
                    .wrap_text()
                    .max_lines(6),
            );
        }
        content.push(column(lines).gap(tokens::SPACE_1).fill_width());

        let mut parts = vec![
            header,
            row(content).gap(tokens::SPACE_3).align(Align::Start).fill_width(),
        ];
        if !n.actions.is_empty() {
            parts.push(
                row(n
                    .actions
                    .iter()
                    .map(|a| {
                        button(a.label.clone())
                            .secondary()
                            .small()
                            .key(format!("n-{}-act-{}", n.id, a.key))
                    })
                    .collect::<Vec<_>>())
                .gap(tokens::SPACE_2),
            );
        }

        let stroke = match n.urgency {
            Urgency::Critical => palette.destructive,
            _ => palette.border.with_alpha(0.6),
        };
        let card = column(parts)
            .key(format!("n-{}", n.id))
            .gap(tokens::SPACE_2)
            .padding(Sides::all(tokens::SPACE_3))
            .fill_width()
            .fill(palette.background.with_alpha(0.92))
            .stroke(stroke)
            .radius(12.0);
        if n.urgency == Urgency::Low {
            card.opacity(0.85)
        } else {
            card
        }
    }
}

impl App for NotifyApp {
    fn build(&self, cx: &BuildCx) -> El {
        let palette = cx.palette();

        let mut cards: Vec<El> = self
            .items
            .iter()
            .map(|n| self.card(n, palette))
            .collect();
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

        // The wl_surface is sized to exactly fit this column (the host
        // measures it at the configured width before committing), so
        // the wrapper contributes nothing but the root viewport rect.
        column([column(cards)
            .key(STACK_KEY)
            .gap(self.gap as f32)
            .fill_width()])
        .fill_width()
    }

    fn on_event(&mut self, event: UiEvent) {
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
