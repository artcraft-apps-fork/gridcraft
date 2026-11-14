//! Rebase-friendly Vim/Evil-inspired modal keybinding adapter for `gridcraft` (`craft-modal`).
//!
//! Per-window modal state is stored inside `egui::Context` memory rather than adding fields to
//! `SheetApp`, keeping upstream rebase surface limited to single-line hooks in `grid/keyboard.rs`
//! and `app.rs`.

use std::sync::{Arc, Mutex};

use craft_modal::defaults::default_tables;
use craft_modal::egui::{filter_frame, paint_mode_badge};
use craft_modal::rc::{RcWarning, load_user_rc};
use craft_modal::{
    AlignPos, CommandCall, ModalAction, ModalEngine, Mode, Motion, Operator, RcEval, ScrollDir, ScrollUnit, Target, TextObj, VisualKind, ZoomOp,
};
use gridcraft_engine::core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_engine::model::{Cell, Sheet};
use serde_json::json;

use crate::SheetApp;
use crate::grid::Geo;

/// Spreadsheet-tailored default bindings layered beneath user `~/.craftvimrc` / `~/.gridcraftvimrc`.
const GRIDCRAFT_BUILTIN_RC: &str = r#"
nmap i <Action>(modal.insert_mode)
nmap a <Action>(modal.edit_cell_end)
nmap A <Action>(modal.edit_cell_end)
nmap I <Action>(modal.edit_cell_start)
nmap = <Action>(modal.start_formula)
nmap o <Action>(modal.open_row_below)
nmap O <Action>(modal.open_row_above)
nmap x <Action>(modal.delete_cell)
nmap X <Action>(modal.backspace_cell)
nmap s <Action>(modal.substitute_cell)
nmap S <Action>(modal.substitute_row)
nmap D <Action>(modal.delete_to_row_end)
nmap C <Action>(modal.change_to_row_end)
nmap gp <Action>(edit.pasteSpecial {"what": "values"})
nmap <leader>r :reload<CR>
"#;

#[derive(Clone)]
struct GridModalHandle(Arc<Mutex<GridModalState>>);

struct GridModalState {
    engine: ModalEngine,
    last_visual: Option<(VisualKind, RangeRef)>,
    last_search: Option<String>,
}

struct GridCtx<'a> {
    app: &'a mut SheetApp,
    ctx: &'a egui::Context,
    geo: &'a Geo,
    sh: &'a Sheet,
}

#[derive(Clone, Copy)]
struct MotionStep {
    motion: Motion,
    count: usize,
    explicit_count: bool,
    extend: bool,
}

#[derive(Clone, Copy)]
struct FindCharSpec {
    ch: char,
    forward: bool,
    till: bool,
    count: usize,
    extend: bool,
}

impl GridModalState {
    fn new() -> Self {
        let mut engine = ModalEngine::new("empty");
        let _ = apply_rc_layers(&mut engine);
        let start = engine.options.start_mode;
        engine.set_mode(start);
        Self { engine, last_visual: None, last_search: None }
    }
}

fn apply_rc_layers(engine: &mut ModalEngine) -> Vec<RcWarning> {
    engine.tables = default_tables();
    {
        let mut eval = RcEval::new("gridcraft", &mut engine.options, &mut engine.tables);
        eval.eval_str(GRIDCRAFT_BUILTIN_RC, "gridcraft-defaults", None);
    }
    load_user_rc("gridcraft", &mut engine.options, &mut engine.tables)
}

fn state_handle(ctx: &egui::Context) -> Arc<Mutex<GridModalState>> {
    let id = egui::Id::unique("gridcraft_craft_modal_state");
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with(id, || GridModalHandle(Arc::new(Mutex::new(GridModalState::new())))).0.clone())
}

/// Intercept worksheet grid keyboard events before native Excel grid handlers run.
pub fn intercept_grid_keys(app: &mut SheetApp, ctx: &egui::Context, geo: &Geo, sh: &Sheet) {
    let handle = state_handle(ctx);
    let Ok(mut state) = handle.lock() else { return };
    let bypass = app.keytips.prefix().is_some() || egui::Popup::is_any_open(ctx);
    let actions = filter_frame(ctx, &mut state.engine, bypass);
    if actions.is_empty() {
        return;
    }
    let mut g = GridCtx { app, ctx, geo, sh };
    for action in actions {
        dispatch_action(&mut g, &mut state, action);
    }
}

