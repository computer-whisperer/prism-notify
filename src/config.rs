//! KDL config — `~/.config/prism-notify/config.kdl`.
//!
//! Same stack and style as prism-bar's config (knuffel-decoded KDL,
//! miette diagnostics). A missing file means defaults; a file that
//! fails to parse is a hard error with a source-annotated report —
//! silently falling back to defaults would mask typos.
//!
//! ```kdl
//! // Pin the stack to one output (connector name). Absent → the
//! // compositor picks (normally the focused output).
//! output "DP-1"
//!
//! corner "top-right"    // top-right | top-left | bottom-right | bottom-left
//! width 360             // card width, logical pixels
//! margin 8              // gap to the screen edges, logical pixels
//! gap 8                 // gap between stacked cards, logical pixels
//! max-visible 8         // overflow collapses into a "+N more" line
//! opacity 0.8           // card translucency, 0.0..=1.0 (1.0 = opaque)
//!
//! // Auto-expiry for notifications that don't set their own timeout,
//! // milliseconds; 0 = never expire. Critical notifications never
//! // auto-expire regardless.
//! default-timeout 5000
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, knuffel::Decode)]
pub struct Config {
    /// Output to pin the stack to. None = compositor picks.
    #[knuffel(child, unwrap(argument))]
    pub output: Option<String>,
    /// Screen corner the stack grows from.
    #[knuffel(child, unwrap(argument, str), default)]
    pub corner: Corner,
    /// Card width in logical pixels.
    #[knuffel(child, unwrap(argument), default = 360)]
    pub width: u32,
    /// Margin off the screen edges, logical pixels.
    #[knuffel(child, unwrap(argument), default = 8)]
    pub margin: i32,
    /// Gap between stacked cards, logical pixels.
    #[knuffel(child, unwrap(argument), default = 8)]
    pub gap: u32,
    /// Cards shown at once; the rest collapse into a "+N more" line.
    #[knuffel(child, unwrap(argument), default = 8)]
    pub max_visible: usize,
    /// Card fill translucency, 0.0..=1.0 (1.0 = opaque). Matches
    /// prism-bar's 0.80 panel by default.
    #[knuffel(child, unwrap(argument), default = 0.8)]
    pub opacity: f32,
    /// Expiry for notifications with `expire_timeout = -1` (server
    /// default), in milliseconds. 0 = never expire.
    #[knuffel(child, unwrap(argument), default = 5000)]
    pub default_timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Corner {
    /// Whether the stack is anchored to the bottom edge (newest card
    /// sits nearest the corner, so the stack grows upward).
    pub fn is_bottom(self) -> bool {
        matches!(self, Corner::BottomRight | Corner::BottomLeft)
    }
}

impl std::str::FromStr for Corner {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "top-right" => Ok(Self::TopRight),
            "top-left" => Ok(Self::TopLeft),
            "bottom-right" => Ok(Self::BottomRight),
            "bottom-left" => Ok(Self::BottomLeft),
            other => Err(format!(
                "expected \"top-right\", \"top-left\", \"bottom-right\" or \
                 \"bottom-left\", got \"{other}\""
            )),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: None,
            corner: Corner::TopRight,
            width: 360,
            margin: 8,
            gap: 8,
            max_visible: 8,
            opacity: 0.8,
            default_timeout: 5000,
        }
    }
}

impl Config {
    /// `$PRISM_NOTIFY_CONFIG`, else `$XDG_CONFIG_HOME/prism-notify/config.kdl`,
    /// else `~/.config/prism-notify/config.kdl`.
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("PRISM_NOTIFY_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("prism-notify").join("config.kdl"))
    }

    pub fn load() -> Result<Self> {
        let Some(path) = Self::path() else {
            tracing::warn!("no config path resolvable (no $HOME); using defaults");
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no config at {}; using defaults", path.display());
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err).context(format!("reading {}", path.display()));
            }
        };
        let config = match knuffel::parse::<Config>(&path.to_string_lossy(), &text) {
            Ok(config) => config,
            Err(err) => {
                // miette's fancy renderer points at the offending span.
                anyhow::bail!("config error:\n{:?}", miette::Report::new(err));
            }
        };
        if config.width == 0 {
            anyhow::bail!("config error: width must be positive");
        }
        if !(0.0..=1.0).contains(&config.opacity) {
            anyhow::bail!(
                "config error: opacity must be within 0.0..=1.0, got {}",
                config.opacity
            );
        }
        Ok(config)
    }
}
