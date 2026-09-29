//! ProcessBar: footer panel showing running/finished file operations.
//!
//! Exact port of `src/internal/ui/processbar` from superfile v1.6.0.
//! Deviations: (1) gradient lerp is RGB instead of CIELAB (visual only);
//! (2) no channel/goroutine — the app calls methods directly and bridges
//!     its WorkerMsg stream; (3) process ids are u64 assigned by the app.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use ratatui::style::Color;

use crate::config::Palette;
use crate::icons::Ui;
use crate::render::{BorderSet, RBuf, St};
use crate::util::truncate_end;

/// The kind of file operation a process is performing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Copy,
    Cut,
    Delete,
    Compress,
    Extract,
    Create,
}

impl Operation {
    /// Present participle used while the operation is running (Go GetVerb).
    pub fn verb(&self) -> &'static str {
        match self {
            Operation::Copy => "Copying",
            Operation::Cut => "Moving",
            Operation::Delete => "Deleting",
            Operation::Compress => "Compressing",
            Operation::Extract => "Extracting",
            Operation::Create => "Creating",
        }
    }

    /// Past participle used once the operation completes (Go GetPastVerb).
    pub fn past_verb(&self) -> &'static str {
        match self {
            Operation::Copy => "Copied",
            Operation::Cut => "Moved",
            Operation::Delete => "Deleted",
            Operation::Compress => "Compressed",
            Operation::Extract => "Extracted",
            Operation::Create => "Created",
        }
    }

    /// Icon glyph for this operation (Go GetIcon).
    pub fn icon(&self, ui: Ui) -> &'static str {
        match self {
            Operation::Copy => ui.copy,
            Operation::Cut => ui.cut,
            Operation::Delete => ui.delete,
            Operation::Compress => ui.compress_file,
            Operation::Extract => ui.extract_file,
            Operation::Create => ui.in_operation,
        }
    }
}

/// Current/terminal state of a process (Go ProcessState).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    InOperation,
    Successful,
    Cancelled,
    Failed,
}

impl State {
    /// Status icon glyph (Go ProcessState.Icon).
    pub fn icon(&self, ui: Ui) -> &'static str {
        match self {
            State::Failed => ui.warn,
            State::Successful => ui.done,
            State::InOperation => ui.in_operation,
            State::Cancelled => ui.error,
        }
    }

    /// (fg, footer bg) style for the status icon (Go ProcessState style consts).
    pub fn icon_style(&self, pal: &Palette) -> (Color, Color) {
        let fg = match self {
            State::Failed => pal.error,
            State::Successful => pal.correct,
            State::InOperation => pal.hint,
            State::Cancelled => pal.cancel,
        };
        (fg, pal.footer_bg)
    }
}

/// A single file operation tracked by the process bar (Go Process).
#[derive(Debug, Clone)]
pub struct Process {
    id: u64,
    operation: Operation,
    state: State,
    current_file: String,
    error_msg: String,
    total: u64,
    done: u64,
    done_time: Option<DateTime<Utc>>,
}

impl Process {
    /// Display name without the leading operation icon (Go displayNameWithoutIcon).
    fn display_name_without_icon(&self) -> String {
        match self.state {
            State::Cancelled => {
                format!("{} cancelled : {}", self.operation.verb(), self.error_msg)
            }
            State::Failed => format!("{} failed : {}", self.operation.verb(), self.error_msg),
            State::InOperation => format!("{} {}", self.operation.verb(), self.current_file),
            _ => {
                if self.total > 1 {
                    format!("{} {} files", self.operation.past_verb(), self.total)
                } else {
                    format!("{} {}", self.operation.past_verb(), self.current_file)
                }
            }
        }
    }
}

/// Footer panel model + renderer for running/finished file operations (Go Model).
pub struct ProcessBar {
    render_index: usize,
    cursor: usize,
    width: usize,
    height: usize,
    processes: Vec<Process>,
}

impl ProcessBar {
    /// New empty bar at the minimum 2x2 dimensions (Go New).
    pub fn new() -> Self {
        ProcessBar {
            render_index: 0,
            cursor: 0,
            width: 2,
            height: 2,
            processes: Vec::new(),
        }
    }

    /// Set panel dimensions, clamping to the 2x2 minimum (Go SetDimensions).
    pub fn set_dimensions(&mut self, w: usize, h: usize) {
        let mut w = w;
        let mut h = h;
        if w < 2 {
            eprintln!("processbar: width {w} is below the minimum of 2; using 2");
            w = 2;
        }
        if h < 2 {
            eprintln!("processbar: height {h} is below the minimum of 2; using 2");
            h = 2;
        }
        self.width = w;
        self.height = h;
    }

