mod touch_state;

use crate::data::input::{
    Event, EventFilter, KeyboardShortcut, Modifiers, MouseWheelUnit, NUM_POINTER_BUTTONS,
    PointerButton, RawInput, TouchDeviceId, ViewportInfo,
};
use crate::{
    emath::{NumExt as _, Pos2, Rect, Vec2, vec2},
    util::History,
    SafeArea,
};
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

pub use crate::Key;
pub use touch_state::MultiTouchInfo;
use touch_state::TouchState;

/// Options for input state handling.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct InputOptions {
    /// Multiplier for the scroll speed when reported in [`crate::MouseWheelUnit::Line`]s.
    pub line_scroll_speed: f32,

    /// Controls the speed at which we zoom in when doing ctrl/cmd + scroll.
    pub scroll_zoom_speed: f32,

    /// After a pointer-down event, if the pointer moves more than this, it won't become a click.
    pub max_click_dist: f32,

    /// If the pointer is down for longer than this it will no longer register as a click.
    ///
    /// If a touch is held for this many seconds while still, then it will register as a
    /// "long-touch" which is equivalent to a secondary click.
    ///
    /// This is to support "press and hold for context menu" on touch screens.
    pub max_click_duration: f64,

    /// The new pointer press must come within this many seconds from previous pointer release
    /// for double click (or when this value is doubled, triple click) to count.
    pub max_double_click_delay: f64,

    /// When this modifier is down, all scroll events are treated as zoom events.
    ///
    /// The default is CTRL/CMD, and it is STRONGLY recommended to NOT change this.
    pub zoom_modifier: Modifiers,

    /// When this modifier is down, all scroll events are treated as horizontal scrolls,
    /// and when combined with [`Self::zoom_modifier`] it will result in zooming
    /// on only the horizontal axis.
    ///
    /// The default is SHIFT, and it is STRONGLY recommended to NOT change this.
    pub horizontal_scroll_modifier: Modifiers,

    /// When this modifier is down, all scroll events are treated as vertical scrolls,
    /// and when combined with [`Self::zoom_modifier`] it will result in zooming
    /// on only the vertical axis.
    pub vertical_scroll_modifier: Modifiers,
}

impl Default for InputOptions {
    fn default() -> Self {
        // TODO(emilk): figure out why these constants need to be different on web and on native (winit).
        let is_web = cfg!(target_arch = "wasm32");
        let line_scroll_speed = if is_web {
            8.0
        } else {
            40.0 // Scroll speed decided by consensus: https://github.com/emilk/egui/issues/461
        };

        Self {
            line_scroll_speed,
            scroll_zoom_speed: 1.0 / 200.0,
            max_click_dist: 6.0,
            max_click_duration: 0.8,
            max_double_click_delay: 0.3,
            zoom_modifier: Modifiers::COMMAND,
            horizontal_scroll_modifier: Modifiers::SHIFT,
            vertical_scroll_modifier: Modifiers::ALT,
        }
    }
}

impl InputOptions {
    /// Show the options in the ui.
    pub fn ui(&mut self, ui: &mut crate::Ui) {
        let Self {
            line_scroll_speed,
            scroll_zoom_speed,
            max_click_dist,
            max_click_duration,
            max_double_click_delay,
            zoom_modifier,
            horizontal_scroll_modifier,
            vertical_scroll_modifier,
        } = self;
        crate::Grid::new("InputOptions")
            .num_columns(2)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Line scroll speed");
                ui.add(crate::DragValue::new(line_scroll_speed).range(0.0..=f32::INFINITY))
                    .on_hover_text(
                        "How many lines to scroll with each tick of the mouse wheel",
                    );
                ui.end_row();

                ui.label("Scroll zoom speed");
                ui.add(
                    crate::DragValue::new(scroll_zoom_speed)
                        .range(0.0..=f32::INFINITY)
                        .speed(0.001),
                )
                .on_hover_text("How fast to zoom with ctrl/cmd + scroll");
                ui.end_row();

                ui.label("Max click distance");
                ui.add(crate::DragValue::new(max_click_dist).range(0.0..=f32::INFINITY))
                    .on_hover_text(
                        "If the pointer moves more than this, it won't become a click",
                    );
                ui.end_row();

                ui.label("Max click duration");
                ui.add(
                    crate::DragValue::new(max_click_duration)
                        .range(0.1..=f64::INFINITY)
                        .speed(0.1),
                    )
                    .on_hover_text(
                        "If the pointer is down for longer than this it will no longer register as a click",
                    );
                ui.end_row();

                ui.label("Max double click delay");
                ui.add(
                    crate::DragValue::new(max_double_click_delay)
                        .range(0.01..=f64::INFINITY)
                        .speed(0.1),
                )
                .on_hover_text("Max time interval for double click to count");
                ui.end_row();

                ui.label("zoom_modifier");
                zoom_modifier.ui(ui);
                ui.end_row();

                ui.label("horizontal_scroll_modifier");
                horizontal_scroll_modifier.ui(ui);
                ui.end_row();

                ui.label("vertical_scroll_modifier");
                vertical_scroll_modifier.ui(ui);
                ui.end_row();

            });
    }
}

