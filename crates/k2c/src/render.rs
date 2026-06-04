//! # render — a pure-std, rustc/ariadne-style diagnostic renderer
//!
//! Given the source text and a [`RichDiagnostic`], this module produces a
//! labelled, multi-line report:
//!
//! ```text
//! error: expected `i32`, found `bool`
//!   --> examples/bad.k2:7:17
//!    |
//!  7 |     const x: i32 = flag;
//!    |              ---   ^^^^ this is `bool`
//!    |              |
//!    |              expected `i32` because of this
//!    |
//!    = help: convert with `@as(i32, …)` if a narrowing cast is intended
//! ```
//!
//! The renderer lives in the driver (`k2c`) — the only consumer that has the
//! source text in hand — so the leaf crates stay free of formatting policy. It
//! is a pure, deterministic function of `(label, source, diagnostic, opts)`,
//! which makes it directly unit-testable against literal source strings.
//!
//! ## Correctness contract
//!
//! Spans are **char (scalar) offsets**, not byte offsets and not display
//! columns. Three coordinate spaces matter:
//!
//! 1. char offset (`span.start`/`end`) — an index into `source.chars()`;
//! 2. line + char-column-within-line — found by walking the source once;
//! 3. **display column** — the on-screen caret position, computed by summing
//!    each char's display width (tabs expand to the next tab stop, CJK/emoji
//!    are width-2, combining marks / zero-width are width-0).
//!
//! The caret is laid out so it aligns under the *right grapheme* regardless of
//! multi-byte encoding or tabs: the leading pad copies the source's own tabs
//! verbatim (so alignment holds at any terminal tab width) and replaces every
//! other prefix display cell with one space.
//!
//! ## Never-panic contract
//!
//! The renderer must not panic on *any* input: empty files, spans at or past
//! EOF, zero-width spans, `end < start`, multi-byte text, tab-only lines, and
//! 100 000-char lines are all handled. All indexing is via `char_indices` /
//! `.get(range)` / saturating arithmetic — never a bare `[]` that could panic.

use std::io::{IsTerminal, Write};

use k2_syntax::{LabelStyle, RichDiagnostic, RichSeverity, Span};

/// Rendering options: whether to emit ANSI color, and the tab stop width.
#[derive(Clone, Copy, Debug)]
pub struct RenderOpts {
    /// Emit ANSI color escapes when `true`; plain text when `false`.
    pub color: bool,
    /// The tab stop width used to expand `\t` for caret alignment.
    pub tab_width: usize,
}

impl Default for RenderOpts {
    fn default() -> RenderOpts {
        RenderOpts {
            color: false,
            tab_width: 4,
        }
    }
}

