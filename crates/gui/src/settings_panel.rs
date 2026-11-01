// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Settings` dock panel, generated from `ParamDesc` alone. Plain Rust,
//! no `use slint::` anywhere in this file -- `build_rows` turns a
//! resolved schema into the flat [`SettingsRow`] list
//! `ui/shell/dock/settings_panel.slint`'s `for` loop renders, and every
//! `focusforge_core` reference the panel needs lives here, so
//! `crate::controller` never names that crate directly. [`crate::settings_panel::control_kind`] is
//! the crate's only `match` on `ParamKind`.

use std::collections::{BTreeMap, BTreeSet};

// Re-exported (`pub(crate)`) so `crate::controller` can name the types it
// stores on a project tab and passes to a mutator -- `ParamDesc`'s own
// fields and `resolve_in`/`registry`/`Family` stay private to this module,
// the only place that resolves a schema.
use focusforge_core::equalize::EqualizeSettings;
use focusforge_core::params::{ParamDesc, ParamKind, ResolvedParams, resolve_in};
pub(crate) use focusforge_core::params::{
    ParamValue, ParamValues, ParamViolation, StackContext, validate,
};
use focusforge_core::registry;

use crate::entry::GuiProject;

/// The active tab's own state [`build_rows`] needs -- a plain view over
/// `crate::controller::ProjectTab`'s fields, so this module never has to
/// name that type.
pub(crate) struct ActiveTabParams<'a> {
    pub ctx: Option<&'a StackContext>,
    pub errors: &'a BTreeMap<String, ParamViolation>,
    pub menu_open: Option<&'a str>,
    /// The sections whose `More...` fold-out the user opened.
    pub expanded: &'a BTreeSet<String>,
    pub fuse_id: &'a str,
    pub fuse_values: &'a ParamValues,
    pub align_id: &'a str,
    pub align_values: &'a ParamValues,
    pub project_values: &'a ParamValues,
    /// Values kept for methods other than the selected ones.
    pub remembered: &'a BTreeMap<String, ParamValues>,
}

/// The Fuse/Align algorithm ids and explicit values, then the explicit
/// project values, a newly created tab starts with: registry defaults and
/// no values for an unsaved project, the sealed manifest's own for a
/// reopened one (every key resolves `Explicit`). A manifest with no `align`
/// seeds the registry default there and the `align` switch off; one whose run
/// applied no crop seeds `auto-crop` off; `equalize` is seeded only where the run differs from its
/// method's default, on with the bound it ran with or off.
pub(crate) fn seed_algorithm_state(
    project: &GuiProject,
) -> (String, ParamValues, String, ParamValues, ParamValues) {
    match project {
        GuiProject::Unsaved(_) => (
            default_fuse_id(),
            ParamValues::new(),
            default_align_id(),
            ParamValues::new(),
            ParamValues::new(),
        ),
        GuiProject::Opened(opened) => {
            let manifest = opened.manifest();
            let (align_id, align_values) = match &manifest.align {
                Some(record) => (record.algorithm.clone(), record.params.clone()),
                None => (default_align_id(), ParamValues::new()),
            };
            let mut project_values = ParamValues::new();
            if manifest.align.is_none() {
                project_values.insert("align".to_string(), ParamValue::Bool(false));
            }
            if manifest.registration.crop.is_none() {
                project_values.insert("auto-crop".to_string(), ParamValue::Bool(false));
            }
            let equalization = manifest.registration.equalization.as_ref();
            let equalized_by_default =
                equalization_settings(&manifest.algorithm, &ParamValues::new()).is_some();
            if equalization.is_some() != equalized_by_default {
                project_values.insert(
                    "equalize".to_string(),
                    ParamValue::Bool(equalization.is_some()),
                );
            }
            if let Some(equalization) = equalization {
                project_values.insert(
                    "equalize-max-gain".to_string(),
                    ParamValue::Float(equalization.max_gain),
                );
                project_values.insert(
                    "equalize-target".to_string(),
                    ParamValue::Float(equalization.target_stops),
                );
            }
            (
                manifest.algorithm.clone(),
                manifest.params.clone(),
                align_id,
                align_values,
                project_values,
            )
        }
    }
}

/// The bool project value `id` (`align`, `auto-crop`): the explicit value
/// when set, the descriptor's default otherwise.
pub(crate) fn project_flag(values: &ParamValues, id: &str) -> bool {
    let desc = focusforge_core::project::PROJECT_PARAMS
        .iter()
        .find(|desc| desc.id == id)
        .expect("PROJECT_PARAMS declares every project flag the panel reads");
    values
        .get(id)
        .unwrap_or(&desc.default)
        .as_bool()
        .expect("a project flag is a bool descriptor")
}

/// `group` of the project schema (`ALIGN_PARAMS`, `PREPARE_PARAMS`,
/// `FINISH_PARAMS` or `PROJECT_PARAMS`) for a run fused by `fuse_id`; the
/// group as declared while the id names no registered method.
fn project_schema(group: &[ParamDesc], fuse_id: &str) -> Vec<ParamDesc> {
    registry::find_stack(fuse_id).map_or_else(
        |_| group.to_vec(),
        |method| focusforge_core::project::params_for_group(group, method),
    )
}

/// The settings a run fused by `fuse_id` equalizes with, `None` while the
/// `equalize` switch is off.
pub(crate) fn equalization_settings(
    fuse_id: &str,
    project_values: &ParamValues,
) -> Option<EqualizeSettings> {
    focusforge_core::project::equalization_settings(
        &project_schema(&focusforge_core::project::PREPARE_PARAMS, fuse_id),
        project_values,
    )
}

/// [`registry::default_stack`]'s id, empty when the registry refuses to name
/// one; a run on an empty id is refused before it starts.
fn default_fuse_id() -> String {
    registry::default_stack()
        .map(|a| a.id().to_string())
        .unwrap_or_default()
}

/// [`default_fuse_id`]'s twin for the alignment family.
fn default_align_id() -> String {
    registry::default_align()
        .map(|a| a.id().to_string())
        .unwrap_or_default()
}

