//! Wayland protocol handlers for embra-comp.
//!
//! Implements just the handlers a kiosk needs: compositor, xdg-shell, shm,
//! seat, output. No data-device / clipboard / drag-and-drop (kiosk has one
//! client, nothing to share with).

use smithay::{
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::Window,
    input::{Seat, SeatHandler, SeatState},
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
        output::OutputHandler,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::state::{ClientState, EmbraComp};

// === wl_compositor ===

impl CompositorHandler for EmbraComp {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Walk parents to root in case this is a subsurface.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        // Don't process commits for sync subsurfaces — they get applied
        // when the parent commits.
        if !is_sync_subsurface(surface) {
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false))
                .cloned()
            {
                window.on_commit();
            }
        }
    }
}

delegate_compositor!(EmbraComp);

// === wl_shm ===

impl BufferHandler for EmbraComp {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for EmbraComp {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(EmbraComp);

// === xdg_shell — kiosk policy lives here ===

impl XdgShellHandler for EmbraComp {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Kiosk policy: only ONE toplevel, ever. Subsequent toplevels are
        // closed immediately. The intent is one fullscreen client per
        // boot; if it crashes and respawns it's a fresh process and we'd
        // be a fresh compositor too, so the count is always 0 or 1.
        if self.kiosk_window_present {
            tracing::warn!(
                "kiosk policy: refusing additional toplevel, one already mapped"
            );
            surface.send_close();
            return;
        }
        self.kiosk_window_present = true;

        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), false);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Single-client popups are allowed (menus, tooltips, etc.).
        let _ = self.popups.track_popup(surface.into());
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Allow a fresh toplevel to take its place if the client (e.g.
        // embra-desktop) restarts within the same compositor lifetime.
        // Clone the matching window first to release the immutable
        // iterator borrow before the mutable `unmap_elem` call.
        let target = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface.wl_surface())
                    .unwrap_or(false)
            })
            .cloned();
        if let Some(window) = target {
            self.space.unmap_elem(&window);
        }
        self.kiosk_window_present = false;
    }
}

delegate_xdg_shell!(EmbraComp);

// === wl_seat ===

impl SeatHandler for EmbraComp {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<EmbraComp> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
}

delegate_seat!(EmbraComp);

// === wl_output / xdg_output ===

impl OutputHandler for EmbraComp {}

delegate_output!(EmbraComp);
