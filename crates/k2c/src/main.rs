//! # k2c — the k2 compiler driver (front-end CLI)
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This binary is the entry point of the k2 toolchain front-end. It wires up
//! the [`k2_lexer`], the [`k2_parse`] parser, and the [`k2_syntax`] AST. It
//! exposes two working subcommands: `tokenize` (alias `lex`), which prints the
//! token stream, and `parse`, which prints the S-expression AST.
//!
//! Argument parsing is done by hand with `std::env::args`: no third-party CLI
//! crate, so the toolchain builds and runs fully offline.
//!
//! ## Usage
//!
//! ```text
//! k2c tokenize <file.k2>     # lex a file and print its tokens
//! k2c tokenize -             # lex from standard input
//! k2c lex <file.k2>          # `lex` is an alias for `tokenize`
//! k2c parse <file.k2>        # parse a file and print its S-expression AST
//! k2c parse -                # parse from standard input
//! k2c fmt <file.k2>          # print the canonical formatting of a file
//! k2c fmt --check <file.k2>  # exit nonzero if the file is not canonical
//! k2c fmt --write <file.k2>  # rewrite the file in place in canonical form
//! k2c ast <file.k2>          # dump the structured AST (S-expression)
//! k2c resolve <file.k2>      # resolve names/scopes and print the scope tree
//! k2c check <file.k2>        # type-check and print per-declaration signatures
//! k2c mir <file.k2>          # lower to MIR and print the dump (Debug mode)
//! k2c mir --release-fast <f> # lower with safety checks stripped
//! k2c run <file.k2>          # compile to bytecode and execute `main` (Debug mode)
//! k2c run --release-fast <f> # execute with safety checks stripped
//! k2c help                   # print usage
//! k2c version                # print the version
//! ```
//!
//! For `tokenize`, the process exits `0` on success and nonzero only on a usage
//! or I/O error (lexical *errors* are reported as `Error` tokens, not a process
//! failure). For `parse`, the process additionally exits nonzero when the
//! source contained one or more parse errors.

mod build_cmd;
mod imports;
mod lock;
mod multi;
mod render;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use std::path::Path;

use k2_codegen::Target;
use k2_fmt::format_source;
use k2_lexer::{tokenize, Token, TokenKind};
use k2_mir::{dump_mir, lower_program, BuildMode, MirProgram};
use k2_opt::{optimize, OptLevel, OptStats};
use k2_parse::{parse, to_sexpr, ParseResult};
use k2_resolve::{
    dump_resolution, dump_scopes, resolve_file, resolve_module, FileLoader, LoadError,
    ResolvedModule,
};
use k2_syntax::{Expr, Item, SourceFile, Span};
use k2_types::{check_file, dump_signatures, dump_types};
use k2_vm::{run_metered, run_program, OsInputs, RunArgs, RunOutcome};

/// Program name used in diagnostics and the usage text.
const PROG: &str = "k2c";
/// Version string, kept in sync with the crate version by hand for now.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    // Skip argv[0] (the executable path) and dispatch on the subcommand.
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            // All diagnostics go to stderr so stdout carries only real output.
            let _ = writeln!(io::stderr(), "{PROG}: error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Parses the subcommand and arguments and runs the requested action. Returns
/// the process exit code on success (a command may report a content failure,
/// such as parse errors, via a non-success code), or a human-readable error
/// string on a usage/I/O error.
fn run(args: &[String]) -> Result<ExitCode, String> {
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => {
            print_usage();
            return Ok(ExitCode::SUCCESS);
        }
    };

    match cmd {
        "tokenize" | "lex" => cmd_tokenize(rest).map(|()| ExitCode::SUCCESS),
        "parse" => cmd_parse(rest),
        "fmt" => cmd_fmt(rest),
        "ast" => cmd_ast(rest),
        "resolve" => cmd_resolve(rest),
        "check" => cmd_check(rest),
        "mir" => cmd_mir(rest),
        "run" => cmd_run(rest),
        "run-native" => cmd_run_native(rest),
        "build-native" => cmd_build_native(rest),
        "build" => build_cmd::cmd_build(rest),
        "bench" => cmd_bench(rest),
        "lsp" => cmd_lsp(rest),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(ExitCode::SUCCESS)
        }
        "version" | "--version" | "-V" => {
            println!("{PROG} {VERSION}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown subcommand `{other}` (try `{PROG} help`)")),
    }
}

/// Parses `source` together with the bundled standard library, producing a
/// single [`ParseResult`] whose [`SourceFile`] is the user's items plus one
/// synthetic `const __k2_std_root = struct { ... };` carrying the whole of
/// `std`, with every `const X = @import("std")` re-pointed at that root.
///
/// This is what makes `@import("std")` resolve to a REAL compiled module: `std`
/// becomes an ordinary type-valued `const`, so `std.heap.GeneralPurposeAllocator`
/// / `std.ArrayList(u32)` resolve to real declarations and monomorphize through
/// the normal pipeline (only the handle-based allocator floor stays intrinsic).
///
/// The std source is *appended* after the user source, so user spans — and thus
/// every user diagnostic's line/column — are byte-for-byte identical to a
/// std-free parse; the std items simply occupy the high end of the span space.
/// Semantic phases that key on the appended (still-`@import`) string never see
/// the original `@import("std")` because the AST node is rewritten in place to a
/// name reference at the very same span.
fn parse_program(source: &str) -> ParseResult {
    // One combined text: user source first (offsets preserved), then std.
    let mut combined = String::with_capacity(source.len() + k2_std::STD_BODY.len() + 64);
    combined.push_str(source);
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&k2_std::std_root_item_source());

    // The user/std boundary: any diagnostic whose span starts at/after this char
    // offset is about the *appended* std prelude, not the user's code — its raw
    // line/col and `found <std-token>` text would leak std internals into a
    // user-facing message. We re-anchor every such diagnostic to end-of-user-input.
    let boundary = source.chars().count() as u32;

    let mut result = k2_parse::parse(&combined);
    clamp_diagnostics_to_user_source(&mut result.diagnostics, source, boundary);
    rewrite_std_imports(&mut result.file);
    result
}

/// Re-anchors every diagnostic that points into the *appended* std prelude back
/// into the user's source, so a parse error at end-of-user-input never leaks std
/// line numbers or internal names ([`k2_std::STD_ROOT_NAME`]).
///
/// A diagnostic whose primary span starts at/after `boundary` arose because the
/// parser ran off the end of the user's (truncated) source into the std body. We
/// treat it as the end-of-input error it really is: clamp its span (and every
/// label's span) to the end of the user source, and rewrite any message/label
/// text that names a std token so the user sees a clean "unexpected end of input"
/// instead of `` found identifier `__k2_std_root` `` at a phantom line.
fn clamp_diagnostics_to_user_source(
    diags: &mut [k2_parse::Diagnostic],
    source: &str,
    boundary: u32,
) {
    let (end_line, end_col) = end_of_source_line_col(source);
    let end_span = Span::point(boundary, end_line, end_col);
    for d in diags.iter_mut() {
        let leaks = d.span.start >= boundary
            || d.message.contains(k2_std::STD_ROOT_NAME)
            || d.labels.iter().any(|l| l.span.start >= boundary);
        if !leaks {
            continue;
        }
        // Re-anchor the primary span to end-of-user-input.
        if d.span.start >= boundary {
            d.span = end_span;
        }
        // Drop labels that point into the std region; clamp any that straddle.
        d.labels.retain(|l| l.span.start < boundary);
        for l in &mut d.labels {
            if l.span.end > boundary {
                l.span.end = boundary;
            }
        }
        // Scrub any std-internal token name from the visible text.
        d.message = clean_eof_message(&d.message);
        scrub_std_name(&mut d.primary_label);
        for l in &mut d.labels {
            scrub_std_name(&mut l.message);
        }
        for n in &mut d.notes {
            scrub_std_name(n);
        }
        if let Some(h) = &mut d.help {
            scrub_std_name(h);
        }
    }
}

