# Temporary Hyprland cursor tracking

This downstream workaround exists until [XDPH #444](https://github.com/hyprwm/xdg-desktop-portal-hyprland/issues/444) is resolved and cursor metadata is available in the deployed portal. **Remove the workaround then**, including `hyprland.rs`, its helper integration, the wrapper opt-in, and the Hyprland click binding. Keep the general `cursorEmbedded` sidecar support and `edit --auto-zoom` interface.

Enable only for a Hyprland recording with an embedded system cursor:

```sh
OPENSCREEN_HYPRLAND_CURSOR_WORKAROUND=1 openscreen record --cursor system --project take.openscreen --json
openscreen edit take.openscreen --auto-zoom --json
```

The helper still uses the portal for consent and video capture. It samples the compositor's `j/cursorpos` IPC at no more than 25 Hz, and normalizes logical coordinates against the selected monitor. A separate `.cursor.json` track supplies the existing camera-follow and click-based zoom planner. `cursorEmbedded: true` prevents preview and export from drawing a duplicate cursor. Editable cursor shape, size, smoothing and hiding remain unavailable because the real pointer is baked into the video.

Clicks require this **non-consuming** Hyprland binding (no `input` group access):

```ini
bindni = , mouse:272, event, openscreen-left-button
```

`n` passes the click through to the application; `i` allows modifiers. The helper listens to socket2 only while this recording is active and retains only `custom>>openscreen-left-button` events. Desktop titles and other IPC events are discarded. This path does not open evdev. Portal metadata capture retains OpenScreen's existing native click provider. When removing the binding, configure its normal Linux click permissions (`input` group); portal cursor metadata does not include button events.

Only unrotated **monitor** captures are supported. The helper uses the portal's `mapping_id` when provided. Older portals without that identifier require exactly one active monitor and matching source dimensions; window captures and ambiguous outputs continue recording without a tracking sidecar. Scaling and negative desktop coordinates are supported. Geometry changes or IPC failures stop tracking and emit a warning; they do not abort the video. Portal metadata always takes priority, even when the opt-in remains set.

The system-config wrapper enables this only when Hyprland is running and the portal advertises embedded cursors without metadata. It requests automatic zoom suggestions only when editing is selected. Saving without editing keeps the video exactly as captured.