/// One row's visual control. `Float { logarithmic: true }` and `Float {
/// logarithmic: false }` are deliberately two different arms -- the
/// `logarithmic` flag changes the control, not a caption on it. `Int`
/// shares [`row_height`]'s linear-slider metric with `LinearFloat` (both
/// render through `FFSlider { scale: "linear" }`) but stays its own arm,
/// since [`control_kind`]'s match has one arm per `ParamKind` shape and a
/// shared render is not a shared kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A section's own heading row: `PREPARE` / `FUSE` / `ALIGN` / `FINISH`, carrying
    /// [`HeadingState`] and the `DEFAULT` badge.
    SectionHeading,
    /// The Fuse/Align algorithm picker -- not a `ParamKind`, a pseudo-row
    /// ahead of the schema's own params.
    Picker,
    LinearFloat,
    LogFloat,
    Int,
    Bool,
    Enum,
    /// One field's violation message, dropped in beside it.
    Error,
    /// The panel's own last row: resets every section at once, enabled only
    /// once `build_rows` has produced at least one [`HeadingState::Explicit`]
    /// heading. `SettingsRow::selected` carries `"true"`/`"false"` for its
    /// enabled state, the same convention [`RowKind::Bool`] uses for its own
    /// value.
    ResetAll,
    /// A section's `More...` header: the band that opens and closes the
    /// advanced rows below it. `SettingsRow::selected` carries
    /// `"true"`/`"false"` for open, and `SettingsRow::extent` the height of
    /// the band including the rows it holds.
    More,
    /// The empty space closing an open fold-out; an open fold-out's rows are
    /// the nested rows between its [`RowKind::More`] and this row.
    MoreEnd,
}

/// A section heading's provenance axis. `Default` carries the `DEFAULT`
/// badge; `Explicit` is what makes the panel's own reset-all row enabled;
/// `Pending` carries neither, since no value has been derived yet. `Host`
/// and `HostFallback` fold into `Default` here -- both mean the app derived
/// the value, not the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingState {
    Default,
    Explicit,
    Pending,
}

/// One row of the panel's flat list -- the plain twin of
/// `ui/shell/dock/settings_panel.slint`'s generated `SettingsRow` struct.
/// Every kind uses a different subset of these fields; unused fields carry
/// their type's default.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRow {
    pub kind: RowKind,
    /// Which section a mutation on this row targets -- `"prepare"`,
    /// `"fuse"`, `"align"` or `"finish"`, or `"align-switch"` for the
    /// ALIGN switch row; every project-group section writes the tab's one
    /// project map. Param ids are unique only *within* one
    /// schema, never across the whole panel, so a callback needs this
    /// alongside `id` to know which map to write.
    pub section: String,
    /// The parameter id (`"algorithm"` for a [`RowKind::Picker`] row, the
    /// section name for a [`RowKind::SectionHeading`] row).
    pub id: String,
    pub label: String,
    /// The control's rendered value: a slider's readout, a field's text.
    pub value_text: String,
    /// A slider's thumb position, `0.0..=1.0`. Meaningless off
    /// `LinearFloat`/`LogFloat`/`Int`.
    pub fraction: f32,
    /// An enum/picker menu's option labels, in order.
    pub choices: Vec<String>,
    /// The same menu's option ids, index-paired with `choices`.
    pub choice_ids: Vec<String>,
    /// The currently selected id (`Enum`/`Picker`) or `"true"`/`"false"`
    /// (`Bool`).
    pub selected: String,
    pub menu_open: bool,
    pub heading_state: HeadingState,
    /// This row's cumulative top offset in the content column, set by
    /// [`assign_tops`] after every row is built -- the `.slint` side reads
    /// it directly rather than accumulating row heights itself, the same
    /// non-uniform-pitch shape the tab strip's fixed `tab-pitch` does not
    /// need.
    pub top: f32,
    /// Whether the row's control is inert and drawn disabled: every row of
    /// the ALIGN section while its `Enabled` switch is off.
    pub disabled: bool,
    /// Whether the row sits inside an open `More...` fold-out, on its tint.
    pub nested: bool,
    /// A [`RowKind::More`] row's band height, its own header while closed and
    /// the header plus every nested row while open; `0` on any other kind.
    pub extent: f32,
}

impl SettingsRow {
    /// A row of `kind` with every field at its type's default.
    pub(crate) fn new(kind: RowKind, section: &str, id: &str) -> Self {
        SettingsRow {
            kind,
            section: section.to_string(),
            id: id.to_string(),
            label: String::new(),
            value_text: String::new(),
            fraction: 0.0,
            choices: Vec::new(),
            choice_ids: Vec::new(),
            selected: String::new(),
            menu_open: false,
            heading_state: HeadingState::Default,
            top: 0.0,
            disabled: false,
            nested: false,
            extent: 0.0,
        }
    }

    pub(crate) fn heading(section: &str, label: &str, state: HeadingState) -> Self {
        SettingsRow {
            label: label.to_string(),
            heading_state: state,
            ..SettingsRow::new(RowKind::SectionHeading, section, section)
        }
    }
}

/// The whole panel, as `AppController::settings_panel` builds it every time
/// the active tab or its values change -- never diffed, always rebuilt: the
/// schema is small (at most a few dozen rows) and rebuilding is what keeps
/// "one section per family, one field per `ParamDesc`" true by construction
/// rather than by an incremental patch that could drift from it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PanelModel {
    pub rows: Vec<SettingsRow>,
    /// The index into `rows` of the heading currently stuck to the
    /// viewport's top, if any.
    pub sticky: Option<usize>,
    /// How far `sticky` has already been pushed off the viewport's own top
    /// edge by the next heading arriving from below -- see
    /// [`sticky_push`].
    pub sticky_push: f32,
    pub content_height: f32,
    pub scroll_y: f32,
    pub viewport_height: f32,
}

// -- Geometry, mirroring `ShellGeometry`'s panel metrics (`ui/geometry.slint`)
// exactly as `crate::controller::TAB_PITCH` mirrors `ShellGeometry.tab-pitch`.
// No inter-row gap constant: the gap before the next row's label is part of
// a row's own height, so `content_height` (a plain sum) is exactly what
// `ui/shell/dock/settings_panel.slint` lays out -- the same discipline the
// tab strip's fixed pitch already uses. --