/// The 1-based `(line, col)` of the position just past the last char of `source`
/// (the end-of-input caret position): one past the final line's last column.
fn end_of_source_line_col(source: &str) -> (u32, u32) {
    let mut line = 1u32;
    let mut col = 1u32;
    for c in source.chars() {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Rewrites a parser message that ran into the std prelude into a clean
/// end-of-input message. Parser messages have the shape
/// `expected <X> <ctx>, found <token>`; the `found <token>` part now names a std
/// token, so we replace it with `found end of input`. Messages with no `, found`
/// clause that still mention the std root are replaced wholesale.
fn clean_eof_message(message: &str) -> String {
    if let Some(idx) = message.rfind(", found ") {
        let mut out = message[..idx].to_string();
        out.push_str(", found end of input");
        return out;
    }
    if message.contains(k2_std::STD_ROOT_NAME) {
        return "unexpected end of input".to_string();
    }
    message.to_string()
}

/// Replaces any occurrence of the internal std-root name with a neutral
/// `end of input` so it never surfaces in a user-facing label/note.
fn scrub_std_name(text: &mut String) {
    if text.contains(k2_std::STD_ROOT_NAME) {
        *text = text.replace(k2_std::STD_ROOT_NAME, "end of input");
    }
}

/// Re-points every user `const X = @import("std")` binding to the synthetic std
/// root, by replacing the `@import("std")` initializer expression with a bare
/// identifier reference to [`k2_std::STD_ROOT_NAME`] at the same span. Other
/// imports (`@import("build")`, path imports) are left untouched and stay
/// opaque exactly as before.
fn rewrite_std_imports(file: &mut SourceFile) {
    for item in &mut file.items {
        if let Item::Const { value, .. } = item {
            if import_target(value).as_deref() == Some("std") {
                let span = value.span();
                *value = Expr::Ident {
                    name: k2_std::STD_ROOT_NAME.to_string(),
                    span,
                };
            }
        }
    }
}

/// If `e` is exactly `@import("name")`, returns the imported `name` (with quotes
/// stripped). Returns `None` for any other expression.
fn import_target(e: &Expr) -> Option<String> {
    if let Expr::Builtin { name, args, .. } = e {
        if name == "@import" {
            if let [Expr::Str { text, .. }] = args.as_slice() {
                return Some(text.trim_matches('"').to_string());
            }
        }
    }
    None
}

/// The `tokenize` / `lex` subcommand: read source from a file path or stdin,
/// lex it, and print the token stream.
fn cmd_tokenize(args: &[String]) -> Result<(), String> {
    let path = match args {
        [] => {
            return Err("`tokenize` needs a <file.k2> argument (or `-` for stdin)".to_string());
        }
        [p] => p.as_str(),
        _ => {
            return Err(format!(
                "`tokenize` takes exactly one argument; got {}",
                args.len()
            ));
        }
    };

    let (source, label) = read_source(path)?;
    let tokens = tokenize(&source);
    print_tokens(&label, &tokens);
    Ok(())
}

/// The `parse` subcommand: read source from a file path or stdin, parse it,
/// print the S-expression AST to stdout (unless `--quiet`), and print each
/// diagnostic to stderr. Exits with a nonzero code if any error-severity
/// diagnostic was produced.
fn cmd_parse(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut quiet = false;
    let mut show_spans = false;
    for arg in args {
        match arg.as_str() {
            "--quiet" => quiet = true,
            "--sexpr" => {} // the default; accepted for explicitness
            "--spans" => show_spans = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `parse` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!(
                        "`parse` takes exactly one path; got extra `{other}`"
                    ));
                }
                path = Some(other);
            }
        }
    }
    let path =
        path.ok_or_else(|| "`parse` needs a <file.k2> argument (or `-` for stdin)".to_string())?;

    let (source, label) = read_source(path)?;
    let result = parse(&source);

    if !quiet {
        let tree = if show_spans {
            k2_parse::to_sexpr_spans(&result.file)
        } else {
            to_sexpr(&result.file)
        };
        print!("{tree}");
    }

    // Diagnostics go to stderr, rendered in the rich caret format.
    let error_count = render::emit_diags(&label, &source, &result.diagnostics);

    if quiet {
        let mut err = io::stderr().lock();
        let _ = writeln!(
            err,
            "# {} item(s), {} diagnostic(s)",
            result.file.items.len(),
            result.diagnostics.len()
        );
    }

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The `fmt` subcommand: read source from a file path or stdin, format it into
/// canonical form, and print it to stdout (default), check it (`--check`), or
/// rewrite the file in place (`--write`).
///
/// Exit codes: `0` on success / already-canonical / written; `1` on parse error,
/// a `--check` mismatch, or a usage/I/O error.
fn cmd_fmt(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut check = false;
    let mut write = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "--write" => write = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `fmt` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("`fmt` takes exactly one path; got extra `{other}`"));
                }
                path = Some(other);
            }
        }
    }
    if check && write {
        return Err("`fmt --check` and `fmt --write` are mutually exclusive".to_string());
    }
    let path =
        path.ok_or_else(|| "`fmt` needs a <file.k2> argument (or `-` for stdin)".to_string())?;
    if write && path == "-" {
        return Err("cannot `fmt --write` standard input".to_string());
    }

    let (source, label) = read_source(path)?;
    match format_source(&source) {
        Err(diags) => {
            render::emit_diags(&label, &source, &diags);
            let _ = writeln!(
                io::stderr(),
                "error: cannot format {label}: it has parse errors"
            );
            Ok(ExitCode::FAILURE)
        }
        Ok(formatted) => {
            if check {
                if formatted == source {
                    Ok(ExitCode::SUCCESS)
                } else {
                    let _ = writeln!(
                        io::stderr(),
                        "{label}: not formatted (run `{PROG} fmt --write`)"
                    );
                    Ok(ExitCode::FAILURE)
                }
            } else if write {
                if formatted == source {
                    // Already canonical: no rewrite, no mtime churn.
                    Ok(ExitCode::SUCCESS)
                } else {
                    fs::write(path, &formatted).map_err(|e| format!("writing `{path}`: {e}"))?;
                    let _ = writeln!(io::stderr(), "{label}: formatted");
                    Ok(ExitCode::SUCCESS)
                }
            } else {
                print!("{formatted}");
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

/// The `ast` subcommand: parse the source and dump the structured AST as an
/// S-expression (the documented front door for tooling). `--spans` annotates
/// each node with its `@start..end` span. Diagnostics go to stderr; the process
/// exits nonzero if the source had any parse error.
fn cmd_ast(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut show_spans = false;
    for arg in args {
        match arg.as_str() {
            "--spans" => show_spans = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `ast` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("`ast` takes exactly one path; got extra `{other}`"));
                }
                path = Some(other);
            }
        }
    }
    let path =
        path.ok_or_else(|| "`ast` needs a <file.k2> argument (or `-` for stdin)".to_string())?;

    let (source, label) = read_source(path)?;
    let result = parse(&source);

    let tree = if show_spans {
        k2_parse::to_sexpr_spans(&result.file)
    } else {
        to_sexpr(&result.file)
    };
    print!("{tree}");

    let error_count = render::emit_diags(&label, &source, &result.diagnostics);

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The `resolve` subcommand: parse the source, run name resolution, print each
/// resolution diagnostic to stderr, and on success print a resolution summary
/// (the scope tree by default, or the full uses table with `--uses`) to stdout.
///
/// Resolution requires a well-formed tree, so any *parse* error gates it: the
/// parse diagnostics are printed and the process exits nonzero without resolving
/// (mirroring `fmt`). With `--modules`, the multi-file module graph is built from
/// the file on disk via a real filesystem loader, reporting import cycles and
/// missing files across the project; `--modules` cannot read from stdin.
///
/// Exits `SUCCESS` iff there were zero resolution (and parse) errors.
fn cmd_resolve(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut show_uses = false;
    let mut quiet = false;
    let mut modules = false;
    for arg in args {
        match arg.as_str() {
            "--uses" => show_uses = true,
            "--scopes" => {} // the default; accepted for explicitness
            "--quiet" => quiet = true,
            "--modules" => modules = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `resolve` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!(
                        "`resolve` takes exactly one path; got extra `{other}`"
                    ));
                }
                path = Some(other);
            }
        }
    }
    let path =
        path.ok_or_else(|| "`resolve` needs a <file.k2> argument (or `-` for stdin)".to_string())?;
    if modules && path == "-" {
        return Err("cannot `resolve --modules` standard input".to_string());
    }

    let (source, label) = read_source(path)?;
    let pres = parse(&source);

    // Parse errors gate resolution: report them and stop, like `fmt`.
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot resolve {label}: it has parse errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Resolve, either single-file or across the module graph.
    if modules {
        let rm = resolve_module(Path::new(path), &FsFileLoader);
        finish_modules(&label, &source, &rm, show_uses, quiet)
    } else {
        let r = resolve_file(&pres.file);
        finish_single(&label, &source, &r, show_uses, quiet)
    }
}

