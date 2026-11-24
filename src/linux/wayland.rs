//! A minimal Wayland client: outputs and `wlr-gamma-control-unstable-v1`, nothing else. The compositor
//! restores normal colors by itself when this connection closes, even if SLC crashes.

use std::fs::File;
use std::io::{self, Error, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
/// Object IDs from here on are allocated in order.
const FIRST_ID: u32 = 3;
/// Larger than any ramp a compositor reports (they use 256–4096).
const MAX_RAMP: u32 = 65_536;

struct Output {
    global: u32,
    object: u32,
    name: String,
    description: String,
    /// The first `done` arrived: name and description are known.
    ready: bool,
    gamma: Option<Gamma>,
}

struct Gamma {
    object: u32,
    /// Ramp entries per channel, once the compositor has told us.
    size: Option<u32>,
    last: Option<([f32; 3], f32)>,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Added {
        name: String,
        description: String,
    },
    Removed(String),
    /// Gamma control works, or was refused or taken away (another program has it).
    Gamma {
        name: String,
        works: bool,
    },
}

pub struct Wayland {
    sock: UnixStream,
    input: Vec<u8>,
    next_id: u32,
    manager: Option<u32>,
    outputs: Vec<Output>,
    events: Vec<Event>,
    pending_sync: Option<u32>,
}

fn socket_path() -> io::Result<PathBuf> {
    let name = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default();
    let p = PathBuf::from(dir).join(name); // an absolute WAYLAND_DISPLAY wins
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(Error::new(ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))
    }
}

/// Reads arguments of an event.
struct Args<'a>(&'a [u8]);

impl Args<'_> {
    fn u32(&mut self) -> io::Result<u32> {
        let (a, rest) = self.0.split_first_chunk::<4>().ok_or(ErrorKind::InvalidData)?;
        self.0 = rest;
        Ok(u32::from_ne_bytes(*a))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.0.get(..len).ok_or(ErrorKind::InvalidData)?;
        let text = String::from_utf8_lossy(bytes.strip_suffix(b"\0").unwrap_or(bytes)).into_owned();
        self.0 = self.0.get(len.next_multiple_of(4)..).unwrap_or(&[]);
        Ok(text)
    }
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend((s.len() as u32 + 1).to_ne_bytes());
    out.extend(s.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
}

/// A gamma table for `size` entries per channel: red, green, blue, native-endian u16.
pub fn ramp(size: u32, white: [f32; 3], scale: f32) -> Vec<u8> {
    let last = size.saturating_sub(1).max(1) as f32;
    white
        .iter()
        .flat_map(|w| {
            (0..size).flat_map(move |i| {
                let v = i as f32 / last * w * scale;
                ((v * 65535.0).round().clamp(0.0, 65535.0) as u16).to_ne_bytes()
            })
        })
        .collect()
}

impl Wayland {
    pub fn connect() -> io::Result<Wayland> {
        let mut w = Wayland {
            sock: UnixStream::connect(socket_path()?)?,
            input: Vec::new(),
            next_id: FIRST_ID,
            manager: None,
            outputs: Vec::new(),
            events: Vec::new(),
            pending_sync: None,
        };
        w.request(DISPLAY, 1, &REGISTRY.to_ne_bytes(), None)?;
        // Globals, then the outputs' details, then their gamma sizes.
        for _ in 0..3 {
            w.roundtrip()?;
        }
        Ok(w)
    }