/// `ShellGeometry.param-label-height`: a slider/field/menu row's own label
/// line, above its control. A toggle row's label sits beside the control
/// instead, so [`row_height`] does not add this for [`RowKind::Bool`].
pub const PARAM_LABEL_HEIGHT: f32 = 17.0;
/// `ShellGeometry.param-row-linear`: `FFSlider { scale: "linear" }`'s own
/// visual height plus the gap to the next row, shared by
/// [`RowKind::LinearFloat`] and [`RowKind::Int`].
pub const PARAM_ROW_LINEAR: f32 = 43.0;
/// `ShellGeometry.param-row-log`: `FFSlider { scale: "logarithmic" }`'s own
/// visual height plus its trailing gap -- 17 px more than the linear scale,
/// for the decade-tick line.
pub const PARAM_ROW_LOG: f32 = 60.0;
/// `ShellGeometry.param-row-field`: the algorithm picker's closed row plus
/// its trailing gap.
pub const PARAM_ROW_FIELD: f32 = 45.0;
/// `ShellGeometry.param-row-bool`: `FFToggle`'s own row, label and control
/// on one line; [`PARAM_ROW_BOOL_GAP`] follows it.
pub const PARAM_ROW_BOOL: f32 = 20.0;
/// `ShellGeometry.param-row-bool-gap`: the space under a toggle row, so the
/// next row or `More...` header starts one grid step below the toggle.
pub const PARAM_ROW_BOOL_GAP: f32 = 12.0;
/// `ShellGeometry.param-row-enum`: `FFMenu`'s closed row plus its trailing
/// gap.
pub const PARAM_ROW_ENUM: f32 = 43.0;
/// `ShellGeometry.param-error-height`.
pub const PARAM_ERROR_HEIGHT: f32 = 16.0;
/// `ShellGeometry.section-heading-height`: the label/badge band itself, one
/// fixed height for every [`HeadingState`] -- only the badge's own
/// visibility varies, never this row's size.
pub const SECTION_HEADING_HEIGHT: f32 = 41.0;
/// `ShellGeometry.section-heading-bottom-gap`: trailing space after the
/// band above, before the section's first field.
pub const SECTION_HEADING_BOTTOM_GAP: f32 = 8.0;
/// `ShellGeometry.panel-content-width`. Every row is this wide, whatever an
/// `EnumVariant.label` measures -- no control's geometry is derived from a
/// label's width.
pub const PANEL_CONTENT_WIDTH: f32 = 228.0;
/// A slider row's readout field, flush right, plus the gap that separates
/// it from the track (`ui/components/ff_slider.slint`'s `70px`).
pub const SLIDER_READOUT_RESERVE: f32 = 70.0;
/// A slider's own draggable track width: the row's full width minus its
/// readout field.
pub const SLIDER_TRACK_WIDTH: f32 = PANEL_CONTENT_WIDTH - SLIDER_READOUT_RESERVE;
/// Read room past the last row's own bottom edge (`Theme.space-4`):
/// without it, scrolling to the end leaves the last row's text flush
/// against the viewport's own bottom edge.
pub const PANEL_BOTTOM_BUFFER: f32 = 16.0;
/// `ShellGeometry.reset-all-top-gap`: two grid steps above the panel's own
/// bottom-of-list reset control, separating it clearly from the last
/// section -- more than [`SECTION_HEADING_BOTTOM_GAP`], since this is a
/// break between the fields and a standalone action, not a heading and its
/// own section.
pub const RESET_ALL_TOP_GAP: f32 = 32.0;
/// `ShellGeometry.reset-all-height`: the reset-all control's own height.
pub const RESET_ALL_HEIGHT: f32 = 32.0;
/// `ShellGeometry.more-height`: a `More...` header band.
pub const MORE_HEIGHT: f32 = 32.0;
/// `ShellGeometry.more-gap`: the space under a `More...` header, between it
/// and its first row (open) or the next section (closed), and under an open
/// fold-out's last row.
pub const MORE_GAP: f32 = 12.0;

/// The one match on [`ParamKind`] in the crate. `Float { logarithmic: true
/// }` and `Float { logarithmic: false }` are separate arms -- the flag
/// changes the control, not a caption on it -- and there is no `_` arm: a
/// fifth `ParamKind` shape is a compile error here, never a silent
/// mis-render.
pub fn control_kind(kind: &ParamKind) -> RowKind {
    match kind {
        ParamKind::Float {
            logarithmic: true, ..
        } => RowKind::LogFloat,
        ParamKind::Float {
            logarithmic: false, ..
        } => RowKind::LinearFloat,
        ParamKind::Int { .. } => RowKind::Int,
        ParamKind::Bool => RowKind::Bool,
        ParamKind::Enum { .. } => RowKind::Enum,
    }
}

/// A slider's thumb position for `value` against `kind`'s declared bounds,
/// `0.0..=1.0`. The logarithmic reading is `(ln v - ln min) / (ln max - ln
/// min)`, falling back to the linear reading when `min <= 0` -- a
/// synthetic `min: 0.0` descriptor cannot take a logarithm of zero.
/// Deliberately an `if let` chain, not a `match`, so [`control_kind`] stays
/// the crate's only `match` on `ParamKind`.
pub fn slider_fraction(value: f64, kind: &ParamKind) -> f32 {
    if let &ParamKind::Float {
        min,
        max,
        logarithmic,
        ..
    } = kind
    {
        if logarithmic && min > 0.0 {
            let fraction = (value.max(min).ln() - min.ln()) / (max.ln() - min.ln());
            return fraction.clamp(0.0, 1.0) as f32;
        }
        return linear_fraction(value, min, max);
    }
    if let &ParamKind::Int { min, max } = kind {
        return linear_fraction(value, min as f64, max as f64);
    }
    0.0
}

fn linear_fraction(value: f64, min: f64, max: f64) -> f32 {
    if max <= min {
        return 0.0;
    }
    (((value - min) / (max - min)).clamp(0.0, 1.0)) as f32
}

/// [`slider_fraction`]'s inverse: a thumb position back into a [`ParamValue`]
/// of `kind`'s own declared shape. `Int` rounds to the nearest whole value.
pub fn slider_value(fraction: f32, kind: &ParamKind) -> ParamValue {
    let fraction = fraction.clamp(0.0, 1.0) as f64;
    if let &ParamKind::Float {
        min,
        max,
        step,
        logarithmic,
    } = kind
    {
        if logarithmic && min > 0.0 {
            return ParamValue::Float((min.ln() + fraction * (max.ln() - min.ln())).exp());
        }
        let raw = min + fraction * (max - min);
        return ParamValue::Float(snap_to_step(raw, min, max, step));
    }
    if let &ParamKind::Int { min, max } = kind {
        let v = min as f64 + fraction * (max as f64 - min as f64);
        return ParamValue::Int(v.round() as i64);
    }
    ParamValue::Float(fraction)
}

