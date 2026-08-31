//! Input events abstraction
//!
//! Defines platform-agnostic input events.

use bitflags::bitflags;

use crate::pane::{PaneDirection, SplitDirection};

bitflags! {
    /// Keyboard modifiers
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
        const HYPER = 1 << 4;
        const META = 1 << 5;
        const CAPS_LOCK = 1 << 6;
        const NUM_LOCK = 1 << 7;
    }
}

/// Keyboard key codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    // Letters
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,

    // Numbers
    Key0,
    Key1,
    Key2,
    Key3,
    Key4,
    Key5,
    Key6,
    Key7,
    Key8,
    Key9,

    // Function keys
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,

    // Navigation
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,

    // Editing
    Insert,
    Delete,
    Backspace,
    Enter,
    Tab,

    // Modifiers (as keys)
    Escape,
    Space,

    // Punctuation
    Minus,
    Equals,
    LeftBracket,
    RightBracket,
    Semicolon,
    Quote,
    Backquote,
    Backslash,
    Comma,
    Period,
    Slash,

    // Numpad
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,

    // Other
    PrintScreen,
    ScrollLock,
    Pause,
    CapsLock,
    NumLock,

    /// Unknown key
    Unknown,
}

/// Mouse button
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// Scroll direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Input event types
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Key was pressed
    KeyPress {
        key: KeyCode,
        modifiers: Modifiers,
        /// Character representation if available
        text: Option<String>,
    },

    /// Key was released
    KeyRelease { key: KeyCode, modifiers: Modifiers },

    /// Mouse button pressed
    MousePress {
        button: MouseButton,
        x: f64,
        y: f64,
        modifiers: Modifiers,
    },

    /// Mouse button released
    MouseRelease {
        button: MouseButton,
        x: f64,
        y: f64,
        modifiers: Modifiers,
    },

    /// Mouse moved
    MouseMove {
        x: f64,
        y: f64,
        modifiers: Modifiers,
    },

    /// Mouse scroll
    Scroll {
        direction: ScrollDirection,
        delta: f64,
        x: f64,
        y: f64,
        modifiers: Modifiers,
    },

    /// Window focus gained
    FocusIn,

    /// Window focus lost
    FocusOut,

    /// Window resized
    Resize { width: f64, height: f64 },

    /// Paste from clipboard
    Paste(String),
}

impl KeyCode {
    /// Get the character for this key (without modifiers)
    pub fn to_char(&self) -> Option<char> {
        match self {
            Self::A => Some('a'),
            Self::B => Some('b'),
            Self::C => Some('c'),
            Self::D => Some('d'),
            Self::E => Some('e'),
            Self::F => Some('f'),
            Self::G => Some('g'),
            Self::H => Some('h'),
            Self::I => Some('i'),
            Self::J => Some('j'),
            Self::K => Some('k'),
            Self::L => Some('l'),
            Self::M => Some('m'),
            Self::N => Some('n'),
            Self::O => Some('o'),
            Self::P => Some('p'),
            Self::Q => Some('q'),
            Self::R => Some('r'),
            Self::S => Some('s'),
            Self::T => Some('t'),
            Self::U => Some('u'),
            Self::V => Some('v'),
            Self::W => Some('w'),
            Self::X => Some('x'),
            Self::Y => Some('y'),
            Self::Z => Some('z'),
            Self::Key0 => Some('0'),
            Self::Key1 => Some('1'),
            Self::Key2 => Some('2'),
            Self::Key3 => Some('3'),
            Self::Key4 => Some('4'),
            Self::Key5 => Some('5'),
            Self::Key6 => Some('6'),
            Self::Key7 => Some('7'),
            Self::Key8 => Some('8'),
            Self::Key9 => Some('9'),
            Self::Space => Some(' '),
            Self::Minus => Some('-'),
            Self::Equals => Some('='),
            Self::LeftBracket => Some('['),
            Self::RightBracket => Some(']'),
            Self::Semicolon => Some(';'),
            Self::Quote => Some('\''),
            Self::Backquote => Some('`'),
            Self::Backslash => Some('\\'),
            Self::Comma => Some(','),
            Self::Period => Some('.'),
            Self::Slash => Some('/'),
            _ => None,
        }
    }

