//! Input event handling.

use std::time::{Duration, Instant};

use calloop::timer::{Timer, TimerHandle};
use calloop::LoopHandle;
use smithay::backend::input::{
    ButtonState, Event, InputBackend, InputEvent, KeyboardKeyEvent, MouseButton,
    PointerButtonEvent, PointerMotionAbsoluteEvent, TouchCancelEvent, TouchDownEvent,
    TouchMotionEvent, TouchSlot, TouchUpEvent,
};
use smithay::backend::winit::WinitEvent;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::{keysyms, FilterResult, TouchHandle};
use smithay::wayland::SERIAL_COUNTER;

use crate::catacomb::{Backend, Catacomb};
use crate::output::{Orientation, Output};

/// Time before a tap is considered a hold.
pub const HOLD_DURATION: Duration = Duration::from_secs(1);

/// Percentage of output dimensions before Home gesture start.
const HOME_HEIGHT_PERCENTAGE: f64 = 0.9;

/// Home gesture distance from the output edges.
const HOME_WIDTH_PERCENTAGE: f64 = 0.25;

/// Maximum distance before touch input is considered a drag.
const MAX_TAP_DISTANCE: f64 = 20.;

/// Friction for velocity computation.
const FRICTION: f64 = 0.1;

/// Touch input state.
pub struct TouchState {
    pub position: Point<f64, Logical>,
    slot: Option<TouchSlot>,
    velocity: Point<f64, Logical>,
    events: Vec<TouchEvent>,
    timer: TimerHandle<()>,
    touch: TouchHandle,
    start: TouchStart,
    is_drag: bool,
}

impl TouchState {
    pub fn new<B: Backend>(loop_handle: LoopHandle<'_, Catacomb<B>>, touch: TouchHandle) -> Self {
        let timer = Timer::new().expect("create timer");
        let timer_handle = timer.handle();
        loop_handle
            .insert_source(timer, |_, _, catacomb| catacomb.on_velocity_tick())
            .expect("insert timer");

        Self {
            start: TouchStart::new(Default::default(), Default::default()),
            timer: timer_handle,
            touch,
            position: Default::default(),
            velocity: Default::default(),
            is_drag: Default::default(),
            events: Default::default(),
            slot: Default::default(),
        }
    }

    /// Stop all touch velocity.
    pub fn cancel_velocity(&mut self) {
        self.velocity = Default::default();
        self.timer.cancel_all_timeouts();
    }

    /// Start a new touch session.
    fn start(&mut self, output_size: Size<f64, Logical>, position: Point<f64, Logical>) {
        self.start = TouchStart::new(output_size, position);
        self.velocity = Default::default();
        self.timer.cancel_all_timeouts();
        self.position = position;
        self.is_drag = false;
    }

    /// Check if there's any touch velocity present.
    fn has_velocity(&self) -> bool {
        self.velocity.x.abs() >= f64::EPSILON || self.velocity.y.abs() >= f64::EPSILON
    }

    /// Check if there's currently any touch interaction active.
    #[inline]
    fn touching(&self) -> bool {
        self.slot.is_some()
    }

    /// Get the updated active touch action.
    fn action(&mut self, output: &Output, overview_active: bool) -> Option<TouchAction> {
        let output_size = output.screen_size().to_f64();
        let touching = self.touching();
        match self.start.gesture {
            Some(Gesture::Overview) if overview_active => (),
            Some(gesture) => {
                if !touching && gesture.end_rect(output_size).contains(self.position) {
                    return Some(TouchAction::Gesture(gesture));
                }
            },
            _ => (),
        }

        // Convert to drag as soon as distance/time was exceeded once.
        let delta = self.start.position - self.position;
        if self.is_drag
            || f64::sqrt(delta.x.powi(2) + delta.y.powi(2)) > MAX_TAP_DISTANCE
            || self.start.time.elapsed() >= HOLD_DURATION
        {
            self.is_drag = true;
            return self.start.gesture.is_none().then(|| TouchAction::Drag);
        }

        (!touching).then(|| TouchAction::Tap)
    }
}

/// Start of a touch interaction.
struct TouchStart {
    position: Point<f64, Logical>,
    gesture: Option<Gesture>,
    time: Instant,
}

impl TouchStart {
    fn new(output_size: Size<f64, Logical>, position: Point<f64, Logical>) -> Self {
        Self { gesture: Gesture::from_start(output_size, position), time: Instant::now(), position }
    }
}

/// Available touch input actions.
#[derive(Debug, Copy, Clone)]
enum TouchAction {
    Gesture(Gesture),
    Drag,
    Tap,
}

/// Touch gestures.
#[derive(Debug, Copy, Clone)]
pub enum Gesture {
    Overview,
    Home,
}