/// The value one Left (`direction < 0`) or Right (`> 0`) key press moves a
/// slider to: a linear `Float` by its declared step (snapped), an `Int` by
/// one, both clamped to the bounds. `None` for a logarithmic `Float`, whose
/// one declared step is invisible on its scale, and for every other kind.
pub fn stepped_value(current: f64, direction: i32, kind: &ParamKind) -> Option<ParamValue> {
    let direction = f64::from(direction.signum());
    if let &ParamKind::Float {
        min,
        max,
        step,
        logarithmic: false,
    } = kind
    {
        let moved = snap_to_step(current, min, max, step) + direction * step;
        return Some(ParamValue::Float(snap_to_step(moved, min, max, step)));
    }
    if let &ParamKind::Int { min, max } = kind {
        let moved = (current + direction).round() as i64;
        return Some(ParamValue::Int(moved.clamp(min, max)));
    }
    None
}

/// Decimal places in `step`'s own written form (`0.1` -> 1, `0.01` -> 2).
fn step_decimals(step: f64) -> usize {
    format!("{step}")
        .split_once('.')
        .map_or(0, |(_, f)| f.len())
}

/// `value` on the grid `min + k * step`, rounded to the step's decimals (no
/// float residue) and clamped to `[min, max]`. A non-positive step leaves
/// `value` as it is, clamped.
fn snap_to_step(value: f64, min: f64, max: f64, step: f64) -> f64 {
    if step <= 0.0 {
        return value.clamp(min, max);
    }
    let on_grid = min + ((value - min) / step).round() * step;
    let scale = 10f64.powi(step_decimals(step) as i32);
    // `+ 0.0` turns a rounded `-0.0` into `0.0`.
    ((on_grid * scale).round() / scale).clamp(min, max) + 0.0
}

/// A linear `Float` slider's readout. A range that dips below zero reads
/// signed: step decimals, an explicit `+`, U+2212 for negatives, `0.0` at
/// zero. Every other range keeps the plain number.
fn float_text(value: f64, kind: &ParamKind) -> String {
    let &ParamKind::Float { min, step, .. } = kind else {
        return format!("{value}");
    };
    if min >= 0.0 {
        return format!("{value}");
    }
    let decimals = step_decimals(step);
    let text = format!("{:.decimals$}", value.abs());
    if text.chars().all(|c| matches!(c, '0' | '.')) {
        text
    } else if value < 0.0 {
        format!("\u{2212}{text}")
    } else {
        format!("+{text}")
    }
}

/// A [`ParamViolation`]'s message with its leading `<id>: ` dropped, so a
/// message rendered beside a field that already carries its own label does
/// not repeat the id. [`ParamViolation::UnknownKey`] carries no such prefix
/// at all and is returned verbatim; [`ParamViolation::Auto`] carries two
/// (its own, and its boxed `source`'s), so this recurses rather than
/// splitting once. [`ParamViolation::Unparsable`] is replaced outright: the
/// core's own message quotes the rejected text and names the target kind for
/// a CLI reader, but a field beside its own (still visible) value only needs
/// to say it was not one.
pub fn violation_text(violation: &ParamViolation) -> String {
    match violation {
        ParamViolation::UnknownKey { .. } => violation.to_string(),
        ParamViolation::Auto { source, .. } => {
            format!("auto value rejected: {}", violation_text(source))
        }
        // The core message quotes the unparsable text and names the target
        // kind for a CLI reader; a field beside its own value needs neither.
        ParamViolation::Unparsable { kind, .. } if *kind == "bool" => "Not true or false".into(),
        ParamViolation::Unparsable { .. } => "Not a number".into(),
        other => {
            let rendered = other.to_string();
            match rendered.split_once(": ") {
                Some((_, rest)) => rest.to_string(),
                None => rendered,
            }
        }
    }
}

/// The section key of the ALIGN section's `Enabled` switch row: it renders
/// under the `align` heading but writes the project map.
pub(crate) const ALIGN_SWITCH_SECTION: &str = "align-switch";

/// `"<section>:<id>"` -- the one key format the picker's own pseudo-field
/// (`id == "algorithm"`), an enum's open menu and a field's stored error all
/// share, so `AppController` never formats it more than once.
pub fn menu_key(section: &str, id: &str) -> String {
    format!("{section}:{id}")
}

/// The `&'static ParamDesc` `section`'s current algorithm (or, for
/// `"prepare"`, `"finish"` and the align switch, the project group) declares for `id` -- `None`
/// when the section is unknown or the algorithm behind `fuse_id`/`align_id`
/// carries no such parameter, which a mid-drag algorithm switch can produce.
pub fn find_desc(
    section: &str,
    fuse_id: &str,
    align_id: &str,
    id: &str,
) -> Option<&'static ParamDesc> {
    let schema: &'static [ParamDesc] = match section {
        "fuse" => registry::find_stack(fuse_id).ok()?.params(),
        "align" => registry::find_align(align_id).ok()?.params(),
        ALIGN_SWITCH_SECTION => &focusforge_core::project::ALIGN_PARAMS,
        "prepare" => &focusforge_core::project::PREPARE_PARAMS,
        "finish" => &focusforge_core::project::FINISH_PARAMS,
        _ => return None,
    };
    schema.iter().find(|desc| desc.id == id)
}

/// What every section shares while [`section_rows`] resolves its own rows --
/// bundled so the function takes one context argument rather than four.
#[derive(Clone, Copy)]
struct SectionContext<'a> {
    ctx: Option<&'a StackContext>,
    host: &'a focusforge_core::host::HostFacts,
    errors: &'a BTreeMap<String, ParamViolation>,
    menu_open: Option<&'a str>,
    expanded: &'a BTreeSet<String>,
}

/// One entry of a section's algorithm picker.
struct PickerOption {
    id: String,
    label: String,
}

