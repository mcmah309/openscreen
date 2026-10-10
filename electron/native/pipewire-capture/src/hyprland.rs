//! Temporary cursor tracking provider for Hyprland monitor captures.
//!
//! Remove once XDPH implements cursor metadata and it is available in supported
//! installations: https://github.com/hyprwm/xdg-desktop-portal-hyprland/issues/444
//! Portal METADATA remains preferred. This opt-in fallback keeps the cursor
//! embedded; it supplies camera tracking, never a replacement cursor sprite.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{events::timestamp_ms, Message};

const IO_TIMEOUT: Duration = Duration::from_millis(100);
const MIN_INTERVAL: Duration = Duration::from_millis(40);
const MAX_REPLY_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Monitor {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    transform: u32,
    #[serde(default)]
    disabled: bool,
}

impl Monitor {
    fn logical_size(&self) -> Option<(f64, f64)> {
        // Rotated buffers differ between Hyprland releases. Decline rather than
        // attach a plausible but incorrect track until that mapping is verified.
        if self.disabled
            || self.transform != 0
            || !self.scale.is_finite()
            || self.scale <= 0.0
            || !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
        {
            return None;
        }
        Some((self.width / self.scale, self.height / self.scale))
    }

    fn normalize(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (w, h) = self.logical_size()?;
        (x.is_finite() && y.is_finite()).then_some(((x - self.x) / w, (y - self.y) / h))
    }
}

fn choose_monitor(
    monitors: &[Monitor],
    mapping_id: Option<&str>,
    size: Option<(i32, i32)>,
) -> Result<Monitor, String> {
    let candidates: Vec<_> = monitors.iter().filter(|m| !m.disabled).collect();
    let monitor = if let Some(name) = mapping_id {
        candidates.iter().find(|m| m.name == name).copied()
    } else if candidates.len() == 1 {
        // Older XDPH has no mapping_id and reports every monitor at (0,0).
        // Only one active output is unambiguous; never guess by resolution.
        candidates.first().copied()
    } else {
        None
    }
    .ok_or("the selected monitor cannot be identified reliably")?;
    let logical = monitor
        .logical_size()
        .ok_or("rotated or invalid monitor geometry is unsupported")?;
    let (w, h) = size.ok_or("the portal did not report the selected source size")?;
    let matches =
        |size: (f64, f64)| (size.0 - w as f64).abs() <= 1.0 && (size.1 - h as f64).abs() <= 1.0;
    if !matches(logical) && !matches((monitor.width, monitor.height)) {
        return Err("the portal source size does not match the selected monitor".into());
    }
    Ok(monitor.clone())
}

fn query(socket: &Path, command: &str) -> Result<String, String> {
    let mut connection = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    connection
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    connection
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    connection
        .write_all(command.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    connection
        .take(MAX_REPLY_BYTES + 1)
        .read_to_string(&mut reply)
        .map_err(|e| e.to_string())?;
    if reply.len() > MAX_REPLY_BYTES as usize {
        return Err("Hyprland IPC reply is too large".into());
    }
    Ok(reply)
}

fn socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is missing")?;
    let signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| "Hyprland is not running")?;
    if signature.is_empty() || signature.contains('/') || signature == "." || signature == ".." {
        return Err("invalid Hyprland instance signature".into());
    }
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock"))
}

// A non-consuming Hyprland binding emits this event; it never opens evdev or
// intercepts the application's click. Only this event is retained from socket2.
const CLICK_EVENT: &[u8] = b"custom>>openscreen-left-button";

#[derive(Default)]
struct ClickEvents {
    pending: Vec<u8>,
}

