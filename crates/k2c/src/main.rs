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

use k2_fmt::format_source;
use k2_lexer::{tokenize, Token, TokenKind};
use k2_parse::{parse, to_sexpr, Severity};
use k2_syntax::Span;

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
         \x20   help                 Show this help.\n\
         \x20   version              Print the version.\n\
         \n\
         PARSE FLAGS:\n\
         \x20   --quiet              Suppress the tree; print only diagnostics + a summary.\n\
         \x20   --spans              Annotate each S-expr node with its @start..end span.\n"
    );
}
