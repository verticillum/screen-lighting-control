//! Real brightness: DDC/CI (VCP 0x10) over i2c for external displays, `/sys/class/backlight` for laptop
//! panels. Device I/O is slow (tens of ms per message), so it runs on a worker thread.

use crate::engine::{from_device, to_device};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::sleep;
use std::time::Duration;

const DRM: &str = "/sys/class/drm";
const I2C_SLAVE: libc::c_ulong = 0x0703;
const DDC_ADDRESS: u8 = 0x37;
const VCP_BRIGHTNESS: u8 = 0x10;
/// Displays need this long to prepare an answer, or to take a command.
const DDC_WAIT: Duration = Duration::from_millis(50);
/// Some displays drop the first message after idling.
const ATTEMPTS: u32 = 3;
/// Writes happen once nothing newer arrived for this long (they wear the display's memory).
const DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
pub enum Event {
    /// The level (0–100) the display has now, or `None` if its backlight cannot be controlled.
    Probed {
        name: String,
        level: Option<f32>,
    },
    Failed {
        name: String,
    },
}

enum Job {
    Probe(String),
    Set(String, f32),
}

/// Talks to the worker. Dropping it lets queued writes finish.
pub struct Backlight {
    jobs: Option<Sender<Job>>,
    events: Receiver<Event>,
    wake: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Last level asked for, per display.
    last: HashMap<String, f32>,
    /// Displays whose last write failed: tried again on `retry`.
    failed: Vec<String>,
}

impl Backlight {
    pub fn start() -> io::Result<Backlight> {
        let (jobs, job_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let (wake, wake_tx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let thread = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
            worker(&job_rx, |e| {
                let _ = event_tx.send(e);
                let _ = (&wake_tx).write(&[0]);
            });
        })?;
        Ok(Backlight {
            jobs: Some(jobs),
            events,
            wake,
            thread: Some(thread),
            last: HashMap::new(),
            failed: Vec::new(),
        })
    }

    /// Readable when there are events.
    pub fn fd(&self) -> i32 {
        self.wake.as_raw_fd()
    }

    fn send(&self, j: Job) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(j);
        }
    }

    pub fn probe(&self, name: &str) {
        self.send(Job::Probe(name.into()));
    }

    pub fn set(&mut self, name: &str, level: f32) {
        if self.last.get(name) != Some(&level) {
            self.last.insert(name.into(), level);
            self.send(Job::Set(name.into(), level));
        }
    }

    pub fn forget(&mut self, name: &str) {
        self.last.remove(name);
    }

    /// Lets failed writes be tried again.
    pub fn retry(&mut self) {
        for name in std::mem::take(&mut self.failed) {
            self.last.remove(&name);
        }
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        let mut buf = [0; 64];
        while (&self.wake).read(&mut buf).is_ok_and(|n| n == buf.len()) {}
        let events: Vec<Event> = self.events.try_iter().collect();
        for e in &events {
            if let Event::Failed { name } = e {
                self.failed.push(name.clone());
            }
        }
        events
    }
}

impl Drop for Backlight {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn worker(jobs: &Receiver<Job>, report: impl Fn(Event)) {
    let mut devices: HashMap<String, Device> = HashMap::new();
    while let Ok(first) = jobs.recv() {
        let mut levels: HashMap<String, f32> = HashMap::new();
        let mut next = Ok(first);
        loop {
            match next {
                Ok(Job::Probe(name)) => {
                    devices.remove(&name);
                    let found = Device::find(&name);
                    let level = found.as_ref().map(|(_, l)| *l);
                    if let Some((d, _)) = found {
                        devices.insert(name.clone(), d);
                    }
                    report(Event::Probed { name, level });
                }
                Ok(Job::Set(name, level)) => {
                    levels.insert(name, level);
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
            next = jobs.recv_timeout(if levels.is_empty() { Duration::ZERO } else { DEBOUNCE });
        }
        for (name, level) in levels {
            if let Some(d) = devices.get_mut(&name) {
                if d.set(level).is_err() {
                    report(Event::Failed { name });
                }
            }
        }
    }
}

enum Device {
    Ddc { bus: File, max: u32, last: u32 },
    Panel { file: PathBuf, max: u32 },
}

impl Device {
    /// Opens the display on a connector and reads its level (0–100).
    fn find(connector: &str) -> Option<(Device, f32)> {
        let dir = connector_dir(connector)?;
        if let Some(panel) = panel_dir(&dir, connector) {
            let max = read_number(&panel.join("max_brightness"))?;
            let file = panel.join("brightness");
            let now = read_number(&file)?;
            OpenOptions::new().write(true).open(&file).ok()?;
            return Some((Device::Panel { file, max }, from_device(now, 0, max)));
        }
        let bus = fs::canonicalize(dir.join("ddc")).ok()?;
        let dev = Path::new("/dev").join(bus.file_name()?);
        let mut bus = OpenOptions::new().read(true).write(true).open(dev).ok()?;
        // SAFETY: I2C_SLAVE takes the address by value.
        if unsafe { libc::ioctl(bus.as_raw_fd(), I2C_SLAVE as _, DDC_ADDRESS as libc::c_ulong) } < 0 {
            return None;
        }
        let (now, max) = attempt(|| ddc_get(&mut bus)).ok()?;
        (max > 0).then(|| (Device::Ddc { bus, max, last: now }, from_device(now, 0, max)))
    }

    fn set(&mut self, level: f32) -> io::Result<()> {
        match self {
            Device::Ddc { bus, max, last } => {
                let v = to_device(level, 0, *max);
                if v != *last {
                    attempt(|| ddc_set(bus, v as u16))?;
                    *last = v;
                }
                Ok(())
            }
            // Never 0: many panels turn off completely.
            Device::Panel { file, max } => fs::write(file, to_device(level, 1, (*max).max(1)).to_string()),
        }
    }
}

fn attempt<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut result = f();
    for _ in 1..ATTEMPTS {
        if result.is_ok() {
            break;
        }
        sleep(DDC_WAIT);
        result = f();
    }
    result
}

fn read_number(p: &Path) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// `card1-DP-2` → `DP-2`.
fn connector_of(entry: &str) -> Option<&str> {
    let (card, connector) = entry.split_once('-')?;
    card.starts_with("card").then_some(connector)
}

fn drm_entries() -> Vec<(String, PathBuf)> {
    let Ok(dir) = fs::read_dir(DRM) else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| Some((connector_of(e.file_name().to_str()?)?.to_string(), e.path())))
        .collect()
}

fn is_connected(dir: &Path) -> bool {
    fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "connected")
}