/// Builds the whole panel's flat row list, Prepare, Fuse, Align, then Finish: each
/// section's heading, its algorithm picker (Fuse/Align only), one row per
/// plain [`ParamDesc`] in declaration order with a [`RowKind::Error`] row
/// trailing any field in violation, then a `More...` header over the advanced
/// ones. With no tab active the panel still renders, collapsed, seeded like a
/// fresh unsaved tab but with no [`StackContext`].
pub(crate) fn build_rows(active: Option<ActiveTabParams<'_>>) -> Vec<SettingsRow> {
    let empty_values = ParamValues::new();
    let empty_errors = BTreeMap::new();
    let empty_expanded = BTreeSet::new();
    let default_fuse_id = default_fuse_id();
    let default_align_id = default_align_id();

    let (
        ctx,
        errors,
        menu_open,
        expanded,
        fuse_id,
        fuse_values,
        align_id,
        align_values,
        project_values,
    ) = match &active {
        Some(a) => (
            a.ctx,
            a.errors,
            a.menu_open,
            a.expanded,
            a.fuse_id,
            a.fuse_values,
            a.align_id,
            a.align_values,
            a.project_values,
        ),
        None => (
            None,
            &empty_errors,
            None,
            &empty_expanded,
            default_fuse_id.as_str(),
            &empty_values,
            default_align_id.as_str(),
            &empty_values,
            &empty_values,
        ),
    };

    let host = focusforge_core::host::snapshot();
    let section_ctx = SectionContext {
        ctx,
        host: &host,
        errors,
        menu_open,
        expanded,
    };
    let mut rows = section_rows(
        "prepare",
        "PREPARE",
        &project_schema(&focusforge_core::project::PREPARE_PARAMS, fuse_id),
        project_values,
        &section_ctx,
        None,
    );
    // Every row after the `equalize` switch greys while it is off; the
    // stored values stay.
    let equalize_on = project_flag(project_values, "equalize");
    for row in rows
        .iter_mut()
        .skip_while(|r| r.id != "equalize")
        .skip(1)
        .take_while(|r| r.kind != RowKind::MoreEnd)
    {
        row.disabled = !equalize_on;
    }

    if let Ok(fuse) = registry::find_stack(fuse_id) {
        let mut methods = registry::stack_algorithms();
        methods.sort_by_key(|a| (!a.is_default(), a.advanced()));
        let picker = methods
            .iter()
            .filter(|a| !a.test_only())
            .map(|a| PickerOption {
                id: a.id().to_string(),
                label: a.display_name().to_string(),
            })
            .collect();
        rows.extend(section_rows(
            "fuse",
            "FUSE",
            fuse.params(),
            fuse_values,
            &section_ctx,
            Some((fuse_id, fuse.display_name(), picker)),
        ));
    }
    if let Ok(align) = registry::find_align(align_id) {
        let picker = registry::align_algorithms()
            .iter()
            .filter(|a| !a.test_only())
            .map(|a| PickerOption {
                id: a.id().to_string(),
                label: a.display_name().to_string(),
            })
            .collect();
        let mut align_rows = section_rows(
            "align",
            "ALIGN",
            align.params(),
            align_values,
            &section_ctx,
            Some((align_id, align.display_name(), picker)),
        );
        // The on/off switch is the project-level `align` value, written to
        // the project map, and is the section's first row after its heading;
        // every row below it greys while the switch is off. The heading's
        // provenance reads the algorithm's own values and the switch.
        let align_on = project_flag(project_values, "align");
        for row in align_rows.iter_mut().skip(1) {
            row.disabled = !align_on;
        }
        let switch_group = project_schema(&focusforge_core::project::ALIGN_PARAMS, fuse_id);
        let switch_rows = switch_group.iter().map(|desc| {
            let value = project_values.get(&desc.id).unwrap_or(&desc.default);
            field_row(ALIGN_SWITCH_SECTION, desc, value, false)
        });
        align_rows.splice(1..1, switch_rows);
        if project_values.contains_key("align") {
            align_rows[0].heading_state = HeadingState::Explicit;
        }
        rows.extend(align_rows);
    }
    rows.extend(section_rows(
        "finish",
        "FINISH",
        &project_schema(&focusforge_core::project::FINISH_PARAMS, fuse_id),
        project_values,
        &section_ctx,
        None,
    ));

    let any_explicit = rows
        .iter()
        .any(|r| r.kind == RowKind::SectionHeading && r.heading_state == HeadingState::Explicit)
        || active.as_ref().is_some_and(|a| !a.remembered.is_empty());
    rows.push(SettingsRow {
        selected: any_explicit.to_string(),
        heading_state: HeadingState::Pending,
        ..SettingsRow::new(RowKind::ResetAll, "", "")
    });

    rows
}

/// One section's own rows: its heading, its algorithm picker when `picker`
/// is `Some`, one row (plus a trailing [`RowKind::Error`] row on a violation)
/// per plain entry of `schema` in declaration order, then -- when the section
/// has advanced content -- a [`RowKind::More`] header, and while it is open
/// the advanced entries as nested rows and the closing [`RowKind::MoreEnd`].
/// A section opens when the user opened it or a hidden field is in violation.
fn section_rows(
    section: &str,
    heading_label: &str,
    schema: &[ParamDesc],
    values: &ParamValues,
    section_ctx: &SectionContext<'_>,
    picker: Option<(&str, &str, Vec<PickerOption>)>,
) -> Vec<SettingsRow> {
    let SectionContext {
        ctx,
        host,
        errors,
        menu_open,
        expanded,
    } = *section_ctx;

    let has_more = schema.iter().any(|desc| desc.advanced);
    let hidden_violation = schema
        .iter()
        .any(|desc| desc.advanced && errors.contains_key(&menu_key(section, &desc.id)));
    let open = expanded.contains(section) || hidden_violation;

    // A map can be shared by several sections, so only keys this schema
    // declares make its heading explicit.
    let heading_state = if schema.iter().any(|desc| values.contains_key(&desc.id)) {
        HeadingState::Explicit
    } else if ctx.is_some() {
        HeadingState::Default
    } else {
        HeadingState::Pending
    };

    let mut rows = vec![SettingsRow::heading(section, heading_label, heading_state)];

    if let Some((selected_id, selected_label, options)) = picker {
        let key = menu_key(section, "algorithm");
        let (choice_ids, choices): (Vec<String>, Vec<String>) =
            options.into_iter().map(|o| (o.id, o.label)).unzip();
        rows.push(SettingsRow {
            label: "Algorithm".to_string(),
            value_text: selected_label.to_string(),
            choices,
            choice_ids,
            selected: selected_id.to_string(),
            menu_open: menu_open == Some(key.as_str()),
            ..SettingsRow::new(RowKind::Picker, section, "algorithm")
        });
    }

    // Resolved once per section against the whole schema: cheap (at most a
    // few dozen parameters) and it keeps every field's value in the one call
    // `resolve_in` documents as authoritative. A resolution failure (an
    // out-of-range `auto`/`host_default` value, never produced by a shipped
    // schema) falls back to each descriptor's own stated default rather than
    // losing every other field in the section.
    let resolved: ResolvedParams = resolve_in(schema, values, ctx, host).unwrap_or_default();

    let param_rows = |desc: &ParamDesc, nested: bool| {
        let key = menu_key(section, &desc.id);
        let value = resolved
            .get(&desc.id)
            .map(|r| r.value.clone())
            .unwrap_or_else(|| desc.default.clone());
        let mut rows = vec![field_row(
            section,
            desc,
            &value,
            menu_open == Some(key.as_str()),
        )];
        if let Some(violation) = errors.get(&key) {
            rows.push(SettingsRow {
                value_text: violation_text(violation),
                ..SettingsRow::new(RowKind::Error, section, &desc.id)
            });
        }
        for row in &mut rows {
            row.nested = nested;
        }
        rows
    };

    for desc in schema.iter().filter(|desc| !desc.advanced) {
        rows.extend(param_rows(desc, false));
    }
    if has_more {
        let more = rows.len();
        rows.push(SettingsRow {
            selected: open.to_string(),
            extent: MORE_HEIGHT,
            ..SettingsRow::new(RowKind::More, section, "more")
        });
        if open {
            for desc in schema.iter().filter(|desc| desc.advanced) {
                rows.extend(param_rows(desc, true));
            }
            let drawer: f32 = rows[more + 1..].iter().map(row_total_height).sum();
            rows[more].extent = MORE_HEIGHT + MORE_GAP + drawer;
            rows.push(SettingsRow::new(RowKind::MoreEnd, section, "more-end"));
        }
    }

    rows
}