impl RenderOpts {
    /// Detects the rendering options from the environment and the stderr stream.
    ///
    /// Color is disabled when `NO_COLOR`/`K2_NO_COLOR` is set (per the NO_COLOR
    /// spec, *any* value counts), and otherwise follows whether stderr is a
    /// terminal — overridable with `K2_COLOR=always|never|auto` and
    /// `CLICOLOR_FORCE`. The tab width comes from `K2_TAB_WIDTH` (clamped to
    /// `1..=16`), defaulting to 4.
    pub fn detect() -> RenderOpts {
        let tab_width = std::env::var("K2_TAB_WIDTH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|w| w.clamp(1, 16))
            .unwrap_or(4);

        let color = match std::env::var("K2_COLOR").ok().as_deref() {
            Some("always") => true,
            Some("never") => false,
            _ => {
                if std::env::var_os("NO_COLOR").is_some()
                    || std::env::var_os("K2_NO_COLOR").is_some()
                {
                    false
                } else if std::env::var_os("CLICOLOR_FORCE").is_some() {
                    true
                } else {
                    std::io::stderr().is_terminal()
                }
            }
        };

        RenderOpts { color, tab_width }
    }
}

/// The maximum display width a single rendered source line may occupy before it
/// is truncated. Bounds the work the renderer does on a giant line.
const MAX_LINE_DISPLAY: usize = 512;

/// ANSI color escapes. Every helper is the identity when `color == false`, so
/// the plain-text layout (what the tests assert against) is byte-identical to
/// the colored layout minus the escapes.
mod ansi {
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const BLUE: &str = "\x1b[34m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

/// Wraps `s` in `code … reset` when `color` is on, else returns `s` unchanged.
fn paint(out: &mut String, color: bool, code: &str, s: &str) {
    if color {
        out.push_str(code);
        out.push_str(s);
        out.push_str(ansi::RESET);
    } else {
        out.push_str(s);
    }
}

/// The display width of a single char.
///
/// Tabs are handled by the caller (their width depends on the current column);
/// this returns `1` for `\t` as a fallback. Combining marks and other
/// zero-width code points return `0`; East-Asian-wide and wide-emoji ranges
/// return `2`.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    // C0/C1 controls (other than tab, handled separately) take no visible cell.
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    if is_zero_width(cp) {
        return 0;
    }
    if is_wide(cp) {
        return 2;
    }
    1
}

/// `true` for code points that occupy no display cell (combining marks,
/// zero-width spaces/joiners, variation selectors).
fn is_zero_width(cp: u32) -> bool {
    // A compact, pragmatic subset — enough for correct caret alignment on real
    // source, without pulling in the full Unicode database.
    matches!(cp,
        0x0300..=0x036F // combining diacritical marks
        | 0x1AB0..=0x1AFF // combining diacritical marks extended
        | 0x1DC0..=0x1DFF // combining diacritical marks supplement
        | 0x20D0..=0x20FF // combining marks for symbols
        | 0xFE00..=0xFE0F // variation selectors
        | 0xFE20..=0xFE2F // combining half marks
        | 0x200B..=0x200F // zero-width space/joiner/non-joiner + marks
        | 0x2060..=0x2064 // word joiner / invisible operators
    )
}

/// `true` for East-Asian-wide / wide-emoji code points (display width 2).
///
/// A compact hard-coded range table covering the canonical wide blocks (CJK,
/// Hangul, Kana, fullwidth forms) and the common wide-emoji ranges. Pure std,
/// no `unicode-width` crate.
fn is_wide(cp: u32) -> bool {
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),   // Hangul Jamo
        (0x2329, 0x232A),   // angle brackets
        (0x2E80, 0x303E),   // CJK radicals .. Kangxi
        (0x3041, 0x33FF),   // Hiragana .. CJK compat
        (0x3400, 0x4DBF),   // CJK ext A
        (0x4E00, 0x9FFF),   // CJK unified
        (0xA000, 0xA4CF),   // Yi
        (0xAC00, 0xD7A3),   // Hangul syllables
        (0xF900, 0xFAFF),   // CJK compat ideographs
        (0xFE10, 0xFE19),   // vertical forms
        (0xFE30, 0xFE6F),   // CJK compat forms / small forms
        (0xFF00, 0xFF60),   // fullwidth forms
        (0xFFE0, 0xFFE6),   // fullwidth signs
        (0x1F300, 0x1F64F), // misc symbols & pictographs + emoticons
        (0x1F900, 0x1F9FF), // supplemental symbols & pictographs
        (0x20000, 0x2FFFD), // CJK ext B..
        (0x30000, 0x3FFFD), // CJK ext G..
    ];
    WIDE.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// Per-line geometry computed in one pass over the source.
struct LineInfo {
    /// The char offset of the first char on this line.
    start_char: usize,
    /// The line's text, *excluding* its trailing `\n` (and `\r`).
    text: String,
}

/// Walks the source once, producing one [`LineInfo`] per line. The newline that
/// terminates a line belongs to that line (it is excluded from `text`), so the
/// last entry is the final line even when the file does not end in `\n`. An
/// empty source yields a single empty line.
fn index_lines(source: &str) -> Vec<LineInfo> {
    let mut lines = Vec::new();
    let mut start_char = 0usize;
    let mut cur = String::new();
    let mut char_idx = 0usize;
    for c in source.chars() {
        if c == '\n' {
            if cur.ends_with('\r') {
                cur.pop();
            }
            lines.push(LineInfo {
                start_char,
                text: std::mem::take(&mut cur),
            });
            char_idx += 1;
            start_char = char_idx;
            continue;
        }
        cur.push(c);
        char_idx += 1;
    }
    if !cur.is_empty() || lines.is_empty() {
        if cur.ends_with('\r') {
            cur.pop();
        }
        lines.push(LineInfo {
            start_char,
            text: cur,
        });
    }
    lines
}

