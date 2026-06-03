//! The `Writer.print` placeholder formatter.
//!
//! `print(fmt, args)` is lowered to an intrinsic whose first argument is the
//! format string (`[]const u8`) and whose second is the positional argument
//! tuple (the `.{...}` aggregate). This module parses the placeholder grammar
//! the examples use and renders each argument by its runtime kind.
//!
//! Supported placeholders: `{}` (default), `{s}` (string), `{d}` (decimal int),
//! `{c}` (a byte/char), `{x}`/`{X}` (lower/upper hex), `{b}` (binary), `{o}`
//! (octal). A `{{`/`}}` is an escaped brace. An optional `:` introduces a
//! `[fill][align][width]` spec (e.g. `{s:>14}` right-aligns to width 14), which
//! `errors.k2` exercises.

use crate::value::Value;

/// Formats `fmt` with `args`, appending the rendered bytes to `out`. Returns an
/// error string only on a malformed format string (it should not happen for the
/// checked corpus, but the VM converts it to a clean panic if it does).
pub fn format_into(out: &mut Vec<u8>, fmt: &[u8], args: &[Value]) -> Result<(), String> {
    let mut i = 0usize;
    let mut next_arg = 0usize;
    while i < fmt.len() {
        let b = fmt[i];
        match b {
            b'{' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'{' {
                    out.push(b'{');
                    i += 2;
                    continue;
                }
                // Find the closing brace.
                let close = fmt[i + 1..]
                    .iter()
                    .position(|&c| c == b'}')
                    .map(|p| i + 1 + p)
                    .ok_or_else(|| "unterminated `{` in format string".to_string())?;
                let spec = &fmt[i + 1..close];
                let arg = args.get(next_arg);
                next_arg += 1;
                render_placeholder(out, spec, arg)?;
                i = close + 1;
            }
            b'}' => {
                if i + 1 < fmt.len() && fmt[i + 1] == b'}' {
                    out.push(b'}');
                    i += 2;
                    continue;
                }
                // A bare `}` is passed through literally.
                out.push(b'}');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(())
}

/// A parsed placeholder spec: a verb plus optional alignment/width.
struct Spec<'a> {
    verb: &'a [u8],
    fill: u8,
    align: Align,
    width: usize,
}

/// Field alignment within a fixed width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Right,
    Center,
}

/// Parses a placeholder body (`s`, `d`, `s:>14`, `:^8`, …) into a [`Spec`].
fn parse_spec(spec: &[u8]) -> Spec<'_> {
    // Split on the first ':'.
    let (verb, rest) = match spec.iter().position(|&c| c == b':') {
        Some(p) => (&spec[..p], &spec[p + 1..]),
        None => (spec, &b""[..]),
    };
    let mut fill = b' ';
    let mut align = Align::None;
    let mut rest = rest;
    // Optional fill char + alignment: `[fill]<|>|^`.
    if !rest.is_empty() {
        // If the second byte is an alignment char, the first is a fill char.
        if rest.len() >= 2 && matches!(rest[1], b'<' | b'>' | b'^') {
            fill = rest[0];
            align = align_of(rest[1]);
            rest = &rest[2..];
        } else if matches!(rest[0], b'<' | b'>' | b'^') {
            align = align_of(rest[0]);
            rest = &rest[1..];
        }
    }
    // Remaining ASCII digits are the width.
    let mut width = 0usize;
    for &c in rest {
        if c.is_ascii_digit() {
            width = width * 10 + (c - b'0') as usize;
        }
    }
    Spec {
        verb,
        fill,
        align,
        width,
    }
}

/// Maps an alignment char to its enum.
fn align_of(c: u8) -> Align {
    match c {
        b'<' => Align::Left,
        b'>' => Align::Right,
        b'^' => Align::Center,
        _ => Align::None,
    }
}

/// Renders one placeholder into `out`, applying any width/alignment padding.
fn render_placeholder(out: &mut Vec<u8>, spec: &[u8], arg: Option<&Value>) -> Result<(), String> {
    let s = parse_spec(spec);
    let mut field = Vec::new();
    render_value(&mut field, s.verb, arg)?;
    pad(out, &field, &s);
    Ok(())
}

/// Pads `field` into `out` according to the spec's width/alignment.
fn pad(out: &mut Vec<u8>, field: &[u8], s: &Spec<'_>) {
    if field.len() >= s.width || s.align == Align::None {
        out.extend_from_slice(field);
        // A width with no explicit alignment still right-pads nothing here; the
        // examples only use explicit alignment, so default is no padding.
        return;
    }
    let total = s.width - field.len();
    match s.align {
        Align::Left => {
            out.extend_from_slice(field);
            out.extend(std::iter::repeat_n(s.fill, total));
        }
        Align::Right => {
            out.extend(std::iter::repeat_n(s.fill, total));
            out.extend_from_slice(field);
        }
        Align::Center => {
            let left = total / 2;
            let right = total - left;
            out.extend(std::iter::repeat_n(s.fill, left));
            out.extend_from_slice(field);
            out.extend(std::iter::repeat_n(s.fill, right));
        }
        Align::None => out.extend_from_slice(field),
    }
}

