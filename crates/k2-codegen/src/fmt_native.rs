//! Compile-time parse of a `print` format string into a render plan.
//!
//! `print(fmt, args)`'s format string is a **compile-time constant** (a
//! `Const::Str`), so the native backend parses it in Rust — reusing the exact
//! grammar of `k2_vm::fmt` — and emits a straight-line sequence of render steps.
//! No runtime format-string interpreter is needed: each literal run becomes a
//! `memcpy` of known bytes into the print buffer, and each `{...}` placeholder
//! becomes a runtime render of the matching tuple field, padded per its spec.
//!
//! The parse mirrors `fmt.rs::format_into` / `parse_spec` byte-for-byte
//! (including `{{`/`}}` escapes, a bare `}` passed through literally, the
//! `[fill][align][width]` spec, and the positional argument counter), so the
//! emitted bytes match the VM's `format_into` output exactly.

/// One step in a parsed format string.
#[derive(Clone, Debug)]
pub enum Step {
    /// Emit these literal bytes verbatim (an escaped-brace-resolved literal run).
    Literal(Vec<u8>),
    /// Render the `arg_index`-th tuple field with `spec` (and pad per `spec`).
    Placeholder {
        /// The positional argument index this placeholder consumes.
        arg_index: usize,
        /// The parsed spec.
        spec: Spec,
    },
}

/// The verb of a placeholder (which renderer to invoke).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    /// `{}` — default, dispatched by the field's static type.
    Default,
    /// `{s}` — string / `[]const u8`.
    Str,
    /// `{d}` — decimal integer.
    Decimal,
    /// `{c}` — a UTF-8-encoded code point.
    Char,
    /// `{x}` / `{X}` — hexadecimal (`upper` selects `{X}`).
    Hex { upper: bool },
    /// `{b}` — binary.
    Bin,
    /// `{o}` — octal.
    Oct,
}

/// Field alignment within a fixed width (mirrors `fmt::Align`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    /// No alignment requested (and so, per `fmt.rs`, no padding even with width).
    None,
    /// Left-aligned (`<`).
    Left,
    /// Right-aligned (`>`).
    Right,
    /// Center-aligned (`^`).
    Center,
}

/// A parsed placeholder spec.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// The render verb.
    pub verb: Verb,
    /// The fill byte (default space). Parsed faithfully so it is available when a
    /// future milestone implements the width/alignment padding the native
    /// renderer currently defers; the value is captured here regardless.
    #[allow(dead_code)]
    pub fill: u8,
    /// The alignment.
    pub align: Align,
    /// The field width (0 = none).
    pub width: usize,
}

/// Parses `fmt` into a step list, or `Err(msg)` for an unknown verb / malformed
/// brace (the VM converts the same conditions to a clean panic). The positional
/// argument counter increments per placeholder exactly as `format_into` does.
pub fn parse(fmt: &[u8]) -> Result<Vec<Step>, String> {
    let mut steps: Vec<Step> = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let mut next_arg = 0usize;
    while i < fmt.len() {
        let b = fmt[i];
        match b {
            b'{' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'{' {
                    lit.push(b'{');
                    i += 2;
                    continue;
                }
                let close = fmt[i + 1..]
                    .iter()
                    .position(|&c| c == b'}')
                    .map(|p| i + 1 + p)
                    .ok_or_else(|| "unterminated `{` in format string".to_string())?;
                let spec_bytes = &fmt[i + 1..close];
                let spec = parse_spec(spec_bytes)?;
                if !lit.is_empty() {
                    steps.push(Step::Literal(std::mem::take(&mut lit)));
                }
                steps.push(Step::Placeholder {
                    arg_index: next_arg,
                    spec,
                });
                next_arg += 1;
                i = close + 1;
            }
            b'}' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'}' {
                    lit.push(b'}');
                    i += 2;
                    continue;
                }
                lit.push(b'}');
                i += 1;
            }
            _ => {
                lit.push(b);
                i += 1;
            }
        }
    }
    if !lit.is_empty() {
        steps.push(Step::Literal(lit));
    }
    Ok(steps)
}

/// Parses one placeholder body into a [`Spec`], mirroring `fmt::parse_spec`.
fn parse_spec(spec: &[u8]) -> Result<Spec, String> {
    let (verb_bytes, rest) = match spec.iter().position(|&c| c == b':') {
        Some(p) => (&spec[..p], &spec[p + 1..]),
        None => (spec, &b""[..]),
    };
    let verb = parse_verb(verb_bytes)?;
    let mut fill = b' ';
    let mut align = Align::None;
    let mut rest = rest;
    if !rest.is_empty() {
        if rest.len() >= 2 && matches!(rest[1], b'<' | b'>' | b'^') {
            fill = rest[0];
            align = align_of(rest[1]);
            rest = &rest[2..];
        } else if matches!(rest[0], b'<' | b'>' | b'^') {
            align = align_of(rest[0]);
            rest = &rest[1..];
        }
    }
    let mut width = 0usize;
    for &c in rest {
        if c.is_ascii_digit() {
            width = width * 10 + (c - b'0') as usize;
        }
    }
    Ok(Spec {
        verb,
        fill,
        align,
        width,
    })
}

/// Maps a verb byte string to a [`Verb`], mirroring `fmt::render_value`.
fn parse_verb(verb: &[u8]) -> Result<Verb, String> {
    Ok(match verb {
        b"" => Verb::Default,
        b"s" => Verb::Str,
        b"d" => Verb::Decimal,
        b"c" => Verb::Char,
        b"x" => Verb::Hex { upper: false },
        b"X" => Verb::Hex { upper: true },
        b"b" => Verb::Bin,
        b"o" => Verb::Oct,
        other => {
            return Err(format!(
                "unknown format verb `{{{}}}`",
                String::from_utf8_lossy(other)
            ))
        }
    })
}

/// Maps an alignment char to [`Align`].
fn align_of(c: u8) -> Align {
    match c {
        b'<' => Align::Left,
        b'>' => Align::Right,
        b'^' => Align::Center,
        _ => Align::None,
    }
}