/// Finds the index of the line containing char offset `off`, clamped into range.
fn line_of(lines: &[LineInfo], off: usize) -> usize {
    let mut lo = 0usize;
    let mut hi = lines.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if lines[mid].start_char <= off {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(1)
}

/// The display column (0-based) of char-column `char_col` within `line_text`,
/// expanding tabs to `tab` stops and summing per-char display widths.
fn display_col(line_text: &str, char_col: usize, tab: usize) -> usize {
    let stop = tab.max(1);
    let mut col = 0usize;
    for (i, c) in line_text.chars().enumerate() {
        if i >= char_col {
            break;
        }
        if c == '\t' {
            col = (col / stop + 1) * stop;
        } else {
            col += char_width(c);
        }
    }
    col
}

/// Builds the leading pad reaching `start_col` display columns by copying the
/// source's own leading run — tabs verbatim, every other display cell one space.
///
/// `prefix` is the line text up to (but not including) the underline's start
/// char. Copying real tabs keeps the caret aligned at any terminal tab width;
/// once `prefix` is exhausted the pad is filled with spaces to `start_col`.
fn leading_pad(prefix: &str, start_col: usize, tab: usize) -> String {
    let stop = tab.max(1);
    let mut pad = String::new();
    let mut col = 0usize;
    for c in prefix.chars() {
        if col >= start_col {
            break;
        }
        if c == '\t' {
            pad.push('\t');
            col = (col / stop + 1) * stop;
        } else {
            let w = char_width(c);
            for _ in 0..w {
                pad.push(' ');
            }
            col += w;
        }
    }
    while col < start_col {
        pad.push(' ');
        col += 1;
    }
    pad
}

/// A single-line label resolved onto a source line: its display-column span and
/// inline message.
struct Placed<'a> {
    /// Start display column (0-based) of the underline.
    start_col: usize,
    /// Underline width in display columns (>= 1).
    width: usize,
    /// `^` (primary) or `-` (secondary).
    style: LabelStyle,
    /// The inline message (may be empty).
    msg: &'a str,
}

/// A label resolved onto source coordinates (line indices + char offsets).
struct Resolved<'a> {
    /// The label's start line index.
    line: usize,
    /// Start char-column within `line`.
    start: usize,
    /// End char-column within the start line (== text length for multi-line).
    end: usize,
    /// `true` if the span crosses lines.
    multiline: bool,
    /// The end line index (== `line` for single-line).
    end_line: usize,
    /// End char-column within `end_line`.
    end_col: usize,
    /// Primary / secondary.
    style: LabelStyle,
    /// The inline message.
    msg: &'a str,
}