/// Render the modal status badge (`NORMAL` / `INSERT` / `VISUAL`) in the bottom-right viewport corner.
pub fn paint_overlay(app: &SheetApp, ctx: &egui::Context) {
    let handle = state_handle(ctx);
    let Ok(state) = handle.lock() else { return };
    let mut status = state.engine.status();
    if app.editor.is_some() || app.text_box_editor.is_some() {
        status.mode = Mode::Insert;
        if status.pending_keys.is_empty() {
            status.pending_keys = "CELL".into();
        }
    }
    paint_mode_badge(ctx, &status);
}

fn dispatch_action(g: &mut GridCtx<'_>, state: &mut GridModalState, action: ModalAction) {
    match action {
        ModalAction::EnterMode { mode, append } => handle_enter_mode(g, state, mode, append),
        ModalAction::SwapVisualAnchor => handle_swap_anchor(g),
        ModalAction::ReselectVisual => handle_reselect_visual(g, state),
        ModalAction::Motion { motion, count, explicit_count } => {
            let extend = state.engine.mode().is_visual();
            apply_motion(g, state, MotionStep { motion, count, explicit_count, extend });
        }
        ModalAction::SelectTextObject { obj, count, .. } => select_text_object(g, state, obj, count),
        ModalAction::Operator { op, target, .. } => handle_operator(g, state, op, target),
        ModalAction::Paste { count, .. } => handle_paste(g, count),
        ModalAction::Undo { count } => run_steps(g, "edit.undo", count),
        ModalAction::Redo { count } => run_steps(g, "edit.redo", count),
        ModalAction::Scroll { dir, unit, count } => handle_scroll(g, state, dir, unit, count),
        ModalAction::AlignView(pos) => handle_align_cursor(g, pos),
        ModalAction::Zoom { op, count } => handle_zoom(g, op, count),
        ModalAction::SearchStart { .. } => g.app.open_dialog("find", json!({})),
        ModalAction::SearchWord { forward } => search_active_cell(g, state, forward, 1),
        ModalAction::SearchStep { forward, count } => search_next(g, state, forward, count),
        ModalAction::DismissTransient => handle_enter_mode(g, state, Mode::Normal, false),
        ModalAction::StepPage { forward, count } => {
            handle_scroll(g, state, if forward { ScrollDir::Down } else { ScrollDir::Up }, ScrollUnit::FullPage, count);
        }
        ModalAction::StepTab { forward, count } => step_sheet_tab(g, forward, count),
        ModalAction::JumpHistory { .. } => {}
        ModalAction::AppCommand { cmd, count } => handle_command(g, state, &cmd, count),
        ModalAction::ReloadConfig => reload_rc(g.app, state),
    }
}

fn handle_paste(g: &mut GridCtx<'_>, count: usize) {
    for _ in 0..count.max(1) {
        g.app.paste_from_clipboard();
    }
    g.app.grid.ensure_visible = true;
}

fn run_steps(g: &mut GridCtx<'_>, cmd_id: &str, count: usize) {
    g.app.run_or_alert(cmd_id, json!({"steps": count.max(1)}));
    g.app.grid.ensure_visible = true;
}

fn step_sheet_tab(g: &mut GridCtx<'_>, forward: bool, count: usize) {
    let id = if forward { "sheet.next" } else { "sheet.previous" };
    for _ in 0..count.max(1) {
        g.app.run_or_alert(id, json!({}));
    }
}

fn handle_enter_mode(g: &mut GridCtx<'_>, state: &mut GridModalState, mode: Mode, append: bool) {
    match mode {
        Mode::Normal => {
            if g.app.selected_object.is_some() {
                g.app.selected_object = None;
                return;
            }
            if g.app.session.clipboard.is_some() || g.app.session.format_painter.is_some() {
                g.app.session.clipboard = None;
                g.app.session.format_painter = None;
            }
            collapse_to_active_cell(g.app);
        }
        Mode::Insert => {
            if append {
                g.app.begin_edit(None, false);
            }
        }
        Mode::Visual(kind) => enter_visual(g.app, state, kind),
    }
}

fn enter_visual(app: &mut SheetApp, state: &mut GridModalState, kind: VisualKind) {
    let Some(active) = active_cell(app) else { return };
    let range = match kind {
        VisualKind::Line => full_row_range(active.row, active.row),
        VisualKind::Char | VisualKind::Block => current_selection_range(app).unwrap_or_else(|| RangeRef::cell(active)),
    };
    set_selection_range(app, range, active, active);
    state.last_visual = Some((kind, range));
}