impl ClickEvents {
    fn drain(&mut self, socket: &mut UnixStream) -> Result<usize, String> {
        let mut count = 0;
        let mut buffer = [0; 4096];
        // Bound the work even when a busy desktop produces continuous events.
        for _ in 0..16 {
            match socket.read(&mut buffer) {
                Ok(0) => return Err("Hyprland event socket closed".into()),
                Ok(n) => {
                    self.pending.extend_from_slice(&buffer[..n]);
                    while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                        if &self.pending[..end] == CLICK_EVENT {
                            count += 1;
                        }
                        self.pending.drain(..=end);
                    }
                    if self.pending.len() > 65536 {
                        return Err("Hyprland event is too large".into());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(count)
    }
}

pub struct Sampler(Arc<AtomicBool>);

impl Drop for Sampler {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn start(
    mapping_id: Option<String>,
    size: Option<(i32, i32)>,
    interval: Duration,
    sender: Sender<Message>,
) -> Sampler {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let socket = socket_path()?;
            let mut clicks = UnixStream::connect(socket.with_file_name(".socket2.sock"))
                .map_err(|e| e.to_string())?;
            clicks.set_nonblocking(true).map_err(|e| e.to_string())?;
            let mut click_events = ClickEvents::default();
            let read_monitor = || -> Result<Monitor, String> {
                let monitors: Vec<Monitor> = serde_json::from_str(&query(&socket, "j/monitors")?)
                    .map_err(|e| e.to_string())?;
                choose_monitor(&monitors, mapping_id.as_deref(), size)
            };
            let monitor = read_monitor()?;
            let mut geometry_checked = Instant::now();
            while !stopped.load(Ordering::Relaxed) {
                let began = Instant::now();
                if geometry_checked.elapsed() >= Duration::from_secs(1) {
                    if read_monitor()? != monitor {
                        return Err("monitor geometry changed during recording".into());
                    }
                    geometry_checked = Instant::now();
                }
                #[derive(Deserialize)]
                struct Position {
                    x: f64,
                    y: f64,
                }
                let position: Position = serde_json::from_str(&query(&socket, "j/cursorpos")?)
                    .map_err(|e| e.to_string())?;
                let (cx, cy) = monitor
                    .normalize(position.x, position.y)
                    .ok_or("invalid cursor coordinates")?;
                if sender
                    .send(Message::HyprlandPointer {
                        cx,
                        cy,
                        timestamp_ms: timestamp_ms(),
                    })
                    .is_err()
                {
                    break;
                }
                for _ in 0..click_events.drain(&mut clicks)? {
                    if sender.send(Message::PointerButton(timestamp_ms())).is_err() {
                        return Ok(());
                    }
                }
                // Direct IPC, one query at a time, at most 25 Hz. Never queue
                // queries or launch a hyprctl subprocess for each sample.
                std::thread::sleep(interval.max(MIN_INTERVAL).saturating_sub(began.elapsed()));
            }
            Ok(())
        })();
        if let Err(message) = result {
            let _ = sender.send(Message::HyprlandUnavailable(message));
        }
    });
    Sampler(stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: f64, scale: f64) -> Monitor {
        Monitor {
            name: name.into(),
            x,
            y: -100.0,
            width: 1920.0,
            height: 1080.0,
            scale,
            transform: 0,
            disabled: false,
        }
    }

    #[test]
    fn maps_global_logical_coordinates_and_keeps_outside_positions() {
        let m = monitor("DP-1", -1280.0, 1.5);
        assert_eq!(m.normalize(-640.0, 260.0), Some((0.5, 0.5)));
        assert_eq!(m.normalize(-1408.0, -100.0), Some((-0.1, 0.0)));
        assert_eq!(m.normalize(f64::NAN, 0.0), None);
    }

    #[test]
    fn identifies_only_unambiguous_outputs() {
        let a = monitor("DP-1", 0.0, 1.0);
        let b = monitor("DP-2", 1920.0, 1.0);
        assert!(choose_monitor(&[a.clone(), b.clone()], None, Some((1920, 1080))).is_err());
        assert_eq!(
            choose_monitor(&[a.clone(), b], Some("DP-1"), Some((1920, 1080))).unwrap(),
            a
        );
        assert!(choose_monitor(&[a.clone()], Some("unknown"), Some((1920, 1080))).is_err());
        assert!(choose_monitor(&[a], None, Some((1280, 720))).is_err());
    }

    #[test]
    fn accepts_portal_logical_or_pixel_size_and_rejects_rotation() {
        let mut m = monitor("DP-1", 0.0, 1.5);
        assert!(choose_monitor(&[m.clone()], None, Some((1280, 720))).is_ok());
        assert!(choose_monitor(&[m.clone()], None, Some((1920, 1080))).is_ok());
        m.transform = 1;
        assert!(choose_monitor(&[m], None, Some((1080, 1920))).is_err());
    }

    #[test]
    fn click_events_accept_only_our_binding_and_preserve_partial_lines() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut events = ClickEvents::default();
        writer
            .write_all(b"activewindow>>ignored,title\ncustom>>openscreen-left-")
            .unwrap();
        assert_eq!(events.drain(&mut reader).unwrap(), 0);
        writer
            .write_all(b"button\ncustom>>other\ncustom>>openscreen-left-button\n")
            .unwrap();
        assert_eq!(events.drain(&mut reader).unwrap(), 2);
        assert!(events.pending.is_empty());
        assert_eq!(events.drain(&mut reader).unwrap(), 0);
    }

    #[test]
    fn socket_queries_use_json_commands_and_bound_replies() {
        use std::os::unix::net::UnixListener;
        let dir =
            std::env::temp_dir().join(format!("openscreen-hyprland-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("test.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut command = [0; 11];
            conn.read_exact(&mut command).unwrap();
            assert_eq!(&command, b"j/cursorpos");
            conn.write_all(br#"{"x":320,"y":240}"#).unwrap();
        });
        assert_eq!(
            query(&socket, "j/cursorpos").unwrap(),
            r#"{"x":320,"y":240}"#
        );
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
