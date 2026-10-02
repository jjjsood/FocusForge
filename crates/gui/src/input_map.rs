// SPDX-License-Identifier: GPL-3.0-or-later

//! The input table as data: one `Trigger` -> `Action` lookup, shared by
//! stacking and editor mode. No
//! `use slint::` and no `use wgpu::` anywhere in this file --
//! `crates/gui/tests/input_map.rs`'s `input_modules_are_slint_free` scans it
//! the same way `controller_api_is_slint_free` scans `controller.rs`.

use std::fmt;
use std::str::FromStr;

use crate::controller::ShellMode;

/// The controller-tracked pseudo-modifier `space` sits beside the three
/// real keyboard modifiers Slint reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub space: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        space: false,
    };

    /// `self` with `shift` forced on -- the horizontal-wheel axis
    /// normalisation.
    pub fn with_shift(self) -> Self {
        Self {
            shift: true,
            ..self
        }
    }
}

/// Mapped from `slint::platform::PointerEventButton` in `bridge.rs`, never
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    Other,
}

fn button_token(button: Button) -> &'static str {
    match button {
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
        Button::Other => "other",
    }
}

/// One gesture the window can dispatch, carrying the modifiers active when
/// it fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Wheel(Modifiers),
    Drag { button: Button, mods: Modifiers },
    Click { button: Button, mods: Modifiers },
    Key { key: char, mods: Modifiers },
}

/// A trigger spelling `Trigger::from_str` refused, by name -- names the
/// accepted device tokens so the message is self-documenting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown trigger {spelling:?} -- accepted device tokens: wheel, left-drag, middle-drag, \
     right-drag, left-click, middle-click, right-click, or a single character, each optionally \
     prefixed by ctrl+/shift+/alt+/space+ in that order"
)]
pub struct TriggerParseError {
    pub spelling: String,
}

fn modifier_prefix(rest: &str) -> (Modifiers, &str) {
    let mut mods = Modifiers::NONE;
    let mut rest = rest;
    loop {
        if let Some(r) = rest.strip_prefix("ctrl+") {
            mods.ctrl = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("shift+") {
            mods.shift = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("alt+") {
            mods.alt = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("space+") {
            mods.space = true;
            rest = r;
        } else {
            break;
        }
    }
    (mods, rest)
}

impl FromStr for Trigger {
    type Err = TriggerParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (mods, device) = modifier_prefix(s);
        let err = || TriggerParseError {
            spelling: s.to_string(),
        };
        match device {
            "wheel" => Ok(Trigger::Wheel(mods)),
            "left-drag" => Ok(Trigger::Drag {
                button: Button::Left,
                mods,
            }),
            "middle-drag" => Ok(Trigger::Drag {
                button: Button::Middle,
                mods,
            }),
            "right-drag" => Ok(Trigger::Drag {
                button: Button::Right,
                mods,
            }),
            "left-click" => Ok(Trigger::Click {
                button: Button::Left,
                mods,
            }),
            "middle-click" => Ok(Trigger::Click {
                button: Button::Middle,
                mods,
            }),
            "right-click" => Ok(Trigger::Click {
                button: Button::Right,
                mods,
            }),
            "" => Err(err()),
            _ => {
                let mut chars = device.chars();
                match (chars.next(), chars.next()) {
                    (Some(key), None) => Ok(Trigger::Key { key, mods }),
                    _ => Err(err()),
                }
            }
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mods = match self {
            Trigger::Wheel(m) | Trigger::Key { mods: m, .. } => *m,
            Trigger::Drag { mods, .. } | Trigger::Click { mods, .. } => *mods,
        };
        if mods.ctrl {
            write!(f, "ctrl+")?;
        }
        if mods.shift {
            write!(f, "shift+")?;
        }
        if mods.alt {
            write!(f, "alt+")?;
        }
        if mods.space {
            write!(f, "space+")?;
        }
        match self {
            Trigger::Wheel(_) => write!(f, "wheel"),
            Trigger::Drag { button, .. } => write!(f, "{}-drag", button_token(*button)),
            Trigger::Click { button, .. } => write!(f, "{}-click", button_token(*button)),
            Trigger::Key { key, .. } => write!(f, "{key}"),
        }
    }
}

/// The ten actions the table can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ZoomAtCursor,
    StepSourceFrame,
    PanHorizontal,
    Pan,
    BrushSize,
    BrushSoftness,
    Undo,
    Redo,
    PickSourceUnderPointer,
    Paint,
}

impl Action {
    pub const ALL: [Action; 10] = [
        Action::ZoomAtCursor,
        Action::StepSourceFrame,
        Action::PanHorizontal,
        Action::Pan,
        Action::BrushSize,
        Action::BrushSoftness,
        Action::Undo,
        Action::Redo,
        Action::PickSourceUnderPointer,
        Action::Paint,
    ];
}

/// An action spelling `Action::from_str` refused, by name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown action {spelling:?} -- accepted actions: zoom-at-cursor, step-source-frame, \
     pan-horizontal, pan, brush-size, brush-softness, undo, redo, \
     pick-source-under-pointer, paint"
)]
pub struct ActionParseError {
    pub spelling: String,
}

