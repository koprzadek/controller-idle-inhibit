//! Minimal Wayland client: enough to bind a handful of globals, drive a
//! single invisible layer surface and create/destroy idle inhibitors.
//!
//! Deliberately hand-rolled (no wayland-client crate) so the daemon builds
//! with zero native dependencies and stays a small binary. Protocol details
//! were verified against:
//! - `protocol/wayland.xml` (core)
//! - `unstable/wlr-layer-shell-unstable-v1.xml`
//! - `unstable/idle-inhibit/idle-inhibit-unstable-v1.xml` (wayland-protocols)
//! - river's `IdleInhibitManager.zig` / `LayerSurface.zig` (0.3.x and master)

use std::collections::HashSet;
use std::env;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::wire::{encode, parse_messages, Args, Message};

/// Errors surfaced by the Wayland client.
#[derive(Debug)]
pub enum WlError {
    Io(io::Error),
    /// The compositor (or the socket) told us something is wrong.
    Protocol(String),
}

impl std::fmt::Display for WlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WlError::Io(e) => write!(f, "I/O error: {e}"),
            WlError::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl From<io::Error> for WlError {
    fn from(e: io::Error) -> Self {
        WlError::Io(e)
    }
}

/// One advertised global (from `wl_registry.global`).
#[derive(Debug, Clone)]
pub struct Global {
    pub name: u32,
    pub interface: String,
    pub version: u32,
}

/// High-level events this client cares about; everything else is consumed
/// and dropped (the wire size field lets us skip unknown payloads safely).
#[derive(Debug)]
pub enum Event {
    Global(Global),
    /// `wl_registry.global_remove` (carries the registry *name*, not object id).
    GlobalRemoved(u32),
    /// `wl_callback.done` for one of our sync round-trips.
    SyncDone(u32),
    /// `zwlr_layer_surface_v1.configure`.
    LayerConfigure { serial: u32, width: u32, height: u32 },
    /// `zwlr_layer_surface_v1.closed`.
    LayerClosed,
}

pub struct Wayland {
    stream: UnixStream,
    /// Buffered, not-yet-complete incoming bytes.
    rbuf: Vec<u8>,
    next_id: u32,
    /// Object id of our `wl_registry`.
    registry: u32,
    /// Sync callbacks we are waiting on.
    pending_syncs: HashSet<u32>,
    /// If set, events from this `zwlr_layer_surface_v1` are translated.
    layer_surface: Option<u32>,
    /// All globals currently advertised by the compositor.
    globals: Vec<Global>,
    pub verbose: bool,
}

