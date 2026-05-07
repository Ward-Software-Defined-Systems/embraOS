//! Compositor state for embra-comp.
//!
//! Modeled on smithay's `smallvil` reference, trimmed for the single-
//! fullscreen-client kiosk profile: no clipboard / drag-and-drop, no
//! second-window allowance, no popups beyond what the single client
//! requires.

use std::{ffi::OsString, sync::Arc, time::Instant};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

pub struct EmbraComp {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    /// Path to write once Wayland globals are advertised. embrad's
    /// supervisor watches this for ProcessAlive + FileExists health.
    pub ready_sentinel: String,
    pub ready_written: bool,

    /// Tracks whether the kiosk has already accepted its one allowed
    /// toplevel. Set true on first `XdgShellHandler::new_toplevel` —
    /// any subsequent toplevel is destroyed by the handler.
    pub kiosk_window_present: bool,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<EmbraComp>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
}

impl EmbraComp {
    pub fn new(
        event_loop: &mut EventLoop<Self>,
        display: Display<Self>,
        ready_sentinel: String,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "embra-seat0");

        // Always-on virtual keyboard + pointer. Real input comes from the
        // backend (winit for nested dev, libinput for production); these
        // globals just advertise their presence to clients.
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("seat.add_keyboard failed");
        seat.add_pointer();

        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();

        Self {
            start_time: Instant::now(),
            display_handle: dh,
            space,
            loop_signal,
            socket_name,
            ready_sentinel,
            ready_written: false,
            kiosk_window_present: false,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            popups,
            seat,
        }
    }

    /// Drop the readiness sentinel so embrad's supervisor can health-check
    /// us. Called once after the first frame is ready to render — at that
    /// point all globals are advertised and the socket is accepting.
    pub fn write_ready_sentinel(&mut self) {
        if self.ready_written {
            return;
        }
        match std::fs::write(&self.ready_sentinel, b"ok\n") {
            Ok(()) => {
                tracing::info!(path = %self.ready_sentinel, "wrote readiness sentinel");
                self.ready_written = true;
            }
            Err(e) => {
                tracing::warn!(path = %self.ready_sentinel, error = %e, "could not write readiness sentinel");
            }
        }
    }

    fn init_wayland_listener(
        display: Display<EmbraComp>,
        event_loop: &mut EventLoop<Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto()
            .expect("failed to bind a Wayland socket");
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .expect("failed to insert wayland client");
            })
            .expect("failed to register wayland listening socket with event loop");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: smithay's API requires unsafe access to the
                    // `Display` to drive client dispatch from the event
                    // loop. The `Generic` source guarantees the FD remains
                    // alive for the duration of the callback; we never
                    // drop `display`.
                    unsafe {
                        display
                            .get_mut()
                            .dispatch_clients(state)
                            .expect("dispatch_clients failed");
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("failed to register wayland display with event loop");

        socket_name
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