/// Renders a value by the placeholder verb into `out`.
fn render_value(out: &mut Vec<u8>, verb: &[u8], arg: Option<&Value>) -> Result<(), String> {
    let Some(v) = arg else {
        out.extend_from_slice(b"<missing>");
        return Ok(());
    };
    match verb {
        b"" => render_default(out, v),
        b"s" => render_string(out, v),
        b"d" => render_decimal(out, v),
        b"c" => render_char(out, v),
        b"x" => render_radix(out, v, 16, false),
        b"X" => render_radix(out, v, 16, true),
        b"b" => render_radix(out, v, 2, false),
        b"o" => render_radix(out, v, 8, false),
        other => Err(format!(
            "unknown format verb `{{{}}}`",
            String::from_utf8_lossy(other)
        )),
    }
}

/// Renders a value in the default `{}` style, picking a representation by kind.
fn render_default(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    match v {
        Value::Int { .. } => render_decimal(out, v),
        Value::Bool(b) => {
            out.extend_from_slice(if *b { b"true" } else { b"false" });
            Ok(())
        }
        Value::Float(f) => {
            out.extend_from_slice(format!("{f}").as_bytes());
            Ok(())
        }
        Value::Str(_) | Value::Slice { .. } => render_string(out, v),
        Value::Unit => Ok(()),
        _ => {
            out.extend_from_slice(b"<value>");
            Ok(())
        }
    }
}

/// Renders a value as a UTF-8 string (a `Str`, or the bytes of a `[]const u8`).
fn render_string(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    match v {
        Value::Str(bytes) => {
            out.extend_from_slice(bytes);
            Ok(())
        }
        // A `[]const u8` slice that the VM materialized as a Str-of-bytes already
        // hits the branch above; a bare integer printed with `{s}` is a misuse the
        // checker would reject, so render its decimal form defensively.
        _ => render_decimal(out, v),
    }
}

/// Renders a value as a decimal integer (full u128/i128 magnitude).
fn render_decimal(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    match v {
        Value::Int { v, repr } => {
            if repr.signed {
                out.extend_from_slice(format!("{v}").as_bytes());
            } else {
                // Render as the unsigned magnitude so large u128 values (the
                // 384600000000000000000000000 line) print without a sign.
                out.extend_from_slice(format!("{}", *v as u128).as_bytes());
            }
            Ok(())
        }
        Value::Bool(b) => {
            out.extend_from_slice(if *b { b"1" } else { b"0" });
            Ok(())
        }
        // `{d}` is the idiomatic decimal verb and is routinely used for floats;
        // render it like the default `{}` style rather than the `<int>` fallback.
        Value::Float(f) => {
            out.extend_from_slice(format!("{f}").as_bytes());
            Ok(())
        }
        _ => {
            out.extend_from_slice(b"<int>");
            Ok(())
        }
    }
}

/// Renders an integer as a single character (its low byte).
fn render_char(out: &mut Vec<u8>, v: &Value) -> Result<(), String> {
    match v.as_i128() {
        Some(n) => {
            // Encode the code point as UTF-8 when valid, else emit the low byte.
            if let Some(ch) = u32::try_from(n).ok().and_then(char::from_u32) {
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            } else {
                out.push(n as u8);
            }
            Ok(())
        }
        None => {
            out.extend_from_slice(b"<char>");
            Ok(())
        }
    }
}

/// Renders an integer in the given radix. The magnitude is masked to the value's
/// own repr width so a negative signed value prints its two's-complement at its
/// *declared* width (`{x}` on `i8` -1 is `ff`, on `i32` -1 is `ffffffff`) rather
/// than the full 128-bit pattern.
fn render_radix(out: &mut Vec<u8>, v: &Value, radix: u32, upper: bool) -> Result<(), String> {
    let Some(n) = v.as_i128() else {
        out.extend_from_slice(b"<int>");
        return Ok(());
    };
    // Mask to the operand's width when it carries a sized repr; a width-0
    // (comptime) / >=128 repr, or a non-`Int` source, keeps the full 128 bits.
    let mut mag = match v {
        Value::Int { repr, .. } if repr.width >= 1 && repr.width < 128 => {
            (n as u128) & ((1u128 << repr.width) - 1)
        }
        _ => n as u128,
    };
    if mag == 0 {
        out.push(b'0');
        return Ok(());
    }
    let digits = b"0123456789abcdef";
    let mut tmp = Vec::new();
    while mag > 0 {
        let d = (mag % radix as u128) as usize;
        let c = digits[d];
        tmp.push(if upper { c.to_ascii_uppercase() } else { c });
        mag /= radix as u128;
    }
    tmp.reverse();
    out.extend_from_slice(&tmp);
    Ok(())
}