fn handle_swap_anchor(g: &mut GridCtx<'_>) {
    let _ = g.app.session.run("selection.nextCorner", json!({}));
    g.app.grid.ensure_visible = true;
}

fn handle_reselect_visual(g: &mut GridCtx<'_>, state: &mut GridModalState) {
    let Some((kind, range)) = state.last_visual else { return };
    state.engine.set_mode(Mode::Visual(kind));
    set_selection_range(g.app, range, range.start, range.start);
}

fn apply_motion(g: &mut GridCtx<'_>, state: &mut GridModalState, step: MotionStep) {
    let n = i64::try_from(step.count.max(1)).unwrap_or(1);
    let rtl_sign: i64 = if g.sh.right_to_left { -1 } else { 1 };
    let extend = step.extend;
    match step.motion {
        Motion::Left => move_delta(g.app, state, (0, -n * rtl_sign), false, extend),
        Motion::Right => move_delta(g.app, state, (0, n * rtl_sign), false, extend),
        Motion::Up => move_delta(g.app, state, (-n, 0), false, extend),
        Motion::Down => move_delta(g.app, state, (n, 0), false, extend),
        Motion::WordForward | Motion::BigWordForward | Motion::WordEnd | Motion::BigWordEnd => {
            repeat_jump(g.app, state, (0, rtl_sign), step.count, extend);
        }
        Motion::WordBack | Motion::BigWordBack => {
            repeat_jump(g.app, state, (0, -rtl_sign), step.count, extend);
        }
        Motion::ParagraphPrev => repeat_jump(g.app, state, (-1, 0), step.count, extend),
        Motion::ParagraphNext => repeat_jump(g.app, state, (1, 0), step.count, extend),
        Motion::LineStart => jump_to_col(g, state, 0, extend),
        Motion::FirstNonBlank => jump_to_col(g, state, first_non_blank_col(g.sh, active_row(g.app)), extend),
        Motion::LineEnd => jump_to_col(g, state, last_non_blank_col(g.sh, active_row(g.app)), extend),
        Motion::DocStart => jump_doc_start(g, state, step.count, step.explicit_count, extend),
        Motion::DocEnd => jump_doc_end(g, state, step.count, step.explicit_count, extend),
        Motion::ScreenTop => jump_to_screen_row(g, state, ScreenBand::Top, extend),
        Motion::ScreenMiddle => jump_to_screen_row(g, state, ScreenBand::Middle, extend),
        Motion::ScreenBottom => jump_to_screen_row(g, state, ScreenBand::Bottom, extend),
        Motion::FindChar { ch, forward, till } => {
            move_find_char(g, state, FindCharSpec { ch, forward, till, count: step.count, extend });
        }
        Motion::RepeatFindChar { .. } => {}
    }
}

fn repeat_jump(app: &mut SheetApp, state: &mut GridModalState, delta: (i64, i64), count: usize, extend: bool) {
    for _ in 0..count.max(1) {
        move_delta(app, state, delta, true, extend);
    }
}

fn move_delta(app: &mut SheetApp, state: &mut GridModalState, delta: (i64, i64), jump: bool, extend: bool) {
    let (dr, dc) = delta;
    if matches!(state.engine.mode(), Mode::Visual(VisualKind::Line)) && extend {
        move_visual_line(app, state, dr, jump);
        return;
    }
    let from = app.session.active().map(|d| if extend { d.selection.moving_end() } else { d.selection.active });
    let _ = app.session.run("selection.move", json!({"dr": dr, "dc": dc, "extend": extend, "jump": jump}));
    app.grid.ensure_visible = true;
    app.grid.reveal_moving_end = extend;
    app.grid.reveal_from = from;
    record_visual_if_active(app, state);
}

fn move_visual_line(app: &mut SheetApp, state: &mut GridModalState, dr: i64, jump: bool) {
    let Some((anchor, moving)) = app.session.active().map(|d| (d.selection.anchor, d.selection.moving_end())) else { return };
    if dr == 0 && !jump {
        return;
    }
    let max_row = i64::from(MAX_ROWS.saturating_sub(1));
    let target_row = (i64::from(moving.row) + dr).clamp(0, max_row) as u32;
    let range = full_row_range(anchor.row, target_row);
    let anchor_cell = CellRef::new(anchor.row, 0);
    set_selection_range(app, range, anchor_cell, anchor_cell);
    state.last_visual = Some((VisualKind::Line, range));
}

