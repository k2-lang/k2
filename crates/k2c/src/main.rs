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
//! k2c help                   # print usage
//! k2c version                # print the version
//! ```
//!
//! For `tokenize`, the process exits `0` on success and nonzero only on a usage
//! or I/O error (lexical *errors* are reported as `Error` tokens, not a process
//! failure). For `parse`, the process additionally exits nonzero when the
//! source contained one or more parse errors.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use std::path::Path;

use k2_fmt::format_source;
use k2_lexer::{tokenize, Token, TokenKind};
use k2_parse::{parse, to_sexpr, Severity};
use k2_resolve::{
    dump_resolution, dump_scopes, resolve_file, resolve_module, FileLoader, LoadError,
    ResolvedModule, Severity as ResolveSeverity,
};
use k2_syntax::{SourceFile, Span};
use k2_types::{check_file, dump_signatures, dump_types, Severity as TypeSeverity};

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
    let pres = parse(&source);

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
         \x20   --quiet              Suppress the dump; print only diagnostics + a summary.\n"
    );
}