impl FromStr for Action {
    type Err = ActionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "zoom-at-cursor" => Ok(Action::ZoomAtCursor),
            "step-source-frame" => Ok(Action::StepSourceFrame),
            "pan-horizontal" => Ok(Action::PanHorizontal),
            "pan" => Ok(Action::Pan),
            "brush-size" => Ok(Action::BrushSize),
            "brush-softness" => Ok(Action::BrushSoftness),
            "undo" => Ok(Action::Undo),
            "redo" => Ok(Action::Redo),
            "pick-source-under-pointer" => Ok(Action::PickSourceUnderPointer),
            "paint" => Ok(Action::Paint),
            _ => Err(ActionParseError {
                spelling: s.to_string(),
            }),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Action::ZoomAtCursor => "zoom-at-cursor",
            Action::StepSourceFrame => "step-source-frame",
            Action::PanHorizontal => "pan-horizontal",
            Action::Pan => "pan",
            Action::BrushSize => "brush-size",
            Action::BrushSoftness => "brush-softness",
            Action::Undo => "undo",
            Action::Redo => "redo",
            Action::PickSourceUnderPointer => "pick-source-under-pointer",
            Action::Paint => "paint",
        };
        write!(f, "{s}")
    }
}

/// One row of the flat, trigger-keyed table. `None` is an explicit `—`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub trigger: Trigger,
    pub stacking: Option<Action>,
    pub editor: Option<Action>,
}

const M: Modifiers = Modifiers::NONE;

/// Nine trigger-keyed entries, presented as six display rows -- the ninth
/// (`Paint`'s drag) is a hidden row, absent from `DISPLAY_GROUPS`.
/// `defaults_match_the_settled_table` asserts the mapping row for row.
pub const DEFAULT_BINDINGS: [Binding; 9] = [
    Binding {
        trigger: Trigger::Wheel(M),
        stacking: Some(Action::ZoomAtCursor),
        editor: Some(Action::ZoomAtCursor),
    },
    Binding {
        trigger: Trigger::Wheel(Modifiers { ctrl: true, ..M }),
        stacking: Some(Action::StepSourceFrame),
        editor: Some(Action::StepSourceFrame),
    },
    Binding {
        trigger: Trigger::Wheel(Modifiers { shift: true, ..M }),
        stacking: Some(Action::PanHorizontal),
        editor: Some(Action::BrushSize),
    },
    Binding {
        trigger: Trigger::Drag {
            button: Button::Middle,
            mods: M,
        },
        stacking: Some(Action::Pan),
        editor: Some(Action::Pan),
    },
    Binding {
        trigger: Trigger::Drag {
            button: Button::Left,
            mods: Modifiers { space: true, ..M },
        },
        stacking: Some(Action::Pan),
        editor: Some(Action::Pan),
    },
    Binding {
        trigger: Trigger::Key {
            key: 'z',
            mods: Modifiers { ctrl: true, ..M },
        },
        stacking: None,
        editor: Some(Action::Undo),
    },
    Binding {
        trigger: Trigger::Key {
            key: 'z',
            mods: Modifiers {
                ctrl: true,
                shift: true,
                ..M
            },
        },
        stacking: None,
        editor: Some(Action::Redo),
    },
    Binding {
        trigger: Trigger::Click {
            button: Button::Left,
            mods: Modifiers { alt: true, ..M },
        },
        stacking: Some(Action::PickSourceUnderPointer),
        editor: Some(Action::PickSourceUnderPointer),
    },
    Binding {
        trigger: Trigger::Drag {
            button: Button::Left,
            mods: M,
        },
        stacking: None,
        editor: Some(Action::Paint),
    },
];