/// Input state that egui updates each frame.
///
/// You can access this with [`crate::Context::input`].
///
/// You can check if `egui` is using the inputs using
/// [`crate::Context::wants_pointer_input`] and [`crate::Context::wants_keyboard_input`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct InputState {
    /// The raw input we got this frame from the backend.
    pub raw: RawInput,

    /// State of the mouse or simple touch gestures which can be mapped to mouse operations.
    pub pointer: PointerState,

    /// State of touches, except those covered by `PointerState` (like clicks and drags).
    /// (We keep a separate [`TouchState`] for each encountered touch device.)
    touch_states: BTreeMap<TouchDeviceId, TouchState>,

    // ----------------------------------------------
    // Scrolling:
    //
    /// Time of the last scroll event.
    last_scroll_time: f64,

    /// Used for smoothing the scroll delta.
    unprocessed_scroll_delta: Vec2,

    /// Used for smoothing the scroll delta when zooming.
    unprocessed_scroll_delta_for_zoom: f32,

    /// You probably want to use [`Self::smooth_scroll_delta`] instead.
    ///
    /// The raw input of how many points the user scrolled.
    ///
    /// The delta dictates how the _content_ should move.
    ///
    /// A positive X-value indicates the content is being moved right,
    /// as when swiping right on a touch-screen or track-pad with natural scrolling.
    ///
    /// A positive Y-value indicates the content is being moved down,
    /// as when swiping down on a touch-screen or track-pad with natural scrolling.
    ///
    /// When using a notched scroll-wheel this will spike very large for one frame,
    /// then drop to zero. For a smoother experience, use [`Self::smooth_scroll_delta`].
    pub raw_scroll_delta: Vec2,

    /// How many points the user scrolled, smoothed over a few frames.
    ///
    /// The delta dictates how the _content_ should move.
    ///
    /// A positive X-value indicates the content is being moved right,
    /// as when swiping right on a touch-screen or track-pad with natural scrolling.
    ///
    /// A positive Y-value indicates the content is being moved down,
    /// as when swiping down on a touch-screen or track-pad with natural scrolling.
    ///
    /// [`crate::ScrollArea`] will both read and write to this field, so that
    /// at the end of the frame this will be zero if a scroll-area consumed the delta.
    pub smooth_scroll_delta: Vec2,

    /// Zoom scale factor this frame (e.g. from ctrl-scroll or pinch gesture).
    ///
    /// * `zoom = 1`: no change.
    /// * `zoom < 1`: pinch together
    /// * `zoom > 1`: pinch spread
    zoom_factor_delta: f32,

    // ----------------------------------------------
    /// Position and size of the egui area.
    ///
    /// This is including the safe area, in contrast to [`crate::Context::screen_rect`], where the safe
    /// area has been subtracted.
    screen_rect: Rect,

    /// The safe area insets of the screen.
    safe_area: SafeArea,

    /// Also known as device pixel ratio, > 1 for high resolution screens.
    pub pixels_per_point: f32,

    /// Maximum size of one side of a texture.
    ///
    /// This depends on the backend.
    pub max_texture_side: usize,

    /// Time in seconds. Relative to whatever. Used for animation.
    pub time: f64,

    /// Time since last frame, in seconds.
    ///
    /// This can be very unstable in reactive mode (when we don't paint each frame).
    /// For animations it is therefore better to use [`Self::stable_dt`].
    pub unstable_dt: f32,

    /// Estimated time until next frame (provided we repaint right away).
    ///
    /// Used for animations to get instant feedback (avoid frame delay).
    /// Should be set to the expected time between frames when painting at vsync speeds.
    ///
    /// On most integrations this has a fixed value of `1.0 / 60.0`, so it is not a very accurate estimate.
    pub predicted_dt: f32,

    /// Time since last frame (in seconds), but gracefully handles the first frame after sleeping in reactive mode.
    ///
    /// In reactive mode (available in e.g. `eframe`), `egui` only updates when there is new input
    /// or something is animating.
    /// This can lead to large gaps of time (sleep), leading to large [`Self::unstable_dt`].
    ///
    /// If `egui` requested a repaint the previous frame, then `egui` will use
    /// `stable_dt = unstable_dt;`, but if `egui` did not not request a repaint last frame,
    /// then `egui` will assume `unstable_dt` is too large, and will use
    /// `stable_dt = predicted_dt;`.
    ///
    /// This means that for the first frame after a sleep,
    /// `stable_dt` will be a prediction of the delta-time until the next frame,
    /// and in all other situations this will be an accurate measurement of time passed
    /// since the previous frame.
    ///
    /// Note that a frame can still stall for various reasons, so `stable_dt` can
    /// still be unusually large in some situations.
    ///
    /// When animating something, it is recommended that you use something like
    /// `stable_dt.min(0.1)` - this will give you smooth animations when the framerate is good
    /// (even in reactive mode), but will avoid large jumps when framerate is bad,
    /// and will effectively slow down the animation when FPS drops below 10.
    pub stable_dt: f32,

    /// The native window has the keyboard focus (i.e. is receiving key presses).
    ///
    /// False when the user alt-tab away from the application, for instance.
    pub focused: bool,

    /// Which modifier keys are down at the start of the frame?
    pub modifiers: Modifiers,

    // The keys that are currently being held down.
    pub keys_down: HashSet<Key>,

    /// In-order events received this frame
    pub events: Vec<Event>,

    /// Input state management configuration.
    ///
    /// This gets copied from `egui::Options` at the start of each frame for convenience.
    options: InputOptions,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            raw: Default::default(),
            pointer: Default::default(),
            touch_states: Default::default(),

            last_scroll_time: f64::NEG_INFINITY,
            unprocessed_scroll_delta: Vec2::ZERO,
            unprocessed_scroll_delta_for_zoom: 0.0,
            raw_scroll_delta: Vec2::ZERO,
            smooth_scroll_delta: Vec2::ZERO,
            zoom_factor_delta: 1.0,

            screen_rect: Rect::from_min_size(Default::default(), vec2(10_000.0, 10_000.0)),
            safe_area: Default::default(),
            pixels_per_point: 1.0,
            max_texture_side: 2048,
            time: 0.0,
            unstable_dt: 1.0 / 60.0,
            predicted_dt: 1.0 / 60.0,
            stable_dt: 1.0 / 60.0,
            focused: false,
            modifiers: Default::default(),
            keys_down: Default::default(),
            events: Default::default(),
            options: Default::default(),
        }
    }
}