#[derive(Clone, Copy)]
enum ScreenBand {
    Top,
    Middle,
    Bottom,
}

fn jump_to_screen_row(g: &mut GridCtx<'_>, state: &mut GridModalState, band: ScreenBand, extend: bool) {
    let top_y = g.geo.cells.top() + g.geo.frozen_h;
    let bot_y = g.geo.cells.bottom().max(top_y);
    let y = match band {
        ScreenBand::Top => top_y + 2.0,
        ScreenBand::Middle => (top_y + bot_y) * 0.5,
        ScreenBand::Bottom => (bot_y - 4.0).max(top_y),
    };
    let row = g.geo.row_at(g.sh, y);
    let col = active_col(g.app);
    move_to_cell(g.app, state, CellRef::new(row, col), extend);
}

fn jump_to_col(g: &mut GridCtx<'_>, state: &mut GridModalState, col: u32, extend: bool) {
    let row = active_row(g.app);
    move_to_cell(g.app, state, CellRef::new(row, col), extend);
}

fn jump_doc_start(g: &mut GridCtx<'_>, state: &mut GridModalState, count: usize, explicit_count: bool, extend: bool) {
    let target = if explicit_count {
        let row = u32::try_from(count.saturating_sub(1)).unwrap_or(MAX_ROWS - 1).min(MAX_ROWS - 1);
        CellRef::new(row, active_col(g.app))
    } else {
        let r = g.sh.freeze.map(|f| f.0).unwrap_or(0);
        let c = g.sh.freeze.map(|f| f.1).unwrap_or(0);
        CellRef::new(r, c)
    };
    move_to_cell(g.app, state, target, extend);
}

fn jump_doc_end(g: &mut GridCtx<'_>, state: &mut GridModalState, count: usize, explicit_count: bool, extend: bool) {
    let target = if explicit_count {
        let row = u32::try_from(count.saturating_sub(1)).unwrap_or(MAX_ROWS - 1).min(MAX_ROWS - 1);
        CellRef::new(row, active_col(g.app))
    } else {
        g.sh.used_range().map(|u| u.end).unwrap_or_default()
    };
    move_to_cell(g.app, state, target, extend);
}

fn move_to_cell(app: &mut SheetApp, state: &mut GridModalState, to: CellRef, extend: bool) {
    if !extend {
        let _ = app.session.run("selection.set", json!({"cell": to.a1()}));
        app.grid.ensure_visible = true;
        return;
    }
    let Some((anchor, active)) = app.session.active().map(|d| (d.selection.anchor, d.selection.active)) else { return };
    if matches!(state.engine.mode(), Mode::Visual(VisualKind::Line)) {
        let range = full_row_range(anchor.row, to.row);
        let anchor_cell = CellRef::new(anchor.row, 0);
        set_selection_range(app, range, anchor_cell, anchor_cell);
        state.last_visual = Some((VisualKind::Line, range));
        return;
    }
    let range = RangeRef::new(anchor, to);
    set_selection_range(app, range, active, anchor);
    record_visual_if_active(app, state);
}

fn move_find_char(g: &mut GridCtx<'_>, state: &mut GridModalState, spec: FindCharSpec) {
    let Some(active) = active_cell(g.app) else { return };
    let ignore_case = state.engine.options.ignore_case && (!state.engine.options.smart_case || !spec.ch.is_uppercase());
    let needle = if ignore_case { spec.ch.to_ascii_lowercase() } else { spec.ch };
    let found = {
        let Some(doc) = g.app.session.active() else { return };
        scan_row_for_char(&doc.wb, g.sh, active, needle, (ignore_case, spec))
    };
    if let Some(col) = found {
        move_to_cell(g.app, state, CellRef::new(active.row, col), spec.extend);
    }
}

fn scan_row_for_char(wb: &gridcraft_engine::model::Workbook, sh: &Sheet, active: CellRef, needle: char, opts: (bool, FindCharSpec)) -> Option<u32> {
    let (ignore_case, spec) = opts;
    let end_col = sh.used_range().map_or(active.col, |u| u.end.col.max(active.col));
    let mut hits_left = spec.count.max(1);
    if spec.forward {
        for col in (active.col.saturating_add(1))..=end_col {
            if cell_matches_char(wb, sh, CellRef::new(active.row, col), needle, ignore_case) {
                hits_left -= 1;
                if hits_left == 0 {
                    return Some(if spec.till { col.saturating_sub(1) } else { col });
                }
            }
        }
    } else if active.col > 0 {
        for col in (0..active.col).rev() {
            if cell_matches_char(wb, sh, CellRef::new(active.row, col), needle, ignore_case) {
                hits_left -= 1;
                if hits_left == 0 {
                    return Some(if spec.till { col.saturating_add(1) } else { col });
                }
            }
        }
    }
    None
}

