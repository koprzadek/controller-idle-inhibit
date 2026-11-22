//! The Wayland session: an invisible layer surface that river will honor for
//! idle inhibition, plus create/destroy of `zwp_idle_inhibitor_v1` objects.
//!
//! Why a layer surface? river's `IdleInhibitManager.checkActive()` only
//! counts inhibitors whose surface it can associate with a scene node:
//! views (visible windows), layer surfaces (if *mapped*), lock surfaces and
//! override-redirect windows. A bare `wl_surface` with no role is invisible
//! to that check, so it never inhibits (see river issue #1079). A mapped
//! 1x1 background-layer surface:
//!   - is honored by river 0.3.x (checked: `surface.mapped`) and by
//!     river master (any layer surface counts),
//!   - is 1 pixel, fully transparent, anchored in a corner of the
//!     background layer (below every window),
//!   - has an empty input region (per the layer-shell spec this is the
//!     documented way to opt out of pointer/touch input),
//!   - reserves no space (exclusive zone -1) and never takes keyboard focus
//!     (keyboard interactivity defaults to "none").

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};

use crate::journal;
use crate::wayland::{Event, Global, Wayland, WlError};

const NAMESPACE: &str = "controller-idle-inhibit";

/// `zwlr_layer_shell_v1.layer` — bottom-most layer, below all windows.
const LAYER_BACKGROUND: u32 = 0;
/// `zwlr_layer_surface_v1.anchor`: top (1) | left (4).
const ANCHOR_TOP_LEFT: u32 = 1 | 4;
/// `wl_shm.format`: ARGB8888 (0); our pixel is 0x00000000 = fully transparent.
const SHM_FORMAT_ARGB8888: u32 = 0;

// zwlr_layer_shell_v1 opcodes
const LAYER_SHELL_GET_LAYER_SURFACE: u16 = 0;
// zwlr_layer_surface_v1 opcodes
const LAYER_SET_SIZE: u16 = 0;
const LAYER_SET_ANCHOR: u16 = 1;
const LAYER_SET_EXCLUSIVE_ZONE: u16 = 2;
const LAYER_ACK_CONFIGURE: u16 = 6;
const LAYER_DESTROY: u16 = 7;
// wl_compositor opcodes
const COMPOSITOR_CREATE_SURFACE: u16 = 0;
const COMPOSITOR_CREATE_REGION: u16 = 1;
// wl_surface opcodes
const SURFACE_DESTROY: u16 = 0;
const SURFACE_SET_INPUT_REGION: u16 = 5;
const SURFACE_COMMIT: u16 = 6;
const SURFACE_ATTACH: u16 = 1;
// wl_shm / wl_shm_pool opcodes
const SHM_CREATE_POOL: u16 = 0;
const POOL_CREATE_BUFFER: u16 = 0;
// zwp_idle_inhibit_manager_v1 opcodes
const MANAGER_CREATE_INHIBITOR: u16 = 1;
// zwp_idle_inhibitor_v1 opcodes
const INHIBITOR_DESTROY: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfState {
    /// Nothing alive right now.
    None,
    /// `get_layer_surface` + initial commit sent; waiting for configure.
    Configuring,
    /// Buffer attached and committed after ack — the compositor counts us.
    Mapped,
}

pub struct Session {
    wl: Wayland,
    compositor: u32,
    layer_shell: u32,
    inhibit_manager: u32,
    /// Every `wl_output` global currently advertised.
    output_candidates: Vec<Global>,
    /// Registry name of the output we bound for the current surface.
    used_output: Option<u32>,
    /// Registry name of an output whose layer surface just got closed.
    last_failed_output: Option<u32>,
    /// Persistent objects (survive surface rebuilds).
    region: u32,
    buffer: u32,
    /// Rebuilt whenever the current output goes away.
    surface: u32,
    layer_surface: u32,
    state: SurfState,
    inhibitor: Option<u32>,
    desired: bool,
    mapped_logged: bool,
}