/// Prints diagnostics + summary for a single-file resolution and returns the
/// exit code.
fn finish_single(
    label: &str,
    source: &str,
    r: &k2_resolve::Resolved,
    show_uses: bool,
    quiet: bool,
) -> Result<ExitCode, String> {
    let error_count = render::emit_diags(label, source, &r.diagnostics);

    if error_count == 0 && !quiet {
        let dump = if show_uses {
            dump_resolution(r)
        } else {
            dump_scopes(r)
        };
        print!("{dump}");
    }
    if quiet {
        let _ = writeln!(
            io::stderr(),
            "# {} def(s), {} scope(s), {} diagnostic(s)",
            r.defs.len(),
            r.scopes.len(),
            r.diagnostics.len()
        );
    }

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// Prints diagnostics + summary for a multi-file module resolution.
///
/// Graph-level diagnostics (missing files, import cycles) are rendered against
/// the root source; each module's own diagnostics are rendered against *that*
/// module's source, re-read from disk so the caret lands in the right file.
fn finish_modules(
    label: &str,
    source: &str,
    rm: &ResolvedModule,
    show_uses: bool,
    quiet: bool,
) -> Result<ExitCode, String> {
    let mut error_count = 0usize;
    // Graph-level diagnostics first, then each module's own.
    error_count += render::emit_diags(label, source, &rm.diagnostics);
    for m in &rm.modules {
        let mod_label = m.path.display().to_string();
        let mod_src = fs::read_to_string(&m.path).unwrap_or_default();
        error_count += render::emit_diags(&mod_label, &mod_src, &m.resolved.diagnostics);
    }

    if error_count == 0 && !quiet {
        if let Some(root) = rm.root() {
            let dump = if show_uses {
                dump_resolution(root)
            } else {
                dump_scopes(root)
            };
            print!("{dump}");
        }
    }
    if quiet {
        let _ = writeln!(
            io::stderr(),
            "# {} module(s), {} graph diagnostic(s)",
            rm.modules.len(),
            rm.diagnostics.len()
        );
    }

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The `check` subcommand: parse the source, resolve names, type-check, print
/// each type diagnostic to stderr, and on success print a per-decl signature
/// dump (the default), the full per-occurrence type dump (`--uses`), or only a
/// one-line summary (`--quiet`) to stdout.
///
/// Both parse errors and resolution errors gate type-checking: they are printed
/// and the process exits nonzero without checking (mirroring `resolve`). Exits
/// `SUCCESS` iff there were zero parse, resolution, and type errors.
fn cmd_check(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut show_uses = false;
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "--uses" => show_uses = true,
            "--signatures" => {} // the default; accepted for explicitness
            "--quiet" => quiet = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `check` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!(
                        "`check` takes exactly one path; got extra `{other}`"
                    ));
                }
                path = Some(other);
            }
        }
    }
    let path =
        path.ok_or_else(|| "`check` needs a <file.k2> argument (or `-` for stdin)".to_string())?;

    let (source, label) = read_source(path)?;
    let pres = parse_program(&source);

    // Parse errors gate type-checking: report them and stop, like `resolve`.
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot check {label}: it has parse errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors also gate type-checking.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        render::emit_errors(&label, &source, &resolved.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot check {label}: it has resolution errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    let typed = check_file(&pres.file, &resolved);

    let error_count = render::emit_diags(&label, &source, &typed.diagnostics);

    if error_count == 0 && !quiet {
        let dump = if show_uses {
            dump_types(&typed, &resolved)
        } else {
            dump_signatures(&typed, &resolved)
        };
        print!("{dump}");
    }
    if quiet {
        let _ = writeln!(
            io::stderr(),
            "# {} decl(s), {} type(s), {} diagnostic(s)",
            typed.binding_types.len(),
            typed.types.len(),
            typed.diagnostics.len()
        );
    }

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The `mir` subcommand: parse the source, resolve names, type-check, then lower
/// to MIR under a chosen build mode and print a readable MIR dump. Parse,
/// resolution, and type errors each gate lowering (printed, then a nonzero exit,
/// mirroring `check`). Build mode is selected with `--release-fast` (drop safety
/// checks), `--release-safe`, or `--debug` (the default, checks on). Lowering and
/// leak diagnostics are printed to stderr; the dump goes to stdout (unless
/// `--quiet`, which prints only a one-line summary). Exits nonzero if any
/// error-severity diagnostic (parse/resolve/type/lower/leak) was produced.
/// Maps a build mode (plus an optional `--opt` override for Debug) to the
/// optimizer level the driver should apply. ReleaseSafe optimizes but keeps
/// non-redundant checks; ReleaseFast optimizes with checks already stripped at
/// lowering; Debug is unoptimized unless `--opt` is passed (which, since Debug
/// retains its checks, runs the *Safe* pipeline so those checks are preserved).
fn opt_level_for(mode: BuildMode, opt_flag: bool) -> OptLevel {
    match mode {
        BuildMode::Debug => {
            if opt_flag {
                OptLevel::Safe
            } else {
                OptLevel::None
            }
        }
        BuildMode::ReleaseSafe => OptLevel::Safe,
        BuildMode::ReleaseFast => OptLevel::Fast,
    }
}

/// Applies the optimizer to `prog` under `mode` (+ optional `--opt`), returning
/// the stats. Verifies the result is still well-formed; a malformed post-opt MIR
/// is an internal bug surfaced as an error rather than executed.
fn run_optimizer(
    prog: &mut MirProgram,
    mode: BuildMode,
    opt_flag: bool,
) -> Result<OptStats, String> {
    let level = opt_level_for(mode, opt_flag);
    let stats = optimize(prog, level);
    let problems = prog.verify();
    if !problems.is_empty() {
        let mut msg = String::from("optimizer produced malformed MIR:");
        for p in &problems {
            msg.push_str("\n  ");
            msg.push_str(&p.message);
        }
        return Err(msg);
    }
    Ok(stats)
}

fn cmd_mir(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut quiet = false;
    let mut mode = BuildMode::Debug;
    let mut opt_flag = false;
    let mut opt_report = false;
    for arg in args {
        match arg.as_str() {
            "--release-fast" => mode = BuildMode::ReleaseFast,
            "--release-safe" => mode = BuildMode::ReleaseSafe,
            "--debug" => mode = BuildMode::Debug,
            "--opt" => opt_flag = true,
            "--opt-report" => opt_report = true,
            "--quiet" => quiet = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `mir` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("`mir` takes exactly one path; got extra `{other}`"));
                }
                path = Some(other);
            }
        }
    }
    let path =
        path.ok_or_else(|| "`mir` needs a <file.k2> argument (or `-` for stdin)".to_string())?;

    let (source, label) = read_source(path)?;
    let pres = parse_program(&source);

    // Parse errors gate lowering: report them and stop, like `check`.
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot lower {label}: it has parse errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors also gate lowering.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        render::emit_errors(&label, &source, &resolved.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot lower {label}: it has resolution errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Type errors also gate lowering.
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        render::emit_errors(&label, &source, &typed.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot lower {label}: it has type errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Lower to MIR.
    let mut prog = match lower_program(&pres.file, &resolved, typed, mode) {
        Ok(p) => p,
        Err(diags) => {
            render::emit_errors(&label, &source, &diags);
            let _ = writeln!(io::stderr(), "error: cannot lower {label}: lowering failed");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Report lowering + leak diagnostics to stderr.
    let error_count = render::emit_diags(&label, &source, &prog.diagnostics);

    // Apply the optimizer once the front-end is clean. Debug is unoptimized
    // unless `--opt` is passed; ReleaseSafe/ReleaseFast always optimize.
    if error_count == 0 {
        let stats = run_optimizer(&mut prog, mode, opt_flag)?;
        if opt_report {
            let _ = writeln!(io::stderr(), "# opt: {stats:?}");
        }
    }

    if error_count == 0 && !quiet {
        print!("{}", dump_mir(&prog));
    }
    if quiet {
        let _ = writeln!(
            io::stderr(),
            "# {} fn(s), {} block(s), {} check(s), {} diag(s)",
            prog.funcs.len(),
            prog.block_count(),
            prog.check_count(),
            prog.diagnostics.len()
        );
    }

    if error_count == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// The `run` subcommand: parse, resolve, type-check, lower to MIR, compile, and
/// execute `main(sys)` on the bytecode VM. The front-end gating mirrors `mir`
/// (parse/resolve/type/lower errors are printed to stderr and gate execution),
/// then the program runs and its exit code is propagated to the process.
///
/// Build mode is selected with `--release-fast` (drop safety checks),
/// `--release-safe`, or `--debug` (the default, checks on). The default `Debug`
/// mode means a runtime safety violation (index OOB, integer overflow,
/// division by zero) traps as a clean panic — a `panic:` line on stderr and a
/// nonzero exit — never an uncontrolled Rust panic.
fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
    let mut path: Option<&str> = None;
    let mut mode = BuildMode::Debug;
    let mut opt_flag = false;
    let mut opt_report = false;
    // Arguments after the path are the program's own argv (read by `sys.os.args`).
    let mut forwarded: Vec<String> = Vec::new();
    // The v0.23 OS inputs: the real-env / real-pid opt-ins and a scripted env map.
    // Defaults keep the run offline-absent and deterministic.
    let mut os = OsInputs::default();
    let mut seen_path = false;
    for arg in args {
        if seen_path {
            forwarded.push(arg.clone());
            continue;
        }
        let a = arg.as_str();
        if let Some(kv) = a.strip_prefix("--env=") {
            // `--env=KEY=VALUE`: inject a single scripted env var (deterministic).
            if let Some((k, v)) = kv.split_once('=') {
                os.env.push((k.to_string(), v.to_string()));
            } else {
                return Err("`--env` expects `KEY=VALUE`".to_string());
            }
            continue;
        }
        match a {
            "--release-fast" => mode = BuildMode::ReleaseFast,
            "--release-safe" => mode = BuildMode::ReleaseSafe,
            "--debug" => mode = BuildMode::Debug,
            "--opt" => opt_flag = true,
            "--opt-report" => opt_report = true,
            // Real OS opt-ins. Without these, `sys.env.get` is offline-absent (or
            // reads only the scripted `--env` map) and `sys.os.getpid()` is a
            // deterministic `1`, so reproducibility is the default.
            "--real-env" => os.env_host = true,
            "--real-pid" => os.real_pid = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `run` flag `{other}`"));
            }
            other => {
                path = Some(other);
                seen_path = true;
            }
        }
    }
    // A leading `--` separates k2c flags from the program's argv; drop it so the
    // program sees only its own args (`k2c run prog.k2 -- a b` -> argv `[a, b]`).
    if forwarded.first().map(String::as_str) == Some("--") {
        forwarded.remove(0);
    }
    let path =
        path.ok_or_else(|| "`run` needs a <file.k2> argument (or `-` for stdin)".to_string())?;

    let (source, label) = read_source(path)?;

    // Multi-file fast path: a program with `@import("./x.k2")` path imports
    // compiles as a merged module graph (path imports + named modules resolve,
    // type-check, monomorphize, lower, and run as one program). A program with
    // only named imports (`std`, …) takes the existing single-file path below,
    // whose spans/diagnostics are byte-identical to before.
    if multi::has_path_imports(&source) {
        if path == "-" {
            // A relative `@import("./...")` has no anchor when the source comes
            // from stdin (there is no file directory to resolve against), so the
            // multi-file merge cannot run. Reject it at COMPILE time with a clear
            // message instead of letting the unresolved import reach the VM as an
            // `unsupported intrinsic module.VERSION` runtime panic.
            let _ = writeln!(
                io::stderr(),
                "{label}: error: cannot resolve a relative `@import(\"./...\")` from stdin; \
                 pass a file path so the build root is known"
            );
            return Ok(ExitCode::FAILURE);
        }
        return run_multi_file(path, &label, mode, opt_flag, opt_report, forwarded, os);
    }

    let pres = parse_program(&source);

    // Parse errors gate execution: report them and stop, like `mir`.
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot run {label}: it has parse errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors gate execution.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        render::emit_errors(&label, &source, &resolved.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot run {label}: it has resolution errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Type errors gate execution.
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        render::emit_errors(&label, &source, &typed.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot run {label}: it has type errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Lower to MIR under the chosen build mode.
    let mut prog = match lower_program(&pres.file, &resolved, typed, mode) {
        Ok(p) => p,
        Err(diags) => {
            render::emit_errors(&label, &source, &diags);
            let _ = writeln!(io::stderr(), "error: cannot run {label}: lowering failed");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Error-severity lowering/leak diagnostics gate execution; warnings are
    // printed but do not stop the run.
    let error_count = render::emit_diags(&label, &source, &prog.diagnostics);
    if error_count > 0 {
        let _ = writeln!(
            io::stderr(),
            "error: cannot run {label}: lowering had errors"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Apply the optimizer: ReleaseSafe keeps non-redundant checks, ReleaseFast is
    // pure speed, Debug is unoptimized unless `--opt`. The optimizer must not
    // change observable behavior — that property is guarded by the differential
    // test corpus in `k2-opt`.
    let stats = run_optimizer(&mut prog, mode, opt_flag)?;
    if opt_report {
        let _ = writeln!(io::stderr(), "# opt: {stats:?}");
    }

    // Verify well-formedness before execution (debug guard); a malformed MIR is
    // an internal bug, reported rather than executed.
    let problems = prog.verify();
    if !problems.is_empty() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for p in &problems {
            let _ = writeln!(err, "error: malformed MIR: {}", p.message);
        }
        return Ok(ExitCode::FAILURE);
    }

    // Execute `main`; the VM streams program output and propagates the exit code.
    // The label is threaded so an escaping error's return trace points at real
    // source locations.
    Ok(run_program(
        &prog,
        RunArgs {
            mode,
            argv: forwarded,
            os,
            trace_label: Some(label.clone()),
        },
    ))
}

/// Compiles + runs a multi-file program: a root `.k2` with `@import("./x.k2")`
/// path imports. The module graph is merged into one program (the std-injection
/// mechanism generalized), then lowered, optimized, and executed exactly like a
/// single-file run. The build root is the root file's own directory, so path
/// imports resolve relative to it.
fn run_multi_file(
    path: &str,
    label: &str,
    mode: BuildMode,
    opt_flag: bool,
    opt_report: bool,
    forwarded: Vec<String>,
    os: OsInputs,
) -> Result<ExitCode, String> {
    let root = Path::new(path);
    let build_root = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let merged = multi::merge(&multi::CompileInputs {
        root_source: root.to_path_buf(),
        build_root,
        named_modules: Vec::new(),
        build_options: Vec::new(),
        inject_build: false,
    })
    .map_err(|d| format!("{}: {}", d.label, d.message))?;

    let mut prog = match multi::compile_merged(&merged.source, label, mode) {
        Ok(p) => p,
        Err(diags) => {
            let stderr = io::stderr();
            let mut err = stderr.lock();
            for d in &diags {
                let _ = writeln!(err, "{}: {}", d.label, d.message);
            }
            let _ = writeln!(err, "error: cannot run {label}: front-end errors");
            return Ok(ExitCode::FAILURE);
        }
    };

    let stats = run_optimizer(&mut prog, mode, opt_flag)?;
    if opt_report {
        let _ = writeln!(io::stderr(), "# opt: {stats:?}");
    }
    let problems = prog.verify();
    if !problems.is_empty() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for p in &problems {
            let _ = writeln!(err, "error: malformed MIR: {}", p.message);
        }
        return Ok(ExitCode::FAILURE);
    }
    Ok(run_program(
        &prog,
        RunArgs {
            mode,
            argv: forwarded,
            os,
            trace_label: Some(label.to_string()),
        },
    ))
}

/// Runs the shared single-file front-end pipeline (parse -> resolve -> check ->
/// lower -> optimize -> verify) and returns the verified [`MirProgram`], or an
/// `Err(ExitCode)` after printing the front-end diagnostics. This is the exact
/// pipeline `cmd_run` uses, factored so the native subcommands compile to the
/// same verified MIR the VM path runs — guaranteeing the differential property.
///
/// Multi-file (`@import("./...")`) programs are out of the native subset for now
/// (the VM path via `k2c run` handles them); a path-import program is rejected
/// here with a clear message.
fn front_end_to_mir(path: &str, mode: BuildMode, opt_flag: bool) -> Result<MirProgram, ExitCode> {
    let (source, label) = read_source(path).map_err(|e| {
        let _ = writeln!(io::stderr(), "{PROG}: error: {e}");
        ExitCode::FAILURE
    })?;

    if multi::has_path_imports(&source) {
        let _ = writeln!(
            io::stderr(),
            "{label}: error: the native backend does not yet support `@import(\"./...\")` \
             multi-file programs; use `{PROG} run` (VM) for this program"
        );
        return Err(ExitCode::FAILURE);
    }

    let pres = parse_program(&source);
    if !pres.is_ok() {
        render::emit_errors(&label, &source, &pres.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot compile {label}: it has parse errors"
        );
        return Err(ExitCode::FAILURE);
    }

    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        render::emit_errors(&label, &source, &resolved.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot compile {label}: it has resolution errors"
        );
        return Err(ExitCode::FAILURE);
    }

    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        render::emit_errors(&label, &source, &typed.diagnostics);
        let _ = writeln!(
            io::stderr(),
            "error: cannot compile {label}: it has type errors"
        );
        return Err(ExitCode::FAILURE);
    }

    let mut prog = match lower_program(&pres.file, &resolved, typed, mode) {
        Ok(p) => p,
        Err(diags) => {
            render::emit_errors(&label, &source, &diags);
            let _ = writeln!(
                io::stderr(),
                "error: cannot compile {label}: lowering failed"
            );
            return Err(ExitCode::FAILURE);
        }
    };

    let error_count = render::emit_diags(&label, &source, &prog.diagnostics);
    if error_count > 0 {
        let _ = writeln!(
            io::stderr(),
            "error: cannot compile {label}: lowering had errors"
        );
        return Err(ExitCode::FAILURE);
    }

    if let Err(e) = run_optimizer(&mut prog, mode, opt_flag) {
        let _ = writeln!(io::stderr(), "{PROG}: error: {e}");
        return Err(ExitCode::FAILURE);
    }

    let problems = prog.verify();
    if !problems.is_empty() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for p in &problems {
            let _ = writeln!(err, "error: malformed MIR: {}", p.message);
        }
        return Err(ExitCode::FAILURE);
    }

    Ok(prog)
}

/// The parsed shared flags for `run-native`/`build-native`.
struct NativeFlags<'a> {
    /// The source path (or `-` for stdin).
    path: &'a str,
    /// The build mode (Debug / ReleaseSafe / ReleaseFast).
    mode: BuildMode,
    /// Whether the MIR optimizer was explicitly requested (`--opt`).
    opt_flag: bool,
    /// The selected native target.
    target: Target,
    /// Whether to emit an object + link it with libc via the system `cc`.
    link_libc: bool,
    /// The args after the path (forwarded argv / output options).
    rest: Vec<&'a String>,
}

/// Parses the `path` + `mode` + `target` + `--link-libc` flags shared by
/// `run-native`/`build-native` into a [`NativeFlags`]. The first non-flag token is
/// the source path; everything after it is forwarded.
///
/// The target is selected with `--target=<triple>` (or its `-Dtarget=<triple>`
/// alias); the default is `x86_64-linux`. Supported triples are documented in the
/// usage banner and `docs/aarch64.md`. `--link-libc` selects the v0.19 C-interop
/// object-emit + system-`cc` link path.
fn parse_native_flags<'a>(args: &'a [String], cmd: &str) -> Result<NativeFlags<'a>, String> {
    let mut path: Option<&str> = None;
    let mut mode = BuildMode::Debug;
    let mut opt_flag = false;
    let mut target = Target::default();
    let mut link_libc = false;
    let mut rest: Vec<&String> = Vec::new();
    let mut seen_path = false;
    for arg in args {
        if seen_path {
            rest.push(arg);
            continue;
        }
        let a = arg.as_str();
        if let Some(triple) = a
            .strip_prefix("--target=")
            .or_else(|| a.strip_prefix("-Dtarget="))
        {
            target = Target::parse_triple(triple)?;
            continue;
        }
        match a {
            "--release-fast" => mode = BuildMode::ReleaseFast,
            "--release-safe" => mode = BuildMode::ReleaseSafe,
            "--debug" => mode = BuildMode::Debug,
            "--opt" => opt_flag = true,
            "--link-libc" => link_libc = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `{cmd}` flag `{other}`"));
            }
            other => {
                path = Some(other);
                seen_path = true;
            }
        }
    }
    let path =
        path.ok_or_else(|| format!("`{cmd}` needs a <file.k2> argument (or `-` for stdin)"))?;
    Ok(NativeFlags {
        path,
        mode,
        opt_flag,
        target,
        link_libc,
        rest,
    })
}