impl Gesture {
    /// Create a gesture from its starting location.
    fn from_start(output_size: Size<f64, Logical>, position: Point<f64, Logical>) -> Option<Self> {
        let match_gesture =
            |gesture: Gesture| gesture.start_rect(output_size).contains(position).then(|| gesture);
        match_gesture(Gesture::Overview).or_else(|| match_gesture(Gesture::Home))
    }

    /// Touch area expected for gesture initiation.
    fn start_rect(&self, output_size: Size<f64, Logical>) -> Rectangle<f64, Logical> {
        match self {
            Gesture::Overview => {
                let loc = (
                    output_size.w * (1. - HOME_WIDTH_PERCENTAGE),
                    output_size.h * (1. - HOME_WIDTH_PERCENTAGE),
                );
                Rectangle::from_loc_and_size(loc, output_size)
            },
            Gesture::Home => {
                let loc =
                    (output_size.w * HOME_WIDTH_PERCENTAGE, output_size.h * HOME_HEIGHT_PERCENTAGE);
                let size = (output_size.w - 2. * loc.0, output_size.h);
                Rectangle::from_loc_and_size(loc, size)
            },
        }
    }

    /// Touch area expected for gesture completion.
    fn end_rect(&self, output_size: Size<f64, Logical>) -> Rectangle<f64, Logical> {
        match self {
            Gesture::Overview => {
                let size = (output_size.w * 0.75, output_size.h * 0.75);
                Rectangle::from_loc_and_size((0., 0.), size)
            },
            Gesture::Home => {
                let size = (output_size.w, output_size.h * 0.75);
                Rectangle::from_loc_and_size((0., 0.), size)
            },
        }
    }
}

/// Generic touch event.
#[derive(Copy, Clone, Debug)]
struct TouchEvent {
    position: Point<f64, Logical>,
    ty: TouchEventType,
    slot: TouchSlot,
    time: u32,
}

impl TouchEvent {
    fn new(ty: TouchEventType, slot: TouchSlot, time: u32, position: Point<f64, Logical>) -> Self {
        Self { slot, time, position, ty }
    }
}

/// Types of touch event.
#[derive(Copy, Clone, Debug)]
enum TouchEventType {
    Down,
    Up,
    Motion,
}

impl<B: Backend> Catacomb<B> {
    /// Process winit-specific input events.
    pub fn handle_winit_input(&mut self, event: WinitEvent) {
        match event {
            // Toggle between portrait/landscape based on window size.
            WinitEvent::Resized { size, .. } => {
                if size.h >= size.w {
                    self.output.orientation = Orientation::Portrait;
                } else {
                    self.output.orientation = Orientation::Landscape;
                }
                self.output.resize(size);
                self.windows.resize_all(&mut self.output);
            },
            WinitEvent::Input(event) => self.handle_input(event),
            _ => (),
        }
    }