    /// Register a new process; errors if the id is already present (Go AddProcess).
    pub fn add(&mut self, id: u64, op: Operation, name: &str, total: u64) -> Result<(), String> {
        if self.processes.iter().any(|p| p.id == id) {
            return Err(format!("process already exists with id : {id}"));
        }
        self.processes.push(Process {
            id,
            operation: op,
            state: State::InOperation,
            current_file: name.to_string(),
            error_msg: String::new(),
            total,
            done: 0,
            done_time: None,
        });
        Ok(())
    }

    /// Update a running process's current file and progress; ignores unknown ids.
    pub fn update_progress(&mut self, id: u64, file: &str, done: u64, total: u64) {
        if let Some(p) = self.processes.iter_mut().find(|p| p.id == id) {
            p.current_file = file.to_string();
            p.done = done;
            p.total = total;
        }
    }

    /// Mark a process finished in the given state; ignores unknown ids.
    pub fn finish(&mut self, id: u64, state: State, err: Option<String>) {
        if let Some(p) = self.processes.iter_mut().find(|p| p.id == id) {
            p.state = state;
            p.error_msg = err.unwrap_or_default();
            p.done_time = Some(Utc::now());
        }
    }

    /// True while any process is still in operation (Go HasRunningProcesses).
    pub fn has_running(&self) -> bool {
        self.processes
            .iter()
            .any(|p| p.state == State::InOperation && p.done != p.total)
    }

    /// Move the cursor up, wrapping to the last process (Go ListUp).
    pub fn list_up(&mut self) {
        let cnt_p = self.processes.len();
        if cnt_p == 0 {
            return;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            if self.cursor < self.render_index {
                self.render_index -= 1;
            }
        } else {
            self.cursor = cnt_p - 1;
            self.render_index = cnt_p.saturating_sub(self.cnt_renderable());
        }
    }