impl InputState {
    #[must_use]
    pub fn begin_pass(
        mut self,
        mut new: RawInput,
        requested_immediate_repaint_prev_frame: bool,
        pixels_per_point: f32,
        options: InputOptions,
    ) -> Self {
        profiling::function_scope!();

        let time = new.time.unwrap_or(self.time + new.predicted_dt as f64);
        let unstable_dt = (time - self.time) as f32;

        let stable_dt = if requested_immediate_repaint_prev_frame {
            // we should have had a repaint straight away,
            // so this should be trustable.
            unstable_dt
        } else {
            new.predicted_dt
        };

        let safe_area = new.safe_area.unwrap_or(self.safe_area);
        let screen_rect = new.screen_rect.map_or(self.screen_rect, |rect| {
            // rect - new.safe_area.unwrap_or_default()
            rect
        });
        self.create_touch_states_for_new_devices(&new.events);
        for touch_state in self.touch_states.values_mut() {
            touch_state.begin_pass(time, &new, self.pointer.interact_pos);
        }
        let pointer = self.pointer.begin_pass(time, &new, options);

        let mut keys_down = self.keys_down;
        let mut zoom_factor_delta = 1.0; // TODO(emilk): smoothing for zoom factor
        let mut raw_scroll_delta = Vec2::ZERO;

        let mut unprocessed_scroll_delta = self.unprocessed_scroll_delta;
        let mut unprocessed_scroll_delta_for_zoom = self.unprocessed_scroll_delta_for_zoom;
        let mut smooth_scroll_delta = Vec2::ZERO;
        let mut smooth_scroll_delta_for_zoom = 0.0;

        for event in &mut new.events {
            match event {
                Event::Key {
                    key,
                    pressed,
                    repeat,
                    ..
                } => {
                    if *pressed {
                        let first_press = keys_down.insert(*key);
                        *repeat = !first_press;
                    } else {
                        keys_down.remove(key);
                    }
                }
                Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } => {
                    let mut delta = match unit {
                        MouseWheelUnit::Point => *delta,
                        MouseWheelUnit::Line => options.line_scroll_speed * *delta,
                        MouseWheelUnit::Page => screen_rect.height() * *delta,
                    };

                    let is_horizontal = modifiers.matches_any(options.horizontal_scroll_modifier);
                    let is_vertical = modifiers.matches_any(options.vertical_scroll_modifier);

                    if is_horizontal && !is_vertical {
                        // Treat all scrolling as horizontal scrolling.
                        // Note: one Mac we already get horizontal scroll events when shift is down.
                        delta = vec2(delta.x + delta.y, 0.0);
                    }
                    if !is_horizontal && is_vertical {
                        // Treat all scrolling as vertical scrolling.
                        delta = vec2(0.0, delta.x + delta.y);
                    }

                    raw_scroll_delta += delta;

                    // Mouse wheels often go very large steps.
                    // A single notch on a logitech mouse wheel connected to a Macbook returns 14.0 raw_scroll_delta.
                    // So we smooth it out over several frames for a nicer user experience when scrolling in egui.
                    // BUT: if the user is using a nice smooth mac trackpad, we don't add smoothing,
                    // because it adds latency.
                    let is_smooth = match unit {
                        MouseWheelUnit::Point => delta.length() < 8.0, // a bit arbitrary here
                        MouseWheelUnit::Line | MouseWheelUnit::Page => false,
                    };

                    let is_zoom = modifiers.matches_any(options.zoom_modifier);

                    #[expect(clippy::collapsible_else_if)]
                    if is_zoom {
                        if is_smooth {
                            smooth_scroll_delta_for_zoom += delta.x + delta.y;
                        } else {
                            unprocessed_scroll_delta_for_zoom += delta.x + delta.y;
                        }
                    } else {
                        if is_smooth {
                            smooth_scroll_delta += delta;
                        } else {
                            unprocessed_scroll_delta += delta;
                        }
                    }
                }
                Event::Zoom(factor) => {
                    zoom_factor_delta *= *factor;
                }
                Event::WindowFocused(false) => {
                    // Example: pressing `Cmd+S` brings up a save-dialog (e.g. using rfd),
                    // but we get no key-up event for the `S` key (in winit).
                    // This leads to `S` being mistakenly marked as down when we switch back to the app.
                    // So we take the safe route and just clear all the keys and modifiers when
                    // the app loses focus.
                    keys_down.clear();
                }
                _ => {}
            }
        }

        {
            let dt = stable_dt.at_most(0.1);
            let t = crate::emath::exponential_smooth_factor(0.90, 0.1, dt); // reach _% in _ seconds. TODO(emilk): parameterize

            if unprocessed_scroll_delta != Vec2::ZERO {
                for d in 0..2 {
                    if unprocessed_scroll_delta[d].abs() < 1.0 {
                        smooth_scroll_delta[d] += unprocessed_scroll_delta[d];
                        unprocessed_scroll_delta[d] = 0.0;
                    } else {
                        let applied = t * unprocessed_scroll_delta[d];
                        smooth_scroll_delta[d] += applied;
                        unprocessed_scroll_delta[d] -= applied;
                    }
                }
            }

            {
                // Smooth scroll-to-zoom:
                if unprocessed_scroll_delta_for_zoom.abs() < 1.0 {
                    smooth_scroll_delta_for_zoom += unprocessed_scroll_delta_for_zoom;
                    unprocessed_scroll_delta_for_zoom = 0.0;
                } else {
                    let applied = t * unprocessed_scroll_delta_for_zoom;
                    smooth_scroll_delta_for_zoom += applied;
                    unprocessed_scroll_delta_for_zoom -= applied;
                }

                zoom_factor_delta *=
                    (options.scroll_zoom_speed * smooth_scroll_delta_for_zoom).exp();
            }
        }

        let is_scrolling = raw_scroll_delta != Vec2::ZERO || smooth_scroll_delta != Vec2::ZERO;
        let last_scroll_time = if is_scrolling {
            time
        } else {
            self.last_scroll_time
        };