fn cell_matches_char(wb: &gridcraft_engine::model::Workbook, sh: &Sheet, cell: CellRef, needle: char, ignore_case: bool) -> bool {
    let text = gridcraft_engine::display::edit_text(wb, sh, cell);
    if text.is_empty() {
        return false;
    }
    text.chars().any(|c| {
        let cmp = if ignore_case { c.to_ascii_lowercase() } else { c };
        cmp == needle
    })
}

fn select_text_object(g: &mut GridCtx<'_>, state: &mut GridModalState, obj: TextObj, count: usize) {
    match obj {
        TextObj::Word | TextObj::BigWord => {
            if let Some(active) = active_cell(g.app) {
                let range = g.sh.merge_at(active).unwrap_or_else(|| RangeRef::cell(active));
                set_selection_range(g.app, range, active, active);
            }
        }
        TextObj::Line => {
            let row = active_row(g.app);
            let span = u32::try_from(count.saturating_sub(1)).unwrap_or(0);
            let end_row = row.saturating_add(span).min(MAX_ROWS - 1);
            let range = full_row_range(row, end_row);
            let anchor = CellRef::new(row, 0);
            set_selection_range(g.app, range, anchor, anchor);
        }
        TextObj::Paragraph => {
            let _ = g.app.session.run("selection.currentRegion", json!({}));
            g.app.grid.ensure_visible = true;
        }
    }
    record_visual_if_active(g.app, state);
}

fn handle_operator(g: &mut GridCtx<'_>, state: &mut GridModalState, op: Operator, target: Target) {
    if matches!(op, Operator::Delete)
        && let Target::Linewise { count } = target
    {
        delete_sheet_rows(g, count);
        return;
    }
    if matches!(op, Operator::Change)
        && let Target::Linewise { count } = target
    {
        change_sheet_rows(g, count);
        return;
    }

    select_operator_target(g, state, target);
    apply_operator_on_selection(g, op);
}

fn apply_operator_on_selection(g: &mut GridCtx<'_>, op: Operator) {
    match op {
        Operator::Yank => {
            g.app.copy_to_clipboard(g.ctx, "edit.copy");
            collapse_to_active_cell(g.app);
        }
        Operator::Delete => {
            g.app.copy_to_clipboard(g.ctx, "edit.copy");
            g.app.run_or_alert("edit.clearContents", json!({}));
            collapse_to_active_cell(g.app);
        }
        Operator::Change => {
            g.app.copy_to_clipboard(g.ctx, "edit.copy");
            g.app.run_or_alert("edit.clearContents", json!({}));
            collapse_to_active_cell(g.app);
            g.app.begin_edit(Some(String::new()), false);
        }
        Operator::Highlight => {
            let color = g.app.grid.last_fill.clone();
            g.app.run_or_alert("home.fillColor", json!({"color": color}));
        }
        Operator::Underline => g.app.run_or_alert("home.underline", json!({})),
        Operator::Squiggly => g.app.run_or_alert("home.doubleUnderline", json!({})),
        Operator::StrikeOut => g.app.run_or_alert("home.strikethrough", json!({})),
        Operator::Note => g.app.open_dialog("comment", json!({"threaded": true})),
    }
}

fn select_operator_target(g: &mut GridCtx<'_>, state: &mut GridModalState, target: Target) {
    match target {
        Target::Selection => {
            if let Some(range) = current_selection_range(g.app) {
                let kind = match state.engine.mode() {
                    Mode::Visual(k) => k,
                    _ => VisualKind::Char,
                };
                state.last_visual = Some((kind, range));
            }
        }
        Target::Linewise { count } => {
            let row = active_row(g.app);
            let span = u32::try_from(count.saturating_sub(1)).unwrap_or(0);
            let end_row = row.saturating_add(span).min(MAX_ROWS - 1);
            let range = full_row_range(row, end_row);
            let active = active_cell(g.app).unwrap_or_default();
            set_selection_range(g.app, range, active, CellRef::new(row, 0));
        }
        Target::Motion { motion, count, explicit_count } => {
            if let Some(start) = active_cell(g.app) {
                set_selection_range(g.app, RangeRef::cell(start), start, start);
                apply_motion(g, state, MotionStep { motion, count, explicit_count, extend: true });
            }
        }
        Target::TextObject { obj, count, .. } => select_text_object(g, state, obj, count),
    }
}

