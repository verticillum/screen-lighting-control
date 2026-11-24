//! The long-running half: one thread waiting on the compositor, the control socket and signals.

use super::backlight::{self, Backlight};
use super::cli::{self, Command, Request};
use super::core::{self, Core, Now};
use super::wayland::{self, Wayland};
use super::{config_path, socket_path, state_path, ARG_SEPARATOR};
use crate::color;
use crate::config::Ini;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime};

/// How often the schedule is looked at.
const TICK: Duration = Duration::from_secs(30);
const SAVE_DELAY: Duration = Duration::from_secs(1);
/// A client that connects and says nothing is dropped after this.
const CLIENT_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest request line.
const MAX_REQUEST: u64 = 4096;

const NO_GAMMA: &str = "warmth and software dimming need a compositor with wlr-gamma-control (Hyprland, \
                        Sway, niri, ...); only the backlight can be controlled here";

pub fn now() -> Now {
    let unix = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = unix as i64;
    // SAFETY: tm is plain data, and localtime_r only writes to it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    Now {
        unix,
        year: tm.tm_year + 1900,
        day_of_year: tm.tm_yday as u32 + 1,
        minute: tm.tm_hour as f64 * 60.0 + tm.tm_min as f64 + tm.tm_sec as f64 / 60.0,
        utc_offset: tm.tm_gmtoff as f64 / 60.0,
    }
}

/// A file descriptor that becomes readable on SIGINT, SIGTERM or SIGHUP. Call before starting threads.
fn quit_signals() -> io::Result<OwnedFd> {
    // SAFETY: sigset_t is plain data initialized by sigemptyset; the new fd is checked and owned.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        match libc::signalfd(-1, &set, libc::SFD_CLOEXEC) {
            fd if fd < 0 => Err(io::Error::last_os_error()),
            fd => Ok(OwnedFd::from_raw_fd(fd)),
        }
    }
}

/// Waits until one of `fds` has something, and says which do.
fn poll(fds: &[RawFd], timeout: Duration) -> io::Result<Vec<bool>> {
    let mut p: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: the pointer and length describe `p`.
    if unsafe { libc::poll(p.as_mut_ptr(), p.len() as _, ms) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(p.iter().map(|p| p.revents != 0).collect())
}

fn load_settings() -> crate::model::Settings {
    core::settings(&Ini::load(&config_path()).unwrap_or_default())
}

struct Daemon {
    core: Core,
    /// `None` outside Wayland: backlight control only.
    wayland: Option<Wayland>,
    backlight: Backlight,
    watchers: Vec<UnixStream>,
    last_json: String,
    save_at: Option<Instant>,
    quit: bool,
}

pub fn run() -> Result<(), String> {
    let socket = socket_path()?;
    if UnixStream::connect(&socket).is_ok() {
        return Err("SLC is already running".into());
    }
    let signals = quit_signals().map_err(|e| e.to_string())?;
    let wayland = match Wayland::connect() {
        Ok(w) => {
            if !w.has_gamma_control() {
                eprintln!("slc: {NO_GAMMA}");
            }
            Some(w)
        }
        Err(e) => {
            eprintln!("slc: no Wayland compositor ({e}); {NO_GAMMA}");
            None
        }
    };
    let backlight = Backlight::start().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&socket); // left over from a crash
    let listener = UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let mut d = Daemon {
        core: Core::new(load_settings(), Ini::load(&state_path()).unwrap_or_default(), &now()),
        wayland,
        backlight,
        watchers: Vec::new(),
        last_json: String::new(),
        save_at: None,
        quit: false,
    };
    if d.wayland.is_none() {
        backlight::connected().iter().for_each(|name| d.add_display(name, ""));
    }
    let result = d.serve(&listener, &signals);
    d.save();
    let _ = std::fs::remove_file(&socket);
    result.map_err(|e| e.to_string())
}