impl Session {
    /// Connect, discover globals and build the invisible surface.
    ///
    /// Errors here are expected while river is still starting up; the caller
    /// simply retries with a backoff.
    pub fn connect(verbose: bool) -> Result<Self, WlError> {
        let stream = Wayland::connect_from_env()?;
        let mut wl = Wayland::new(stream, verbose);
        let _registry = wl.get_registry();
        let sync = wl.sync();
        wl.roundtrip(sync)?;

        let need = |wl: &Wayland, iface: &str| -> Result<Global, WlError> {
            wl.globals()
                .find(|g| g.interface == iface)
                .cloned()
                .ok_or_else(|| WlError::Protocol(format!("compositor does not advertise {iface}")))
        };

        let compositor_g = need(&wl, "wl_compositor")?;
        let shm_g = need(&wl, "wl_shm")?;
        let layer_shell_g = need(&wl, "zwlr_layer_shell_v1")
            .map_err(|_| WlError::Protocol("compositor does not support zwlr_layer_shell_v1 (layer shell)".into()))?;
        let inhibit_g = need(&wl, "zwp_idle_inhibit_manager_v1").map_err(|_| {
            WlError::Protocol(
                "compositor does not support zwp_idle_inhibit_manager_v1 (idle inhibit)".into(),
            )
        })?;
        if wl.find_global("wl_output").is_none() {
            return Err(WlError::Protocol(
                "no wl_output global yet (no display connected?)".into(),
            ));
        }

        // We only use version-1 requests of every interface, so bind at v1
        // (binding lower than advertised is always allowed).
        let compositor = wl.bind(compositor_g.name, "wl_compositor", 1);
        let shm = wl.bind(shm_g.name, "wl_shm", 1);
        let layer_shell = wl.bind(layer_shell_g.name, "zwlr_layer_shell_v1", 1);
        let inhibit_manager = wl.bind(inhibit_g.name, "zwp_idle_inhibit_manager_v1", 1);

        // One transparent 1x1 ARGB8888 pixel, reused by every surface rebuild.
        let region = wl.alloc_id();
        wl.send(compositor, COMPOSITOR_CREATE_REGION, &crate::wire::Args::new().object(region).into_payload())?;

        let pool = wl.alloc_id();
        let pixel_fd = create_transparent_pixel_memfd()?;
        wl.send_with_fd(shm, SHM_CREATE_POOL, &crate::wire::Args::new().object(pool).i32(4).into_payload(), pixel_fd.as_raw_fd())?;
        drop(pixel_fd); // the compositor dup()ed the fd via SCM_RIGHTS

        let buffer = wl.alloc_id();
        wl.send(
            pool,
            POOL_CREATE_BUFFER,
            &crate::wire::Args::new()
                .object(buffer)
                .i32(0) // offset
                .i32(1) // width
                .i32(1) // height
                .i32(4) // stride
                .u32(SHM_FORMAT_ARGB8888)
                .into_payload(),
        )?;

        let mut session = Session {
            wl,
            compositor,
            layer_shell,
            inhibit_manager,
            output_candidates: Vec::new(),
            used_output: None,
            last_failed_output: None,
            region,
            buffer,
            surface: 0,
            layer_surface: 0,
            state: SurfState::None,
            inhibitor: None,
            desired: false,
            mapped_logged: false,
        };

        // Collect outputs advertised so far (before we started watching).
        let outputs: Vec<Global> = session
            .wl
            .globals()
            .filter(|g| g.interface == "wl_output")
            .cloned()
            .collect();
        session.output_candidates = outputs;
        session.build_surface_if_possible()?;
        Ok(session)
    }

    pub fn set_desired(&mut self, inhibit: bool) {
        self.desired = inhibit;
    }

    pub fn desired(&self) -> bool {
        self.desired
    }

    pub fn is_inhibited(&self) -> bool {
        self.inhibitor.is_some()
    }

    /// Pump events, maintain the surface and apply the desired inhibitor state.
    pub fn tick(&mut self) -> Result<(), WlError> {
        for ev in self.wl.pump()? {
            match ev {
                Event::Global(g) => {
                    if g.interface == "wl_output" {
                        if !self.output_candidates.iter().any(|c| c.name == g.name) {
                            self.output_candidates.push(g);
                        }
                        // A new output may let us (re)build after a failure.
                        self.last_failed_output = None;
                    }
                }
                Event::GlobalRemoved(name) => {
                    self.output_candidates.retain(|c| c.name != name);
                    if self.used_output == Some(name) {
                        journal::debug("bound output removed — tearing down surface");
                        self.teardown_surface()?;
                    }
                }
                Event::LayerConfigure { serial, .. } => {
                    if self.state == SurfState::Configuring {
                        // The documented mapping sequence: ack, attach, commit.
                        self.wl.send(
                            self.layer_surface,
                            LAYER_ACK_CONFIGURE,
                            &crate::wire::Args::new().u32(serial).into_payload(),
                        )?;
                        self.wl.send(
                            self.surface,
                            SURFACE_ATTACH,
                            &crate::wire::Args::new()
                                .object(self.buffer)
                                .i32(0)
                                .i32(0)
                                .into_payload(),
                        )?;
                        self.wl.send(self.surface, SURFACE_COMMIT, &[])?;
                        self.state = SurfState::Mapped;
                        if !self.mapped_logged {
                            journal::info("invisible layer surface mapped — idle inhibition active path ready");
                            self.mapped_logged = true;
                        }
                    } else {
                        // Re-configure (e.g. output changes): ack and stay.
                        let _ = self.wl.send(
                            self.layer_surface,
                            LAYER_ACK_CONFIGURE,
                            &crate::wire::Args::new().u32(serial).into_payload(),
                        );
                    }
                }
                Event::LayerClosed => {
                    journal::debug("layer surface closed — rebuilding on another output if available");
                    self.teardown_surface()?;
                }
                Event::SyncDone(_) => {}
            }
        }

        if self.state == SurfState::None {
            self.build_surface_if_possible()?;
        }
        self.apply_desired()?;
        Ok(())
    }