fn delete_sheet_rows(g: &mut GridCtx<'_>, count: usize) {
    let row = active_row(g.app);
    let span = u32::try_from(count.saturating_sub(1)).unwrap_or(0);
    let end_row = row.saturating_add(span).min(MAX_ROWS - 1);
    let range = full_row_range(row, end_row);
    let anchor = CellRef::new(row, 0);
    set_selection_range(g.app, range, anchor, anchor);
    g.app.copy_to_clipboard(g.ctx, "edit.copy");
    let rows_spec = format!("{}:{}", row + 1, end_row + 1);
    g.app.run_or_alert("home.deleteRows", json!({"rows": rows_spec}));
    collapse_to_active_cell(g.app);
}

fn change_sheet_rows(g: &mut GridCtx<'_>, count: usize) {
    let row = active_row(g.app);
    let span = u32::try_from(count.saturating_sub(1)).unwrap_or(0);
    let end_row = row.saturating_add(span).min(MAX_ROWS - 1);
    let active = active_cell(g.app).unwrap_or_default();
    let range = full_row_range(row, end_row);
    set_selection_range(g.app, range, active, CellRef::new(row, 0));
    g.app.copy_to_clipboard(g.ctx, "edit.copy");
    g.app.run_or_alert("edit.clearContents", json!({}));
    collapse_to_active_cell(g.app);
    g.app.begin_edit(Some(String::new()), false);
}

fn handle_scroll(g: &mut GridCtx<'_>, state: &mut GridModalState, dir: ScrollDir, unit: ScrollUnit, count: usize) {
    let page_rows = ((g.geo.cells.height() / g.geo.z) / g.sh.default_row_height.max(1.0)).floor().max(2.0) as i64;
    let page_cols = ((g.geo.cells.width() / g.geo.z) / g.sh.default_col_width.max(1.0)).floor().max(2.0) as i64;
    let vertical = matches!(dir, ScrollDir::Up | ScrollDir::Down);
    let step = match unit {
        ScrollUnit::Line => 1,
        ScrollUnit::HalfPage => (if vertical { page_rows } else { page_cols } / 2).max(1),
        ScrollUnit::FullPage => {
            if vertical {
                page_rows
            } else {
                page_cols
            }
        }
    };
    let factor = i64::try_from(count.max(1)).unwrap_or(1);
    let total = step.saturating_mul(factor);
    let extend = state.engine.mode().is_visual();
    let delta = match dir {
        ScrollDir::Down => (total, 0),
        ScrollDir::Up => (-total, 0),
        ScrollDir::Right => (0, total),
        ScrollDir::Left => (0, -total),
    };
    move_delta(g.app, state, delta, false, extend);
}

fn handle_align_cursor(g: &mut GridCtx<'_>, pos: AlignPos) {
    let _ = pos;
    g.app.grid.ensure_visible = true;
}

fn handle_zoom(g: &mut GridCtx<'_>, op: ZoomOp, count: usize) {
    let steps = i32::try_from(count.max(1)).unwrap_or(1);
    match op {
        ZoomOp::In => {
            let pct = i32::from(g.sh.zoom).saturating_add(10 * steps).clamp(10, 400);
            g.app.run_or_alert("view.zoom", json!({"percent": pct}));
        }
        ZoomOp::Out => {
            let pct = i32::from(g.sh.zoom).saturating_sub(10 * steps).clamp(10, 400);
            g.app.run_or_alert("view.zoom", json!({"percent": pct}));
        }
        ZoomOp::Reset | ZoomOp::FitPage => g.app.run_or_alert("view.zoom100", json!({})),
        ZoomOp::FitWidth => g.app.run_or_alert("view.zoomToSelection", json!({})),
    }
}

fn search_active_cell(g: &mut GridCtx<'_>, state: &mut GridModalState, forward: bool, count: usize) {
    let Some(active) = active_cell(g.app) else { return };
    let query = g.app.session.active().and_then(|d| {
        g.sh.value_ref(active).map(|v| gridcraft_engine::display::format(v, "General", &d.wb).text).filter(|s| !s.is_empty()).or_else(|| {
            let raw = gridcraft_engine::display::edit_text(&d.wb, g.sh, active);
            (!raw.is_empty()).then_some(raw)
        })
    });
    let Some(query) = query else { return };
    state.last_search = Some(query);
    search_next(g, state, forward, count);
}