impl Daemon {
    fn serve(&mut self, listener: &UnixListener, signals: &OwnedFd) -> io::Result<()> {
        let mut next_tick = Instant::now() + TICK;
        while !self.quit {
            self.take_events();
            self.apply()?;
            self.notify();

            let t = Instant::now();
            let mut timeout = next_tick.saturating_duration_since(t);
            if let Some(s) = self.save_at {
                timeout = timeout.min(s.saturating_duration_since(t));
            }
            if let Some(s) = self.core.pause_left(&now()) {
                timeout = timeout.min(Duration::from_secs(s));
            }
            let mut fds = vec![
                signals.as_raw_fd(),
                // poll ignores negative descriptors
                self.wayland.as_ref().map_or(-1, Wayland::fd),
                listener.as_raw_fd(),
                self.backlight.fd(),
            ];
            fds.extend(self.watchers.iter().map(|w| w.as_raw_fd()));
            // Round up so a deadline has really passed when we wake.
            let ready = poll(&fds, timeout + Duration::from_millis(1))?;

            if ready[0] {
                self.quit = true;
            }
            if ready[1] {
                if let Some(w) = &mut self.wayland {
                    w.read(false)?;
                }
            }
            // Watchers never send anything: readable means gone.
            let mut gone = ready[4..].iter();
            self.watchers.retain(|_| !gone.next().copied().unwrap_or(false));
            if ready[2] {
                if let Ok((client, _)) = listener.accept() {
                    let _ = self.serve_client(client);
                }
            }
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + TICK;
                self.backlight.retry();
                if let Some(w) = &mut self.wayland {
                    w.claim()?; // another program may have let go of the gamma
                }
            }
            self.core.tick(&now());
            if self.save_at.is_some_and(|s| Instant::now() >= s) {
                self.save();
            }
        }
        Ok(())
    }

    fn add_display(&mut self, name: &str, description: &str) {
        self.core.add_display(name, description);
        self.backlight.probe(name);
    }

    fn take_events(&mut self) {
        let events = self.wayland.as_mut().map(Wayland::take_events).unwrap_or_default();
        for e in events {
            match e {
                wayland::Event::Added { name, description } => self.add_display(&name, &description),
                wayland::Event::Removed(name) => {
                    self.core.state(); // remember its brightness
                    self.core.remove_display(&name);
                    self.backlight.forget(&name);
                }
                wayland::Event::Gamma { name, works } => self.core.set_gamma(&name, works),
            }
        }
        for e in self.backlight.take_events() {
            match e {
                backlight::Event::Probed { name, level } => self.core.set_hardware(&name, level),
                backlight::Event::Failed { name } => eprintln!("slc: {name}: backlight write failed"),
            }
        }
    }

    fn apply(&mut self) -> io::Result<()> {
        for d in self.core.displays() {
            let o = self.core.output(d);
            if let Some(w) = &mut self.wayland {
                w.set_gamma(&d.name, color::white_point(o.kelvin), o.software)?;
            }
            if let Some(level) = o.hardware {
                self.backlight.set(&d.name, level);
            }
        }
        Ok(())
    }

    /// Tells watchers about a new state.
    fn notify(&mut self) {
        let json = self.core.status_json();
        if json != self.last_json {
            let line = format!("{json}\n");
            self.watchers.retain_mut(|w| w.write_all(line.as_bytes()).is_ok());
            self.last_json = json;
        }
    }

    fn save(&mut self) {
        self.save_at = None;
        if let Err(e) = self.core.state().save(&state_path()) {
            eprintln!("slc: {}: {e}", state_path().display());
        }
    }

    fn serve_client(&mut self, mut client: UnixStream) -> io::Result<()> {
        client.set_read_timeout(Some(CLIENT_TIMEOUT))?;
        client.set_write_timeout(Some(CLIENT_TIMEOUT))?;
        let mut line = String::new();
        BufReader::new(io::Read::take(&client, MAX_REQUEST)).read_line(&mut line)?;
        let args: Vec<String> =
            line.trim_end_matches('\n').split(ARG_SEPARATOR).map(str::to_string).collect();
        match cli::parse(&args) {
            Ok(Command::Request(Request::Watch)) => {
                writeln!(client, "ok\n{}", self.core.status_json())?;
                client.set_nonblocking(true)?;
                self.watchers.push(client);
                Ok(())
            }
            Ok(Command::Request(r)) => match self.answer(&r) {
                Ok(text) => write!(client, "ok\n{text}"),
                Err(e) => write!(client, "error\n{e}\n"),
            },
            Ok(_) | Err(_) => write!(client, "error\nthe daemon does not understand this request\n"),
        }
    }

    fn answer(&mut self, r: &Request) -> Result<String, String> {
        match r {
            Request::Status { json: true } => Ok(format!("{}\n", self.core.status_json())),
            Request::Status { json: false } => Ok(self.core.status_text(&now())),
            Request::Scenes => Ok(self.core.scenes_text()),
            Request::Reload => {
                self.core.set_settings(load_settings(), &now());
                // Device permissions may have changed too.
                for d in self.core.displays() {
                    self.backlight.forget(&d.name);
                    self.backlight.probe(&d.name);
                }
                Ok(String::new())
            }
            Request::Quit => {
                self.quit = true;
                Ok(String::new())
            }
            r => {
                self.core.handle(r, &now())?;
                self.save_at = Some(Instant::now() + SAVE_DELAY);
                Ok(String::new())
            }
        }
    }
}