    /// Get the shifted character for this key
    pub fn to_shifted_char(&self) -> Option<char> {
        match self {
            Self::A => Some('A'),
            Self::B => Some('B'),
            Self::C => Some('C'),
            Self::D => Some('D'),
            Self::E => Some('E'),
            Self::F => Some('F'),
            Self::G => Some('G'),
            Self::H => Some('H'),
            Self::I => Some('I'),
            Self::J => Some('J'),
            Self::K => Some('K'),
            Self::L => Some('L'),
            Self::M => Some('M'),
            Self::N => Some('N'),
            Self::O => Some('O'),
            Self::P => Some('P'),
            Self::Q => Some('Q'),
            Self::R => Some('R'),
            Self::S => Some('S'),
            Self::T => Some('T'),
            Self::U => Some('U'),
            Self::V => Some('V'),
            Self::W => Some('W'),
            Self::X => Some('X'),
            Self::Y => Some('Y'),
            Self::Z => Some('Z'),
            Self::Key0 => Some(')'),
            Self::Key1 => Some('!'),
            Self::Key2 => Some('@'),
            Self::Key3 => Some('#'),
            Self::Key4 => Some('$'),
            Self::Key5 => Some('%'),
            Self::Key6 => Some('^'),
            Self::Key7 => Some('&'),
            Self::Key8 => Some('*'),
            Self::Key9 => Some('('),
            Self::Space => Some(' '),
            Self::Minus => Some('_'),
            Self::Equals => Some('+'),
            Self::LeftBracket => Some('{'),
            Self::RightBracket => Some('}'),
            Self::Semicolon => Some(':'),
            Self::Quote => Some('"'),
            Self::Backquote => Some('~'),
            Self::Backslash => Some('|'),
            Self::Comma => Some('<'),
            Self::Period => Some('>'),
            Self::Slash => Some('?'),
            _ => None,
        }
    }
}

/// Action that can be bound to a shortcut
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    // Tab actions
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    NextAlertedTab,
    Tab(u8), // Tab 1-9

    // Pane actions
    SplitPane(SplitDirection),
    ClosePane,
    FocusPane(PaneDirection),
    ResizePane(PaneDirection),
    TogglePaneZoom,

    // Window actions
    NewWindow,
    CloseWindow,

    // Edit actions
    Copy,
    Paste,
    SelectAll,

    // View actions
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFullscreen,

    // Scroll actions
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    PromptPrevious,
    PromptNext,

    // Other
    OpenPreferences,
    FindText,
    ResetTerminal,
    QuickOpenTemplate,
}

/// Stable identifiers for built-in actions.
///
/// These strings form a compatibility boundary for configuration, automation,
/// and future plugin APIs. They must not be changed when Rust names are
/// refactored.
pub mod action_ids {
    pub const NEW_TAB: &str = "cterm:new-tab";
    pub const CLOSE_TAB: &str = "cterm:close-tab";
    pub const NEXT_TAB: &str = "cterm:next-tab";
    pub const PREVIOUS_TAB: &str = "cterm:previous-tab";
    pub const NEXT_ALERTED_TAB: &str = "cterm:next-alerted-tab";
    pub const SELECT_TAB: &str = "cterm:select-tab";

    pub const SPLIT_PANE: &str = "cterm:split-pane";
    pub const CLOSE_PANE: &str = "cterm:close-pane";
    pub const FOCUS_PANE: &str = "cterm:focus-pane";
    pub const RESIZE_PANE: &str = "cterm:resize-pane";
    pub const TOGGLE_PANE_ZOOM: &str = "cterm:toggle-pane-zoom";

    pub const NEW_WINDOW: &str = "cterm:new-window";
    pub const CLOSE_WINDOW: &str = "cterm:close-window";

    pub const COPY: &str = "cterm:copy";
    pub const PASTE: &str = "cterm:paste";
    pub const SELECT_ALL: &str = "cterm:select-all";