    /// Move the cursor down, wrapping to the first process (Go ListDown).
    pub fn list_down(&mut self) {
        let cnt_p = self.processes.len();
        if cnt_p == 0 {
            return;
        }
        if self.cursor < cnt_p - 1 {
            self.cursor += 1;
            let cnt = self.cnt_renderable() as i64;
            if self.cursor as i64 > self.render_index as i64 + cnt - 1 {
                self.render_index += 1;
            }
        } else {
            self.render_index = 0;
            self.cursor = 0;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Number of processes that fit the current height (Go cntRenderableProcess:
    /// (height - borderSize + 1) / linesPerProcess with height-2 and lines 3).
    fn cnt_renderable(&self) -> usize {
        if self.height <= 1 {
            return 0;
        }
        (self.height - 1) / 3
    }

    /// Processes in display order (Go getSortedProcesses). Not-done first (lower
    /// completion first — Cancelled sits in this group), then done newest-first.
    fn sorted(&self) -> Vec<&Process> {
        let mut v: Vec<&Process> = self.processes.iter().collect();
        v.sort_by(|a, b| {
            let done_a = matches!(a.state, State::Successful | State::Failed);
            let done_b = matches!(b.state, State::Successful | State::Failed);
            if done_a != done_b {
                // Not-done processes come first (Go: return !doneI).
                return if done_a { Ordering::Greater } else { Ordering::Less };
            }
            if !done_a {
                // Lower completion first; NaN (Total == 0) compares equal.
                let ca = a.done as f64 / a.total as f64;
                let cb = b.done as f64 / b.total as f64;
                return ca.partial_cmp(&cb).unwrap_or(Ordering::Equal);
            }
            // Done: newest completion time first.
            let ta = a.done_time.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            let tb = b.done_time.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            tb.cmp(&ta)
        });
        v
    }

    /// Render the panel into `buf` at (x, y) with size w x h (Go Render).
    pub fn draw(
        &self,
        buf: &mut RBuf,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        focused: bool,
        pal: &Palette,
        ui: Ui,
    ) {
        // House draw pattern (matches every other component).
        let border_fg = if focused {
            pal.footer_border_active
        } else {
            pal.footer_border
        };
        buf.fill(x, y, w, h, St::fg_bg(pal.footer_bg, pal.footer_bg));
        if w < 2 || h < 2 {
            return;
        }
        buf.border_title(
            x, y, w, h, BorderSet::plain(), border_fg, pal.footer_bg, "Processes", border_fg,
        );

        let view_w = w.saturating_sub(2);
        let view_h = h.saturating_sub(2);
        let footer_st = St::fg_bg(pal.footer_fg, pal.footer_bg);

        // Cursor window validity (Go isValid).
        let cnt = (self.height as i64 - 2 + 1) / 3;
        let valid = self.render_index as i64 <= self.cursor as i64
            && self.cursor as i64 <= self.render_index as i64 + cnt - 1;
        if !valid {
            buf.put_line(x + 1, y + 1, view_w, "Invalid state", footer_st);
            return;
        }

        // Empty state: blank line then the "none running" hint (Go ProcessBarNoneText).
        if self.processes.is_empty() {
            buf.put_line(x + 1, y + 1, view_w, "", footer_st);
            let none = format!(" {} No processes running", ui.error);
            buf.put_line(x + 1, y + 2, view_w, &none, footer_st);
            return;
        }

        // Border info: 1-based cursor position / total, right-aligned in bottom border.
        let info = format!("{}/{}", self.cursor + 1, self.processes.len());
        buf.border_info(
            x, y, w, h, BorderSet::plain(), border_fg, pal.footer_bg, &[info.as_str()],
        );

        let sorted = self.sorted();
        let mut rendered_h = 0usize;
        for (abs, p) in sorted.iter().enumerate().skip(self.render_index) {
            if view_h < rendered_h + 2 {
                break;
            }
            rendered_h += 3;
            let row = y + 1 + (rendered_h - 3);

            let cursor_str = if abs == self.cursor { "┃ " } else { "  " };
            let cursor_st = St::fg_bg(pal.cursor, pal.footer_bg);

            // Line 1: cursor, truncated display name, space, state icon.
            let full = format!(
                "{} {}",
                p.operation.icon(ui),
                p.display_name_without_icon()
            );
            let truncated = truncate_end(&full, view_w.saturating_sub(7), "...");
            let mut cx = x + 1;
            cx = buf.put_str(cx, row, cursor_str, cursor_st);
            cx = buf.put_str(cx, row, &truncated, footer_st);
            cx = buf.put_str(cx, row, " ", footer_st);
            let (icon_fg, icon_bg) = p.state.icon_style(pal);
            buf.put_str(cx, row, p.state.icon(ui), St::fg_bg(icon_fg, icon_bg));

            // Line 2: cursor, progress bar, percentage.
            let pct = if p.total != 0 {
                p.done as f64 / p.total as f64
            } else {
                1.0
            };
            let bar_total = view_w.saturating_sub(3);
            let pct_text = format_percent(pct);
            let tw = bar_total.saturating_sub(5);
            let mut fw = (tw as f64 * pct).round() as usize;
            fw = fw.min(tw);
            let blend = blend_steps(fw * 2, pal.gradient_color(0), pal.gradient_color(1));
            let mut cx = x + 1;
            cx = buf.put_str(cx, row + 1, cursor_str, cursor_st);
            for i in 0..fw {
                let fg = blend[2 * i];
                let bg = blend[2 * i + 1];
                cx = buf.put_str(cx, row + 1, "▌", St::fg_bg(fg, bg));
            }
            for _ in fw..tw {
                cx = buf.put_str(
                    cx,
                    row + 1,
                    "░",
                    St::fg_bg(Color::Rgb(0x60, 0x60, 0x60), pal.footer_bg),
                );
            }
            buf.put_str(cx, row + 1, &pct_text, footer_st);
            // Line 3: left blank (already footer bg from the initial fill).
        }
    }
}

/// Format a fraction as the 5-column percentage string (Go " %3.0f%%").
fn format_percent(pct: f64) -> String {
    let v = (pct * 100.0).clamp(0.0, 100.0);
    format!("{}{}%", " ", format!("{:3.0}", v))
}

/// Per-channel RGB lerp between two colors (Go/CSS lerp is CIELAB; RGB here).
fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let l = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round().clamp(0.0, 255.0) as u8;
            Color::Rgb(l(ar, br), l(ag, bg), l(ab, bb))
        }
        _ => a,
    }
}

/// Blended color stops for the bar (Go/lipgloss Blend1D with two stops):
/// exact endpoints when steps <= 2, evenly spaced lerps otherwise.
fn blend_steps(steps: usize, c0: Color, c1: Color) -> Vec<Color> {
    if steps == 0 {
        return Vec::new();
    }
    if steps == 1 {
        return vec![c0];
    }
    if steps == 2 {
        return vec![c0, c1];
    }
    (0..steps)
        .map(|j| lerp_color(c0, c1, j as f64 / (steps - 1) as f64))
        .collect()
}