fn search_next(g: &mut GridCtx<'_>, state: &mut GridModalState, forward: bool, count: usize) {
    let Some(query) = state.last_search.clone() else {
        g.app.open_dialog("find", json!({}));
        return;
    };
    let match_case = !state.engine.options.ignore_case || (state.engine.options.smart_case && query.chars().any(char::is_uppercase));
    for _ in 0..count.max(1) {
        if forward {
            let _ = g.app.run("edit.find", json!({"what": query, "matchCase": match_case, "within": "sheet"}));
            g.app.grid.ensure_visible = true;
        } else if !step_search_backward(g.app, &query, match_case) {
            break;
        }
    }
}

fn step_search_backward(app: &mut SheetApp, query: &str, match_case: bool) -> bool {
    let Some(active) = active_cell(app) else { return false };
    let Ok(res) = app.run("edit.find", json!({"what": query, "matchCase": match_case, "within": "sheet", "all": true})) else {
        return false;
    };
    let Some(items) = res.get("results").and_then(serde_json::Value::as_array) else { return false };
    let cells: Vec<CellRef> = items.iter().filter_map(|it| it.get("cell").and_then(serde_json::Value::as_str)).filter_map(CellRef::parse).collect();
    if cells.is_empty() {
        return false;
    }
    let prev = cells.iter().copied().rev().find(|c| (c.row, c.col) < (active.row, active.col)).or_else(|| cells.last().copied());
    if let Some(target) = prev {
        let _ = app.session.run("selection.set", json!({"cell": target.a1()}));
        app.grid.ensure_visible = true;
        return true;
    }
    false
}

fn handle_command(g: &mut GridCtx<'_>, state: &mut GridModalState, cmd: &CommandCall, count: usize) {
    match cmd.id.as_str() {
        "modal.insert_mode" => state.engine.set_mode(Mode::Insert),
        "modal.edit_cell_end" => g.app.begin_edit(None, false),
        "modal.edit_cell_start" => edit_cell_at_start(g.app),
        "modal.start_formula" => start_formula_edit(g.app),
        "modal.open_row_below" => open_row(g, true),
        "modal.open_row_above" => open_row(g, false),
        "modal.delete_cell" => delete_cells_inline(g, count, true),
        "modal.backspace_cell" => delete_cells_inline(g, count, false),
        "modal.substitute_cell" => {
            delete_cells_inline(g, count, true);
            g.app.begin_edit(Some(String::new()), false);
        }
        "modal.substitute_row" => change_sheet_rows(g, count),
        "modal.delete_to_row_end" => clear_to_row_end(g, false),
        "modal.change_to_row_end" => clear_to_row_end(g, true),
        "modal.reload_rc" => reload_rc(g.app, state),
        "edit.copy" | "edit.cut" => g.app.copy_to_clipboard(g.ctx, cmd.id.as_str()),
        "edit.paste" => g.app.paste_from_clipboard(),
        other => run_generic_command(g.app, other, &cmd.params, count),
    }
}

fn edit_cell_at_start(app: &mut SheetApp) {
    app.begin_edit(None, false);
    if let Some(ed) = app.editor.as_mut() {
        ed.caret = 0;
        ed.anchor = 0;
        ed.caret_to_end = false;
        ed.sync_caret = true;
    }
}

fn start_formula_edit(app: &mut SheetApp) {
    app.begin_edit(Some("=".into()), false);
    if let Some(ed) = app.editor.as_mut() {
        ed.caret = 1;
        ed.anchor = 1;
        ed.caret_to_end = true;
        ed.sync_caret = true;
    }
}

fn run_generic_command(app: &mut SheetApp, id: &str, params: &serde_json::Value, count: usize) {
    for _ in 0..count.max(1) {
        app.run_or_alert(id, params.clone());
    }
    app.grid.ensure_visible = true;
}

fn open_row(g: &mut GridCtx<'_>, below: bool) {
    let active = active_cell(g.app).unwrap_or_default();
    let insert_row_1based = if below { active.row.saturating_add(2) } else { active.row.saturating_add(1) };
    let rows_spec = format!("{insert_row_1based}:{insert_row_1based}");
    g.app.run_or_alert("home.insertRows", json!({"rows": rows_spec}));
    let target_row = if below { active.row.saturating_add(1).min(MAX_ROWS - 1) } else { active.row };
    let _ = g.app.session.run("selection.set", json!({"cell": CellRef::new(target_row, active.col).a1()}));
    g.app.grid.ensure_visible = true;
    g.app.begin_edit(Some(String::new()), false);
}