        Self {
            pointer,
            touch_states: self.touch_states,

            last_scroll_time,
            unprocessed_scroll_delta,
            unprocessed_scroll_delta_for_zoom,
            raw_scroll_delta,
            smooth_scroll_delta,
            zoom_factor_delta,

            screen_rect,
            safe_area,
            pixels_per_point,
            max_texture_side: new.max_texture_side.unwrap_or(self.max_texture_side),
            time,
            unstable_dt,
            predicted_dt: new.predicted_dt,
            stable_dt,
            focused: new.focused,
            modifiers: new.modifiers,
            keys_down,
            events: new.events.clone(), // TODO(emilk): remove clone() and use raw.events
            raw: new,
            options,
        }
    }

    /// Info about the active viewport
    #[inline]
    pub fn viewport(&self) -> &ViewportInfo {
        self.raw.viewport()
    }

    /// Which part of the screen does egui cover?
    ///
    /// Returns the `RawInput::screen_rect` - [`Self::safe_area`].
    /// To get the full `screen_rect`, use [`Self::screen_rect_including_unsafe_area`]
    #[inline(always)]
    pub fn screen_rect(&self) -> Rect {
        self.screen_rect - self.safe_area
    }

    /// Returns the full area available to egui, including parts that might be partially
    /// covered, for example, by the OS status bar or notches.
    ///
    /// Usually you want to use [`Self::screen_rect`] instead.
    ///
    /// _Note that the name has nothing to do with rust safety and it's always safe to call this._
    pub fn screen_rect_including_unsafe_area(&self) -> Rect {
        self.screen_rect
    }

    /// Get the safe area insets.
    pub fn safe_area(&self) -> SafeArea {
        self.safe_area
    }

    /// Uniform zoom scale factor this frame (e.g. from ctrl-scroll or pinch gesture).
    /// * `zoom = 1`: no change
    /// * `zoom < 1`: pinch together
    /// * `zoom > 1`: pinch spread
    ///
    /// If your application supports non-proportional zooming,
    /// then you probably want to use [`Self::zoom_delta_2d`] instead.
    #[inline(always)]
    pub fn zoom_delta(&self) -> f32 {
        // If a multi touch gesture is detected, it measures the exact and linear proportions of
        // the distances of the finger tips. It is therefore potentially more accurate than
        // `zoom_factor_delta` which is based on the `ctrl-scroll` event which, in turn, may be
        // synthesized from an original touch gesture.
        self.multi_touch()
            .map_or(self.zoom_factor_delta, |touch| touch.zoom_delta)
    }

    /// 2D non-proportional zoom scale factor this frame (e.g. from ctrl-scroll or pinch gesture).
    ///
    /// For multitouch devices the user can do a horizontal or vertical pinch gesture.
    /// In these cases a non-proportional zoom factor is a available.
    /// In other cases, this reverts to `Vec2::splat(self.zoom_delta())`.
    ///
    /// For horizontal pinches, this will return `[z, 1]`,
    /// for vertical pinches this will return `[1, z]`,
    /// and otherwise this will return `[z, z]`,
    /// where `z` is the zoom factor:
    /// * `zoom = 1`: no change
    /// * `zoom < 1`: pinch together
    /// * `zoom > 1`: pinch spread
    #[inline(always)]
    pub fn zoom_delta_2d(&self) -> Vec2 {
        // If a multi touch gesture is detected, it measures the exact and linear proportions of
        // the distances of the finger tips.  It is therefore potentially more accurate than
        // `zoom_factor_delta` which is based on the `ctrl-scroll` event which, in turn, may be
        // synthesized from an original touch gesture.
        if let Some(multi_touch) = self.multi_touch() {
            multi_touch.zoom_delta_2d
        } else {
            let mut zoom = Vec2::splat(self.zoom_factor_delta);

            let is_horizontal = self
                .modifiers
                .matches_any(self.options.horizontal_scroll_modifier);
            let is_vertical = self
                .modifiers
                .matches_any(self.options.vertical_scroll_modifier);

            if is_horizontal && !is_vertical {
                // Horizontal-only zooming.
                zoom.y = 1.0;
            }
            if !is_horizontal && is_vertical {
                // Vertical-only zooming.
                zoom.x = 1.0;
            }

            zoom
        }
    }

    /// How long has it been (in seconds) since the use last scrolled?
    #[inline(always)]
    pub fn time_since_last_scroll(&self) -> f32 {
        (self.time - self.last_scroll_time) as f32
    }

    /// The [`crate::Context`] will call this at the beginning of each frame to see if we need a repaint.
    ///
    /// Returns how long to wait for a repaint.
    ///
    /// NOTE: It's important to call this immediately after [`Self::begin_pass`] since calls to
    /// [`Self::consume_key`] will remove events from the vec, meaning those key presses wouldn't
    /// cause a repaint.
    pub(crate) fn wants_repaint_after(&self) -> Option<Duration> {
        if self.pointer.wants_repaint()
            || self.unprocessed_scroll_delta.abs().max_elem() > 0.2
            || self.unprocessed_scroll_delta_for_zoom.abs() > 0.2
            || !self.events.is_empty()
        {
            // Immediate repaint
            return Some(Duration::ZERO);
        }

        if self.any_touches() && !self.pointer.is_decidedly_dragging() {
            // We need to wake up and check for press-and-hold for the context menu.
            if let Some(press_start_time) = self.pointer.press_start_time {
                let press_duration = self.time - press_start_time;
                if self.options.max_click_duration.is_finite()
                    && press_duration < self.options.max_click_duration
                {
                    let secs_until_menu = self.options.max_click_duration - press_duration;
                    return Some(Duration::from_secs_f64(secs_until_menu));
                }
            }
        }

        None
    }

    /// Count presses of a key. If non-zero, the presses are consumed, so that this will only return non-zero once.
    ///
    /// Includes key-repeat events.
    ///
    /// This uses [`Modifiers::matches_logically`] to match modifiers,
    /// meaning extra Shift and Alt modifiers are ignored.
    /// Therefore, you should match most specific shortcuts first,
    /// i.e. check for `Cmd-Shift-S` ("Save as…") before `Cmd-S` ("Save"),
    /// so that a user pressing `Cmd-Shift-S` won't trigger the wrong command!
    pub fn count_and_consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> usize {
        let mut count = 0usize;

        self.events.retain(|event| {
            let is_match = matches!(
                event,
                Event::Key {
                    key: ev_key,
                    modifiers: ev_mods,
                    pressed: true,
                    ..
                } if *ev_key == logical_key && ev_mods.matches_logically(modifiers)
            );

            count += is_match as usize;

            !is_match
        });

        count
    }

    /// Check for a key press. If found, `true` is returned and the key pressed is consumed, so that this will only return `true` once.
    ///
    /// Includes key-repeat events.
    ///
    /// This uses [`Modifiers::matches_logically`] to match modifiers,
    /// meaning extra Shift and Alt modifiers are ignored.
    /// Therefore, you should match most specific shortcuts first,
    /// i.e. check for `Cmd-Shift-S` ("Save as…") before `Cmd-S` ("Save"),
    /// so that a user pressing `Cmd-Shift-S` won't trigger the wrong command!
    pub fn consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> bool {
        self.count_and_consume_key(modifiers, logical_key) > 0
    }

    /// Check if the given shortcut has been pressed.
    ///
    /// If so, `true` is returned and the key pressed is consumed, so that this will only return `true` once.
    ///
    /// This uses [`Modifiers::matches_logically`] to match modifiers,
    /// meaning extra Shift and Alt modifiers are ignored.
    /// Therefore, you should match most specific shortcuts first,
    /// i.e. check for `Cmd-Shift-S` ("Save as…") before `Cmd-S` ("Save"),
    /// so that a user pressing `Cmd-Shift-S` won't trigger the wrong command!
    pub fn consume_shortcut(&mut self, shortcut: &KeyboardShortcut) -> bool {
        let KeyboardShortcut {
            modifiers,
            logical_key,
        } = *shortcut;
        self.consume_key(modifiers, logical_key)
    }

    /// Was the given key pressed this frame?
    ///
    /// Includes key-repeat events.
    pub fn key_pressed(&self, desired_key: Key) -> bool {
        self.num_presses(desired_key) > 0
    }

    /// How many times was the given key pressed this frame?
    ///
    /// Includes key-repeat events.
    pub fn num_presses(&self, desired_key: Key) -> usize {
        self.events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::Key { key, pressed: true, .. }
                    if *key == desired_key
                )
            })
            .count()
    }

    /// Is the given key currently held down?
    pub fn key_down(&self, desired_key: Key) -> bool {
        self.keys_down.contains(&desired_key)
    }

    /// Was the given key released this frame?
    pub fn key_released(&self, desired_key: Key) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event,
                Event::Key {
                    key,
                    pressed: false,
                    ..
                } if *key == desired_key
            )
        })
    }

    /// Also known as device pixel ratio, > 1 for high resolution screens.
    #[inline(always)]
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// Size of a physical pixel in logical gui coordinates (points).
    #[inline(always)]
    pub fn physical_pixel_size(&self) -> f32 {
        1.0 / self.pixels_per_point()
    }

    /// How imprecise do we expect the mouse/touch input to be?
    /// Returns imprecision in points.
    #[inline(always)]
    pub fn aim_radius(&self) -> f32 {
        // TODO(emilk): multiply by ~3 for touch inputs because fingers are fat
        self.physical_pixel_size()
    }

    /// Returns details about the currently ongoing multi-touch gesture, if any. Note that this
    /// method returns `None` for single-touch gestures (click, drag, …).
    ///
    /// ```
    /// # use egui::emath::Rot2;
    /// # egui::__run_test_ui(|ui| {
    /// let mut zoom = 1.0; // no zoom
    /// let mut rotation = 0.0; // no rotation
    /// let multi_touch = ui.input(|i| i.multi_touch());
    /// if let Some(multi_touch) = multi_touch {
    ///     zoom *= multi_touch.zoom_delta;
    ///     rotation += multi_touch.rotation_delta;
    /// }
    /// let transform = zoom * Rot2::from_angle(rotation);
    /// # });
    /// ```
    ///
    /// By far not all touch devices are supported, and the details depend on the `egui`
    /// integration backend you are using. `eframe` web supports multi touch for most mobile
    /// devices, but not for a `Trackpad` on `MacOS`, for example. The backend has to be able to
    /// capture native touch events, but many browsers seem to pass such events only for touch
    /// _screens_, but not touch _pads._
    ///
    /// Refer to [`MultiTouchInfo`] for details about the touch information available.
    ///
    /// Consider using `zoom_delta()` instead of `MultiTouchInfo::zoom_delta` as the former
    /// delivers a synthetic zoom factor based on ctrl-scroll events, as a fallback.
    pub fn multi_touch(&self) -> Option<MultiTouchInfo> {
        // In case of multiple touch devices simply pick the touch_state of the first active device
        self.touch_states.values().find_map(|t| t.info())
    }

    /// True if there currently are any fingers touching egui.
    pub fn any_touches(&self) -> bool {
        self.touch_states.values().any(|t| t.any_touches())
    }

    /// True if we have ever received a touch event.
    pub fn has_touch_screen(&self) -> bool {
        !self.touch_states.is_empty()
    }

    /// Scans `events` for device IDs of touch devices we have not seen before,
    /// and creates a new [`TouchState`] for each such device.
    fn create_touch_states_for_new_devices(&mut self, events: &[Event]) {
        for event in events {
            if let Event::Touch { device_id, .. } = event {
                self.touch_states
                    .entry(*device_id)
                    .or_insert_with(|| TouchState::new(*device_id));
            }
        }
    }

    #[cfg(feature = "accesskit")]
    pub fn accesskit_action_requests(
        &self,
        id: crate::Id,
        action: accesskit::Action,
    ) -> impl Iterator<Item = &accesskit::ActionRequest> {
        let accesskit_id = id.accesskit_id();
        self.events.iter().filter_map(move |event| {
            if let Event::AccessKitActionRequest(request) = event {
                if request.target == accesskit_id && request.action == action {
                    return Some(request);
                }
            }
            None
        })
    }

    #[cfg(feature = "accesskit")]
    pub fn consume_accesskit_action_requests(
        &mut self,
        id: crate::Id,
        mut consume: impl FnMut(&accesskit::ActionRequest) -> bool,
    ) {
        let accesskit_id = id.accesskit_id();
        self.events.retain(|event| {
            if let Event::AccessKitActionRequest(request) = event {
                if request.target == accesskit_id {
                    return !consume(request);
                }
            }
            true
        });
    }

    #[cfg(feature = "accesskit")]
    pub fn has_accesskit_action_request(&self, id: crate::Id, action: accesskit::Action) -> bool {
        self.accesskit_action_requests(id, action).next().is_some()
    }

    #[cfg(feature = "accesskit")]
    pub fn num_accesskit_action_requests(&self, id: crate::Id, action: accesskit::Action) -> usize {
        self.accesskit_action_requests(id, action).count()
    }

    /// Get all events that matches the given filter.
    pub fn filtered_events(&self, filter: &EventFilter) -> Vec<Event> {
        self.events
            .iter()
            .filter(|event| filter.matches(event))
            .cloned()
            .collect()
    }

    /// A long press is something we detect on touch screens
    /// to trigger a secondary click (context menu).
    ///
    /// Returns `true` only on one frame.
    pub(crate) fn is_long_touch(&self) -> bool {
        self.any_touches() && self.pointer.is_long_press()
    }
}

