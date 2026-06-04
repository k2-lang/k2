//! # trace — error-return traces (Zig-style)
//!
//! When an error propagates out of `main` in a Debug or ReleaseSafe build, the
//! VM prints an *error return trace*: the chain of source locations where the
//! error first appeared (the `return error.X` origin) and each `try` site that
//! re-propagated it, newest-first. This is exactly Zig's `error return trace`
//! and the shape required by spec §6.9, and it answers "where did this error
//! come from?" without a debugger.
//!
//! ## How it is recorded
//!
//! Both the error *origin* (`return error.X`, which lowers to a `MakeErr`) and
//! each `try` site lower (in MIR) to an error-path `Return` carrying that site's
//! span. The VM compiler turns those into an [`Instr::ReturnErr`] (only in
//! Debug/ReleaseSafe), indexing a per-function `trace_sites` table. When that
//! instruction executes, the VM pushes the site onto the *current fiber's*
//! trace buffer, then performs the ordinary return. The origin's `MakeErr`
//! clears the buffer first (a fresh error starts clean), so the origin frame is
//! the deepest seed; each `try` the error then passes through appends one frame
//! above it — yielding the newest-first chain with the origin last.
//!
//! The trace lives on the [`Fiber`](crate::sched::Fiber), so concurrent fibers
//! never cross-contaminate.
//!
//! In ReleaseFast no `ReturnErr` is ever emitted, the per-fiber buffer stays
//! empty, and the escape printer sees nothing — the trace is fully stripped.
//!
//! [`Instr::ReturnErr`]: crate::isa::Instr::ReturnErr

/// One frame of an error-return trace: a function name + source location of the
/// `try` (or origin) that recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceFrame {
    /// The display name of the function the propagation site is in.
    pub fn_name: String,
    /// 1-based source line of the site.
    pub line: u32,
    /// 1-based source column of the site.
    pub col: u32,
}

/// Formats a captured error-return trace as the lines printed after the
/// `error: <name>` header. The `file` label prefixes each location. Returns an
/// empty string when the trace is empty (ReleaseFast, or an error that never
/// propagated through a `try`).
///
/// The output mirrors Zig's shape:
///
/// ```text
/// error return trace:
///     at parseDoubled (examples/errors.k2:34:5)
///     at processAll (examples/errors.k2:163:5)
///     at main (examples/errors.k2:115:5)
/// ```
pub fn format_trace(frames: &[TraceFrame], file: &str) -> String {
    if frames.is_empty() {
        return String::new();
    }
    let mut out = String::from("error return trace:\n");
    for f in frames {
        out.push_str(&format!(
            "    at {} ({}:{}:{})\n",
            f.fn_name, file, f.line, f.col
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_trace_renders_nothing() {
        assert_eq!(format_trace(&[], "x.k2"), "");
    }

    #[test]
    fn trace_lists_frames_newest_first() {
        let frames = vec![
            TraceFrame {
                fn_name: "b".into(),
                line: 3,
                col: 5,
            },
            TraceFrame {
                fn_name: "a".into(),
                line: 9,
                col: 5,
            },
            TraceFrame {
                fn_name: "main".into(),
                line: 12,
                col: 5,
            },
        ];
        let s = format_trace(&frames, "errors.k2");
        assert!(s.starts_with("error return trace:\n"));
        assert!(s.contains("    at b (errors.k2:3:5)\n"));
        assert!(s.contains("    at a (errors.k2:9:5)\n"));
        assert!(s.contains("    at main (errors.k2:12:5)\n"));
        // Order: b before a before main.
        let bi = s.find("at b").unwrap();
        let ai = s.find("at a").unwrap();
        let mi = s.find("at main").unwrap();
        assert!(bi < ai && ai < mi);
    }
}