    pub const ZOOM_IN: &str = "cterm:zoom-in";
    pub const ZOOM_OUT: &str = "cterm:zoom-out";
    pub const ZOOM_RESET: &str = "cterm:zoom-reset";
    pub const TOGGLE_FULLSCREEN: &str = "cterm:toggle-fullscreen";

    pub const SCROLL_UP: &str = "cterm:scroll-up";
    pub const SCROLL_DOWN: &str = "cterm:scroll-down";
    pub const SCROLL_PAGE_UP: &str = "cterm:scroll-page-up";
    pub const SCROLL_PAGE_DOWN: &str = "cterm:scroll-page-down";
    pub const SCROLL_TO_TOP: &str = "cterm:scroll-to-top";
    pub const SCROLL_TO_BOTTOM: &str = "cterm:scroll-to-bottom";
    pub const PROMPT_PREVIOUS: &str = "cterm:previous-prompt";
    pub const PROMPT_NEXT: &str = "cterm:next-prompt";

    pub const OPEN_PREFERENCES: &str = "cterm:open-preferences";
    pub const FIND_TEXT: &str = "cterm:find-text";
    pub const RESET_TERMINAL: &str = "cterm:reset-terminal";
    pub const QUICK_OPEN_TEMPLATE: &str = "cterm:quick-open-template";

    /// Every stable built-in action identifier.
    pub const BUILTIN: &[&str] = &[
        NEW_TAB,
        CLOSE_TAB,
        NEXT_TAB,
        PREVIOUS_TAB,
        NEXT_ALERTED_TAB,
        SELECT_TAB,
        SPLIT_PANE,
        CLOSE_PANE,
        FOCUS_PANE,
        RESIZE_PANE,
        TOGGLE_PANE_ZOOM,
        NEW_WINDOW,
        CLOSE_WINDOW,
        COPY,
        PASTE,
        SELECT_ALL,
        ZOOM_IN,
        ZOOM_OUT,
        ZOOM_RESET,
        TOGGLE_FULLSCREEN,
        SCROLL_UP,
        SCROLL_DOWN,
        SCROLL_PAGE_UP,
        SCROLL_PAGE_DOWN,
        SCROLL_TO_TOP,
        SCROLL_TO_BOTTOM,
        PROMPT_PREVIOUS,
        PROMPT_NEXT,
        OPEN_PREFERENCES,
        FIND_TEXT,
        RESET_TERMINAL,
        QUICK_OPEN_TEMPLATE,
    ];
}

/// The type of parameter expected by a parameterized action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionParameterKind {
    Tab,
    SplitDirection,
    PaneDirection,
}

impl std::fmt::Display for ActionParameterKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Tab => "tab",
            Self::SplitDirection => "split direction",
            Self::PaneDirection => "pane direction",
        };
        formatter.write_str(name)
    }
}

/// A typed parameter for a stable action invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionParameter {
    Tab(u8),
    SplitDirection(SplitDirection),
    PaneDirection(PaneDirection),
}

impl ActionParameter {
    /// Return the parameter's type.
    pub const fn kind(self) -> ActionParameterKind {
        match self {
            Self::Tab(_) => ActionParameterKind::Tab,
            Self::SplitDirection(_) => ActionParameterKind::SplitDirection,
            Self::PaneDirection(_) => ActionParameterKind::PaneDirection,
        }
    }
}

/// A stable, owned action identifier and its optional typed parameter.
///
/// Unlike [`Action`], this representation can carry identifiers that cterm
/// does not know yet. That makes parsing configuration or plugin output
/// lossless before built-in action validation occurs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionInvocation {
    id: String,
    parameter: Option<ActionParameter>,
}

impl ActionInvocation {
    pub fn new(id: impl Into<String>, parameter: Option<ActionParameter>) -> Self {
        Self {
            id: id.into(),
            parameter,
        }
    }

    /// Return the stable action identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Return the invocation parameter, if any.
    pub const fn parameter(&self) -> Option<ActionParameter> {
        self.parameter
    }