// ----------------------------------------------------------------------------

/// A pointer (mouse or touch) click.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub(crate) struct Click {
    pub pos: Pos2,

    /// 1 or 2 (double-click) or 3 (triple-click)
    pub count: u32,

    /// Allows you to check for e.g. shift-click
    pub modifiers: Modifiers,
}

impl Click {
    pub fn is_double(&self) -> bool {
        self.count == 2
    }

    pub fn is_triple(&self) -> bool {
        self.count == 3
    }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub(crate) enum PointerEvent {
    Moved(Pos2),
    Pressed {
        position: Pos2,
        button: PointerButton,
    },
    Released {
        click: Option<Click>,
        button: PointerButton,
    },
}

impl PointerEvent {
    pub fn is_press(&self) -> bool {
        matches!(self, Self::Pressed { .. })
    }

    pub fn is_release(&self) -> bool {
        matches!(self, Self::Released { .. })
    }

    pub fn is_click(&self) -> bool {
        matches!(self, Self::Released { click: Some(_), .. })
    }
}

/// Mouse or touch state.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct PointerState {
    /// Latest known time
    time: f64,

    // Consider a finger tapping a touch screen.
    // What position should we report?
    // The location of the touch, or `None`, because the finger is gone?
    //
    // For some cases we want the first: e.g. to check for interaction.
    // For showing tooltips, we want the latter (no tooltips, since there are no fingers).
    /// Latest reported pointer position.
    /// When tapping a touch screen, this will be `None`.
    latest_pos: Option<Pos2>,

    /// Latest position of the mouse, but ignoring any [`Event::PointerGone`]
    /// if there were interactions this frame.
    /// When tapping a touch screen, this will be the location of the touch.
    interact_pos: Option<Pos2>,

    /// How much the pointer moved compared to last frame, in points.
    delta: Vec2,

