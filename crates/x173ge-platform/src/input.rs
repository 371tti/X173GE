//! Window input state and the small event vocabulary exposed to games.

use std::collections::HashSet;

use winit::{event::MouseButton, keyboard::KeyCode};

/// An input or window event normalized from winit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlatformEvent {
    CloseRequested,
    Resized {
        width: u32,
        height: u32,
    },
    Focused(bool),
    Key {
        code: KeyCode,
        pressed: bool,
        repeat: bool,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    CursorMoved {
        position: [f64; 2],
    },
    MouseWheel {
        /// Vertical wheel distance in normalized wheel steps.
        delta: f64,
    },
    /// Raw physical mouse movement, independent of cursor position.
    MouseMotion {
        delta: [f64; 2],
    },
    RedrawRequested,
}

/// Current held input plus transitions accumulated since the last update.
#[derive(Default)]
pub struct InputState {
    keys: HashSet<KeyCode>,
    pressed_keys: HashSet<KeyCode>,
    released_keys: HashSet<KeyCode>,
    buttons: HashSet<MouseButton>,
    pressed_buttons: HashSet<MouseButton>,
    released_buttons: HashSet<MouseButton>,
    cursor: Option<[f64; 2]>,
    cursor_delta: [f64; 2],
    wheel_delta: f64,
}

impl InputState {
    /// Returns whether a key is currently held.
    pub fn key_down(&self, code: KeyCode) -> bool {
        self.keys.contains(&code)
    }

    /// Returns whether a key became held since the last update.
    pub fn key_pressed(&self, code: KeyCode) -> bool {
        self.pressed_keys.contains(&code)
    }

    /// Returns whether a key was released since the last update.
    pub fn key_released(&self, code: KeyCode) -> bool {
        self.released_keys.contains(&code)
    }

    /// Returns whether a mouse button is currently held.
    pub fn button_down(&self, button: MouseButton) -> bool {
        self.buttons.contains(&button)
    }

    /// Returns whether a mouse button became held since the last consumed update.
    pub fn button_pressed(&self, button: MouseButton) -> bool {
        self.pressed_buttons.contains(&button)
    }

    /// Returns whether a mouse button was released since the last consumed update.
    pub fn button_released(&self, button: MouseButton) -> bool {
        self.released_buttons.contains(&button)
    }

    /// Returns the accumulated cursor movement in physical pixels.
    pub fn cursor_delta(&self) -> [f64; 2] {
        self.cursor_delta
    }

    /// Returns the latest cursor position in physical pixels.
    pub fn cursor_position(&self) -> Option<[f64; 2]> {
        self.cursor
    }

    /// Returns the accumulated vertical mouse-wheel distance in wheel steps.
    pub fn wheel_delta(&self) -> f64 {
        self.wheel_delta
    }

    /// Clears edge and motion values after fixed updates consume them.
    pub fn clear_transient(&mut self) {
        self.pressed_keys.clear();
        self.released_keys.clear();
        self.pressed_buttons.clear();
        self.released_buttons.clear();
        self.cursor_delta = [0.0; 2];
        self.wheel_delta = 0.0;
    }

    /// Forgets held input when focus is lost so keys cannot remain stuck.
    pub fn clear_held(&mut self) {
        self.released_keys.extend(self.keys.drain());
        self.released_buttons.extend(self.buttons.drain());
        self.cursor = None;
        self.cursor_delta = [0.0; 2];
        self.wheel_delta = 0.0;
    }

    pub(super) fn set_key(&mut self, code: KeyCode, pressed: bool) {
        if pressed {
            if self.keys.insert(code) {
                self.pressed_keys.insert(code);
            }
        } else if self.keys.remove(&code) {
            self.released_keys.insert(code);
        }
    }

    pub(super) fn set_button(&mut self, button: MouseButton, pressed: bool) {
        if pressed {
            if self.buttons.insert(button) {
                self.pressed_buttons.insert(button);
            }
        } else if self.buttons.remove(&button) {
            self.released_buttons.insert(button);
        }
    }

    pub(super) fn move_cursor(&mut self, position: [f64; 2]) {
        if let Some(previous) = self.cursor {
            self.cursor_delta[0] += position[0] - previous[0];
            self.cursor_delta[1] += position[1] - previous[1];
        }
        self.cursor = Some(position);
    }

    pub(super) fn scroll(&mut self, delta: f64) {
        self.wheel_delta += delta;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_state_lasts_until_the_game_consumes_it() {
        let mut input = InputState::default();
        input.set_key(KeyCode::Space, true);
        assert!(input.key_down(KeyCode::Space));
        assert!(input.key_pressed(KeyCode::Space));
        input.clear_transient();
        assert!(input.key_down(KeyCode::Space));
        assert!(!input.key_pressed(KeyCode::Space));
        input.set_key(KeyCode::Space, false);
        assert!(input.key_released(KeyCode::Space));
    }

    #[test]
    fn losing_focus_releases_held_keys() {
        let mut input = InputState::default();
        input.set_key(KeyCode::Space, true);
        input.move_cursor([128.0, 64.0]);
        input.clear_held();
        assert!(!input.key_down(KeyCode::Space));
        assert!(input.key_released(KeyCode::Space));
        assert_eq!(input.cursor_position(), None);
        input.move_cursor([4.0, 4.0]);
        assert_eq!(input.cursor_delta(), [0.0, 0.0]);
    }

    #[test]
    fn clearing_transients_preserves_held_input_and_resets_wheel() {
        let mut input = InputState::default();
        input.set_key(KeyCode::KeyW, true);
        input.scroll(1.0);
        input.clear_transient();
        assert!(input.key_down(KeyCode::KeyW));
        assert_eq!(input.wheel_delta(), 0.0);
    }
}