    /// Consume the invocation and return its lossless parts.
    pub fn into_parts(self) -> (String, Option<ActionParameter>) {
        (self.id, self.parameter)
    }
}

/// Why a stable invocation cannot be converted to a built-in [`Action`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionInvocationError {
    #[error("unknown action identifier `{0}`")]
    UnknownId(String),
    #[error("action `{id}` requires a {expected} parameter")]
    MissingParameter {
        id: String,
        expected: ActionParameterKind,
    },
    #[error("action `{id}` does not accept a {actual} parameter")]
    UnexpectedParameter {
        id: String,
        actual: ActionParameterKind,
    },
    #[error("action `{id}` requires a {expected} parameter, not {actual}")]
    WrongParameterKind {
        id: String,
        expected: ActionParameterKind,
        actual: ActionParameterKind,
    },
}

impl Action {
    /// Return this action's stable identifier.
    pub const fn id(&self) -> &'static str {
        use action_ids as ids;

        match self {
            Self::NewTab => ids::NEW_TAB,
            Self::CloseTab => ids::CLOSE_TAB,
            Self::NextTab => ids::NEXT_TAB,
            Self::PrevTab => ids::PREVIOUS_TAB,
            Self::NextAlertedTab => ids::NEXT_ALERTED_TAB,
            Self::Tab(_) => ids::SELECT_TAB,
            Self::SplitPane(_) => ids::SPLIT_PANE,
            Self::ClosePane => ids::CLOSE_PANE,
            Self::FocusPane(_) => ids::FOCUS_PANE,
            Self::ResizePane(_) => ids::RESIZE_PANE,
            Self::TogglePaneZoom => ids::TOGGLE_PANE_ZOOM,
            Self::NewWindow => ids::NEW_WINDOW,
            Self::CloseWindow => ids::CLOSE_WINDOW,
            Self::Copy => ids::COPY,
            Self::Paste => ids::PASTE,
            Self::SelectAll => ids::SELECT_ALL,
            Self::ZoomIn => ids::ZOOM_IN,
            Self::ZoomOut => ids::ZOOM_OUT,
            Self::ZoomReset => ids::ZOOM_RESET,
            Self::ToggleFullscreen => ids::TOGGLE_FULLSCREEN,
            Self::ScrollUp => ids::SCROLL_UP,
            Self::ScrollDown => ids::SCROLL_DOWN,
            Self::ScrollPageUp => ids::SCROLL_PAGE_UP,
            Self::ScrollPageDown => ids::SCROLL_PAGE_DOWN,
            Self::ScrollToTop => ids::SCROLL_TO_TOP,
            Self::ScrollToBottom => ids::SCROLL_TO_BOTTOM,
            Self::PromptPrevious => ids::PROMPT_PREVIOUS,
            Self::PromptNext => ids::PROMPT_NEXT,
            Self::OpenPreferences => ids::OPEN_PREFERENCES,
            Self::FindText => ids::FIND_TEXT,
            Self::ResetTerminal => ids::RESET_TERMINAL,
            Self::QuickOpenTemplate => ids::QUICK_OPEN_TEMPLATE,
        }
    }

    /// Return this action's typed parameter, if any.
    pub const fn parameter(&self) -> Option<ActionParameter> {
        match self {
            Self::Tab(tab) => Some(ActionParameter::Tab(*tab)),
            Self::SplitPane(direction) => Some(ActionParameter::SplitDirection(*direction)),
            Self::FocusPane(direction) | Self::ResizePane(direction) => {
                Some(ActionParameter::PaneDirection(*direction))
            }
            _ => None,
        }
    }

    /// Convert this action to its stable, lossless invocation.
    pub fn to_invocation(&self) -> ActionInvocation {
        ActionInvocation::new(self.id(), self.parameter())
    }
}

impl From<&Action> for ActionInvocation {
    fn from(action: &Action) -> Self {
        action.to_invocation()
    }
}

impl From<Action> for ActionInvocation {
    fn from(action: Action) -> Self {
        Self::from(&action)
    }
}