/// Renders one [`RichDiagnostic`] over `source` into a `String`. `label` is the
/// file path shown in the locator. Never panics on any input.
pub fn render(label: &str, source: &str, diag: &RichDiagnostic, opts: &RenderOpts) -> String {
    let lines = index_lines(source);
    let char_count = source.chars().count();
    let color = opts.color;
    let tab = opts.tab_width.max(1);

    let mut out = String::new();

    // ---- Header: `severity: message` -----------------------------------
    let sev_code = match diag.severity {
        RichSeverity::Error => ansi::RED,
        RichSeverity::Warning => ansi::YELLOW,
        RichSeverity::Note => ansi::CYAN,
    };
    if color {
        out.push_str(ansi::BOLD);
    }
    paint(&mut out, color, sev_code, diag.severity.word());
    if color {
        out.push_str(ansi::BOLD);
    }
    out.push_str(": ");
    out.push_str(&diag.message);
    if color {
        out.push_str(ansi::RESET);
    }
    out.push('\n');

    // ---- Resolve every label onto source coordinates -------------------
    //
    // The primary label is resolved first so the locator header can derive its
    // line/col from the *same* clamped coordinates the snippet uses. Trusting the
    // span's own `line`/`col` fields instead would let the header and the shown
    // snippet disagree whenever the span's offset was clamped (e.g. an EOF span
    // whose offset lands at end-of-user-source but whose `line` points elsewhere).
    let mut labs: Vec<Resolved> = Vec::new();
    labs.push(resolve_label(
        &lines,
        char_count,
        diag.primary.span,
        LabelStyle::Primary,
        &diag.primary.message,
    ));
    for s in &diag.secondary {
        labs.push(resolve_label(
            &lines, char_count, s.span, s.style, &s.message,
        ));
    }

    // ---- Gutter width + locator ----------------------------------------
    // Line numbers come from the resolved (clamped) coordinates, never the raw
    // span fields, so the gutter and locator agree with the printed snippet.
    let mut max_line = 1usize;
    for l in &labs {
        max_line = max_line.max(l.line + 1);
        if l.multiline {
            max_line = max_line.max(l.end_line + 1);
        }
    }
    let gutter_w = max_line.to_string().len().max(1);
    let pad_g = " ".repeat(gutter_w);

    // The locator points at the primary label's resolved start: 1-based line
    // index + 1-based char-column within that line.
    let primary = &labs[0];
    let loc_line = primary.line + 1;
    let loc_col = primary.start + 1;
    paint(&mut out, color, ansi::BLUE, &format!("{pad_g} --> "));
    out.push_str(&format!("{label}:{loc_line}:{loc_col}\n"));

    // The source lines we must print.
    let mut shown: Vec<usize> = Vec::new();
    for l in &labs {
        shown.push(l.line);
        if l.multiline {
            shown.push(l.end_line);
        }
    }
    shown.sort_unstable();
    shown.dedup();

    let blank_gutter = |out: &mut String| {
        paint(out, color, ansi::BLUE, &format!("{pad_g} |"));
        out.push('\n');
    };
    blank_gutter(&mut out);

    let mut prev: Option<usize> = None;
    for &li in &shown {
        if let Some(p) = prev {
            if li > p + 1 {
                paint(&mut out, color, ansi::BLUE, &format!("{pad_g}..."));
                out.push('\n');
            }
        }
        prev = Some(li);

        let line_text = &lines[li].text;
        let num = (li + 1).to_string();
        let num_pad = " ".repeat(gutter_w.saturating_sub(num.len()));
        let display_text = truncate_line(line_text, tab);
        paint(&mut out, color, ansi::BLUE, &format!("{num_pad}{num} | "));
        out.push_str(&display_text);
        out.push('\n');

        // Multi-line opening rails on this line.
        for l in labs.iter().filter(|l| l.multiline && l.line == li) {
            // Clamp into the truncation window so the rail never runs past the
            // visible (possibly `…`-truncated) source on a giant line.
            let start_col = display_col(line_text, l.start, tab).min(MAX_LINE_DISPLAY);
            let prefix: String = line_text.chars().take(l.start).collect();
            let pad = leading_pad(&prefix, start_col, tab);
            let rail: String = pad
                .chars()
                .map(|c| if c == '\t' { '\t' } else { '_' })
                .collect();
            paint(&mut out, color, ansi::BLUE, &format!("{pad_g} | "));
            paint(
                &mut out,
                color,
                style_code(l.style),
                &format!("{rail}{}", l.style.glyph()),
            );
            out.push('\n');
        }

        // Multi-line closing rails on this line.
        //
        // The closing glyph must sit under the span's LAST char (inclusive), not
        // one past it: `l.end_col` is the EXCLUSIVE end char-column, so we target
        // the display column of `end_col - 1`. The leading `|` (the rail's left
        // edge) occupies display column 0 — we OVERWRITE the pad's first cell with
        // it rather than prepending an extra cell, so the underline `^` lands
        // exactly under the last char (prepending would shift the whole rail +1,
        // and using the exclusive end would add another +1: the old +2 bug).
        for l in labs.iter().filter(|l| l.multiline && l.end_line == li) {
            let last = l.end_col.saturating_sub(1);
            // Clamp into the truncation window (see the opening rail above).
            let target = display_col(line_text, last, tab).min(MAX_LINE_DISPLAY);
            let prefix: String = line_text.chars().take(last).collect();
            let pad = leading_pad(&prefix, target, tab);
            let mut rail: String = pad
                .chars()
                .map(|c| if c == '\t' { '\t' } else { '_' })
                .collect();
            // Put the `|` at column 0 by replacing the first cell of the rail.
            // (When the target is column 0 the rail is empty; the `|` then simply
            // is the whole rail and the glyph sits at column 1 — under the last
            // char, which is itself at column 0's grapheme.)
            if rail.is_empty() {
                rail.push('|');
            } else {
                let mut chars: Vec<char> = rail.chars().collect();
                chars[0] = '|';
                rail = chars.into_iter().collect();
            }
            let mut piece = format!("{rail}{}", l.style.glyph());
            if !l.msg.is_empty() {
                piece.push(' ');
                piece.push_str(l.msg);
            }
            paint(&mut out, color, ansi::BLUE, &format!("{pad_g} | "));
            paint(&mut out, color, style_code(l.style), &piece);
            out.push('\n');
        }

        // Single-line underline row. Columns are clamped to the truncation
        // boundary so a caret on a line wider than MAX_LINE_DISPLAY (rendered
        // with a trailing `…`) never floats off past the visible source.
        let placed: Vec<Placed> = labs
            .iter()
            .filter(|l| !l.multiline && l.line == li)
            .map(|l| {
                let start_col = display_col(line_text, l.start, tab);
                let end_col = display_col(line_text, l.end, tab);
                let width = end_col.saturating_sub(start_col).max(1);
                let (start_col, width) = clamp_underline(start_col, width);
                Placed {
                    start_col,
                    width,
                    style: l.style,
                    msg: l.msg,
                }
            })
            .collect();
        if !placed.is_empty() {
            render_underline(&mut out, line_text, &placed, &pad_g, tab, color);
        }
    }

    // ---- Notes / help --------------------------------------------------
    if !diag.notes.is_empty() || diag.help.is_some() {
        blank_gutter(&mut out);
    }
    for note in &diag.notes {
        paint(&mut out, color, ansi::BLUE, &format!("{pad_g} = "));
        paint(&mut out, color, ansi::CYAN, "note");
        out.push_str(": ");
        out.push_str(note);
        out.push('\n');
    }
    if let Some(help) = &diag.help {
        paint(&mut out, color, ansi::BLUE, &format!("{pad_g} = "));
        paint(&mut out, color, ansi::CYAN, "help");
        out.push_str(": ");
        out.push_str(help);
        out.push('\n');
    }

    out
}