/// Locates a usable C compiler to drive the link step (`--link-libc`). Probes
/// `$CC` if set, then `cc`, then `gcc`, returning the first whose `--version` runs.
/// Returns `None` when no C toolchain is present — the caller then reports an
/// actionable error (build-native) or skips cleanly (the FFI tests).
///
/// NOTE: `--link-libc` is the only place the k2 toolchain shells out to an
/// external program. The compiler itself stays pure-std; linking uses the system
/// `cc`/`gcc` as the link driver — exactly as `rustc` invokes the platform linker —
/// so crt startup + libc are pulled in correctly.
fn find_cc() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(cc) = env::var("CC") {
        if !cc.is_empty() {
            candidates.push(cc);
        }
    }
    candidates.push("cc".to_string());
    candidates.push("gcc".to_string());
    for cand in candidates {
        let ok = std::process::Command::new(&cand)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(cand);
        }
    }
    None
}

/// Links a relocatable object into a dynamic executable by invoking the system
/// `cc`/`gcc` as the link driver. Uses `-no-pie` so the object's absolute
/// `.rodata` relocations (`R_X86_64_64`) resolve correctly. On failure, returns
/// the driver's captured stderr so the user sees the real linker error.
fn link_object_with_cc(
    cc: &str,
    obj_path: &Path,
    out_path: &Path,
    extra_inputs: &[String],
) -> Result<(), String> {
    let mut cmd = std::process::Command::new(cc);
    cmd.arg("-no-pie")
        .arg("-o")
        .arg(out_path)
        .arg(obj_path)
        .args(extra_inputs);
    let output = cmd
        .output()
        .map_err(|e| format!("could not run the link driver `{cc}`: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "the system linker (`{cc}`) failed to link the object:\n{stderr}"
        ))
    }
}