impl TryFrom<&ActionInvocation> for Action {
    type Error = ActionInvocationError;

    fn try_from(invocation: &ActionInvocation) -> Result<Self, Self::Error> {
        use action_ids as ids;

        let action = match invocation.id() {
            ids::NEW_TAB => no_parameter(invocation, Self::NewTab),
            ids::CLOSE_TAB => no_parameter(invocation, Self::CloseTab),
            ids::NEXT_TAB => no_parameter(invocation, Self::NextTab),
            ids::PREVIOUS_TAB => no_parameter(invocation, Self::PrevTab),
            ids::NEXT_ALERTED_TAB => no_parameter(invocation, Self::NextAlertedTab),
            ids::SELECT_TAB => tab_parameter(invocation).map(Self::Tab),
            ids::SPLIT_PANE => split_direction_parameter(invocation).map(Self::SplitPane),
            ids::CLOSE_PANE => no_parameter(invocation, Self::ClosePane),
            ids::FOCUS_PANE => pane_direction_parameter(invocation).map(Self::FocusPane),
            ids::RESIZE_PANE => pane_direction_parameter(invocation).map(Self::ResizePane),
            ids::TOGGLE_PANE_ZOOM => no_parameter(invocation, Self::TogglePaneZoom),
            ids::NEW_WINDOW => no_parameter(invocation, Self::NewWindow),
            ids::CLOSE_WINDOW => no_parameter(invocation, Self::CloseWindow),
            ids::COPY => no_parameter(invocation, Self::Copy),
            ids::PASTE => no_parameter(invocation, Self::Paste),
            ids::SELECT_ALL => no_parameter(invocation, Self::SelectAll),
            ids::ZOOM_IN => no_parameter(invocation, Self::ZoomIn),
            ids::ZOOM_OUT => no_parameter(invocation, Self::ZoomOut),
            ids::ZOOM_RESET => no_parameter(invocation, Self::ZoomReset),
            ids::TOGGLE_FULLSCREEN => no_parameter(invocation, Self::ToggleFullscreen),
            ids::SCROLL_UP => no_parameter(invocation, Self::ScrollUp),
            ids::SCROLL_DOWN => no_parameter(invocation, Self::ScrollDown),
            ids::SCROLL_PAGE_UP => no_parameter(invocation, Self::ScrollPageUp),
            ids::SCROLL_PAGE_DOWN => no_parameter(invocation, Self::ScrollPageDown),
            ids::SCROLL_TO_TOP => no_parameter(invocation, Self::ScrollToTop),
            ids::SCROLL_TO_BOTTOM => no_parameter(invocation, Self::ScrollToBottom),
            ids::PROMPT_PREVIOUS => no_parameter(invocation, Self::PromptPrevious),
            ids::PROMPT_NEXT => no_parameter(invocation, Self::PromptNext),
            ids::OPEN_PREFERENCES => no_parameter(invocation, Self::OpenPreferences),
            ids::FIND_TEXT => no_parameter(invocation, Self::FindText),
            ids::RESET_TERMINAL => no_parameter(invocation, Self::ResetTerminal),
            ids::QUICK_OPEN_TEMPLATE => no_parameter(invocation, Self::QuickOpenTemplate),
            unknown => Err(ActionInvocationError::UnknownId(unknown.to_string())),
        }?;

        Ok(action)
    }
}

impl TryFrom<ActionInvocation> for Action {
    type Error = ActionInvocationError;

    fn try_from(invocation: ActionInvocation) -> Result<Self, Self::Error> {
        Self::try_from(&invocation)
    }
}

fn no_parameter(
    invocation: &ActionInvocation,
    action: Action,
) -> Result<Action, ActionInvocationError> {
    match invocation.parameter() {
        None => Ok(action),
        Some(parameter) => Err(ActionInvocationError::UnexpectedParameter {
            id: invocation.id().to_string(),
            actual: parameter.kind(),
        }),
    }
}