/// Resolves a label's span onto source coordinates (line indices + char
/// offsets within those lines). Clamps every offset into range.
fn resolve_label<'a>(
    lines: &[LineInfo],
    char_count: usize,
    span: Span,
    style: LabelStyle,
    msg: &'a str,
) -> Resolved<'a> {
    let start_off = (span.start as usize).min(char_count);
    let end_off = (span.end as usize).min(char_count).max(start_off);
    let li = line_of(lines, start_off);
    let lj = line_of(lines, end_off);
    let start = start_off.saturating_sub(lines[li].start_char);
    let end_col = end_off.saturating_sub(lines[lj].start_char);
    let end = if li == lj {
        end_col.max(start)
    } else {
        lines[li].text.chars().count()
    };
    Resolved {
        line: li,
        start,
        end,
        multiline: lj > li,
        end_line: lj,
        end_col,
        style,
        msg,
    }
}

/// The ANSI code a label's underline is drawn in.
fn style_code(style: LabelStyle) -> &'static str {
    match style {
        LabelStyle::Primary => ansi::RED,
        LabelStyle::Secondary => ansi::CYAN,
    }
}

/// Renders the underline row(s) for every single-line label on one source line.
///
/// The underline row carries each label's `^`/`-` glyphs at their display
/// columns. The rightmost-ending label keeps its message inline; the others get
/// stacked connector + message rows below (rustc-style), ordered by start
/// column.
fn render_underline(
    out: &mut String,
    line_text: &str,
    placed: &[Placed<'_>],
    pad_g: &str,
    tab: usize,
    color: bool,
) {
    // The underline cell grid: a glyph (or space) per display column.
    let total = placed
        .iter()
        .map(|p| p.start_col + p.width)
        .max()
        .unwrap_or(0);
    let mut grid: Vec<Option<LabelStyle>> = vec![None; total];
    for p in placed {
        for col in p.start_col..(p.start_col + p.width) {
            if col < grid.len() {
                grid[col] = Some(p.style);
            }
        }
    }

    // The leading pad uses the source's own tabs so the row aligns. Build it to
    // the first glyph column.
    let first_col = placed.iter().map(|p| p.start_col).min().unwrap_or(0);
    let prefix: String = {
        // The char prefix up to the first label's start char-column. Recompute
        // it from display columns by walking the line.
        let mut s = String::new();
        let mut col = 0usize;
        let stop = tab.max(1);
        for c in line_text.chars() {
            if col >= first_col {
                break;
            }
            s.push(c);
            if c == '\t' {
                col = (col / stop + 1) * stop;
            } else {
                col += char_width(c);
            }
        }
        s
    };
    let lead = leading_pad(&prefix, first_col, tab);

    paint(out, color, ansi::BLUE, &format!("{pad_g} | "));
    out.push_str(&lead);
    // Emit glyph cells from `first_col`..`total`.
    let mut col = first_col;
    while col < total {
        match grid[col] {
            None => {
                out.push(' ');
                col += 1;
            }
            Some(style) => {
                let mut run = String::new();
                while col < total && grid[col] == Some(style) {
                    run.push(style.glyph());
                    col += 1;
                }
                paint(out, color, style_code(style), &run);
            }
        }
    }

    // Inline message: the label whose underline ends rightmost.
    let rightmost_end = placed
        .iter()
        .map(|p| p.start_col + p.width)
        .max()
        .unwrap_or(0);
    let inline = placed
        .iter()
        .filter(|p| p.start_col + p.width == rightmost_end && !p.msg.is_empty())
        .max_by_key(|p| p.start_col);
    if let Some(p) = inline {
        out.push(' ');
        paint(out, color, style_code(p.style), p.msg);
    }
    out.push('\n');

    // Stacked rows for the remaining labelled spans.
    let mut remaining: Vec<&Placed> = placed
        .iter()
        .filter(|p| {
            // Skip empty messages and the one label drawn inline (the rightmost).
            let is_inline = p.start_col + p.width == rightmost_end
                && inline.map(|x| x.start_col) == Some(p.start_col);
            !p.msg.is_empty() && !is_inline
        })
        .collect();
    // Process right-to-left so connectors stack cleanly.
    remaining.sort_by_key(|p| std::cmp::Reverse(p.start_col));
    while let Some(cur) = remaining.first().copied() {
        // Connector row: a `|` under every still-remaining label's start.
        let mut cells: Vec<(usize, LabelStyle)> =
            remaining.iter().map(|p| (p.start_col, p.style)).collect();
        cells.sort_by_key(|(c, _)| *c);
        render_marks_row(out, pad_g, &cells, color);
        // Message row: `cur`'s message at its column; `|` for the others.
        let others: Vec<(usize, LabelStyle)> = remaining
            .iter()
            .filter(|p| p.start_col != cur.start_col)
            .map(|p| (p.start_col, p.style))
            .collect();
        render_message_row(
            out,
            pad_g,
            cur.start_col,
            cur.msg,
            cur.style,
            &others,
            color,
        );
        remaining.retain(|p| p.start_col != cur.start_col);
    }
}

/// Renders a row with a `|` at each given display column.
fn render_marks_row(out: &mut String, pad_g: &str, cells: &[(usize, LabelStyle)], color: bool) {
    paint(out, color, ansi::BLUE, &format!("{pad_g} | "));
    let mut col = 0usize;
    for &(c, style) in cells {
        while col < c {
            out.push(' ');
            col += 1;
        }
        if col == c {
            paint(out, color, style_code(style), "|");
            col += 1;
        }
    }
    out.push('\n');
}

/// Renders a message row: `|` connectors for the other labels, then the current
/// label's message at its column.
fn render_message_row(
    out: &mut String,
    pad_g: &str,
    msg_col: usize,
    msg: &str,
    msg_style: LabelStyle,
    others: &[(usize, LabelStyle)],
    color: bool,
) {
    paint(out, color, ansi::BLUE, &format!("{pad_g} | "));
    let mut all: Vec<(usize, Option<LabelStyle>)> =
        others.iter().map(|(c, s)| (*c, Some(*s))).collect();
    all.push((msg_col, None));
    all.sort_by_key(|(c, _)| *c);
    let mut col = 0usize;
    for (c, s) in all {
        while col < c {
            out.push(' ');
            col += 1;
        }
        match s {
            Some(style) => {
                if col == c {
                    paint(out, color, style_code(style), "|");
                    col += 1;
                }
            }
            None => {
                paint(out, color, style_code(msg_style), msg);
                col += msg.chars().count();
            }
        }
    }
    out.push('\n');
}

/// Truncates a giant source line so the renderer never does unbounded work,
/// appending `…` when it cuts. Short lines are returned verbatim.
fn truncate_line(line_text: &str, tab: usize) -> String {
    if display_col(line_text, line_text.chars().count(), tab) <= MAX_LINE_DISPLAY {
        return line_text.to_string();
    }
    let truncated: String = line_text.chars().take(MAX_LINE_DISPLAY).collect();
    format!("{truncated}…")
}

/// Clamps an underline label's `(start_col, width)` into the truncation window so
/// the caret row never extends past the visible (possibly `…`-truncated) source.
///
/// A label that starts at/after [`MAX_LINE_DISPLAY`] (i.e. under the truncated
/// region or the `…`) collapses to a single caret at the `…` column; otherwise
/// the width is capped so the underline ends at most one cell past
/// `MAX_LINE_DISPLAY` (the `…` glyph's cell, which a caret may share). The
/// emitted underline row is thus bounded by `MAX_LINE_DISPLAY + 1` display cells
/// regardless of how far out the original span lay. Labels comfortably inside the
/// window are returned unchanged.
fn clamp_underline(start_col: usize, width: usize) -> (usize, usize) {
    // The `…` sits at display column MAX_LINE_DISPLAY; allow a caret on it.
    const CAP: usize = MAX_LINE_DISPLAY;
    if start_col >= CAP {
        // Cut off entirely: a single marker at the `…` position.
        return (CAP, 1);
    }
    let max_width = (CAP + 1).saturating_sub(start_col);
    (start_col, width.min(max_width).max(1))
}

/// Renders every diagnostic in `diags` over `source` to `out`, in source order
/// (sorted by primary span start; ties keep emission order). Returns the number
/// of error-severity diagnostics.
pub fn emit_rich<W: Write>(
    out: &mut W,
    label: &str,
    source: &str,
    diags: &[RichDiagnostic],
    opts: &RenderOpts,
) -> usize {
    let mut order: Vec<usize> = (0..diags.len()).collect();
    order.sort_by(|&a, &b| {
        let sa = diags[a].primary.span;
        let sb = diags[b].primary.span;
        (sa.start, sa.end, a).cmp(&(sb.start, sb.end, b))
    });
    let mut errors = 0usize;
    for &i in &order {
        if diags[i].severity == RichSeverity::Error {
            errors += 1;
        }
        let s = render(label, source, &diags[i], opts);
        let _ = out.write_all(s.as_bytes());
    }
    errors
}

/// A phase diagnostic that can be turned into the shared rich form. Each phase's
/// `Diagnostic` implements this so the driver renders them all through one path.
pub trait AsRich {
    /// The rich rendering form of this diagnostic.
    fn as_rich(&self) -> RichDiagnostic;
    /// `true` if this is an error-severity diagnostic.
    fn is_error(&self) -> bool;
}

impl AsRich for k2_parse::Diagnostic {
    fn as_rich(&self) -> RichDiagnostic {
        self.to_rich()
    }
    fn is_error(&self) -> bool {
        self.severity == k2_parse::Severity::Error
    }
}

impl AsRich for k2_resolve::Diagnostic {
    fn as_rich(&self) -> RichDiagnostic {
        self.to_rich()
    }
    fn is_error(&self) -> bool {
        k2_resolve::Diagnostic::is_error(self)
    }
}

impl AsRich for k2_types::Diagnostic {
    fn as_rich(&self) -> RichDiagnostic {
        self.to_rich()
    }
    fn is_error(&self) -> bool {
        k2_types::Diagnostic::is_error(self)
    }
}

impl AsRich for k2_mir::Diagnostic {
    fn as_rich(&self) -> RichDiagnostic {
        self.to_rich()
    }
    fn is_error(&self) -> bool {
        k2_mir::Diagnostic::is_error(self)
    }
}

/// Renders `diags` (any phase's diagnostics) over `source` to stderr in source
/// order, using the auto-detected [`RenderOpts`]. Returns the error count.
///
/// This is the single rendering path that replaces the ~12 hand-rolled
/// `label:line:col: sev: msg` loops scattered across the subcommands.
pub fn emit_diags<D: AsRich>(label: &str, source: &str, diags: &[D]) -> usize {
    let opts = RenderOpts::detect();
    let rich: Vec<RichDiagnostic> = diags.iter().map(|d| d.as_rich()).collect();
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    emit_rich(&mut lock, label, source, &rich, &opts)
}

/// Like [`emit_diags`] but renders only the error-severity diagnostics (used by
/// gating commands that want to print just the blocking errors of an earlier
/// phase before stopping). Returns the error count.
pub fn emit_errors<D: AsRich>(label: &str, source: &str, diags: &[D]) -> usize {
    let opts = RenderOpts::detect();
    let rich: Vec<RichDiagnostic> = diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.as_rich())
        .collect();
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    emit_rich(&mut lock, label, source, &rich, &opts)
}

#[cfg(test)]
mod tests;