/// The `build-native` subcommand: compile a k2 program to a static x86-64 Linux
/// ELF and write it to disk (default output: the input stem). The emitted file
/// is `chmod +x`-ed and runs directly with no dynamic linker.
///
/// Usage:
///   k2c build-native <file.k2> [-o <out>] [--release-fast|--release-safe|--debug]
fn cmd_build_native(args: &[String]) -> Result<ExitCode, String> {
    // Split off `-o <out>` before the shared flag parse (it is build-only).
    let (out_path, rest) = take_output_flag(args)?;
    let flags = parse_native_flags(&rest, "build-native")?;
    let (path, target) = (flags.path, flags.target);

    let prog = match front_end_to_mir(path, flags.mode, flags.opt_flag) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };

    // ---- C-interop path: emit a relocatable object and link it with libc. ----
    if flags.link_libc {
        let out = out_path.unwrap_or_else(|| default_native_output(path));
        return build_native_libc(&prog, path, target, &out);
    }

    let img = match k2_codegen::compile_program_to_elf_for(&prog, target) {
        Ok(img) => img,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "error: native backend ({}): {e}\nnote: this program is outside the {} native \
                 subset; run it on the VM with `{PROG} run {path}`",
                target.triple(),
                target.triple()
            );
            return Ok(ExitCode::FAILURE);
        }
    };

    let out = out_path.unwrap_or_else(|| default_native_output(path));
    if let Err(e) = fs::write(&out, &img.bytes) {
        return Err(format!("could not write `{out}`: {e}"));
    }
    set_executable(&out);
    // aarch64 binaries are cross-compiled + structurally validated here, never
    // executed (no emulator); say so plainly so nobody mistakes a successful build
    // for a successful run.
    let note = if target == Target::Aarch64Linux {
        " [aarch64: cross-compiled + structurally validated; not executed here]"
    } else {
        ""
    };
    let _ = writeln!(
        io::stderr(),
        "wrote {} ({} bytes, {}){}",
        out,
        img.bytes.len(),
        target.triple(),
        note
    );
    Ok(ExitCode::SUCCESS)
}

/// The `build-native --link-libc` path: emit a relocatable object from `prog`,
/// write it next to the output, and link it into a dynamic executable with the
/// system `cc` (`-no-pie`). Documents that linking shells out to the system C
/// compiler. Returns FAILURE (not a panic) on a missing toolchain or a link error.
fn build_native_libc(
    prog: &MirProgram,
    path: &str,
    target: Target,
    out: &str,
) -> Result<ExitCode, String> {
    if target != Target::default() {
        return Err(format!(
            "`--link-libc` is x86-64-linux only; `{}` is not supported",
            target.triple()
        ));
    }
    let obj = match k2_codegen::compile_program_to_object(prog, target) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "error: native backend ({}): {e}\nnote: this program is outside the FFI/libc \
                 native subset; run it on the VM with `{PROG} run {path}`",
                target.triple()
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    let Some(cc) = find_cc() else {
        let _ = writeln!(
            io::stderr(),
            "error: `--link-libc` needs a system C compiler (`cc`/`gcc`) to link against libc, \
             but none was found on PATH (set $CC, or install a C toolchain)"
        );
        return Ok(ExitCode::FAILURE);
    };
    // Write the object beside the output, then link it.
    let obj_path = std::path::PathBuf::from(format!("{out}.o"));
    if let Err(e) = fs::write(&obj_path, &obj.bytes) {
        return Err(format!("could not write `{}`: {e}", obj_path.display()));
    }
    let result = link_object_with_cc(&cc, &obj_path, Path::new(out), &[]);
    let _ = fs::remove_file(&obj_path);
    match result {
        Ok(()) => {
            set_executable(out);
            let _ = writeln!(
                io::stderr(),
                "wrote {out} (linked with libc via `{cc} -no-pie`, {})",
                target.triple()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            let _ = writeln!(io::stderr(), "error: {e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// The `run-native --link-libc` path: emit an object, link it with libc via the
/// system `cc` into a temp executable, run it (inheriting stdio so libc's `puts`
/// output appears on the driver's stdout), and propagate the child's exit code.
fn run_native_libc(
    prog: &MirProgram,
    path: &str,
    target: Target,
    forwarded: &[&String],
) -> Result<ExitCode, String> {
    if target != Target::default() {
        return Err(format!(
            "`--link-libc` is x86-64-linux only; `{}` is not supported",
            target.triple()
        ));
    }
    let obj = match k2_codegen::compile_program_to_object(prog, target) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "error: native backend: {e}\nnote: this program is outside the FFI/libc native \
                 subset; run it on the VM with `{PROG} run {path}`"
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    let Some(cc) = find_cc() else {
        let _ = writeln!(
            io::stderr(),
            "error: `--link-libc` needs a system C compiler (`cc`/`gcc`) to link against libc, \
             but none was found on PATH (set $CC, or install a C toolchain)"
        );
        return Ok(ExitCode::FAILURE);
    };
    let obj_path = native_temp_path().with_extension("o");
    let exe_path = native_temp_path();
    if let Err(e) = fs::write(&obj_path, &obj.bytes) {
        return Err(format!("could not write temp object: {e}"));
    }
    let link = link_object_with_cc(&cc, &obj_path, &exe_path, &[]);
    let _ = fs::remove_file(&obj_path);
    if let Err(e) = link {
        let _ = writeln!(io::stderr(), "error: {e}");
        let _ = fs::remove_file(&exe_path);
        return Ok(ExitCode::FAILURE);
    }
    set_executable(&exe_path);
    let status = std::process::Command::new(&exe_path)
        .args(forwarded.iter().map(|s| s.as_str()))
        .status();
    let _ = fs::remove_file(&exe_path);
    match status {
        Ok(st) => {
            let code = native_exit_code(&st);
            Ok(ExitCode::from((code & 0xff) as u8))
        }
        Err(e) => Err(format!("could not execute linked binary: {e}")),
    }
}

/// The `run-native` subcommand: compile a k2 program to a temporary ELF, execute
/// it directly, and propagate its exit code. Inherits stdio so the program's
/// output and exit status are the driver's.
///
/// Usage:
///   k2c run-native <file.k2> [flags] [-- argv...]
fn cmd_run_native(args: &[String]) -> Result<ExitCode, String> {
    let flags = parse_native_flags(args, "run-native")?;
    let (path, mode, opt_flag, target, link_libc, forwarded) = (
        flags.path,
        flags.mode,
        flags.opt_flag,
        flags.target,
        flags.link_libc,
        flags.rest,
    );

    // `run-native` executes the emitted binary; refuse a non-host target up front
    // (a foreign-ISA binary cannot be executed here — there is no emulator).
    if !target.is_host() {
        let _ = writeln!(
            io::stderr(),
            "error: cannot execute a {} binary on this host; use \
             `{PROG} build-native --target={} {path} -o <out>` to cross-compile it for \
             transfer to a {} target (it is structurally validated but not run here)",
            target.triple(),
            target.triple(),
            target.triple()
        );
        return Ok(ExitCode::FAILURE);
    }

    let prog = match front_end_to_mir(path, mode, opt_flag) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };

    // ---- C-interop path: object -> cc-link -> run, propagating the exit code. ----
    if link_libc {
        return run_native_libc(&prog, path, target, &forwarded);
    }

    let img = match k2_codegen::compile_program_to_elf_for(&prog, target) {
        Ok(img) => img,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "error: native backend: {e}\nnote: this program is outside the v0.14 native \
                 subset; run it on the VM with `{PROG} run {path}`"
            );
            return Ok(ExitCode::FAILURE);
        }
    };

    // `run-native` executes the emitted binary, which only works on x86-64 Linux.
    // On other hosts the ELF is still valid; tell the user to use `build-native`.
    if !(cfg!(target_arch = "x86_64") && cfg!(target_os = "linux")) {
        let _ = writeln!(
            io::stderr(),
            "error: `run-native` requires an x86_64 Linux host to execute the emitted ELF; \
             use `{PROG} build-native {path}` to write it for transfer to a target"
        );
        return Ok(ExitCode::FAILURE);
    }

    // Write to a unique temp file, make it executable, run it, propagate the
    // child's exit code, and clean up.
    let tmp = native_temp_path();
    if let Err(e) = fs::write(&tmp, &img.bytes) {
        return Err(format!("could not write temp executable: {e}"));
    }
    set_executable(&tmp);

    // A leading `--` separates k2c flags from the program's argv; drop it so the
    // child process sees only its own args on the stack (`_start` reads argv there).
    let forwarded_args: Vec<&String> = match forwarded.split_first() {
        Some((first, rest)) if first.as_str() == "--" => rest.to_vec(),
        _ => forwarded,
    };
    let status = std::process::Command::new(&tmp)
        .args(forwarded_args.iter().map(|s| s.as_str()))
        .status();
    let _ = fs::remove_file(&tmp);

    match status {
        Ok(st) => {
            // A clean child exit propagates its exit code. A child killed by a
            // signal (e.g. SIGSEGV from native stack exhaustion on deep recursion,
            // or SIGFPE) has no exit code; report it as a shell does — `128 + signo`
            // — so a SIGSEGV (139) is *distinguishable* from a real k2 panic-trap
            // (exit 134), which the emitted binary produces by an explicit
            // `exit(134)` after printing its `panic:` line. Conflating the two
            // would mask a genuine native crash as an ordinary trap.
            let code = native_exit_code(&st);
            Ok(ExitCode::from((code & 0xff) as u8))
        }
        Err(e) => Err(format!("could not execute emitted binary: {e}")),
    }
}

/// Maps a finished child's [`ExitStatus`] to a process exit code. A normal exit
/// yields its code; a signal death yields `128 + signo` (the shell convention),
/// so a signal-killed child (e.g. SIGSEGV = 139) is not silently reported as a
/// k2 panic-trap (134). Falls back to `134` only if neither is available (which
/// `wait(2)` never reports on Linux).
#[cfg(unix)]
fn native_exit_code(st: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = st.code() {
        code
    } else if let Some(signo) = st.signal() {
        128 + signo
    } else {
        134
    }
}

/// Non-Unix fallback (the executing path is gated to x86_64 Linux, so this is
/// never reached at run time; it keeps the function total for other targets).
#[cfg(not(unix))]
fn native_exit_code(st: &std::process::ExitStatus) -> i32 {
    st.code().unwrap_or(134)
}

/// Splits an optional `-o <out>` pair out of `args`, returning the output path
/// (if given) and the remaining arguments. Used by `build-native`.
fn take_output_flag(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut out: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "`-o` needs an output path".to_string())?;
                out = Some(v.clone());
                i += 2;
            }
            other => {
                rest.push(other.to_string());
                i += 1;
            }
        }
    }
    Ok((out, rest))
}

