# prism-notify

Notification daemon for the
[prism](https://github.com/computer-whisperer/prism) compositor — an
`org.freedesktop.Notifications` server rendered with the
[damascene](https://github.com/computer-whisperer/damascene) UI toolkit on a
`wlr-layer-shell` surface, drawn by wgpu. Works on any compositor that
speaks layer-shell.

zbus serves D-Bus on its own executor thread and feeds the main calloop
loop through a channel; the notification stack, expiry timers, and
animation deadlines all drive redraws through the same loop — no fixed
frame cadence.

## Behavior

- **Cards stack newest-first** at a configurable corner, on a configurable
  output (default: compositor's choice, usually the focused output).
  Overflow beyond `max-visible` collapses into a "+N more" line.
- **Actions**: spec `(key, label)` pairs render as buttons; the special
  `default` action is invoked by clicking the card body instead. The ×
  button dismisses.
- **Icons**: `image-data` → `image-path` → `app_icon` → legacy `icon_data`,
  raw pixels or PNG paths/`file://` URIs. Themed icon names are skipped.
- **Body markup** is stripped to plain text (capabilities advertise `body`,
  `actions`, `icon-static` — not `body-markup`).
- **Urgency**: critical cards get a red stroke, full opacity, and never
  auto-expire; low urgency reads slightly more translucent.
- **Expiry/replacement** per spec: `expire_timeout` -1/0/ms honored,
  `replaces_id` reuses the card and resets its timer.
- **Exclusive bus ownership, loudly.** Startup fails immediately if
  `org.freedesktop.Notifications` is already owned, and the daemon exits if
  it later loses the name (dbus-broker displaces owners silently) — it
  never lingers blind.

## Configuration

KDL, at `$PRISM_NOTIFY_CONFIG`, else
`$XDG_CONFIG_HOME/prism-notify/config.kdl`, else
`~/.config/prism-notify/config.kdl`. A missing file means built-in
defaults; a file that fails to parse is a startup error with
miette-annotated diagnostics — no silent fallback over a typo.

Edits apply live (inotify, 150 ms debounce); a save that fails to parse
logs the error and keeps the current config.

[`resources/default-config.kdl`](resources/default-config.kdl) documents
every option with its default: `output`, `corner`, `width`, `margin`,
`gap`, `max-visible`, `opacity`, `default-timeout`.

## Requirements

- Wayland compositor with `wlr-layer-shell`
- A wgpu-capable GPU (Vulkan on Linux) and system libwayland — wgpu's WSI
  needs raw `wl_display`/`wl_surface` pointers
- A D-Bus session bus; no other notification daemon running

## Building

```
cargo build --release
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