    /// How much the mouse moved since the last frame, in unspecified units.
    /// Represents the actual movement of the mouse, without acceleration or clamped by screen edges.
    /// May be unavailable on some integrations.
    motion: Option<Vec2>,

    /// Current velocity of pointer.
    velocity: Vec2,

    /// Current direction of pointer.
    direction: Vec2,

    /// Recent movement of the pointer.
    /// Used for calculating velocity of pointer.
    pos_history: History<Pos2>,

    down: [bool; NUM_POINTER_BUTTONS],

    /// Where did the current click/drag originate?
    /// `None` if no mouse button is down.
    press_origin: Option<Pos2>,

    /// When did the current click/drag originate?
    /// `None` if no mouse button is down.
    press_start_time: Option<f64>,

    /// Set to `true` if the pointer has moved too much (since being pressed)
    /// for it to be registered as a click.
    pub(crate) has_moved_too_much_for_a_click: bool,

    /// Did [`Self::is_decidedly_dragging`] go from `false` to `true` this frame?
    ///
    /// This could also be the trigger point for a long-touch.
    pub(crate) started_decidedly_dragging: bool,

    /// When did the pointer get click last?
    /// Used to check for double-clicks.
    last_click_time: f64,

    /// When did the pointer get click two clicks ago?
    /// Used to check for triple-clicks.
    last_last_click_time: f64,

    /// When was the pointer last moved?
    /// Used for things like showing hover ui/tooltip with a delay.
    last_move_time: f64,

    /// All button events that occurred this frame
    pub(crate) pointer_events: Vec<PointerEvent>,

    /// Input state management configuration.
    ///
    /// This gets copied from `egui::Options` at the start of each frame for convenience.
    options: InputOptions,
}

impl Default for PointerState {
    fn default() -> Self {
        Self {
            time: -f64::INFINITY,
            latest_pos: None,
            interact_pos: None,
            delta: Vec2::ZERO,
            motion: None,
            velocity: Vec2::ZERO,
            direction: Vec2::ZERO,
            pos_history: History::new(2..1000, 0.1),
            down: Default::default(),
            press_origin: None,
            press_start_time: None,
            has_moved_too_much_for_a_click: false,
            started_decidedly_dragging: false,
            last_click_time: f64::NEG_INFINITY,
            last_last_click_time: f64::NEG_INFINITY,
            last_move_time: f64::NEG_INFINITY,
            pointer_events: vec![],
            options: Default::default(),
        }
    }
}

impl PointerState {
    #[must_use]
    pub(crate) fn begin_pass(mut self, time: f64, new: &RawInput, options: InputOptions) -> Self {
        let was_decidedly_dragging = self.is_decidedly_dragging();

        self.time = time;
        self.options = options;

        self.pointer_events.clear();

        let old_pos = self.latest_pos;
        self.interact_pos = self.latest_pos;
        if self.motion.is_some() {
            self.motion = Some(Vec2::ZERO);
        }

        let mut clear_history_after_velocity_calculation = false;
        for event in &new.events {
            match event {
                Event::PointerMoved(pos) => {
                    let pos = *pos;

                    self.latest_pos = Some(pos);
                    self.interact_pos = Some(pos);

                    if let Some(press_origin) = self.press_origin {
                        self.has_moved_too_much_for_a_click |=
                            press_origin.distance(pos) > self.options.max_click_dist;
                    }

                    self.last_move_time = time;
                    self.pointer_events.push(PointerEvent::Moved(pos));
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let pos = *pos;
                    let button = *button;
                    let pressed = *pressed;
                    let modifiers = *modifiers;

                    self.latest_pos = Some(pos);
                    self.interact_pos = Some(pos);

                    if pressed {
                        // Start of a drag: we want to track the velocity for during the drag
                        // and ignore any incoming movement
                        self.pos_history.clear();
                    }

                    if pressed {
                        self.press_origin = Some(pos);
                        self.press_start_time = Some(time);
                        self.has_moved_too_much_for_a_click = false;
                        self.pointer_events.push(PointerEvent::Pressed {
                            position: pos,
                            button,
                        });
                    } else {
                        // Released
                        let clicked = self.could_any_button_be_click();

                        let click = if clicked {
                            let double_click =
                                (time - self.last_click_time) < self.options.max_double_click_delay;
                            let triple_click = (time - self.last_last_click_time)
                                < (self.options.max_double_click_delay * 2.0);
                            let count = if triple_click {
                                3
                            } else if double_click {
                                2
                            } else {
                                1
                            };

                            self.last_last_click_time = self.last_click_time;
                            self.last_click_time = time;

                            Some(Click {
                                pos,
                                count,
                                modifiers,
                            })
                        } else {
                            None
                        };

                        self.pointer_events
                            .push(PointerEvent::Released { click, button });

                        self.press_origin = None;
                        self.press_start_time = None;
                    }

                    self.down[button as usize] = pressed; // must be done after the above call to `could_any_button_be_click`
                }
                Event::PointerGone => {
                    self.latest_pos = None;
                    // When dragging a slider and the mouse leaves the viewport, we still want the drag to work,
                    // so we don't treat this as a `PointerEvent::Released`.
                    // NOTE: we do NOT clear `self.interact_pos` here. It will be cleared next frame.

                    // Delay the clearing until after the final velocity calculation, so we can
                    // get the final velocity when `drag_stopped` is true.
                    clear_history_after_velocity_calculation = true;
                }
                Event::MouseMoved(delta) => *self.motion.get_or_insert(Vec2::ZERO) += *delta,
                _ => {}
            }
        }

        self.delta = if let (Some(old_pos), Some(new_pos)) = (old_pos, self.latest_pos) {
            new_pos - old_pos
        } else {
            Vec2::ZERO
        };

        if let Some(pos) = self.latest_pos {
            self.pos_history.add(time, pos);
        } else {
            // we do not clear the `pos_history` here, because it is exactly when a finger has
            // released from the touch screen that we may want to assign a velocity to whatever
            // the user tried to throw.
        }

        self.pos_history.flush(time);

        self.velocity = if self.pos_history.len() >= 3 && self.pos_history.duration() > 0.01 {
            self.pos_history.velocity().unwrap_or_default()
        } else {
            Vec2::default()
        };
        if self.velocity != Vec2::ZERO {
            self.last_move_time = time;
        }
        if clear_history_after_velocity_calculation {
            self.pos_history.clear();
        }

        self.direction = self.pos_history.velocity().unwrap_or_default().normalized();

        self.started_decidedly_dragging = self.is_decidedly_dragging() && !was_decidedly_dragging;

        self
    }