/// The default native output path for `build-native`: the input file's stem
/// (e.g. `compute.k2` -> `compute`), or `a.out` for stdin / a stem-less path.
fn default_native_output(input: &str) -> String {
    if input == "-" {
        return "a.out".to_string();
    }
    Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "a.out".to_string())
}

/// A unique temp path for a `run-native` executable, keyed by pid + a nanosecond
/// timestamp so concurrent runs do not collide.
fn native_temp_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("k2c-native-{}-{}", std::process::id(), nanos))
}

/// Marks a file executable (`0o755`) on unix; a no-op with a warning elsewhere.
fn set_executable(path: impl AsRef<Path>) {
    let path = path.as_ref();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = writeln!(
            io::stderr(),
            "warning: cannot set the executable bit on this platform; chmod +x the file manually"
        );
    }
}

/// The result of one benchmark: its executed-instruction counts under each mode.
struct BenchResult {
    name: String,
    debug: u64,
    fast: u64,
    safe: u64,
}

/// The `bench` subcommand: a reproducible benchmark harness over the committed
/// `bench/*.k2` programs. For each program it lowers under Debug, ReleaseFast,
/// and ReleaseSafe (optimizing the release modes), runs each on the VM with the
/// deterministic executed-instruction counter, asserts the optimized output is
/// byte-identical to the unoptimized Debug output (a divergence is a miscompile
/// and aborts the bench), and reports the Debug-vs-ReleaseFast reduction.
///
/// Usage:
///   k2c bench                  Run the committed benchmark suite and print a table.
///   k2c bench <file.k2> ...    Benchmark the given programs instead.
/// The `lsp` subcommand: run the k2 language server over stdio.
///
/// This speaks the Language Server Protocol (JSON-RPC framed by `Content-Length`)
/// on stdin/stdout, reusing the front-end crates for every feature so the editor
/// experience matches `k2c check`/`k2c fmt` exactly. The only transport is stdio;
/// the optional `--stdio` flag is accepted for the conventional spelling. All RPC
/// goes to stdout, all logging to stderr; the process exits `0` on a clean
/// `shutdown`/`exit` or stdin close.
fn cmd_lsp(args: &[String]) -> Result<ExitCode, String> {
    for arg in args {
        match arg.as_str() {
            // The standard, default transport; accepted explicitly.
            "--stdio" => {}
            other => return Err(format!("unknown `lsp` flag `{other}` (only --stdio)")),
        }
    }
    match k2_lsp::run_stdio() {
        Ok(0) => Ok(ExitCode::SUCCESS),
        Ok(code) => Ok(ExitCode::from(code as u8)),
        Err(e) => Err(format!("language server I/O error: {e}")),
    }
}

