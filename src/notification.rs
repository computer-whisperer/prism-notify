//! Notification model + wire-format decoding.
//!
//! Everything here runs on the zbus executor thread (inside `Notify`
//! handlers) so the main loop receives ready-to-render values: body
//! markup already stripped, image hints already converted to a
//! damascene [`Image`].

use std::time::Duration;

use damascene_core::image::Image;

/// Freedesktop urgency hint (byte 0/1/2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// One action from the `actions` wire array, `(key, label)` pairs.
/// The conventional `"default"` action is split out: it has no button,
/// it's what a click on the card body invokes.
#[derive(Debug, Clone)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// `NotificationClosed` reason codes (spec §Signals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    CloseCall = 3,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    /// Markup-stripped, entity-unescaped body.
    pub body: String,
    pub urgency: Urgency,
    /// Button actions, in wire order, `default` excluded.
    pub actions: Vec<Action>,
    /// Whether the wire actions included a `default` entry (invoked by
    /// clicking the card body).
    pub has_default_action: bool,
    pub image: Option<Image>,
    /// Raw wire value: -1 = server default, 0 = never, >0 ms.
    pub expire_timeout: i32,
}

impl Notification {
    /// Resolve the wire timeout against the configured default.
    /// `None` = never expires. Critical notifications never auto-expire.
    pub fn timeout(&self, default_ms: u64) -> Option<Duration> {
        if self.urgency == Urgency::Critical {
            return None;
        }
        match self.expire_timeout {
            0 => None,
            ms if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => (default_ms > 0).then(|| Duration::from_millis(default_ms)),
        }
    }
}

/// Split the flat `[key, label, key, label, ...]` wire array.
pub fn parse_actions(raw: &[String]) -> (Vec<Action>, bool) {
    let mut actions = Vec::new();
    let mut has_default = false;
    for pair in raw.chunks_exact(2) {
        if pair[0] == "default" {
            has_default = true;
        } else {
            actions.push(Action {
                key: pair[0].clone(),
                label: pair[1].clone(),
            });
        }
    }
    (actions, has_default)
}

/// Convert the `image-data` hint structure (raw pixels: width, height,
/// rowstride, has_alpha, bits_per_sample, channels, data) to RGBA8.
/// Returns None for layouts we don't speak (16-bit, palettes, ...).
pub fn image_from_data(
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    bits_per_sample: i32,
    channels: i32,
    data: &[u8],
) -> Option<Image> {
    if width <= 0 || height <= 0 || rowstride <= 0 || bits_per_sample != 8 {
        return None;
    }
    let (w, h, stride) = (width as usize, height as usize, rowstride as usize);
    let ch = match (channels, has_alpha) {
        (3, false) => 3,
        (4, true) => 4,
        _ => return None,
    };
    if stride < w * ch || data.len() < stride * (h - 1) + w * ch {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in data.chunks(stride).take(h) {
        for px in row[..w * ch].chunks_exact(ch) {
            rgba.extend_from_slice(&px[..3]);
            rgba.push(if ch == 4 { px[3] } else { 0xff });
        }
    }
    Some(Image::from_rgba8(width as u32, height as u32, rgba))
}

/// Load an icon referenced by path (`app_icon` / `image-path` hint).
/// Accepts absolute paths and `file://` URIs; PNG only — themed icon
/// names are logged and skipped (no icon-theme lookup yet).
pub fn image_from_path(path: &str) -> Option<Image> {
    let path = path.strip_prefix("file://").unwrap_or(path);
    if !path.starts_with('/') {
        if !path.is_empty() {
            tracing::debug!(icon = %path, "themed icon names not supported; skipping");
        }
        return None;
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(err) => {
            tracing::debug!(%path, %err, "icon file unreadable; skipping");
            return None;
        }
    };
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(%path, %err, "not a decodable png; skipping");
            return None;
        }
    };
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba if info.bit_depth == png::BitDepth::Eight => buf,
        png::ColorType::Rgb if info.bit_depth == png::BitDepth::Eight => {
            let mut rgba = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(px);
                rgba.push(0xff);
            }
            rgba
        }
        other => {
            tracing::debug!(%path, ?other, "unsupported png color type; skipping");
            return None;
        }
    };
    Some(Image::from_rgba8(info.width, info.height, rgba))
}

/// Strip the spec's body markup (`<b> <i> <u> <a> <img>` plus whatever
/// else clients send) down to plain text: tags dropped, `<br>` becomes
/// a newline, the five XML entities unescaped.
pub fn strip_markup(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                let tag: String = chars.by_ref().take_while(|&c| c != '>').collect();
                let name = tag.trim_start_matches('/');
                if name.starts_with("br") {
                    out.push('\n');
                }
            }
            '&' => {
                let mut entity = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ';' {
                        chars.next();
                        break;
                    }
                    if !c.is_ascii_alphanumeric() && c != '#' || entity.len() > 6 {
                        // Not an entity — emit what we swallowed.
                        break;
                    }
                    entity.push(c);
                    chars.next();
                }
                match entity.as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "apos" => out.push('\''),
                    "quot" => out.push('"'),
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_stripped() {
        assert_eq!(strip_markup("<b>bold</b> &amp; <i>x</i>"), "bold & x");
        assert_eq!(strip_markup("a<br/>b"), "a\nb");
        assert_eq!(strip_markup("AT&T 5 &lt; 6"), "AT&T 5 < 6");
        assert_eq!(strip_markup("plain"), "plain");
    }

    #[test]
    fn actions_split() {
        let raw = vec![
            "default".into(),
            "Open".into(),
            "mark-read".into(),
            "Mark read".into(),
        ];
        let (actions, has_default) = parse_actions(&raw);
        assert!(has_default);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].key, "mark-read");
    }

    #[test]
    fn image_data_rgb_to_rgba() {
        // 2x1 RGB with 8 bytes rowstride (2 pad bytes).
        let img = image_from_data(2, 1, 8, false, 8, 3, &[1, 2, 3, 4, 5, 6, 0, 0]).unwrap();
        assert_eq!((img.width(), img.height()), (2, 1));
        assert_eq!(img.pixels(), &[1, 2, 3, 0xff, 4, 5, 6, 0xff]);
    }
}