/// One parameter's own row, its shape ([`control_kind`]) driving which of
/// `value_text`/`fraction`/`choices`/`selected` carries the value.
pub(crate) fn field_row(
    section: &str,
    desc: &ParamDesc,
    value: &ParamValue,
    menu_open: bool,
) -> SettingsRow {
    let kind = control_kind(&desc.kind);
    let mut row = SettingsRow {
        label: desc.label.clone(),
        ..SettingsRow::new(kind, section, &desc.id)
    };
    match kind {
        RowKind::LinearFloat | RowKind::LogFloat => {
            let v = value.as_f64().unwrap_or_default();
            row.value_text = float_text(v, &desc.kind);
            row.fraction = slider_fraction(v, &desc.kind);
        }
        RowKind::Int => {
            let v = value.as_i64().unwrap_or_default();
            row.value_text = v.to_string();
            row.fraction = slider_fraction(v as f64, &desc.kind);
        }
        RowKind::Bool => {
            let v = value.as_bool().unwrap_or_default();
            row.selected = v.to_string();
            row.value_text = row.selected.clone();
        }
        RowKind::Enum => {
            let selected = value.as_enum().unwrap_or_default().to_string();
            if let ParamKind::Enum { variants } = &desc.kind {
                row.value_text = variants
                    .iter()
                    .find(|v| v.id == selected)
                    .map(|v| v.label.clone())
                    .unwrap_or_default();
                row.choice_ids = variants.iter().map(|v| v.id.clone()).collect();
                row.choices = variants.iter().map(|v| v.label.clone()).collect();
            }
            row.selected = selected;
            row.menu_open = menu_open;
        }
        RowKind::SectionHeading
        | RowKind::Picker
        | RowKind::Error
        | RowKind::ResetAll
        | RowKind::More
        | RowKind::MoreEnd => {
            unreachable!("control_kind never returns a pseudo-row kind")
        }
    }
    row
}

/// One `RowKind`'s own height -- `PARAM_LABEL_HEIGHT` plus the control's
/// height for every label-above-control row, the control's own height alone
/// for [`RowKind::Bool`] (label beside the toggle) and [`RowKind::Error`]
/// (no label). [`RowKind::SectionHeading`] is never queried here: only
/// [`content_height`] reads its dedicated [`SECTION_HEADING_HEIGHT`].
pub fn row_height(kind: RowKind) -> f32 {
    match kind {
        RowKind::SectionHeading => {
            unreachable!("a heading's height is content_height's own constant, not this table")
        }
        RowKind::Picker => PARAM_LABEL_HEIGHT + PARAM_ROW_FIELD,
        RowKind::LinearFloat | RowKind::Int => PARAM_LABEL_HEIGHT + PARAM_ROW_LINEAR,
        RowKind::LogFloat => PARAM_LABEL_HEIGHT + PARAM_ROW_LOG,
        RowKind::Enum => PARAM_LABEL_HEIGHT + PARAM_ROW_ENUM,
        RowKind::Bool => PARAM_ROW_BOOL + PARAM_ROW_BOOL_GAP,
        RowKind::Error => PARAM_ERROR_HEIGHT,
        RowKind::ResetAll => RESET_ALL_TOP_GAP + RESET_ALL_HEIGHT,
        RowKind::More => MORE_HEIGHT + MORE_GAP,
        RowKind::MoreEnd => MORE_GAP,
    }
}

/// The whole content column's height: every row's own height, a
/// [`RowKind::SectionHeading`] reading [`SECTION_HEADING_HEIGHT`] rather
/// than from [`row_height`].
pub fn content_height(rows: &[SettingsRow]) -> f32 {
    rows.iter().map(row_total_height).sum::<f32>() + PANEL_BOTTOM_BUFFER
}

/// Sets every row's [`SettingsRow::top`] to its cumulative offset in the
/// content column -- called once after the row list is built, before the
/// model is handed to `.slint`.
pub fn assign_tops(rows: &mut [SettingsRow]) {
    let mut y = 0.0;
    for row in rows.iter_mut() {
        row.top = y;
        y += row_total_height(row);
    }
}

fn row_total_height(row: &SettingsRow) -> f32 {
    if row.kind == RowKind::SectionHeading {
        SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP
    } else {
        row_height(row.kind)
    }
}

/// `offset` clamped to `0 ..= max(content - viewport, 0)` -- the panel's own
/// twin of `crate::controller::clamp_tab_scroll`.
pub fn clamp_panel_scroll(offset: f32, content_height: f32, viewport_height: f32) -> f32 {
    offset.clamp(0.0, (content_height - viewport_height).max(0.0))
}

/// The index into `rows` whose own `[top, top + height)` band contains
/// `y`, a content-column offset -- `None` above the first row or at or past
/// the last row's bottom edge.
pub fn row_at(rows: &[SettingsRow], y: f32) -> Option<usize> {
    if y < 0.0 {
        return None;
    }
    rows.iter()
        .position(|row| y < row.top + row_total_height(row))
}