    /// The compositor supports gamma control at all.
    pub fn has_gamma_control(&self) -> bool {
        self.manager.is_some()
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn roundtrip(&mut self) -> io::Result<()> {
        let id = self.new_id();
        self.request(DISPLAY, 0, &id.to_ne_bytes(), None)?;
        self.pending_sync = Some(id);
        while self.pending_sync.is_some() {
            self.read(true)?;
        }
        Ok(())
    }

    fn request(&self, object: u32, opcode: u16, args: &[u8], fd: Option<RawFd>) -> io::Result<()> {
        let mut msg = Vec::with_capacity(8 + args.len());
        msg.extend(object.to_ne_bytes());
        msg.extend((((8 + args.len()) as u32) << 16 | opcode as u32).to_ne_bytes());
        msg.extend(args);
        let mut iov = libc::iovec { iov_base: msg.as_mut_ptr().cast(), iov_len: msg.len() };
        // u64 so the buffer is aligned for cmsghdr.
        let mut control = [0u64; 8];
        // SAFETY: zeroed is a valid msghdr; every pointer stored in it outlives the sendmsg call, and
        // the control buffer is larger than CMSG_SPACE of one fd.
        let sent = unsafe {
            let mut h: libc::msghdr = std::mem::zeroed();
            h.msg_iov = &mut iov;
            h.msg_iovlen = 1;
            if let Some(fd) = fd {
                let len = size_of::<RawFd>() as u32;
                h.msg_control = control.as_mut_ptr().cast();
                h.msg_controllen = libc::CMSG_SPACE(len) as _;
                let c = libc::CMSG_FIRSTHDR(&h);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(len) as _;
                libc::CMSG_DATA(c).cast::<RawFd>().write_unaligned(fd);
            }
            libc::sendmsg(self.sock.as_raw_fd(), &h, libc::MSG_NOSIGNAL)
        };
        match sent {
            n if n < 0 => Err(Error::last_os_error()),
            n if n as usize != msg.len() => Err(ErrorKind::WriteZero.into()),
            _ => Ok(()),
        }
    }

    fn bind(&mut self, global: u32, interface: &str, version: u32) -> io::Result<u32> {
        let id = self.new_id();
        let mut a = global.to_ne_bytes().to_vec();
        put_string(&mut a, interface);
        a.extend(version.to_ne_bytes());
        a.extend(id.to_ne_bytes());
        self.request(REGISTRY, 0, &a, None)?;
        Ok(id)
    }

    /// Asks for gamma control of every ready output that lacks it.
    pub fn claim(&mut self) -> io::Result<()> {
        let Some(manager) = self.manager else { return Ok(()) };
        for i in 0..self.outputs.len() {
            if self.outputs[i].ready && self.outputs[i].gamma.is_none() {
                let id = self.new_id();
                let mut a = id.to_ne_bytes().to_vec();
                a.extend(self.outputs[i].object.to_ne_bytes());
                self.request(manager, 0, &a, None)?;
                self.outputs[i].gamma = Some(Gamma { object: id, size: None, last: None });
            }
        }
        Ok(())
    }

    /// Handles what the compositor has sent; `wait` blocks for at least one read.
    pub fn read(&mut self, wait: bool) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        let flags = if wait { 0 } else { libc::MSG_DONTWAIT };
        // SAFETY: the buffer is valid for its whole length.
        let n = unsafe { libc::recv(self.sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), flags) };
        if n < 0 {
            let e = Error::last_os_error();
            return match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::Interrupted => Ok(()),
                _ => Err(e),
            };
        }
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "the compositor closed the connection"));
        }
        self.input.extend(&buf[..n as usize]);
        while self.input.len() >= 8 {
            let mut head = Args(&self.input);
            let (object, word) = (head.u32()?, head.u32()?);
            let (size, opcode) = ((word >> 16) as usize, word & 0xffff);
            if size < 8 {
                return Err(ErrorKind::InvalidData.into());
            }
            if self.input.len() < size {
                break;
            }
            let msg: Vec<u8> = self.input.drain(..size).collect();
            self.dispatch(object, opcode, Args(&msg[8..]))?;
        }
        Ok(())
    }

    fn dispatch(&mut self, object: u32, opcode: u32, mut a: Args) -> io::Result<()> {
        match (object, opcode) {
            (DISPLAY, 0) => {
                let (_, _, text) = (a.u32()?, a.u32()?, a.string()?);
                return Err(Error::other(format!("Wayland error: {text}")));
            }
            (DISPLAY, _) => {}
            (REGISTRY, 0) => {
                let (global, interface, version) = (a.u32()?, a.string()?, a.u32()?);
                match interface.as_str() {
                    "wl_output" if version >= 4 => {
                        let object = self.bind(global, "wl_output", 4)?;
                        self.outputs.push(Output {
                            global,
                            object,
                            name: String::new(),
                            description: String::new(),
                            ready: false,
                            gamma: None,
                        });
                    }
                    "zwlr_gamma_control_manager_v1" => {
                        self.manager = Some(self.bind(global, "zwlr_gamma_control_manager_v1", 1)?);
                        self.claim()?;
                    }
                    _ => {}
                }
            }
            (REGISTRY, _) => {
                let global = a.u32()?;
                if let Some(i) = self.outputs.iter().position(|o| o.global == global) {
                    let o = self.outputs.remove(i);
                    if let Some(g) = o.gamma {
                        self.request(g.object, 1, &[], None)?;
                    }
                    if o.ready {
                        self.events.push(Event::Removed(o.name));
                    }
                }
            }
            _ => self.dispatch_output(object, opcode, a)?,
        }
        Ok(())
    }

    fn dispatch_output(&mut self, object: u32, opcode: u32, mut a: Args) -> io::Result<()> {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.object == object) {
            match opcode {
                2 if !o.ready => {
                    o.ready = true;
                    let (name, description) = (o.name.clone(), o.description.clone());
                    self.events.push(Event::Added { name, description });
                    self.claim()?;
                }
                4 => o.name = a.string()?,
                5 => o.description = a.string()?,
                _ => {}
            }
            return Ok(());
        }
        let found = self.outputs.iter_mut().find(|o| o.gamma.as_ref().is_some_and(|g| g.object == object));
        if let Some(o) = found {
            let name = o.name.clone();
            if opcode == 0 {
                let size = a.u32()?;
                if !(2..=MAX_RAMP).contains(&size) {
                    return Err(Error::other(format!("bad gamma size {size}")));
                }
                if let Some(g) = &mut o.gamma {
                    g.size = Some(size);
                }
                self.events.push(Event::Gamma { name, works: true });
            } else {
                o.gamma = None;
                self.request(object, 1, &[], None)?;
                self.events.push(Event::Gamma { name, works: false });
            }
            return Ok(());
        }
        // Late events of destroyed objects end up here too, and don't match.
        if self.pending_sync == Some(object) {
            self.pending_sync = None;
        }
        Ok(())
    }

    /// Shows a white point and brightness on one output. Does nothing if it already shows them.
    pub fn set_gamma(&mut self, name: &str, white: [f32; 3], scale: f32) -> io::Result<()> {
        let Some(g) = self.outputs.iter().find(|o| o.name == name).and_then(|o| o.gamma.as_ref()) else {
            return Ok(());
        };
        let Some(size) = g.size.filter(|_| g.last != Some((white, scale))) else { return Ok(()) };
        // SAFETY: the name is a valid C string; the result is checked before it is owned.
        let file = unsafe {
            let fd = libc::memfd_create(c"slc-gamma".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                return Err(Error::last_os_error());
            }
            File::from(OwnedFd::from_raw_fd(fd))
        };
        // Positioned write: compositors read from the shared file offset, which must stay at 0.
        file.write_all_at(&ramp(size, white, scale), 0)?;
        self.request(g.object, 0, &[], Some(file.as_raw_fd()))?;
        if let Some(g) = self.outputs.iter_mut().find(|o| o.name == name).and_then(|o| o.gamma.as_mut()) {
            g.last = Some((white, scale));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_layout() {
        let r = ramp(3, [1.0, 0.5, 0.0], 0.5);
        let v: Vec<u16> = r.chunks(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect();
        assert_eq!(v, [0, 16384, 32768, 0, 8192, 16384, 0, 0, 0]);
    }

    #[test]
    fn strings_are_padded_and_bounded() {
        let mut b = Vec::new();
        put_string(&mut b, "DP-2");
        assert_eq!(b.len(), 12);
        b.extend(7u32.to_ne_bytes());
        let mut a = Args(&b);
        assert_eq!((a.string().unwrap().as_str(), a.u32().unwrap()), ("DP-2", 7));
        assert!(Args(&100u32.to_ne_bytes()).string().is_err());
    }
}