///   k2c bench --emit-baseline  Print a `name debug=.. fast=.. safe=..` line per bench.
fn cmd_bench(args: &[String]) -> Result<ExitCode, String> {
    let mut files: Vec<String> = Vec::new();
    let mut emit_baseline = false;
    let mut native_only = false;
    let mut emit_report = false;
    for arg in args {
        match arg.as_str() {
            "--emit-baseline" => emit_baseline = true,
            // `--native` runs *only* the native-vs-VM wall-clock harness;
            // `--emit-report` additionally regenerates the committed markdown report.
            "--native" => native_only = true,
            "--emit-report" => {
                native_only = true;
                emit_report = true;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown `bench` flag `{other}`"));
            }
            other => files.push(other.to_string()),
        }
    }

    // The native wall-clock benchmark: compile the compute kernels to a native
    // ReleaseFast ELF and time them against the VM running the same optimized MIR.
    if native_only {
        return run_native_bench(emit_report);
    }

    // Default corpus: the committed bench/*.k2 next to this crate.
    if files.is_empty() {
        files = default_bench_files();
        if files.is_empty() {
            return Err("no benchmark programs found (looked in crates/k2c/bench)".to_string());
        }
    }

    let mut results: Vec<BenchResult> = Vec::new();
    for file in &files {
        let result = bench_one(file)?;
        results.push(result);
    }

    if emit_baseline {
        for r in &results {
            println!(
                "{} debug={} fast={} safe={}",
                r.name, r.debug, r.fast, r.safe
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // A readable table plus a total row.
    println!(
        "{:<22} {:>14} {:>14} {:>14} {:>10}",
        "benchmark", "debug", "rel-fast", "rel-safe", "speedup"
    );
    println!("{}", "-".repeat(78));
    let mut tot_debug = 0u64;
    let mut tot_fast = 0u64;
    for r in &results {
        let speedup = if r.fast == 0 {
            0.0
        } else {
            r.debug as f64 / r.fast as f64
        };
        println!(
            "{:<22} {:>14} {:>14} {:>14} {:>9.2}x",
            r.name, r.debug, r.fast, r.safe, speedup
        );
        tot_debug += r.debug;
        tot_fast += r.fast;
    }
    println!("{}", "-".repeat(78));
    let tot_speedup = if tot_fast == 0 {
        0.0
    } else {
        tot_debug as f64 / tot_fast as f64
    };
    let reduction = if tot_debug == 0 {
        0.0
    } else {
        100.0 * (tot_debug - tot_fast) as f64 / tot_debug as f64
    };
    println!(
        "{:<22} {:>14} {:>14} {:>14} {:>9.2}x",
        "TOTAL", tot_debug, tot_fast, "", tot_speedup
    );
    println!(
        "# ReleaseFast executed {:.1}% fewer instructions than Debug across the suite.",
        reduction
    );

    // Append the native-vs-VM wall-clock comparison so the default `bench` also
    // reports the real native speedup. It is only meaningful where the emitted ELF
    // can execute (x86_64 Linux); elsewhere a note is printed instead.
    println!();
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        if let Err(e) = run_native_bench(false) {
            // A native-bench failure must not mask the instruction-count results;
            // report it and continue with a success exit (the deterministic table
            // above is the committed metric).
            let _ = writeln!(io::stderr(), "{PROG}: warning: native bench skipped: {e}");
        }
    } else {
        println!("# native-vs-VM wall-clock bench: skipped (requires an x86_64 Linux host).");
    }
    Ok(ExitCode::SUCCESS)
}

/// One native-vs-VM wall-clock measurement for a single compute kernel.
struct NativeBench {
    /// The kernel's display name.
    name: String,
    /// Best-of-N native process wall time (microseconds).
    native_us: u128,
    /// Best-of-N in-process VM wall time (microseconds).
    vm_us: u128,
    /// `.text` bytes with the peephole disabled.
    text_before: usize,
    /// `.text` bytes with the peephole enabled (the shipped image).
    text_after: usize,
}

impl NativeBench {
    /// The wall-clock speedup (VM time / native time).
    fn speedup(&self) -> f64 {
        if self.native_us == 0 {
            0.0
        } else {
            self.vm_us as f64 / self.native_us as f64
        }
    }
    /// The peephole `.text` reduction percentage.
    fn peephole_pct(&self) -> f64 {
        if self.text_before == 0 {
            0.0
        } else {
            100.0 * (self.text_before - self.text_after) as f64 / self.text_before as f64
        }
    }
}

/// The compute kernels timed by the native-vs-VM benchmark. All are in the native
/// subset and finish comfortably on the VM (well under its 200M-step budget and 5s
/// wall guard), so the same optimized MIR runs on both backends.
fn native_bench_files() -> Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/bench");
    // `bench_slice_sum` is included so the `for (xs) |x|` over-a-slice value
    // capture is gated native-vs-VM (it once silently summed to 0 on native — a
    // pointer-to-array passed to a `[]T` parameter was marshalled as a single
    // register, so the callee read a garbage `.len`). The other kernels are the
    // fib/loop compute kernels that headline the speedup table.
    [
        "bench_fib_rec_native",
        "bench_fib_rec",
        "bench_loop_sum",
        "bench_slice_sum",
    ]
    .iter()
    .map(|n| format!("{dir}/{n}.k2"))
    .collect()
}

/// The number of timed repetitions; the *minimum* elapsed time is reported (min
/// rejects scheduler noise and is the most reproducible "how fast can this run").
const NATIVE_BENCH_REPS: u32 = 5;

/// Runs the native-vs-VM wall-clock benchmark over the compute kernels, printing a
/// human-readable table and (when `emit_report`) regenerating the committed
/// `bench/native_baseline.md`. Each kernel is compiled once to a native
/// ReleaseFast ELF, executed `NATIVE_BENCH_REPS` times (min wall), and compared
/// against the same optimized MIR run in-process on the VM the same number of
/// times. Native and VM stdout/exit are asserted identical — a divergence is a
/// miscompile and aborts the bench.
fn run_native_bench(emit_report: bool) -> Result<ExitCode, String> {
    if !cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        return Err("the native bench requires an x86_64 Linux host to execute the ELF".into());
    }
    let mut results: Vec<NativeBench> = Vec::new();
    for file in native_bench_files() {
        results.push(measure_native_vs_vm(&file)?);
    }

    println!(
        "{:<26} {:>12} {:>12} {:>10} {:>16}",
        "native bench", "native(ms)", "vm(ms)", "speedup", "peephole .text"
    );
    println!("{}", "-".repeat(80));
    for r in &results {
        println!(
            "{:<26} {:>12.3} {:>12.3} {:>9.1}x {:>9} -> {:<5}",
            r.name,
            r.native_us as f64 / 1000.0,
            r.vm_us as f64 / 1000.0,
            r.speedup(),
            r.text_before,
            r.text_after,
        );
    }
    println!("{}", "-".repeat(80));
    if let Some(best) = results.iter().max_by(|a, b| {
        a.speedup()
            .partial_cmp(&b.speedup())
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        println!(
            "# native is up to {:.1}x faster than the VM (best: {}); \
             wall-clock is best-of-{} on x86_64-linux.",
            best.speedup(),
            best.name,
            NATIVE_BENCH_REPS
        );
    }

    if emit_report {
        write_native_report(&results)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Measures one kernel: compiles to a native ReleaseFast ELF (capturing the
/// peephole size reduction), times the native binary and the in-process VM
/// best-of-N, and asserts their stdout/exit agree.
fn measure_native_vs_vm(file: &str) -> Result<NativeBench, String> {
    let source = fs::read_to_string(file).map_err(|e| format!("reading `{file}`: {e}"))?;
    let name = bench_name(file);

    // Lower + optimize under ReleaseFast (the same path `run-native --release-fast`
    // uses), then build the ELF and capture the peephole size statistics.
    let prog = lower_to_opt_mir(&source, file, BuildMode::ReleaseFast)?;
    let (img, stats) = k2_codegen::compile_program_to_elf_stats(&prog)
        .map_err(|e| format!("native backend: {e} (kernel {name} is outside the native subset)"))?;

    // The expected output + exit, from the VM running the same optimized MIR.
    let (vm_outcome, vm_code, vm_out, _vm_err, _count) = run_metered(&prog);
    let _ = matches!(vm_outcome, RunOutcome::Ok);

    // Write the ELF once, make it executable, then exec it best-of-N.
    let tmp = native_temp_path();
    fs::write(&tmp, &img.bytes).map_err(|e| format!("writing temp ELF: {e}"))?;
    set_executable(&tmp);

    let mut native_min = u128::MAX;
    let mut native_out: Vec<u8> = Vec::new();
    let mut native_code: i32 = 0;
    for _ in 0..NATIVE_BENCH_REPS {
        let (us, code, out) = time_native_exec(&tmp)?;
        if us < native_min {
            native_min = us;
        }
        native_out = out;
        native_code = code;
    }
    let _ = fs::remove_file(&tmp);

    // The differential gate: native must reproduce the VM's stdout + exit. A
    // divergence is a miscompile (mirrors `bench_one`), so abort the whole bench.
    if native_out != vm_out || native_code != vm_code {
        return Err(format!(
            "MISCOMPILE in {name}: native output/exit differs from the VM\n  \
             vm    =({vm_code}) {:?}\n  native=({native_code}) {:?}",
            String::from_utf8_lossy(&vm_out),
            String::from_utf8_lossy(&native_out),
        ));
    }

    // Time the in-process VM best-of-N on the same optimized MIR.
    let mut vm_min = u128::MAX;
    for _ in 0..NATIVE_BENCH_REPS {
        let t0 = std::time::Instant::now();
        let _ = run_metered(&prog);
        let us = t0.elapsed().as_micros();
        if us < vm_min {
            vm_min = us;
        }
    }

    Ok(NativeBench {
        name,
        native_us: native_min,
        vm_us: vm_min,
        text_before: stats.text_bytes_before,
        text_after: stats.text_bytes_after,
    })
}

/// Executes a native ELF once and returns `(elapsed_us, exit_code, stdout)`. The
/// ETXTBSY retry mirrors `run-native`/the codegen test harness: a freshly-written,
/// not-yet-flushed executable can transiently fail to exec.
fn time_native_exec(path: &Path) -> Result<(u128, i32, Vec<u8>), String> {
    let mut attempt = 0;
    loop {
        let t0 = std::time::Instant::now();
        match std::process::Command::new(path).output() {
            Ok(out) => {
                let us = t0.elapsed().as_micros();
                let code = native_exit_code(&out.status);
                return Ok((us, code, out.stdout));
            }
            Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(format!("executing native ELF: {e}")),
        }
    }
}

/// Lowers `source` to a verified, optimized [`MirProgram`] under `mode` (the same
/// front-end + optimizer path the driver's run/native commands use), returning a
/// human-readable error on any front-end failure.
fn lower_to_opt_mir(source: &str, label: &str, mode: BuildMode) -> Result<MirProgram, String> {
    let pres = parse_program(source);
    if !pres.is_ok() {
        return Err(format!("{label}: parse errors"));
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        return Err(format!("{label}: resolution errors"));
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        return Err(format!("{label}: type errors"));
    }
    let mut prog = lower_program(&pres.file, &resolved, typed, mode)
        .map_err(|_| format!("{label}: lowering failed"))?;
    if !prog.is_ok() {
        return Err(format!("{label}: lowering had errors"));
    }
    run_optimizer(&mut prog, mode, false)?;
    let problems = prog.verify();
    if !problems.is_empty() {
        return Err(format!("{label}: optimizer produced malformed MIR"));
    }
    Ok(prog)
}

/// Regenerates the committed `bench/native_baseline.md` from freshly-measured
/// numbers. Every figure in the file is measured here, never hand-written.
fn write_native_report(results: &[NativeBench]) -> Result<(), String> {
    let mut md = String::new();
    md.push_str("# k2 native-vs-VM benchmark baseline\n\n");
    md.push_str("Host: x86_64-linux. Wall-clock is **best-of-");
    md.push_str(&NATIVE_BENCH_REPS.to_string());
    md.push_str(
        "** (the minimum elapsed time rejects scheduler noise and is the most \
         reproducible statistic). These numbers are *measured*, regenerated with \
         `k2c bench --emit-report`, and will vary run-to-run; the CI gate is the \
         conservative `>= 5x` speedup floor in the test suite, **not** these exact \
         values. Native time includes a fixed process-spawn/startup cost, so the \
         pure-compute ratio is higher still.\n\n",
    );
    md.push_str(
        "| kernel | native (ms) | vm (ms) | speedup | peephole .text (bytes) | reduction |\n",
    );
    md.push_str("|---|---:|---:|---:|---:|---:|\n");
    for r in results {
        md.push_str(&format!(
            "| `{}` | {:.3} | {:.3} | {:.1}x | {} -> {} | {:.1}% |\n",
            r.name,
            r.native_us as f64 / 1000.0,
            r.vm_us as f64 / 1000.0,
            r.speedup(),
            r.text_before,
            r.text_after,
            r.peephole_pct(),
        ));
    }
    md.push('\n');
    md.push_str(
        "The peephole pass (redundant self-moves, dead stores, `mov r,0`->`xor`, \
         jump-to-next/jump-to-jump) shrinks `.text` while leaving behavior \
         byte-identical (verified differentially: native-opt == native-unopt == \
         VM). The speedup is the headline result: native executes the same program \
         many times faster than the bytecode VM.\n",
    );

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/bench/native_baseline.md");
    fs::write(path, md).map_err(|e| format!("writing {path}: {e}"))?;
    let _ = writeln!(io::stderr(), "wrote {path}");
    Ok(())
}

/// Benchmarks a single program: lowers + runs it under all three modes, asserts
/// the optimized output matches the unoptimized Debug output, and returns the
/// instruction counts. A behavioral divergence is a miscompile and is returned as
/// an error (aborting the bench).
fn bench_one(file: &str) -> Result<BenchResult, String> {
    let source = fs::read_to_string(file).map_err(|e| format!("reading `{file}`: {e}"))?;
    let name = bench_name(file);

    let (out_d, code_d, count_d) = lower_run_metered(&source, file, BuildMode::Debug, false)?;
    let (out_f, code_f, count_f) = lower_run_metered(&source, file, BuildMode::ReleaseFast, false)?;
    let (out_s, code_s, count_s) = lower_run_metered(&source, file, BuildMode::ReleaseSafe, false)?;

    // The acceptance invariant: the optimized release modes must produce
    // byte-identical stdout and the same exit code as unoptimized Debug.
    if out_f != out_d || code_f != code_d {
        return Err(format!(
            "MISCOMPILE in {name}: ReleaseFast output/exit differs from Debug\n  \
             debug=({code_d}) {out_d:?}\n  fast =({code_f}) {out_f:?}"
        ));
    }
    if out_s != out_d || code_s != code_d {
        return Err(format!(
            "MISCOMPILE in {name}: ReleaseSafe output/exit differs from Debug\n  \
             debug=({code_d}) {out_d:?}\n  safe =({code_s}) {out_s:?}"
        ));
    }

    Ok(BenchResult {
        name,
        debug: count_d,
        fast: count_f,
        safe: count_s,
    })
}

/// Lowers `source` under `mode`, optimizes per the mode (Debug optionally via
/// `opt_flag`), runs it metered, and returns `(stdout, exit_code, instr_count)`.
fn lower_run_metered(
    source: &str,
    label: &str,
    mode: BuildMode,
    opt_flag: bool,
) -> Result<(String, i32, u64), String> {
    let pres = parse_program(source);
    if !pres.is_ok() {
        return Err(format!("{label}: parse errors"));
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        return Err(format!("{label}: resolution errors"));
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        return Err(format!("{label}: type errors"));
    }
    let mut prog = lower_program(&pres.file, &resolved, typed, mode)
        .map_err(|_| format!("{label}: lowering failed"))?;
    if !prog.is_ok() {
        return Err(format!("{label}: lowering had errors"));
    }
    run_optimizer(&mut prog, mode, opt_flag)?;
    let (outcome, code, out, _err, count) = run_metered(&prog);
    // Treat a clean error/panic as part of the observable behavior; the bench
    // programs are written to succeed, but the comparison still holds either way.
    let _ = matches!(outcome, RunOutcome::Ok);
    Ok((String::from_utf8_lossy(&out).into_owned(), code, count))
}

/// The display name of a benchmark file (its file stem).
fn bench_name(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file)
        .to_string()
}

/// Locates the committed benchmark programs. Tries the path relative to the crate
/// source (so `cargo run` works from the workspace root) and a couple of common
/// fallbacks.
fn default_bench_files() -> Vec<String> {
    // The directory next to this source file, resolved at compile time.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/bench");
    let mut files: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("k2") {
                if let Some(s) = p.to_str() {
                    files.push(s.to_string());
                }
            }
        }
    }
    files.sort();
    files
}

/// The filesystem [`FileLoader`] used by `resolve --modules`: it reads a `.k2`
/// file from disk and parses it, mapping I/O and parse failures to the loader's
/// error type. The driver is the only crate that performs filesystem I/O for
/// resolution.
struct FsFileLoader;

impl FileLoader for FsFileLoader {
    fn load(&self, path: &Path) -> Result<SourceFile, LoadError> {
        let src = std::fs::read_to_string(path).map_err(|_| LoadError::Missing)?;
        let pres = parse(&src);
        if pres.is_ok() {
            Ok(pres.file)
        } else {
            Err(LoadError::ParseFailed)
        }
    }
}

/// Reads the source either from `-` (standard input) or from a file path.
/// Returns the source text together with a human-readable label for headers.
fn read_source(path: &str) -> Result<(String, String), String> {
    if path == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        Ok((buf, "<stdin>".to_string()))
    } else {
        if !path.ends_with(".k2") {
            // A soft warning, not an error: the lexer is content-agnostic, but
            // the .k2 extension is the single canonical one (spec §1.1).
            let _ = writeln!(
                io::stderr(),
                "{PROG}: warning: `{path}` does not end in `.k2`"
            );
        }
        let text = fs::read_to_string(path).map_err(|e| format!("reading `{path}`: {e}"))?;
        Ok((text, path.to_string()))
    }
}