fn delete_cells_inline(g: &mut GridCtx<'_>, count: usize, forward: bool) {
    let Some(active) = active_cell(g.app) else { return };
    let range = if forward {
        let span = u32::try_from(count.saturating_sub(1)).unwrap_or(0);
        let end_col = active.col.saturating_add(span).min(MAX_COLS - 1);
        RangeRef::new(active, CellRef::new(active.row, end_col))
    } else {
        if active.col == 0 {
            return;
        }
        let back = u32::try_from(count.max(1)).unwrap_or(1);
        let start_col = active.col.saturating_sub(back);
        RangeRef::new(CellRef::new(active.row, start_col), CellRef::new(active.row, active.col - 1))
    };
    set_selection_range(g.app, range, range.start, range.start);
    g.app.copy_to_clipboard(g.ctx, "edit.copy");
    g.app.run_or_alert("edit.clearContents", json!({}));
    collapse_to_active_cell(g.app);
}

fn clear_to_row_end(g: &mut GridCtx<'_>, enter_edit: bool) {
    let Some(active) = active_cell(g.app) else { return };
    let end_col = last_non_blank_col(g.sh, active.row).max(active.col);
    let range = RangeRef::new(active, CellRef::new(active.row, end_col));
    set_selection_range(g.app, range, active, active);
    g.app.copy_to_clipboard(g.ctx, "edit.copy");
    g.app.run_or_alert("edit.clearContents", json!({}));
    collapse_to_active_cell(g.app);
    if enter_edit {
        g.app.begin_edit(Some(String::new()), false);
    }
}

fn reload_rc(app: &mut SheetApp, state: &mut GridModalState) {
    let warnings = apply_rc_layers(&mut state.engine);
    let msg = if warnings.is_empty() {
        "Reloaded modal configuration".to_string()
    } else {
        format!("Reloaded modal config ({} warning(s))", warnings.len())
    };
    app.toast = Some((msg, crate::now_ms()));
}

fn active_cell(app: &SheetApp) -> Option<CellRef> {
    app.session.active().map(|d| d.selection.active)
}

fn active_row(app: &SheetApp) -> u32 {
    active_cell(app).map_or(0, |c| c.row)
}

fn active_col(app: &SheetApp) -> u32 {
    active_cell(app).map_or(0, |c| c.col)
}

fn current_selection_range(app: &SheetApp) -> Option<RangeRef> {
    app.session.active().map(|d| d.selection.current())
}

fn full_row_range(r1: u32, r2: u32) -> RangeRef {
    let (lo, hi) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
    RangeRef::new(CellRef::new(lo, 0), CellRef::new(hi, MAX_COLS - 1))
}

fn set_selection_range(app: &mut SheetApp, range: RangeRef, active: CellRef, anchor: CellRef) {
    let _ = app.session.run("selection.set", json!({"range": range.a1(), "active": active.a1()}));
    if let Some(d) = app.session.active_mut() {
        d.selection.anchor = anchor;
    }
    app.grid.ensure_visible = true;
}

fn collapse_to_active_cell(app: &mut SheetApp) {
    let Some(active) = active_cell(app) else { return };
    if app.session.active().is_some_and(|d| !d.selection.is_single_cell()) {
        let _ = app.session.run("selection.set", json!({"cell": active.a1()}));
    }
}

fn record_visual_if_active(app: &SheetApp, state: &mut GridModalState) {
    if let Mode::Visual(kind) = state.engine.mode()
        && let Some(range) = current_selection_range(app)
    {
        state.last_visual = Some((kind, range));
    }
}

fn cell_has_content(cell: &Cell) -> bool {
    !cell.value.is_empty() || cell.formula.is_some() || cell.py.is_some()
}

fn first_non_blank_col(sh: &Sheet, row: u32) -> u32 {
    let max_col = sh.used_range().map_or(0, |u| u.end.col);
    for col in 0..=max_col {
        if sh.cell(CellRef::new(row, col)).is_some_and(cell_has_content) {
            return col;
        }
    }
    0
}

fn last_non_blank_col(sh: &Sheet, row: u32) -> u32 {
    let max_col = sh.used_range().map_or(0, |u| u.end.col);
    for col in (0..=max_col).rev() {
        if sh.cell(CellRef::new(row, col)).is_some_and(cell_has_content) {
            return col;
        }
    }
    0
}
