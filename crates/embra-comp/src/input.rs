//! Input event dispatch.
//!
//! Backends (winit, libinput) translate hardware events into smithay's
//! `InputEvent`. This module forwards keyboard / pointer events into the
//! compositor's seat and the focused client.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::state::EmbraComp;

impl EmbraComp {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotionAbsolute { event } => self.on_pointer_motion_absolute::<I>(event),
            InputEvent::PointerButton { event } => self.on_pointer_button::<I>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<I>(event),
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let key = event.key_code();
        let state = event.state();

        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        keyboard.input::<(), _>(
            self,
            key,
            state,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );

        // Auto-focus the kiosk window on first keypress so the client
        // actually gets keyboard events. Clone the toplevel surface out
        // before the `set_focus(self, ...)` call so we don't hold an
        // immutable borrow of `self.space` across the mutable borrow.
        if state == KeyState::Pressed && keyboard.current_focus().is_none() {
            let target_surface = self
                .space
                .elements()
                .next()
                .and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
            if let Some(surface) = target_surface {
                keyboard.set_focus(self, Some(surface), serial);
            }
        }
    }

    fn on_pointer_motion_absolute<I: InputBackend>(
        &mut self,
        event: I::PointerMotionAbsoluteEvent,
    ) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);

        // Map absolute coords into the (single) output's logical space.
        let output = self.space.outputs().next().cloned();
        let Some(output) = output else {
            return;
        };
        let output_geo = self.space.output_geometry(&output).unwrap_or_default();
        let pos = Point::<f64, Logical>::from((
            event.x_transformed(output_geo.size.w),
            event.y_transformed(output_geo.size.h),
        )) + output_geo.loc.to_f64();

        let under = self.surface_under(pos);

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_button<I: InputBackend>(&mut self, event: I::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time_msec(&event);
        let button = event.button_code();
        let state = match event.state() {
            ButtonState::Pressed => smithay::backend::input::ButtonState::Pressed,
            ButtonState::Released => smithay::backend::input::ButtonState::Released,
        };

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let mut frame = AxisFrame::new(Event::time_msec(&event)).source(event.source());
        if let Some(v) = event.amount(Axis::Horizontal) {
            frame = frame.value(Axis::Horizontal, v);
        }
        if let Some(v) = event.amount(Axis::Vertical) {
            frame = frame.value(Axis::Vertical, v);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }
}