/// Prints the token stream in a stable, greppable, column-aligned format, with
/// a trailing summary of how many tokens (and how many lexical errors) were
/// produced.
fn print_tokens(label: &str, tokens: &[Token]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let _ = writeln!(out, "# tokens for {label}");
    let _ = writeln!(out, "# {:>4} {:<3}  {:<16} text", "line", "col", "kind");

    let mut offset: u32 = 0;
    let mut error_count = 0usize;
    for tok in tokens {
        if tok.kind == TokenKind::Error {
            error_count += 1;
        }
        // Demonstrate the syntax-crate span helper on each token. The span is
        // computed but only its width is shown, to keep the output compact.
        let span: Span = Span::of_token(tok, offset);
        offset = span.end;

        let _ = writeln!(
            out,
            "{:>6}:{:<3} {:<16} {}",
            tok.line,
            tok.col,
            format!("{:?}", tok.kind),
            render_text(&tok.text),
        );
    }

    let _ = writeln!(
        out,
        "# {} token(s), {} lexical error(s)",
        tokens.len(),
        error_count
    );
}

/// Renders a token's source text for display, escaping newlines/tabs and
/// quoting so multi-line lexemes (multiline strings, doc comments) stay on one
/// output line.
fn render_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let escaped = text
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    format!("{escaped:?}")
}

/// Prints the usage banner to standard output.
fn print_usage() {
    // Plain `println!` is fine here; usage is normal program output.
    println!(
        "{PROG} {VERSION} — the k2 compiler front-end\n\
         \n\
         USAGE:\n\
         \x20   {PROG} <subcommand> [args]\n\
         \n\
         SUBCOMMANDS:\n\
         \x20   tokenize <file.k2>   Lex a source file and print its token stream.\n\
         \x20   tokenize -           Lex from standard input.\n\
         \x20   lex <file.k2>        Alias for `tokenize`.\n\
         \x20   parse <file.k2>      Parse a source file and print its S-expression AST.\n\
         \x20   parse -              Parse from standard input.\n\
         \x20   fmt <file.k2>        Print the canonical formatting of a source file.\n\
         \x20   fmt -                Format from standard input.\n\
         \x20   fmt --check <file>   Exit nonzero if the file is not already canonical.\n\
         \x20   fmt --write <file>   Rewrite the file in place in canonical form.\n\
         \x20   ast <file.k2>        Dump the structured AST (S-expression).\n\
         \x20   ast --spans <file>   Annotate AST nodes with @start..end spans.\n\
         \x20   resolve <file.k2>    Resolve names/scopes; print the scope tree (or diagnostics).\n\
         \x20   resolve --uses <f>   Also dump the per-occurrence resolution table.\n\
         \x20   resolve --modules <f>  Build the module graph across path imports; report cycles.\n\
         \x20   check <file.k2>      Type-check a file; print per-decl signatures (or diagnostics).\n\
         \x20   check --uses <f>     Also dump the per-occurrence type table.\n\
         \x20   mir <file.k2>        Lower to MIR (Debug mode) and print the dump.\n\
         \x20   mir --release-fast <f>  Lower + optimize with safety checks stripped.\n\
         \x20   run <file.k2>        Compile to bytecode and execute `main` (Debug mode).\n\
         \x20   run --release-fast <f>  Optimize + execute with safety checks stripped.\n\
         \x20   run-native <file.k2>  Compile to a native x86-64 ELF, execute it, propagate exit code.\n\
         \x20   build-native <f> -o <out>  Compile to a static Linux ELF written to <out>.\n\
         \x20       --target=<triple>  Select the target ISA. Supported triples:\n\
         \x20         x86_64-linux   (default; build + run + native==VM verified)\n\
         \x20         aarch64-linux  (cross-compile; structurally validated, NOT executed here —\n\
         \x20                         no emulator on this host; expected to run on real aarch64 Linux)\n\
         \x20   bench [file.k2 ...]  Benchmark Debug vs ReleaseFast executed VM instructions.\n\
         \x20   lsp                  Run the language server over stdio (LSP / JSON-RPC).\n\
         \x20   help                 Show this help.\n\
         \x20   version              Print the version.\n\
         \n\
         PARSE FLAGS:\n\
         \x20   --quiet              Suppress the tree; print only diagnostics + a summary.\n\
         \x20   --spans              Annotate each S-expr node with its @start..end span.\n\
         \n\
         RESOLVE FLAGS:\n\
         \x20   --scopes             Print the scope/definition tree (the default).\n\
         \x20   --uses               Also print the resolution of every identifier occurrence.\n\
         \x20   --modules            Resolve across `.k2` path imports; report cycles/missing files.\n\
         \x20   --quiet              Suppress the dump; print only diagnostics + a summary.\n\
         \n\
         CHECK FLAGS:\n\
         \x20   --signatures         Print one signature/type per declaration (the default).\n\
         \x20   --uses               Also print the inferred type of every expression occurrence.\n\
         \x20   --quiet              Suppress the dump; print only diagnostics + a summary.\n\
         \n\
         MIR FLAGS:\n\
         \x20   --debug              Lower in Debug mode with safety checks (the default).\n\
         \x20   --release-safe       Lower + optimize in ReleaseSafe mode (safety checks kept).\n\
         \x20   --release-fast       Lower + optimize in ReleaseFast mode (safety checks stripped).\n\
         \x20   --opt                Optimize even in Debug mode (keeps checks; for testing).\n\
         \x20   --opt-report         Print the optimizer's pass statistics to stderr.\n\
         \x20   --quiet              Suppress the dump; print only diagnostics + a summary.\n\
         \n\
         RUN FLAGS:\n\
         \x20   --debug              Execute with safety checks (the default).\n\
         \x20   --release-safe       Optimize + execute in ReleaseSafe mode (safety checks kept).\n\
         \x20   --release-fast       Optimize + execute in ReleaseFast mode (safety checks stripped).\n\
         \x20   --opt                Optimize even in Debug mode (keeps checks; for testing).\n\
         \x20   --opt-report         Print the optimizer's pass statistics to stderr.\n\
         \n\
         BENCH FLAGS:\n\
         \x20   --emit-baseline      Print a machine-readable instruction-count baseline.\n"
    );
}