    /// Create or destroy the inhibitor object to match `desired`.
    fn apply_desired(&mut self) -> Result<(), WlError> {
        match (self.desired, self.inhibitor.is_some()) {
            (true, false) if self.state == SurfState::Mapped => {
                let id = self.wl.alloc_id();
                self.wl.send(
                    self.inhibit_manager,
                    MANAGER_CREATE_INHIBITOR,
                    &crate::wire::Args::new()
                        .object(id)
                        .object(self.surface)
                        .into_payload(),
                )?;
                self.inhibitor = Some(id);
                journal::info("gamepad activity — idle inhibited");
            }
            (false, true) => {
                if let Some(id) = self.inhibitor.take() {
                    self.wl.send(id, INHIBITOR_DESTROY, &[])?;
                    journal::info("grace period elapsed — idle inhibition released");
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// (Re)build surface + layer surface on an output that is not the one
    /// that just failed. If no output is available, silently wait for one.
    fn build_surface_if_possible(&mut self) -> Result<(), WlError> {
        let candidate = self
            .output_candidates
            .iter()
            .find(|c| Some(c.name) != self.last_failed_output)
            .cloned();
        let Some(output) = candidate else {
            return Ok(()); // wait for a wl_output global to show up
        };

        let output_obj = self.wl.bind(output.name, "wl_output", 1);
        self.used_output = Some(output.name);

        let surface = self.wl.alloc_id();
        let layer_surface = self.wl.alloc_id();

        // Plain surface with an empty input region (the layer-shell spec's
        // documented way to never receive pointer/touch events).
        self.wl.send(
            self.compositor,
            COMPOSITOR_CREATE_SURFACE,
            &crate::wire::Args::new().object(surface).into_payload(),
        )?;
        self.wl.send(
            surface,
            SURFACE_SET_INPUT_REGION,
            &crate::wire::Args::new().object(self.region).into_payload(),
        )?;

        // Assign the layer-shell role. river requires an explicit output;
        // passing null would panic its LayerSurface.create().
        self.wl.send(
            self.layer_shell,
            LAYER_SHELL_GET_LAYER_SURFACE,
            &crate::wire::Args::new()
                .object(layer_surface)
                .object(surface)
                .object(output_obj)
                .u32(LAYER_BACKGROUND)
                .string(NAMESPACE)
                .into_payload(),
        )?;

        // 1x1, anchored top-left, participating in no exclusive zones.
        self.wl.send(
            layer_surface,
            LAYER_SET_SIZE,
            &crate::wire::Args::new().u32(1).u32(1).into_payload(),
        )?;
        self.wl.send(
            layer_surface,
            LAYER_SET_ANCHOR,
            &crate::wire::Args::new().u32(ANCHOR_TOP_LEFT).into_payload(),
        )?;
        self.wl.send(
            layer_surface,
            LAYER_SET_EXCLUSIVE_ZONE,
            &crate::wire::Args::new().i32(-1).into_payload(),
        )?;

        self.wl.watch_layer_surface(Some(layer_surface));
        // Initial commit without a buffer: the compositor replies with a
        // configure event; only then may we attach the buffer (per spec).
        self.wl.send(surface, SURFACE_COMMIT, &[])?;

        self.surface = surface;
        self.layer_surface = layer_surface;
        self.state = SurfState::Configuring;
        Ok(())
    }

    /// Destroy the current surface pair. The inhibitor dies with the
    /// surface server-side, but we also destroy it explicitly first.
    fn teardown_surface(&mut self) -> Result<(), WlError> {
        if let Some(id) = self.inhibitor.take() {
            let _ = self.wl.send(id, INHIBITOR_DESTROY, &[]);
        }
        if self.state != SurfState::None {
            let _ = self.wl.send(self.layer_surface, LAYER_DESTROY, &[]);
            let _ = self.wl.send(self.surface, SURFACE_DESTROY, &[]);
            self.wl.watch_layer_surface(None);
            self.state = SurfState::None;
            self.last_failed_output = self.used_output;
        }
        Ok(())
    }

    /// Best-effort cleanup before exiting. Wayland guarantees that all of
    /// our objects (and inhibitors) are destroyed on disconnect anyway.
    pub fn shutdown(&mut self) {
        if let Err(e) = self.teardown_surface() {
            journal::debug(&format!("teardown: {e}"));
        }
    }
}

/// A 4-byte memfd holding one fully transparent ARGB8888 pixel.
fn create_transparent_pixel_memfd() -> Result<std::os::fd::OwnedFd, WlError> {
    let name = c"controller-idle-inhibit";
    let fd = unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(WlError::Io(std::io::Error::last_os_error()));
    }
    let mut file = std::fs::File::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
    // 0x00000000 = ARGB8888 pixel with alpha 0 (invisible).
    file.write_all(&[0, 0, 0, 0])?;
    Ok(file.into())
}