    fn wants_repaint(&self) -> bool {
        !self.pointer_events.is_empty() || self.delta != Vec2::ZERO
    }

    /// How much the pointer moved compared to last frame, in points.
    #[inline(always)]
    pub fn delta(&self) -> Vec2 {
        self.delta
    }

    /// How much the mouse moved since the last frame, in unspecified units.
    /// Represents the actual movement of the mouse, without acceleration or clamped by screen edges.
    /// May be unavailable on some integrations.
    #[inline(always)]
    pub fn motion(&self) -> Option<Vec2> {
        self.motion
    }

    /// Current velocity of pointer.
    ///
    /// This is smoothed over a few frames,
    /// but can be ZERO when frame-rate is bad.
    #[inline(always)]
    pub fn velocity(&self) -> Vec2 {
        self.velocity
    }

    /// Current direction of the pointer.
    ///
    /// This is less sensitive to bad framerate than [`Self::velocity`].
    #[inline(always)]
    pub fn direction(&self) -> Vec2 {
        self.direction
    }

    /// Where did the current click/drag originate?
    /// `None` if no mouse button is down.
    #[inline(always)]
    pub fn press_origin(&self) -> Option<Pos2> {
        self.press_origin
    }

    /// When did the current click/drag originate?
    /// `None` if no mouse button is down.
    #[inline(always)]
    pub fn press_start_time(&self) -> Option<f64> {
        self.press_start_time
    }

    /// Latest reported pointer position.
    /// When tapping a touch screen, this will be `None`.
    #[inline(always)]
    pub fn latest_pos(&self) -> Option<Pos2> {
        self.latest_pos
    }

    /// If it is a good idea to show a tooltip, where is pointer?
    #[inline(always)]
    pub fn hover_pos(&self) -> Option<Pos2> {
        self.latest_pos
    }

    /// If you detect a click or drag and wants to know where it happened, use this.
    ///
    /// Latest position of the mouse, but ignoring any [`Event::PointerGone`]
    /// if there were interactions this frame.
    /// When tapping a touch screen, this will be the location of the touch.
    #[inline(always)]
    pub fn interact_pos(&self) -> Option<Pos2> {
        self.interact_pos
    }

    /// Do we have a pointer?
    ///
    /// `false` if the mouse is not over the egui area, or if no touches are down on touch screens.
    #[inline(always)]
    pub fn has_pointer(&self) -> bool {
        self.latest_pos.is_some()
    }

    /// Is the pointer currently still?
    /// This is smoothed so a few frames of stillness is required before this returns `true`.
    #[inline(always)]
    pub fn is_still(&self) -> bool {
        self.velocity == Vec2::ZERO
    }

    /// Is the pointer currently moving?
    /// This is smoothed so a few frames of stillness is required before this returns `false`.
    #[inline]
    pub fn is_moving(&self) -> bool {
        self.velocity != Vec2::ZERO
    }

    /// How long has it been (in seconds) since the pointer was last moved?
    #[inline(always)]
    pub fn time_since_last_movement(&self) -> f32 {
        (self.time - self.last_move_time) as f32
    }

    /// How long has it been (in seconds) since the pointer was clicked?
    #[inline(always)]
    pub fn time_since_last_click(&self) -> f32 {
        (self.time - self.last_click_time) as f32
    }

    /// Was any pointer button pressed (`!down -> down`) this frame?
    ///
    /// This can sometimes return `true` even if `any_down() == false`
    /// because a press can be shorted than one frame.
    pub fn any_pressed(&self) -> bool {
        self.pointer_events.iter().any(|event| event.is_press())
    }

    /// Was any pointer button released (`down -> !down`) this frame?
    pub fn any_released(&self) -> bool {
        self.pointer_events.iter().any(|event| event.is_release())
    }

    /// Was the button given pressed this frame?
    pub fn button_pressed(&self, button: PointerButton) -> bool {
        self.pointer_events
            .iter()
            .any(|event| matches!(event, &PointerEvent::Pressed{button: b, ..} if button == b))
    }

    /// Was the button given released this frame?
    pub fn button_released(&self, button: PointerButton) -> bool {
        self.pointer_events
            .iter()
            .any(|event| matches!(event, &PointerEvent::Released{button: b, ..} if button == b))
    }

    /// Was the primary button pressed this frame?
    pub fn primary_pressed(&self) -> bool {
        self.button_pressed(PointerButton::Primary)
    }

    /// Was the secondary button pressed this frame?
    pub fn secondary_pressed(&self) -> bool {
        self.button_pressed(PointerButton::Secondary)
    }

    /// Was the primary button released this frame?
    pub fn primary_released(&self) -> bool {
        self.button_released(PointerButton::Primary)
    }

    /// Was the secondary button released this frame?
    pub fn secondary_released(&self) -> bool {
        self.button_released(PointerButton::Secondary)
    }

    /// Is any pointer button currently down?
    pub fn any_down(&self) -> bool {
        self.down.iter().any(|&down| down)
    }

    /// Were there any type of click this frame?
    pub fn any_click(&self) -> bool {
        self.pointer_events.iter().any(|event| event.is_click())
    }

    /// Was the given pointer button given clicked this frame?
    ///
    /// Returns true on double- and triple- clicks too.
    pub fn button_clicked(&self, button: PointerButton) -> bool {
        self.pointer_events
            .iter()
            .any(|event| matches!(event, &PointerEvent::Released { button: b, click: Some(_) } if button == b))
    }

    /// Was the button given double clicked this frame?
    pub fn button_double_clicked(&self, button: PointerButton) -> bool {
        self.pointer_events.iter().any(|event| {
            matches!(
                &event,
                PointerEvent::Released {
                    click: Some(click),
                    button: b,
                } if *b == button && click.is_double()
            )
        })
    }

    /// Was the button given triple clicked this frame?
    pub fn button_triple_clicked(&self, button: PointerButton) -> bool {
        self.pointer_events.iter().any(|event| {
            matches!(
                &event,
                PointerEvent::Released {
                    click: Some(click),
                    button: b,
                } if *b == button && click.is_triple()
            )
        })
    }

    /// Was the primary button clicked this frame?
    pub fn primary_clicked(&self) -> bool {
        self.button_clicked(PointerButton::Primary)
    }

    /// Was the secondary button clicked this frame?
    pub fn secondary_clicked(&self) -> bool {
        self.button_clicked(PointerButton::Secondary)
    }

    /// Is this button currently down?
    #[inline(always)]
    pub fn button_down(&self, button: PointerButton) -> bool {
        self.down[button as usize]
    }