/// The index into `rows` of the last [`RowKind::SectionHeading`] whose top
/// edge has scrolled past the viewport's own top -- `None` before the first
/// heading has done so. Section headings stick while the panel scrolls
/// because the heading is the only place the panel shows where a value came
/// from.
pub fn sticky_heading(rows: &[SettingsRow], offset: f32) -> Option<usize> {
    let mut y = 0.0;
    let mut sticky = None;
    for (i, row) in rows.iter().enumerate() {
        if row.kind == RowKind::SectionHeading {
            if y <= offset {
                sticky = Some(i);
            } else {
                break;
            }
        }
        y += row_total_height(row);
    }
    sticky
}

/// How far the sticky heading `sticky_heading` returned has already been
/// pushed up and off the viewport's own top edge by the next heading
/// arriving from below -- `0` until that heading is closer than one whole
/// heading row ([`SECTION_HEADING_HEIGHT`] plus its own trailing gap, the
/// same total the heading component actually draws) away, then growing to
/// that same total at the exact offset the two swap: the outgoing
/// heading's own bottom edge lines up exactly with the incoming one's top
/// edge throughout.
pub fn sticky_push(rows: &[SettingsRow], sticky: Option<usize>, offset: f32) -> f32 {
    let Some(i) = sticky else {
        return 0.0;
    };
    let Some(next) = rows[i + 1..]
        .iter()
        .find(|row| row.kind == RowKind::SectionHeading)
    else {
        return 0.0;
    };
    let row_height = SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP;
    let gap = next.top - offset;
    (row_height - gap).clamp(0.0, row_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float_desc(min: f64, max: f64, logarithmic: bool) -> ParamKind {
        ParamKind::Float {
            min,
            max,
            step: 0.1,
            logarithmic,
        }
    }

    #[test]
    fn every_param_kind_has_one_control() {
        assert_eq!(
            control_kind(&float_desc(0.0, 10.0, false)),
            RowKind::LinearFloat
        );
        assert_eq!(
            control_kind(&float_desc(1e-9, 1.0, true)),
            RowKind::LogFloat
        );
        assert_eq!(
            control_kind(&ParamKind::Int { min: 0, max: 10 }),
            RowKind::Int
        );
        assert_eq!(control_kind(&ParamKind::Bool), RowKind::Bool);
        assert_eq!(
            control_kind(&ParamKind::Enum { variants: vec![] }),
            RowKind::Enum
        );
        // The logarithmic flag changes the control, not a caption on it.
        assert_ne!(
            control_kind(&float_desc(0.0, 10.0, false)),
            control_kind(&float_desc(0.0, 10.0, true))
        );
    }

    #[test]
    fn logarithmic_float_positions_by_decade() {
        // epsilon-px: min 1e-9, max 1.0, default 1e-4.
        let kind = float_desc(1e-9, 1.0, true);
        let fraction = slider_fraction(1e-4, &kind);
        assert!(
            (fraction - 0.5556).abs() < 0.001,
            "expected ~0.5556, got {fraction}"
        );
        // The linear reading of the same value is near zero.
        let linear = linear_fraction(1e-4, 1e-9, 1.0);
        assert!(linear < 0.001);

        // A synthetic descriptor with min <= 0 must fall back to linear
        // rather than dividing by an infinite log.
        let non_positive_min = float_desc(0.0, 10.0, true);
        assert_eq!(slider_fraction(5.0, &non_positive_min), 0.5);
    }

    #[test]
    fn slider_value_round_trips_slider_fraction() {
        let kind = float_desc(1e-9, 1.0, true);
        let fraction = slider_fraction(1e-4, &kind);
        let value = slider_value(fraction, &kind);
        match value {
            ParamValue::Float(v) => assert!((v - 1e-4).abs() / 1e-4 < 0.01),
            other => panic!("expected Float, got {other:?}"),
        }

        let int_kind = ParamKind::Int { min: 0, max: 6 };
        assert_eq!(slider_value(1.0, &int_kind), ParamValue::Int(6));
        assert_eq!(slider_value(0.0, &int_kind), ParamValue::Int(0));

        // A linear Float lands exactly on its step grid: no residue, ends
        // reached, the fraction of a grid value round-trips to that value.
        let target = ParamKind::Float {
            min: -0.5,
            max: 0.5,
            step: 0.1,
            logarithmic: false,
        };
        for (fraction, expected) in [(0.0, -0.5), (0.37, -0.1), (0.5, 0.0), (1.0, 0.5)] {
            assert_eq!(slider_value(fraction, &target), ParamValue::Float(expected));
        }
        let gain = ParamKind::Float {
            min: 1.01,
            max: 2.0,
            step: 0.01,
            logarithmic: false,
        };
        assert_eq!(slider_value(0.37, &gain), ParamValue::Float(1.38));
        for value in [-0.3, 0.0, 0.2] {
            let round_trip = slider_value(slider_fraction(value, &target), &target);
            assert_eq!(round_trip, ParamValue::Float(value));
        }
    }

    #[test]
    fn stepped_value_moves_linear_and_int_sliders_and_skips_log() {
        let linear = ParamKind::Float {
            min: -0.5,
            max: 0.5,
            step: 0.1,
            logarithmic: false,
        };
        assert_eq!(stepped_value(0.2, 1, &linear), Some(ParamValue::Float(0.3)));
        assert_eq!(
            stepped_value(0.5, 1, &linear),
            Some(ParamValue::Float(0.5)),
            "clamped at the maximum"
        );
        assert_eq!(
            stepped_value(-0.5, -1, &linear),
            Some(ParamValue::Float(-0.5)),
            "clamped at the minimum"
        );

        let int_kind = ParamKind::Int { min: 0, max: 6 };
        assert_eq!(stepped_value(3.0, 1, &int_kind), Some(ParamValue::Int(4)));
        assert_eq!(stepped_value(6.0, 1, &int_kind), Some(ParamValue::Int(6)));
        assert_eq!(stepped_value(0.0, -1, &int_kind), Some(ParamValue::Int(0)));

        assert_eq!(stepped_value(1e-4, 1, &float_desc(1e-9, 1.0, true)), None);
        assert_eq!(stepped_value(1.0, 1, &ParamKind::Bool), None);
    }

    #[test]
    fn violation_text_drops_the_id_prefix() {
        let out_of_range = ParamViolation::OutOfRange {
            id: "levels".to_string(),
            value: 20.0,
            min: 0.0,
            max: 10.0,
        };
        assert_eq!(
            violation_text(&out_of_range),
            "value 20 is outside the allowed range [0, 10]"
        );

        let unparsable = ParamViolation::Unparsable {
            id: "max-iterations".to_string(),
            kind: "int",
            text: "abc".to_string(),
        };
        assert_eq!(
            violation_text(&unparsable),
            "Not a number",
            "a field's own error stays plain, not the core's quoted parse message"
        );

        let unparsable_bool = ParamViolation::Unparsable {
            id: "auto-crop".to_string(),
            kind: "bool",
            text: "maybe".to_string(),
        };
        assert_eq!(violation_text(&unparsable_bool), "Not true or false");

        let unknown_key = ParamViolation::UnknownKey {
            key: "bogus".to_string(),
            declared: vec!["levels".to_string()],
        };
        assert_eq!(
            violation_text(&unknown_key),
            unknown_key.to_string(),
            "UnknownKey carries no id prefix to drop"
        );

        let auto = ParamViolation::Auto {
            id: "pyramid-depth".to_string(),
            source: Box::new(ParamViolation::OutOfRange {
                id: "pyramid-depth".to_string(),
                value: 99.0,
                min: 0.0,
                max: 6.0,
            }),
        };
        let text = violation_text(&auto);
        assert!(!text.contains("pyramid-depth"), "got: {text}");
        assert!(text.contains("auto value rejected"));
        assert!(text.contains("value 99 is outside"));
    }

    fn linear_row(id: &str) -> SettingsRow {
        SettingsRow {
            label: id.to_string(),
            ..SettingsRow::new(RowKind::LinearFloat, "fuse", id)
        }
    }

    #[test]
    fn assign_tops_is_the_cumulative_sum_content_height_agrees_with() {
        let mut rows = vec![
            SettingsRow::heading("fuse", "FUSE", HeadingState::Default),
            linear_row("levels"),
            linear_row("radius"),
        ];
        assign_tops(&mut rows);
        assert_eq!(rows[0].top, 0.0);
        assert_eq!(
            rows[1].top,
            SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP
        );
        assert_eq!(
            rows[2].top,
            SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP + row_height(RowKind::LinearFloat)
        );
        assert_eq!(
            rows[2].top + row_height(RowKind::LinearFloat) + PANEL_BOTTOM_BUFFER,
            content_height(&rows)
        );
    }

    #[test]
    fn content_height_reads_a_heading_row_without_row_height() {
        // `row_height` panics on `RowKind::SectionHeading` -- this must go
        // through the dedicated formula instead, for every `HeadingState`.
        let rows = vec![
            SettingsRow::heading("fuse", "FUSE", HeadingState::Explicit),
            linear_row("levels"),
        ];
        let expected = SECTION_HEADING_HEIGHT
            + SECTION_HEADING_BOTTOM_GAP
            + row_height(RowKind::LinearFloat)
            + PANEL_BOTTOM_BUFFER;
        assert_eq!(content_height(&rows), expected);
    }

    #[test]
    fn clamp_panel_scroll_matches_tab_scroll_shape() {
        assert_eq!(clamp_panel_scroll(-10.0, 762.0, 602.0), 0.0);
        assert_eq!(clamp_panel_scroll(1000.0, 762.0, 602.0), 160.0);
        assert_eq!(clamp_panel_scroll(50.0, 400.0, 602.0), 0.0);
    }

    #[test]
    fn sticky_heading_is_the_last_one_scrolled_past() {
        let rows = vec![
            SettingsRow::heading("fuse", "FUSE", HeadingState::Default),
            linear_row("levels"),
            SettingsRow::heading("align", "ALIGN", HeadingState::Default),
            linear_row("epsilon-px"),
        ];
        assert_eq!(sticky_heading(&rows, 0.0), Some(0));
        let fuse_heading_height = SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP;
        let first_row_height = row_height(RowKind::LinearFloat);
        let past_fuse_row = fuse_heading_height + first_row_height + 1.0;
        assert_eq!(sticky_heading(&rows, past_fuse_row), Some(2));
    }

    #[test]
    fn sticky_push_grows_as_the_next_heading_arrives_then_resets_on_swap() {
        let mut rows = vec![
            SettingsRow::heading("fuse", "FUSE", HeadingState::Default),
            linear_row("levels"),
            SettingsRow::heading("align", "ALIGN", HeadingState::Default),
            linear_row("epsilon-px"),
        ];
        assign_tops(&mut rows);
        let align_top = rows[2].top;
        let heading_row = SECTION_HEADING_HEIGHT + SECTION_HEADING_BOTTOM_GAP;

        // Far from the swap: no push yet.
        assert_eq!(sticky_push(&rows, Some(0), 0.0), 0.0);
        assert_eq!(
            sticky_push(&rows, Some(0), align_top - heading_row - 1.0),
            0.0
        );

        // Inside one heading row of the swap: pushed by exactly the
        // remaining distance, so the outgoing heading's own bottom edge
        // always lines up with the incoming heading's own top edge.
        assert_eq!(
            sticky_push(&rows, Some(0), align_top - heading_row + 10.0),
            10.0
        );

        // Exactly at the swap: fully pushed off, one heading row's worth.
        assert_eq!(sticky_push(&rows, Some(0), align_top), heading_row);

        // Past the swap, `sticky_heading` itself has already moved to
        // ALIGN, and with no third heading behind it there is nothing left
        // to push it.
        assert_eq!(sticky_heading(&rows, align_top), Some(2));
        assert_eq!(sticky_push(&rows, Some(2), align_top), 0.0);
    }

    #[test]
    fn row_at_finds_the_row_a_content_offset_falls_in() {
        let mut rows = vec![
            SettingsRow::heading("fuse", "FUSE", HeadingState::Default),
            linear_row("levels"),
            linear_row("radius"),
        ];
        assign_tops(&mut rows);

        assert_eq!(row_at(&rows, 0.0), Some(0), "the heading's own top edge");
        assert_eq!(
            row_at(&rows, rows[1].top),
            Some(1),
            "a row's own top edge belongs to that row"
        );
        assert_eq!(
            row_at(&rows, rows[2].top + row_height(RowKind::LinearFloat) - 1.0),
            Some(2),
            "just before the last row's bottom edge"
        );
        assert_eq!(
            row_at(&rows, content_height(&rows)),
            None,
            "past every row's bottom edge"
        );
        assert_eq!(row_at(&rows, -1.0), None, "above the content column");
    }
}