fn tab_parameter(invocation: &ActionInvocation) -> Result<u8, ActionInvocationError> {
    match invocation.parameter() {
        None => Err(ActionInvocationError::MissingParameter {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::Tab,
        }),
        Some(ActionParameter::Tab(tab)) => Ok(tab),
        Some(parameter) => Err(ActionInvocationError::WrongParameterKind {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::Tab,
            actual: parameter.kind(),
        }),
    }
}

fn split_direction_parameter(
    invocation: &ActionInvocation,
) -> Result<SplitDirection, ActionInvocationError> {
    match invocation.parameter() {
        None => Err(ActionInvocationError::MissingParameter {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::SplitDirection,
        }),
        Some(ActionParameter::SplitDirection(direction)) => Ok(direction),
        Some(parameter) => Err(ActionInvocationError::WrongParameterKind {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::SplitDirection,
            actual: parameter.kind(),
        }),
    }
}

fn pane_direction_parameter(
    invocation: &ActionInvocation,
) -> Result<PaneDirection, ActionInvocationError> {
    match invocation.parameter() {
        None => Err(ActionInvocationError::MissingParameter {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::PaneDirection,
        }),
        Some(ActionParameter::PaneDirection(direction)) => Ok(direction),
        Some(parameter) => Err(ActionInvocationError::WrongParameterKind {
            id: invocation.id().to_string(),
            expected: ActionParameterKind::PaneDirection,
            actual: parameter.kind(),
        }),
    }
}

/// A keyboard shortcut
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub key: KeyCode,
    pub modifiers: Modifiers,
}

impl Shortcut {
    pub fn new(key: KeyCode, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }

    /// Create a shortcut with Ctrl modifier
    pub fn ctrl(key: KeyCode) -> Self {
        Self::new(key, Modifiers::CTRL)
    }

    /// Create a shortcut with Ctrl+Shift modifiers
    pub fn ctrl_shift(key: KeyCode) -> Self {
        Self::new(key, Modifiers::CTRL | Modifiers::SHIFT)
    }

