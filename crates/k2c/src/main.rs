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

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use std::path::Path;

use k2_fmt::format_source;
use k2_lexer::{tokenize, Token, TokenKind};
use k2_mir::{dump_mir, lower_program, BuildMode, MirProgram, Severity as MirSeverity};
use k2_opt::{optimize, OptLevel, OptStats};
use k2_parse::{parse, to_sexpr, ParseResult, Severity};
use k2_resolve::{
    dump_resolution, dump_scopes, resolve_file, resolve_module, FileLoader, LoadError,
    ResolvedModule, Severity as ResolveSeverity,
};
use k2_syntax::{Expr, Item, SourceFile, Span};
use k2_types::{check_file, dump_signatures, dump_types, Severity as TypeSeverity};
use k2_vm::{run_metered, run_program, RunArgs, RunOutcome};

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

    let mut result = k2_parse::parse(&combined);
    rewrite_std_imports(&mut result.file);
    result
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

    // Diagnostics go to stderr, formatted `label:line:col: severity: message`.
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &result.diagnostics {
        let sev = match diag.severity {
            Severity::Error => {
                error_count += 1;
                "error"
            }
            Severity::Warning => "warning",
        };
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }

    if quiet {
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
            let stderr = io::stderr();
            let mut err = stderr.lock();
            for diag in &diags {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
            let _ = writeln!(err, "error: cannot format {label}: it has parse errors");
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

    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &result.diagnostics {
        let sev = match diag.severity {
            Severity::Error => {
                error_count += 1;
                "error"
            }
            Severity::Warning => "warning",
        };
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }

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
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &pres.diagnostics {
            if diag.severity == Severity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot resolve {label}: it has parse errors");
        return Ok(ExitCode::FAILURE);
    }

    // Resolve, either single-file or across the module graph.
    if modules {
        let rm = resolve_module(Path::new(path), &FsFileLoader);
        finish_modules(&label, &rm, show_uses, quiet)
    } else {
        let r = resolve_file(&pres.file);
        finish_single(&label, &r, show_uses, quiet)
    }
}

/// Prints diagnostics + summary for a single-file resolution and returns the
/// exit code.
fn finish_single(
    label: &str,
    r: &k2_resolve::Resolved,
    show_uses: bool,
    quiet: bool,
) -> Result<ExitCode, String> {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &r.diagnostics {
        if diag.severity == ResolveSeverity::Error {
            error_count += 1;
        }
        let sev = sev_word(diag.severity);
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }
    drop(err);

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
fn finish_modules(
    label: &str,
    rm: &ResolvedModule,
    show_uses: bool,
    quiet: bool,
) -> Result<ExitCode, String> {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    // Graph-level diagnostics first, then each module's own.
    for diag in &rm.diagnostics {
        if diag.severity == ResolveSeverity::Error {
            error_count += 1;
        }
        let _ = writeln!(
            err,
            "{label}:{}:{}: {}: {}",
            diag.span.line,
            diag.span.col,
            sev_word(diag.severity),
            diag.message
        );
    }
    for m in &rm.modules {
        for diag in &m.resolved.diagnostics {
            if diag.severity == ResolveSeverity::Error {
                error_count += 1;
            }
            let _ = writeln!(
                err,
                "{}:{}:{}: {}: {}",
                m.path.display(),
                diag.span.line,
                diag.span.col,
                sev_word(diag.severity),
                diag.message
            );
        }
    }
    drop(err);

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

/// The lowercase severity word used in the `label:line:col: sev: msg` format,
/// for resolution diagnostics.
fn sev_word(sev: ResolveSeverity) -> &'static str {
    match sev {
        ResolveSeverity::Error => "error",
        ResolveSeverity::Warning => "warning",
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
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &pres.diagnostics {
            if diag.severity == Severity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot check {label}: it has parse errors");
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors also gate type-checking.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &resolved.diagnostics {
            if diag.severity == ResolveSeverity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot check {label}: it has resolution errors");
        return Ok(ExitCode::FAILURE);
    }

    let typed = check_file(&pres.file, &resolved);

    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &typed.diagnostics {
        if diag.severity == TypeSeverity::Error {
            error_count += 1;
        }
        let sev = match diag.severity {
            TypeSeverity::Error => "error",
            TypeSeverity::Warning => "warning",
        };
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }
    drop(err);

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
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &pres.diagnostics {
            if diag.severity == Severity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot lower {label}: it has parse errors");
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors also gate lowering.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &resolved.diagnostics {
            if diag.severity == ResolveSeverity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot lower {label}: it has resolution errors");
        return Ok(ExitCode::FAILURE);
    }

    // Type errors also gate lowering.
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &typed.diagnostics {
            if diag.severity == TypeSeverity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot lower {label}: it has type errors");
        return Ok(ExitCode::FAILURE);
    }

    // Lower to MIR.
    let mut prog = match lower_program(&pres.file, &resolved, typed, mode) {
        Ok(p) => p,
        Err(diags) => {
            let stderr = io::stderr();
            let mut err = stderr.lock();
            for diag in &diags {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
            let _ = writeln!(err, "error: cannot lower {label}: lowering failed");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Report lowering + leak diagnostics to stderr.
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &prog.diagnostics {
        if diag.severity == MirSeverity::Error {
            error_count += 1;
        }
        let sev = match diag.severity {
            MirSeverity::Error => "error",
            MirSeverity::Warning => "warning",
        };
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }
    drop(err);

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
    // Arguments after the path are reserved for the program's own argv.
    let mut forwarded: Vec<String> = Vec::new();
    let mut seen_path = false;
    for arg in args {
        if seen_path {
            forwarded.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--release-fast" => mode = BuildMode::ReleaseFast,
            "--release-safe" => mode = BuildMode::ReleaseSafe,
            "--debug" => mode = BuildMode::Debug,
            "--opt" => opt_flag = true,
            "--opt-report" => opt_report = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `run` flag `{other}`"));
            }
            other => {
                path = Some(other);
                seen_path = true;
            }
        }
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
        return run_multi_file(path, &label, mode, opt_flag, opt_report, forwarded);
    }

    let pres = parse_program(&source);

    // Parse errors gate execution: report them and stop, like `mir`.
    if !pres.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &pres.diagnostics {
            if diag.severity == Severity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot run {label}: it has parse errors");
        return Ok(ExitCode::FAILURE);
    }

    // Resolution errors gate execution.
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &resolved.diagnostics {
            if diag.severity == ResolveSeverity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot run {label}: it has resolution errors");
        return Ok(ExitCode::FAILURE);
    }

    // Type errors gate execution.
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        let stderr = io::stderr();
        let mut err = stderr.lock();
        for diag in &typed.diagnostics {
            if diag.severity == TypeSeverity::Error {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
        }
        let _ = writeln!(err, "error: cannot run {label}: it has type errors");
        return Ok(ExitCode::FAILURE);
    }

    // Lower to MIR under the chosen build mode.
    let mut prog = match lower_program(&pres.file, &resolved, typed, mode) {
        Ok(p) => p,
        Err(diags) => {
            let stderr = io::stderr();
            let mut err = stderr.lock();
            for diag in &diags {
                let _ = writeln!(
                    err,
                    "{label}:{}:{}: error: {}",
                    diag.span.line, diag.span.col, diag.message
                );
            }
            let _ = writeln!(err, "error: cannot run {label}: lowering failed");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Error-severity lowering/leak diagnostics gate execution; warnings are
    // printed but do not stop the run.
    let stderr = io::stderr();
    let mut err = stderr.lock();
    let mut error_count = 0usize;
    for diag in &prog.diagnostics {
        if diag.severity == MirSeverity::Error {
            error_count += 1;
        }
        let sev = match diag.severity {
            MirSeverity::Error => "error",
            MirSeverity::Warning => "warning",
        };
        let _ = writeln!(
            err,
            "{label}:{}:{}: {sev}: {}",
            diag.span.line, diag.span.col, diag.message
        );
    }
    drop(err);
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
    Ok(run_program(
        &prog,
        RunArgs {
            mode,
            argv: forwarded,
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
        },
    ))
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
    for arg in args {
        match arg.as_str() {
            "--emit-baseline" => emit_baseline = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown `bench` flag `{other}`"));
            }
            other => files.push(other.to_string()),
        }
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
    Ok(ExitCode::SUCCESS)
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