#[cfg(test)]
impl ProcessBar {
    /// Test helper: finish a process with a controlled done time.
    fn finish_with_time(&mut self, id: u64, state: State, err: Option<String>, t: DateTime<Utc>) {
        if let Some(p) = self.processes.iter_mut().find(|p| p.id == id) {
            p.state = state;
            p.error_msg = err.unwrap_or_default();
            p.done_time = Some(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use ratatui::style::Color;

    /// Process bar with `n` Copy processes (ids 0..n), each 0/10, at 20x7.
    fn make_bar(n: usize) -> ProcessBar {
        let mut pb = ProcessBar::new();
        pb.set_dimensions(20, 7);
        for id in 0..n {
            pb.add(id as u64, Operation::Copy, "file.txt", 10).unwrap();
        }
        pb
    }

    #[test]
    fn test_format_percent() {
        assert_eq!(format_percent(0.0), "   0%");
        assert_eq!(format_percent(0.42), "  42%");
        assert_eq!(format_percent(0.996), " 100%");
        assert_eq!(format_percent(1.0), " 100%");
    }

    #[test]
    fn test_add_duplicate() {
        let mut pb = make_bar(1);
        let res = pb.add(0, Operation::Copy, "x", 5);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("process already exists with id :"));
    }

    #[test]
    fn test_sort_order() {
        let mut pb = ProcessBar::new();
        pb.set_dimensions(20, 7);
        // A: InOperation 0/10
        pb.add(1, Operation::Copy, "a", 10).unwrap();
        pb.update_progress(1, "a", 0, 10);
        // B: InOperation 9/10
        pb.add(2, Operation::Copy, "b", 10).unwrap();
        pb.update_progress(2, "b", 9, 10);
        // C: Cancelled 5/10 (not "done" for sorting)
        pb.add(3, Operation::Copy, "c", 10).unwrap();
        pb.update_progress(3, "c", 5, 10);
        pb.finish(3, State::Cancelled, None);
        // D: Successful, oldest done time
        pb.add(4, Operation::Copy, "d", 10).unwrap();
        // E: Successful, newest done time
        pb.add(5, Operation::Copy, "e", 10).unwrap();
        let t1 = DateTime::<Utc>::UNIX_EPOCH + Duration::seconds(100);
        let t2 = DateTime::<Utc>::UNIX_EPOCH + Duration::seconds(200);
        pb.finish_with_time(4, State::Successful, None, t1);
        pb.finish_with_time(5, State::Successful, None, t2);

        let ids: Vec<u64> = pb.sorted().iter().map(|p| p.id).collect();
        // Not-done by completion asc (A, C, B); done newest-first (E, D).
        assert_eq!(ids, vec![1, 3, 2, 5, 4]);
    }

    #[test]
    fn test_navigation() {
        // 5 processes, height 7 -> cntRenderable = 2.
        let mut pb = make_bar(5);
        pb.list_down();
        assert_eq!(pb.cursor, 1);
        assert_eq!(pb.render_index, 0);
        pb.list_down();
        assert_eq!(pb.cursor, 2);
        assert_eq!(pb.render_index, 1);

        let mut pb2 = make_bar(5);
        // list_up from cursor 0 wraps to the last.
        pb2.list_up();
        assert_eq!(pb2.cursor, 4);
        assert_eq!(pb2.render_index, 3);
        // list_down at the last wraps to the first.
        pb2.list_down();
        assert_eq!(pb2.cursor, 0);
        assert_eq!(pb2.render_index, 0);
    }

    #[test]
    fn test_blend_steps() {
        let c0 = Color::Rgb(0, 0, 0);
        let c1 = Color::Rgb(255, 255, 255);
        assert!(blend_steps(0, c0, c1).is_empty());
        // steps == 2 -> exact endpoints, no interpolation.
        assert_eq!(blend_steps(2, c0, c1), vec![c0, c1]);
        // steps == 4 -> endpoints exact, middle are even lerps (255/3, 2*255/3).
        let b4 = blend_steps(4, c0, c1);
        assert_eq!(b4.len(), 4);
        assert_eq!(b4[0], c0);
        assert_eq!(b4[3], c1);
        assert_eq!(b4[1], Color::Rgb(85, 85, 85));
        assert_eq!(b4[2], Color::Rgb(170, 170, 170));
    }

    #[test]
    fn test_has_running() {
        let mut pb = make_bar(1);
        // InOperation with done (0) < total (10) -> running.
        assert!(pb.has_running());
        pb.finish(0, State::Successful, None);
        assert!(!pb.has_running());
    }
}