    /// If the pointer button is down, will it register as a click when released?
    ///
    /// See also [`Self::is_decidedly_dragging`].
    pub fn could_any_button_be_click(&self) -> bool {
        if self.any_down() || self.any_released() {
            if self.has_moved_too_much_for_a_click {
                return false;
            }

            if let Some(press_start_time) = self.press_start_time {
                if self.time - press_start_time > self.options.max_click_duration {
                    return false;
                }
            }

            true
        } else {
            false
        }
    }

    /// Just because the mouse is down doesn't mean we are dragging.
    /// We could be at the start of a click.
    /// But if the mouse is down long enough, or has moved far enough,
    /// then we consider it a drag.
    ///
    /// This function can return true on the same frame the drag is released,
    /// but NOT on the first frame it was started.
    ///
    /// See also [`Self::could_any_button_be_click`].
    pub fn is_decidedly_dragging(&self) -> bool {
        (self.any_down() || self.any_released())
            && !self.any_pressed()
            && !self.could_any_button_be_click()
            && !self.any_click()
    }

    /// A long press is something we detect on touch screens
    /// to trigger a secondary click (context menu).
    ///
    /// Returns `true` only on one frame.
    pub(crate) fn is_long_press(&self) -> bool {
        self.started_decidedly_dragging
            && !self.has_moved_too_much_for_a_click
            && self.button_down(PointerButton::Primary)
            && self.press_start_time.is_some_and(|press_start_time| {
                self.time - press_start_time > self.options.max_click_duration
            })
    }

    /// Is the primary button currently down?
    #[inline(always)]
    pub fn primary_down(&self) -> bool {
        self.button_down(PointerButton::Primary)
    }

    /// Is the secondary button currently down?
    #[inline(always)]
    pub fn secondary_down(&self) -> bool {
        self.button_down(PointerButton::Secondary)
    }

    /// Is the middle button currently down?
    #[inline(always)]
    pub fn middle_down(&self) -> bool {
        self.button_down(PointerButton::Middle)
    }

    /// Is the mouse moving in the direction of the given rect?
    pub fn is_moving_towards_rect(&self, rect: &Rect) -> bool {
        if self.is_still() {
            return false;
        }

        if let Some(pos) = self.hover_pos() {
            let dir = self.direction();
            if dir != Vec2::ZERO {
                return rect.intersects_ray(pos, self.direction());
            }
        }
        false
    }
}

impl InputState {
    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            raw,
            pointer,
            touch_states,

            last_scroll_time,
            unprocessed_scroll_delta,
            unprocessed_scroll_delta_for_zoom,
            raw_scroll_delta,
            smooth_scroll_delta,

            zoom_factor_delta,
            screen_rect,
            safe_area,
            pixels_per_point,
            max_texture_side,
            time,
            unstable_dt,
            predicted_dt,
            stable_dt,
            focused,
            modifiers,
            keys_down,
            events,
            options: _,
        } = self;

        ui.style_mut()
            .text_styles
            .get_mut(&crate::TextStyle::Body)
            .unwrap()
            .family = crate::FontFamily::Monospace;

        ui.collapsing("Raw Input", |ui| raw.ui(ui));

        crate::containers::CollapsingHeader::new("🖱 Pointer")
            .default_open(false)
            .show(ui, |ui| {
                pointer.ui(ui);
            });

        for (device_id, touch_state) in touch_states {
            ui.collapsing(format!("Touch State [device {}]", device_id.0), |ui| {
                touch_state.ui(ui);
            });
        }

        ui.label(format!(
            "Time since last scroll: {:.1} s",
            time - last_scroll_time
        ));
        if cfg!(debug_assertions) {
            ui.label(format!(
                "unprocessed_scroll_delta: {unprocessed_scroll_delta:?} points"
            ));
            ui.label(format!(
                "unprocessed_scroll_delta_for_zoom: {unprocessed_scroll_delta_for_zoom:?} points"
            ));
        }
        ui.label(format!("raw_scroll_delta: {raw_scroll_delta:?} points"));
        ui.label(format!(
            "smooth_scroll_delta: {smooth_scroll_delta:?} points"
        ));
        ui.label(format!("zoom_factor_delta: {zoom_factor_delta:4.2}x"));

        ui.label(format!("screen_rect: {screen_rect:?} points"));
        ui.label(format!("safe_area: {safe_area:?} points"));
        ui.label(format!(
            "{pixels_per_point} physical pixels for each logical point"
        ));
        ui.label(format!(
            "max texture size (on each side): {max_texture_side}"
        ));
        ui.label(format!("time: {time:.3} s"));
        ui.label(format!(
            "time since previous frame: {:.1} ms",
            1e3 * unstable_dt
        ));
        ui.label(format!("predicted_dt: {:.1} ms", 1e3 * predicted_dt));
        ui.label(format!("stable_dt:    {:.1} ms", 1e3 * stable_dt));
        ui.label(format!("focused:   {focused}"));
        ui.label(format!("modifiers: {modifiers:#?}"));
        ui.label(format!("keys_down: {keys_down:?}"));
        ui.scope(|ui| {
            ui.set_min_height(150.0);
            ui.label(format!("events: {events:#?}"))
                .on_hover_text("key presses etc");
        });
    }
}

impl PointerState {
    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            time: _,
            latest_pos,
            interact_pos,
            delta,
            motion,
            velocity,
            direction,
            pos_history: _,
            down,
            press_origin,
            press_start_time,
            has_moved_too_much_for_a_click,
            started_decidedly_dragging,
            last_click_time,
            last_last_click_time,
            pointer_events,
            last_move_time,
            options: _,
        } = self;

        ui.label(format!("latest_pos: {latest_pos:?}"));
        ui.label(format!("interact_pos: {interact_pos:?}"));
        ui.label(format!("delta: {delta:?}"));
        ui.label(format!("motion: {motion:?}"));
        ui.label(format!(
            "velocity: [{:3.0} {:3.0}] points/sec",
            velocity.x, velocity.y
        ));
        ui.label(format!("direction: {direction:?}"));
        ui.label(format!("down: {down:#?}"));
        ui.label(format!("press_origin: {press_origin:?}"));
        ui.label(format!("press_start_time: {press_start_time:?} s"));
        ui.label(format!(
            "has_moved_too_much_for_a_click: {has_moved_too_much_for_a_click}"
        ));
        ui.label(format!(
            "started_decidedly_dragging: {started_decidedly_dragging}"
        ));
        ui.label(format!("last_click_time: {last_click_time:#?}"));
        ui.label(format!("last_last_click_time: {last_last_click_time:#?}"));
        ui.label(format!("last_move_time: {last_move_time:#?}"));
        ui.label(format!("pointer_events: {pointer_events:?}"));
    }
}