fn connector_dir(connector: &str) -> Option<PathBuf> {
    // With several graphics cards, connector names repeat: take the connected one.
    drm_entries().into_iter().find(|(c, p)| c == connector && is_connected(p)).map(|(_, p)| p)
}

/// Connectors with a display, for sessions where the compositor can't tell us.
pub fn connected() -> Vec<String> {
    drm_entries().into_iter().filter(|(_, p)| is_connected(p)).map(|(c, _)| c).collect()
}

/// The laptop panel's backlight, if this connector is one.
fn panel_dir(connector_dir: &Path, connector: &str) -> Option<PathBuf> {
    let panels: Vec<PathBuf> =
        fs::read_dir("/sys/class/backlight").ok()?.flatten().map(|e| e.path()).collect();
    let here = fs::canonicalize(connector_dir).ok();
    let own = panels.iter().find(|p| fs::canonicalize(p.join("device")).ok() == here && here.is_some());
    let internal = ["eDP", "LVDS", "DSI"].iter().any(|k| connector.starts_with(k));
    own.or(if internal { panels.first() } else { None }).cloned()
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |a, b| a ^ b)
}

/// A host → display message: source, length, payload, checksum.
fn message(payload: &[u8]) -> Vec<u8> {
    let mut m = vec![0x51, 0x80 | payload.len() as u8];
    m.extend(payload);
    m.push(checksum(DDC_ADDRESS << 1, &m));
    m
}

/// Current and maximum value from a brightness reply.
fn parse_reply(r: &[u8; 11]) -> Option<(u32, u32)> {
    let good = r[1] == 0x88
        && r[2] == 0x02
        && r[3] == 0
        && r[4] == VCP_BRIGHTNESS
        && checksum(0x50, &r[..10]) == r[10];
    let word = |i: usize| u32::from(r[i]) << 8 | u32::from(r[i + 1]);
    good.then(|| (word(8), word(6)))
}

fn ddc_get(bus: &mut File) -> io::Result<(u32, u32)> {
    bus.write_all(&message(&[0x01, VCP_BRIGHTNESS]))?;
    sleep(DDC_WAIT);
    let mut r = [0; 11];
    bus.read_exact(&mut r)?;
    sleep(DDC_WAIT);
    parse_reply(&r).ok_or_else(|| ErrorKind::InvalidData.into())
}

fn ddc_set(bus: &mut File, v: u16) -> io::Result<()> {
    let [hi, lo] = v.to_be_bytes();
    bus.write_all(&message(&[0x03, VCP_BRIGHTNESS, hi, lo]))?;
    sleep(DDC_WAIT);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddc_messages() {
        // The well-known "get brightness" and "set brightness to 50" byte strings.
        assert_eq!(message(&[0x01, 0x10]), [0x51, 0x82, 0x01, 0x10, 0xac]);
        assert_eq!(message(&[0x03, 0x10, 0x00, 0x32]), [0x51, 0x84, 0x03, 0x10, 0x00, 0x32, 0x9a]);
    }

    #[test]
    fn ddc_replies() {
        let mut r = [0x6e, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x4b, 0x00];
        r[10] = 0x6e ^ 0x50 ^ 0x88 ^ 0x02 ^ 0x10 ^ 0x64 ^ 0x4b;
        assert_eq!(parse_reply(&r), Some((75, 100)));
        r[9] = 0x4c;
        assert_eq!(parse_reply(&r), None, "bad checksum");
        assert_eq!(parse_reply(&[0; 11]), None, "no answer");
    }

    #[test]
    fn connector_names() {
        assert_eq!(connector_of("card1-DP-2"), Some("DP-2"));
        assert_eq!(connector_of("card0-HDMI-A-1"), Some("HDMI-A-1"));
        assert_eq!(connector_of("card1"), None);
        assert_eq!(connector_of("renderD128"), None);
    }
}