    /// Check if an input event matches this shortcut
    pub fn matches(&self, event: &InputEvent) -> bool {
        match event {
            InputEvent::KeyPress { key, modifiers, .. } => {
                *key == self.key && *modifiers == self.modifiers
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn every_action() -> Vec<Action> {
        vec![
            Action::NewTab,
            Action::CloseTab,
            Action::NextTab,
            Action::PrevTab,
            Action::NextAlertedTab,
            Action::Tab(0),
            Action::Tab(1),
            Action::Tab(u8::MAX),
            Action::SplitPane(SplitDirection::Horizontal),
            Action::SplitPane(SplitDirection::Vertical),
            Action::ClosePane,
            Action::FocusPane(PaneDirection::Left),
            Action::FocusPane(PaneDirection::Right),
            Action::FocusPane(PaneDirection::Up),
            Action::FocusPane(PaneDirection::Down),
            Action::ResizePane(PaneDirection::Left),
            Action::ResizePane(PaneDirection::Right),
            Action::ResizePane(PaneDirection::Up),
            Action::ResizePane(PaneDirection::Down),
            Action::TogglePaneZoom,
            Action::NewWindow,
            Action::CloseWindow,
            Action::Copy,
            Action::Paste,
            Action::SelectAll,
            Action::ZoomIn,
            Action::ZoomOut,
            Action::ZoomReset,
            Action::ToggleFullscreen,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::ScrollPageUp,
            Action::ScrollPageDown,
            Action::ScrollToTop,
            Action::ScrollToBottom,
            Action::PromptPrevious,
            Action::PromptNext,
            Action::OpenPreferences,
            Action::FindText,
            Action::ResetTerminal,
            Action::QuickOpenTemplate,
        ]
    }

    #[test]
    fn every_action_round_trips_through_its_stable_invocation() {
        for action in every_action() {
            let invocation = action.to_invocation();
            assert!(action_ids::BUILTIN.contains(&invocation.id()));
            assert_eq!(Action::try_from(&invocation), Ok(action));
        }
    }

    #[test]
    fn stable_action_ids_match_the_public_contract() {
        let cases = [
            (Action::NewTab, "cterm:new-tab"),
            (Action::CloseTab, "cterm:close-tab"),
            (Action::NextTab, "cterm:next-tab"),
            (Action::PrevTab, "cterm:previous-tab"),
            (Action::NextAlertedTab, "cterm:next-alerted-tab"),
            (Action::Tab(7), "cterm:select-tab"),
            (
                Action::SplitPane(SplitDirection::Horizontal),
                "cterm:split-pane",
            ),
            (Action::ClosePane, "cterm:close-pane"),
            (Action::FocusPane(PaneDirection::Left), "cterm:focus-pane"),
            (
                Action::ResizePane(PaneDirection::Right),
                "cterm:resize-pane",
            ),
            (Action::TogglePaneZoom, "cterm:toggle-pane-zoom"),
            (Action::NewWindow, "cterm:new-window"),
            (Action::CloseWindow, "cterm:close-window"),
            (Action::Copy, "cterm:copy"),
            (Action::Paste, "cterm:paste"),
            (Action::SelectAll, "cterm:select-all"),
            (Action::ZoomIn, "cterm:zoom-in"),
            (Action::ZoomOut, "cterm:zoom-out"),
            (Action::ZoomReset, "cterm:zoom-reset"),
            (Action::ToggleFullscreen, "cterm:toggle-fullscreen"),
            (Action::ScrollUp, "cterm:scroll-up"),
            (Action::ScrollDown, "cterm:scroll-down"),
            (Action::ScrollPageUp, "cterm:scroll-page-up"),
            (Action::ScrollPageDown, "cterm:scroll-page-down"),
            (Action::ScrollToTop, "cterm:scroll-to-top"),
            (Action::ScrollToBottom, "cterm:scroll-to-bottom"),
            (Action::PromptPrevious, "cterm:previous-prompt"),
            (Action::PromptNext, "cterm:next-prompt"),
            (Action::OpenPreferences, "cterm:open-preferences"),
            (Action::FindText, "cterm:find-text"),
            (Action::ResetTerminal, "cterm:reset-terminal"),
            (Action::QuickOpenTemplate, "cterm:quick-open-template"),
        ];

        for (action, expected) in cases {
            assert_eq!(action.id(), expected);
        }
    }

    #[test]
    fn builtin_action_ids_are_namespaced_and_unique() {
        let unique = action_ids::BUILTIN.iter().copied().collect::<HashSet<_>>();

        assert_eq!(unique.len(), action_ids::BUILTIN.len());
        assert!(action_ids::BUILTIN
            .iter()
            .all(|id| id.starts_with("cterm:") && id.len() > "cterm:".len()));
    }

    #[test]
    fn unknown_action_ids_are_rejected_without_losing_the_id() {
        let invocation = ActionInvocation::new("plugin:example/command", None);

        assert_eq!(
            Action::try_from(&invocation),
            Err(ActionInvocationError::UnknownId(
                "plugin:example/command".to_string()
            ))
        );
    }

    #[test]
    fn action_parameters_are_validated_by_kind_and_presence() {
        assert_eq!(
            Action::try_from(&ActionInvocation::new(action_ids::SELECT_TAB, None)),
            Err(ActionInvocationError::MissingParameter {
                id: action_ids::SELECT_TAB.to_string(),
                expected: ActionParameterKind::Tab,
            })
        );
        assert_eq!(
            Action::try_from(&ActionInvocation::new(
                action_ids::NEW_TAB,
                Some(ActionParameter::Tab(1)),
            )),
            Err(ActionInvocationError::UnexpectedParameter {
                id: action_ids::NEW_TAB.to_string(),
                actual: ActionParameterKind::Tab,
            })
        );
        assert_eq!(
            Action::try_from(&ActionInvocation::new(
                action_ids::SPLIT_PANE,
                Some(ActionParameter::PaneDirection(PaneDirection::Left)),
            )),
            Err(ActionInvocationError::WrongParameterKind {
                id: action_ids::SPLIT_PANE.to_string(),
                expected: ActionParameterKind::SplitDirection,
                actual: ActionParameterKind::PaneDirection,
            })
        );
    }
}