impl Wayland {
    /// Connect using `WAYLAND_SOCKET` or `XDG_RUNTIME_DIR` + `WAYLAND_DISPLAY`.
    pub fn connect_from_env() -> Result<UnixStream, WlError> {
        if let Ok(fd_str) = env::var("WAYLAND_SOCKET") {
            let fd: RawFd = fd_str.trim().parse().map_err(|_| {
                WlError::Protocol(format!("bad WAYLAND_SOCKET value {fd_str:?}"))
            })?;
            // Take ownership of the fd (it is CLOEXEC per spec).
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            stream.set_nonblocking(true)?;
            return Ok(stream);
        }
        let runtime = env::var("XDG_RUNTIME_DIR").map_err(|_| {
            WlError::Protocol(
                "XDG_RUNTIME_DIR is not set (not running inside a Wayland session?)".into(),
            )
        })?;
        let display = env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
        let path: PathBuf = if display.starts_with('/') {
            PathBuf::from(&display)
        } else {
            PathBuf::from(runtime).join(&display)
        };
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    pub fn new(stream: UnixStream, verbose: bool) -> Self {
        Wayland {
            stream,
            rbuf: Vec::with_capacity(4096),
            next_id: 2, // 1 is wl_display
            registry: 0,
            pending_syncs: HashSet::new(),
            layer_surface: None,
            globals: Vec::new(),
            verbose,
        }
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send_raw(&mut self, bytes: &[u8]) -> Result<(), WlError> {
        let mut off = 0;
        while off < bytes.len() {
            match self.stream.write(&bytes[off..]) {
                Ok(0) => return Err(WlError::Protocol("socket write returned 0".into())),
                Ok(n) => off += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn send(&mut self, object_id: u32, opcode: u16, payload: &[u8]) -> Result<(), WlError> {
        let mut buf = Vec::with_capacity(crate::wire::HEADER_LEN + payload.len());
        encode(&mut buf, object_id, opcode, payload);
        if self.verbose {
            crate::journal::debug(&format!(
                "-> obj {object_id} op {opcode} payload {} bytes",
                payload.len()
            ));
        }
        self.send_raw(&buf)
    }

    /// Send a message that carries one file descriptor (`wl_shm.create_pool`).
    pub fn send_with_fd(
        &mut self,
        object_id: u32,
        opcode: u16,
        payload: &[u8],
        fd: RawFd,
    ) -> Result<(), WlError> {
        let mut buf = Vec::with_capacity(crate::wire::HEADER_LEN + payload.len());
        encode(&mut buf, object_id, opcode, payload);

        let iov = libc::iovec {
            iov_base: buf.as_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let cmsg_space =
            (unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as libc::c_uint) }) as usize;
        let mut control = [0u8; 32]; // >= CMSG_SPACE(4) on every real platform
        debug_assert!(cmsg_space <= control.len());
        unsafe {
            let hdr = control.as_mut_ptr() as *mut libc::cmsghdr;
            // musl declares cmsghdr.cmsg_len and msghdr.msg_controllen as
            // socklen_t (u32), glibc as size_t (usize). `as _` adapts to
            // whichever field type the target libc uses, so both build.
            (*hdr).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as libc::c_uint) as _;
            (*hdr).cmsg_level = libc::SOL_SOCKET;
            (*hdr).cmsg_type = libc::SCM_RIGHTS;
            *(libc::CMSG_DATA(hdr) as *mut RawFd) = fd;

            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_space as _;

            loop {
                let n = libc::sendmsg(self.stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
                if n >= 0 {
                    break;
                }
                let err = io::Error::last_os_error();
                match err.kind() {
                    ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
                    ErrorKind::Interrupted => continue,
                    _ => return Err(WlError::Io(err)),
                }
            }
        }
        Ok(())
    }

    pub fn get_registry(&mut self) -> u32 {
        let id = self.alloc_id();
        let _ = self.send(1, 1, &Args::new().object(id).into_payload());
        self.registry = id;
        id
    }

    /// `wl_registry.bind`. The chosen version must be <= the advertised one.
    pub fn bind(&mut self, name: u32, interface: &str, version: u32) -> u32 {
        let id = self.alloc_id();
        let payload = Args::new()
            .u32(name)
            .string(interface)
            .u32(version)
            .object(id)
            .into_payload();
        let _ = self.send(self.registry, 0, &payload);
        id
    }

    /// `wl_display.sync`; returns the callback object id.
    pub fn sync(&mut self) -> u32 {
        let id = self.alloc_id();
        let _ = self.send(1, 0, &Args::new().object(id).into_payload());
        self.pending_syncs.insert(id);
        id
    }

    /// Dispatch events until the given sync callback fires.
    pub fn roundtrip(&mut self, callback: u32) -> Result<(), WlError> {
        loop {
            let events = self.pump()?;
            if events
                .iter()
                .any(|e| matches!(e, Event::SyncDone(id) if *id == callback))
            {
                return Ok(());
            }
            if !self.pending_syncs.contains(&callback) {
                return Ok(());
            }
        }
    }

    /// Mark which `zwlr_layer_surface_v1` object we want events for.
    pub fn watch_layer_surface(&mut self, id: Option<u32>) {
        self.layer_surface = id;
    }

    /// All globals currently advertised (kept up to date across the session).
    pub fn globals(&self) -> impl Iterator<Item = &Global> {
        self.globals.iter()
    }

    pub fn find_global(&self, interface: &str) -> Option<&Global> {
        self.globals.iter().find(|g| g.interface == interface)
    }

    /// Read whatever is available right now (non-blocking) and translate it.
    pub fn pump(&mut self) -> Result<Vec<Event>, WlError> {
        let mut tmp = [0u8; 4096];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(WlError::Protocol("compositor closed the socket".into())),
                Ok(n) => self.rbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let mut events = Vec::new();
        for msg in parse_messages(&mut self.rbuf) {
            if self.verbose {
                crate::journal::debug(&format!(
                    "<- obj {} op {} payload {} bytes",
                    msg.object_id,
                    msg.opcode,
                    msg.payload.len()
                ));
            }
            match self.translate(msg)? {
                Some(ev) => events.push(ev),
                None => {}
            }
        }
        Ok(events)
    }

    fn translate(&mut self, msg: Message) -> Result<Option<Event>, WlError> {
        let Message { object_id, opcode, payload } = msg;
        if object_id == 1 {
            // wl_display
            match opcode {
                0 => {
                    // error(object_id, code, message)
                    let mut p = crate::wire::Parser::new(&payload);
                    let obj = p.u32().unwrap_or(0);
                    let code = p.u32().unwrap_or(0);
                    let text = p.string().unwrap_or_default();
                    return Err(WlError::Protocol(format!(
                        "compositor raised error on object {obj:#x}: code {code}: {text}"
                    )));
                }
                _ => return Ok(None), // delete_id: we never reuse ids, ignore
            }
        }
        if object_id == self.registry {
            return Ok(match opcode {
                0 => {
                    let mut p = crate::wire::Parser::new(&payload);
                    let name = p.u32().unwrap_or(0);
                    let interface = p.string().unwrap_or_default();
                    let version = p.u32().unwrap_or(0);
                    let g = Global { name, interface, version };
                    self.globals.push(g.clone());
                    Some(Event::Global(g))
                }
                1 => {
                    let mut p = crate::wire::Parser::new(&payload);
                    let name = p.u32().unwrap_or(0);
                    self.globals.retain(|g| g.name != name);
                    Some(Event::GlobalRemoved(name))
                }
                _ => None,
            });
        }
        if self.pending_syncs.remove(&object_id) {
            // wl_callback.done(serial) — we only care that it fired.
            return Ok(Some(Event::SyncDone(object_id)));
        }
        if Some(object_id) == self.layer_surface {
            return Ok(match opcode {
                0 => {
                    // configure(serial, width, height)
                    let mut p = crate::wire::Parser::new(&payload);
                    let serial = p.u32().unwrap_or(0);
                    let width = p.u32().unwrap_or(0);
                    let height = p.u32().unwrap_or(0);
                    Some(Event::LayerConfigure { serial, width, height })
                }
                1 => Some(Event::LayerClosed),
                _ => None,
            });
        }
        Ok(None) // wl_output events, shm format events, buffer release, ...
    }
}