    /// Process new input events.
    pub fn handle_input<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard_input(event),
            InputEvent::PointerButton { event } if event.button() == Some(MouseButton::Left) => {
                let slot = TouchSlot::default();
                let position = self.touch_state.position;
                if event.state() == ButtonState::Pressed {
                    self.on_touch_down(TouchEvent::new(TouchEventType::Down, slot, 0, position));
                } else {
                    self.on_touch_up(TouchEvent::new(TouchEventType::Up, slot, 0, position));
                }
            },
            InputEvent::PointerMotionAbsolute { event } => {
                let position = event.position_transformed(self.output.screen_size());
                let slot = TouchSlot::default();
                self.on_touch_motion(TouchEvent::new(TouchEventType::Down, slot, 0, position));
                self.touch_state.position = position;
            },
            InputEvent::TouchDown { event } => {
                let position = event.position_transformed(self.output.screen_size());
                let event_type = TouchEventType::Down;
                let event = TouchEvent::new(event_type, event.slot(), event.time(), position);
                self.touch_state.events.push(event);
            },
            InputEvent::TouchUp { event } => {
                let position = self.touch_state.position;
                let event_type = TouchEventType::Up;
                let event = TouchEvent::new(event_type, event.slot(), event.time(), position);
                self.touch_state.events.push(event);
            },
            InputEvent::TouchMotion { event } => {
                let position = event.position_transformed(self.output.screen_size());
                let event_type = TouchEventType::Motion;
                let event = TouchEvent::new(event_type, event.slot(), event.time(), position);
                self.touch_state.events.push(event);
            },
            // Apply all pending touch events.
            InputEvent::TouchFrame { .. } => {
                for i in 0..self.touch_state.events.len() {
                    let event = self.touch_state.events[i];
                    match event.ty {
                        TouchEventType::Down => self.on_touch_down(event),
                        TouchEventType::Up => self.on_touch_up(event),
                        TouchEventType::Motion => self.on_touch_motion(event),
                    }
                }
                self.touch_state.events.clear();
            },
            // Handle gesture touch cancel for nested compositors.
            InputEvent::TouchCancel { event } => {
                self.touch_state.events.retain(|touch_event| touch_event.slot != event.slot());
            },
            _ => (),
        };
    }

    /// Handle new touch input start.
    fn on_touch_down(&mut self, event: TouchEvent) {
        // Notify client.
        let surface = self.windows.surface_at_position(&self.output, event.position);
        if let Some((surface, offset)) = surface {
            self.touch_state.touch.down(event.time, &surface, offset, event.slot, event.position);
        }

        // Allow only a single touch at a time.
        if self.touch_state.slot.is_some() {
            return;
        }
        self.touch_state.slot = Some(event.slot);

        // Initialize the touch state.
        let output_size = self.output.screen_size().to_f64();
        self.touch_state.start(output_size, event.position);

        // Only send touch start if there's no gesture in progress.
        if self.touch_state.start.gesture.is_none() {
            self.windows.on_touch_start(&self.output, event.position);
        }
    }

    /// Handle touch input release.
    fn on_touch_up(&mut self, event: TouchEvent) {
        // Notify client.
        self.touch_state.touch.up(event.time, event.slot);

        // Check if slot is the active one.
        if self.touch_state.slot != Some(event.slot) {
            return;
        }
        self.touch_state.slot = None;

        let overview_active = self.windows.overview_active();
        match self.touch_state.action(&self.output, overview_active) {
            Some(TouchAction::Tap) => {
                self.windows.on_tap(&self.output, self.touch_state.position);
            },
            Some(TouchAction::Drag | TouchAction::Gesture(_)) => {
                self.add_velocity_timeout();
                self.update_position(self.touch_state.position);
            },
            None => self.add_velocity_timeout(),
        }
    }

    /// Handle touch input movement.
    fn on_touch_motion(&mut self, event: TouchEvent) {
        // Notify client.
        self.touch_state.touch.motion(event.time, event.slot, event.position);

        // Ignore anything but the active touch slot.
        if self.touch_state.slot != Some(event.slot) {
            return;
        }

        self.touch_state.velocity = event.position - self.touch_state.position;
        self.update_position(event.position);
    }

    /// Update the touch position.
    ///
    /// NOTE: This should be called after adding new timeouts to allow clearing them instead of
    /// creating a loop which continuously triggers these actions.
    fn update_position(&mut self, position: Point<f64, Logical>) {
        let overview_active = self.windows.overview_active();
        match self.touch_state.action(&self.output, overview_active) {
            Some(TouchAction::Drag) => {
                self.windows.on_drag(&self.output, &mut self.touch_state, position);

                // Signal drag end once no more velocity is present.
                if !self.touch_state.touching() && !self.touch_state.has_velocity() {
                    self.windows.on_drag_release(&self.output);
                }
            },
            Some(TouchAction::Gesture(gesture)) => self.on_gesture(gesture),
            _ => (),
        }

        self.touch_state.position = position;
    }

    /// Dispatch gestures if it was completed.
    fn on_gesture(&mut self, gesture: Gesture) {
        // Only accept gestures when the touch input was released.
        if self.touch_state.touching() {
            return;
        }

        self.windows.on_gesture(&self.output, gesture);
        self.touch_state.timer.cancel_all_timeouts();

        // Notify client.
        self.touch_state.touch.cancel();
    }

    /// Process a single velocity tick.
    fn on_velocity_tick(&mut self) {
        // Update velocity and new position.
        //
        // The animations are designed for 60FPS, but should still behave properly for other
        // refresh rates.
        let velocity = &mut self.touch_state.velocity;
        let position = &mut self.touch_state.position;
        let animation_speed = self.output.frame_interval() as f64 / 16.;
        velocity.x -= velocity.x.signum()
            * (velocity.x.abs() * FRICTION * animation_speed + 1.).min(velocity.x.abs());
        velocity.y -= velocity.y.signum()
            * (velocity.y.abs() * FRICTION * animation_speed + 1.).min(velocity.y.abs());
        *position += *velocity * animation_speed;

        // Request another callback.
        self.add_velocity_timeout();

        // Generate motion events.
        self.update_position(self.touch_state.position);
    }

    /// Request a new velocity timer callback.
    fn add_velocity_timeout(&self) {
        if self.touch_state.has_velocity() {
            self.touch_state
                .timer
                .add_timeout(Duration::from_millis(self.output.frame_interval()), ());
        }
    }

    /// Handle new keyboard input events.
    fn on_keyboard_input<I: InputBackend>(&mut self, event: impl KeyboardKeyEvent<I>) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = Event::time(&event);
        let keycode = event.key_code();
        let state = event.state();

        self.keyboard.input(keycode, state, serial, time, |_modifiers, keysym| {
            match keysym.modified_sym() {
                keysym @ keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12 => {
                    let vt = (keysym - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                    self.backend.change_vt(vt);
                },
                _ => return FilterResult::Forward,
            }

            FilterResult::Intercept(())
        });
    }
}