/// The six display rows the settings table shows, `—` where the column is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRowView {
    pub input: String,
    pub stacking: String,
    pub editor: String,
}

/// The `DEFAULT_BINDINGS` indices each of the six display rows groups --
/// `[5, 6]` (Ctrl+Z / Ctrl+Shift+Z) is the one row whose two triggers name
/// *different* actions per mode column.
const DISPLAY_GROUPS: [&[usize]; 6] = [&[0], &[1], &[2], &[3, 4], &[5, 6], &[7]];

fn column(
    bindings: &[Binding],
    group: &[usize],
    select: impl Fn(&Binding) -> Option<Action>,
) -> String {
    let mut values: Vec<Action> = Vec::new();
    for &i in group {
        if let Some(action) = select(&bindings[i])
            && values.last() != Some(&action)
        {
            values.push(action);
        }
    }
    if values.is_empty() {
        "—".to_string()
    } else {
        values
            .iter()
            .map(Action::to_string)
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// The whole table: one `Vec<Binding>`, `Default` = the shipped defaults, so
/// `AppController::new()` already has the table before any disk access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTable {
    pub bindings: Vec<Binding>,
}

impl Default for InputTable {
    fn default() -> Self {
        Self {
            bindings: DEFAULT_BINDINGS.to_vec(),
        }
    }
}

impl InputTable {
    pub fn action(&self, trigger: &Trigger, mode: ShellMode) -> Option<Action> {
        let binding = self.bindings.iter().find(|b| b.trigger == *trigger)?;
        match mode {
            ShellMode::Stacking => binding.stacking,
            ShellMode::Editor => binding.editor,
        }
    }

    /// The six display rows the settings table shows.
    pub fn display_rows(&self) -> Vec<InputRowView> {
        DISPLAY_GROUPS
            .iter()
            .map(|group| InputRowView {
                input: group
                    .iter()
                    .map(|&i| self.bindings[i].trigger.to_string())
                    .collect::<Vec<_>>()
                    .join(" / "),
                stacking: column(&self.bindings, group, |b| b.stacking),
                editor: column(&self.bindings, group, |b| b.editor),
            })
            .collect()
    }
}

/// Every event the input dispatch point can raise -- `Debug, Clone,
/// PartialEq` only, not `Eq`: it carries `f32`.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionEvent {
    Fired { action: Action, steps: f32 },
    Start { action: Action, x: f32, y: f32 },
    Update { action: Action, dx: f32, dy: f32 },
    End { action: Action },
}

impl ActionEvent {
    pub fn action(&self) -> Action {
        match *self {
            ActionEvent::Fired { action, .. }
            | ActionEvent::Start { action, .. }
            | ActionEvent::Update { action, .. }
            | ActionEvent::End { action } => action,
        }
    }
}

/// Whether a pointer-down, pointer-up (or cancel, folded into `Up`) or a
/// bare pointer-move reached the dispatch point.
///
/// `Move` exists because Slint's `pointer-event` callback fires with
/// `PointerEventKind::Move` on *every* mouse movement over a `TouchArea`,
/// before `moved` (`i-slint-core` 1.17.1's `TouchArea` implementation) --
/// including the very first movement of a drag. `AppController::pointer`
/// ignores it entirely: `moved` (`AppController::pointer_moved`) is the real
/// motion source, and folding `Move` into `Up` would end a live drag after
/// zero `ActionUpdate`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    Down,
    Up,
    Move,
}
