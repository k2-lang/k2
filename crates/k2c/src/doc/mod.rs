//! `k2c doc` — the documentation generator (milestone v0.28).
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This module owns the doc subcommand: it builds a [`DocModel`] from a parsed
//! file (pub items + their `///` doc comments + their type-checker SIGNATURES),
//! then renders it to a self-contained HTML site (and optionally Markdown) via
//! [`render`], and — with `--test` — runs the doc examples via [`doctest`].
//!
//! ## Design tenets (mapped to the acceptance gate)
//!
//! * **Signatures are PULLED from the type checker** ([`k2_types::Typed`]), so a
//!   `pub fn`'s rendered type is the resolved truth (`[]const u8`, `?*T`,
//!   `error{…}!void`), with PARAM NAMES taken from the AST. Only a file that fails
//!   to type-check falls back to rendering the AST type expressions directly.
//! * **Never panics on parseable input.** Only a *parse* error gates (a
//!   non-parseable file has no usable AST); resolve/type errors degrade to a
//!   warning and a partial model. The fence/markdown scanners are total and the VM
//!   run is `catch_unwind`-wrapped.
//! * **Deterministic.** Modules and items are collected then sorted; no `HashMap`
//!   iteration reaches the output.

pub mod doctest;
pub mod render;

#[cfg(test)]
mod tests;

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use k2_resolve::{resolve_file, Def, DefKind, Resolved};
use k2_syntax::{Container, ContainerKind, Expr, Field, Item, Member, UnionTag};
use k2_types::{check_file, Type, Typed};

use doctest::ExMode;

/// The kind of a documented item (drives the kind pill + section layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocKind {
    /// A `pub fn`.
    Fn,
    /// A plain `pub const`.
    Const,
    /// A plain `pub var`.
    Var,
    /// A `pub const T = struct {...}`.
    Struct,
    /// A `pub const T = enum {...}`.
    Enum,
    /// A `pub const T = union {...}`.
    Union,
}

impl DocKind {
    /// A short lowercase label for the kind (used in pills / the index).
    pub fn label(self) -> &'static str {
        match self {
            DocKind::Fn => "fn",
            DocKind::Const => "const",
            DocKind::Var => "var",
            DocKind::Struct => "struct",
            DocKind::Enum => "enum",
            DocKind::Union => "union",
        }
    }
}

/// The whole documentation model for one source file.
pub struct DocModel {
    /// The source path / label, used in titles.
    pub label: String,
    /// The rendered file-level doc (`SourceFile.doc`, `///`-stripped, joined).
    pub file_doc: String,
    /// Doc examples in the file-level doc (rare, but supported).
    pub file_examples: Vec<DocExample>,
    /// The modules (namespaces): the root module plus each pub container type.
    pub modules: Vec<DocModule>,
}

/// One module / namespace: the root, or a container type.
pub struct DocModule {
    /// The dotted path: `[]` for root, `["List"]` for a container namespace.
    pub path: Vec<String>,
    /// The container's own doc comment (Markdown), empty for the root.
    pub doc_md: String,
    /// The pub members documented in this module.
    pub items: Vec<DocItem>,
    /// Examples found in the container's own doc comment.
    pub examples: Vec<DocExample>,
}

/// One documented declaration.
pub struct DocItem {
    /// The declared name.
    pub name: String,
    /// The item kind.
    pub kind: DocKind,
    /// The parent module's dotted path (for the anchor + cross-links).
    pub module_path: Vec<String>,
    /// The html-safe anchor slug (unique within the page).
    pub anchor: String,
    /// The rendered signature line.
    pub signature: String,
    /// The raw Markdown doc text (`///`-stripped).
    pub doc_md: String,
    /// Struct/union fields or enum variants (pub-filtered).
    pub fields: Vec<DocField>,
    /// Function parameters (structured, for the table).
    pub params: Vec<DocParam>,
    /// The function return type, rendered.
    pub ret: Option<String>,
    /// The fenced examples extracted from the doc.
    pub examples: Vec<DocExample>,
}

impl DocItem {
    /// The dotted qualified name (`Module.name`, or just `name` at the root).
    pub fn qualified_name(&self) -> String {
        if self.module_path.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.module_path.join("."), self.name)
        }
    }
}

/// One struct/union field or enum variant.
pub struct DocField {
    /// The field/variant name.
    pub name: String,
    /// The rendered type (`void` for a payload-less variant).
    pub ty: String,
    /// The field's own doc comment (Markdown).
    pub doc_md: String,
}

/// One function parameter.
pub struct DocParam {
    /// The parameter name.
    pub name: String,
    /// The rendered parameter type.
    pub ty: String,
    /// `true` for a `comptime` parameter.
    pub is_comptime: bool,
}

/// One fenced code example extracted from a doc comment.
pub struct DocExample {
    /// The owning item / module name (for the synthetic test name).
    pub item_name: String,
    /// The 0-based index of this example within its owner's doc.
    pub index: usize,
    /// The verbatim fenced body.
    pub code: String,
    /// The classification (run / no-run / compile-fail / ignore).
    pub mode: ExMode,
    /// The doc-test result, set by [`doctest::run_doctests`] for the HTML badge.
    pub passed: Option<bool>,
}

// =========================================================================
//  The `k2c doc` command
// =========================================================================

/// The output format(s) to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// HTML only (the default).
    Html,
    /// Markdown only.
    Md,
    /// Both HTML and Markdown.
    Both,
}

/// The parsed `k2c doc` options.
struct DocArgs {
    /// The file or directory to document.
    path: String,
    /// The output directory (default `./doc`).
    output: String,
    /// The output format.
    format: Format,
    /// Run doc-tests after generating, embedding pass/fail and gating the exit code.
    test: bool,
    /// Treat every example as compile-only.
    no_run: bool,
}

/// The `doc` subcommand entry point.
pub fn cmd_doc(args: &[String]) -> Result<ExitCode, String> {
    let parsed = parse_args(args)?;
    let p = Path::new(&parsed.path);
    if parsed.path != "-" && p.is_dir() {
        run_directory(&parsed)
    } else {
        run_one(&parsed)
    }
}

/// Parses the `doc` flags into a [`DocArgs`].
fn parse_args(args: &[String]) -> Result<DocArgs, String> {
    let mut path: Option<String> = None;
    let mut output: Option<String> = None;
    let mut format = Format::Html;
    let mut test = false;
    let mut no_run = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let a = arg.as_str();
        if let Some(v) = a.strip_prefix("--output=") {
            output = Some(v.to_string());
            continue;
        }
        if let Some(v) = a.strip_prefix("-o=") {
            output = Some(v.to_string());
            continue;
        }
        if let Some(v) = a.strip_prefix("--format=") {
            format = parse_format(v)?;
            continue;
        }
        match a {
            "-o" | "--output" => {
                let v = it
                    .next()
                    .ok_or_else(|| "`-o`/`--output` needs a directory".to_string())?;
                output = Some(v.clone());
            }
            "--format" => {
                let v = it
                    .next()
                    .ok_or_else(|| "`--format` needs a value".to_string())?;
                format = parse_format(v)?;
            }
            "--test" => test = true,
            "--no-run" => no_run = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown `doc` flag `{other}`"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("`doc` takes one path; got extra `{other}`"));
                }
                path = Some(other.to_string());
            }
        }
    }
    let path =
        path.ok_or_else(|| "`doc` needs a <file.k2|dir> argument (or `-` for stdin)".to_string())?;
    Ok(DocArgs {
        path,
        output: output.unwrap_or_else(|| "doc".to_string()),
        format,
        test,
        no_run,
    })
}

/// Parses a `--format` value.
fn parse_format(v: &str) -> Result<Format, String> {
    match v {
        "html" => Ok(Format::Html),
        "md" | "markdown" => Ok(Format::Md),
        "both" | "all" => Ok(Format::Both),
        other => Err(format!("unknown `--format` value `{other}` (html|md|both)")),
    }
}

/// Documents one source file (or stdin): builds the model, runs doc-tests if asked,
/// writes the site, and returns the exit code (nonzero iff a `--test` run failed).
fn run_one(parsed: &DocArgs) -> Result<ExitCode, String> {
    let (source, label) = crate::read_source(&parsed.path)?;
    let mut model = match build_doc_model(&source, &label) {
        Ok(m) => m,
        Err(reason) => return gate(&label, &reason),
    };

    // Doc-tests (optional). They mutate the model with pass/fail badges.
    let test_exit = if parsed.test {
        Some(doctest::run_doctests(&mut model, &source, parsed.no_run))
    } else {
        None
    };

    write_site(parsed, &[model])?;

    match test_exit {
        Some(code) if !is_success(code) => Ok(ExitCode::FAILURE),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// Documents every `*.k2` under a directory (sorted, deterministic) into the same
/// output dir, with `index.html` linking all files.
fn run_directory(parsed: &DocArgs) -> Result<ExitCode, String> {
    let dir = Path::new(&parsed.path);
    let mut files: Vec<std::path::PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("reading `{}`: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "k2").unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no `.k2` files under `{}`", dir.display()));
    }

    let mut models = Vec::new();
    let mut any_test_failed = false;
    for f in &files {
        let label = f.to_string_lossy().into_owned();
        let source = fs::read_to_string(f).map_err(|e| format!("reading `{label}`: {e}"))?;
        let mut model = match build_doc_model(&source, &label) {
            Ok(m) => m,
            Err(reason) => {
                let _ = writeln!(io::stderr(), "warning: skipping {label}: {reason}");
                continue;
            }
        };
        if parsed.test {
            let code = doctest::run_doctests(&mut model, &source, parsed.no_run);
            if !is_success(code) {
                any_test_failed = true;
            }
        }
        models.push(model);
    }
    write_site(parsed, &models)?;
    if any_test_failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Writes the rendered pages (and a top-level `index.html` linking the per-file
/// pages when there is more than one model) to the output directory.
fn write_site(parsed: &DocArgs, models: &[DocModel]) -> Result<ExitCode, String> {
    let outdir = Path::new(&parsed.output);
    fs::create_dir_all(outdir).map_err(|e| format!("creating `{}`: {e}", outdir.display()))?;

    let mut written = 0usize;
    // Per-file output: when documenting a single file we write its pages directly;
    // for a directory we namespace each file's pages under its slug to avoid name
    // collisions, plus a top-level index linking them.
    let single = models.len() == 1;
    for model in models {
        // The per-file filename prefix (empty for a single file; `{slug}--` in
        // directory mode). It is threaded into the emitters so the page FILENAMES
        // and every generated URL agree — cross-links and nav target the actual
        // written files instead of un-prefixed names that 404.
        let prefix = if single {
            String::new()
        } else {
            format!("{}--", render::slug(&model.label))
        };
        if matches!(parsed.format, Format::Html | Format::Both) {
            // `emit_html` is the empty-prefix fast path (single-file mode);
            // `emit_html_prefixed` threads the directory-mode `{slug}--` prefix.
            let pages = if prefix.is_empty() {
                render::emit_html(model)
            } else {
                render::emit_html_prefixed(model, &prefix)
            };
            for page in pages {
                write_page(outdir, &page.filename, &page.contents)?;
                written += 1;
            }
        }
        if matches!(parsed.format, Format::Md | Format::Both) {
            let pages = if prefix.is_empty() {
                render::emit_markdown(model)
            } else {
                render::emit_markdown_prefixed(model, &prefix)
            };
            for page in pages {
                write_page(outdir, &page.filename, &page.contents)?;
                written += 1;
            }
        }
    }

    // Directory mode: a top-level index linking each file's index page.
    if !single && matches!(parsed.format, Format::Html | Format::Both) {
        let mut body = String::from("<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>k2 documentation</title></head>\n<body>\n<h1>k2 documentation</h1>\n<ul>\n");
        for model in models {
            let prefix = render::slug(&model.label);
            body.push_str(&format!(
                "<li><a href=\"{prefix}--index.html\">{}</a></li>\n",
                render::esc_html(&model.label)
            ));
        }
        body.push_str("</ul>\n</body>\n</html>\n");
        write_page(outdir, "index.html", &body)?;
        written += 1;
    }

    let _ = writeln!(
        io::stderr(),
        "wrote {written} page(s) to {}",
        outdir.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Writes one page to `outdir/name`, mapping I/O errors to a string.
fn write_page(outdir: &Path, name: &str, contents: &str) -> Result<(), String> {
    let path = outdir.join(name);
    fs::write(&path, contents).map_err(|e| format!("writing `{}`: {e}", path.display()))
}

/// A small gate that prints a reason and returns the failure exit code.
fn gate(label: &str, reason: &str) -> Result<ExitCode, String> {
    let _ = writeln!(io::stderr(), "error: cannot document {label}: {reason}");
    Ok(ExitCode::FAILURE)
}

/// `true` if `code` is the success exit code. `ExitCode` is opaque, so we compare
/// via the `Debug` form (stable enough for an internal success/failure split).
fn is_success(code: ExitCode) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::SUCCESS)
}

// =========================================================================
//  Model construction
// =========================================================================

/// Builds the [`DocModel`] from a source's text. Only a PARSE error is fatal (it
/// leaves no usable AST); resolve/type errors degrade to a warning and a partial
/// model whose signatures fall back to the AST type expressions.
pub fn build_doc_model(source: &str, label: &str) -> Result<DocModel, String> {
    let pres = crate::parse_program(source);
    if !pres.is_ok() {
        return Err("parse errors".to_string());
    }
    let resolved = resolve_file(&pres.file);
    if !resolved.is_ok() {
        let _ = writeln!(
            io::stderr(),
            "warning: {label}: name-resolution errors; signatures may be degraded"
        );
    }
    let typed = check_file(&pres.file, &resolved);
    if !typed.is_ok() {
        let _ = writeln!(
            io::stderr(),
            "warning: {label}: type errors; some signatures fall back to source syntax"
        );
    }

    let ctx = ModelCtx {
        resolved: &resolved,
        typed: &typed,
    };

    // The file-level doc. We must read it from the RAW user AST, NOT from the
    // std-injected `pres`: `parse_program` APPENDS the std prelude after the user
    // source, so a *trailing* `///` block (the only way to populate `SourceFile.doc`
    // — k2 has no `//!` inner-doc syntax) would attach to the synthetic std-root
    // item there and be lost. Parsing the user source alone attributes it correctly.
    let raw = k2_parse::parse(source);
    let file_doc = render::doc_to_markdown(&raw.file.doc.join("\n"));
    let file_examples = doctest::extract_examples("(file)", &file_doc);

    // The root module: top-level items. Each pub container also becomes its own
    // namespace module.
    let mut modules: Vec<DocModule> = Vec::new();
    let mut root = DocModule {
        path: Vec::new(),
        doc_md: String::new(),
        items: Vec::new(),
        examples: Vec::new(),
    };
    let mut anchors = AnchorPool::default();

    for item in &pres.file.items {
        // Skip the synthetic std root injected by `parse_program`.
        if item_name(item) == Some(k2_std::STD_ROOT_NAME) {
            continue;
        }
        if let Some((doc_item, submodule)) = ctx.item_to_doc(item, &[], &mut anchors) {
            if let Some(sub) = submodule {
                modules.push(sub);
            }
            root.items.push(doc_item);
        }
    }

    // Deterministic ordering.
    root.items.sort_by(|a, b| a.name.cmp(&b.name));
    modules.sort_by(|a, b| a.path.cmp(&b.path));
    for m in &mut modules {
        m.items.sort_by(|a, b| a.name.cmp(&b.name));
    }
    modules.insert(0, root);

    Ok(DocModel {
        label: label.to_string(),
        file_doc,
        file_examples,
        modules,
    })
}

/// The read-only context threaded through model construction.
struct ModelCtx<'a> {
    resolved: &'a Resolved,
    typed: &'a Typed,
}

/// A dedup pool for anchors: a repeated slug gets a numeric suffix so anchors stay
/// unique (and thus the HTML is well-formed).
#[derive(Default)]
struct AnchorPool {
    seen: std::collections::HashSet<String>,
}

impl AnchorPool {
    /// Returns a unique anchor for `base`, suffixing `-N` on a collision.
    fn unique(&mut self, base: &str) -> String {
        let slug = render::slug(base);
        if self.seen.insert(slug.clone()) {
            return slug;
        }
        for n in 1.. {
            let cand = format!("{slug}-{n}");
            if self.seen.insert(cand.clone()) {
                return cand;
            }
        }
        slug
    }
}

impl ModelCtx<'_> {
    /// Converts one top-level/nested item into a [`DocItem`] (and, for a container,
    /// the namespace [`DocModule`] holding its pub members). Returns `None` for an
    /// item we do not document (a non-pub plain decl with no doc, a test, etc.).
    ///
    /// Visibility policy: a `pub` item is always documented; a NON-pub item is
    /// documented only if it carries a `///` doc comment (so the example corpus —
    /// where helper fns are documented but not `pub` — renders richly, mirroring
    /// rustdoc's `--document-private-items`). Tests and `comptime` blocks are never
    /// documented.
    fn item_to_doc(
        &self,
        item: &Item,
        parent: &[String],
        anchors: &mut AnchorPool,
    ) -> Option<(DocItem, Option<DocModule>)> {
        match item {
            Item::Fn {
                doc, is_pub, name, ..
            } => {
                if !*is_pub && doc.is_none() {
                    return None;
                }
                let (signature, params, ret) = self.fn_signature(item);
                Some((
                    self.make_item(
                        name,
                        DocKind::Fn,
                        parent,
                        anchors,
                        signature,
                        doc.as_deref(),
                        Vec::new(),
                        params,
                        ret,
                    ),
                    None,
                ))
            }
            Item::Const {
                doc,
                is_pub,
                name,
                ty,
                value,
                ..
            } => {
                if !*is_pub && doc.is_none() {
                    return None;
                }
                // A container const is both an item AND a namespace module.
                if let Expr::Container(container) = value {
                    let (kind, fields) = self.container_fields(container);
                    let signature = self.container_signature(name, container, "const");
                    let item_doc = self.make_item(
                        name,
                        kind,
                        parent,
                        anchors,
                        signature,
                        doc.as_deref(),
                        fields,
                        Vec::new(),
                        None,
                    );
                    // Only spin up a namespace page when the container actually has
                    // documented nested members (a fields-only struct or a bare
                    // enum needs no separate module page — its fields render inline
                    // on the item).
                    let submodule = self.container_module(name, parent, container, anchors);
                    let submodule = if submodule.items.is_empty() {
                        None
                    } else {
                        Some(submodule)
                    };
                    Some((item_doc, submodule))
                } else {
                    let signature = self.const_signature("const", name, ty.as_ref(), item);
                    Some((
                        self.make_item(
                            name,
                            DocKind::Const,
                            parent,
                            anchors,
                            signature,
                            doc.as_deref(),
                            Vec::new(),
                            Vec::new(),
                            None,
                        ),
                        None,
                    ))
                }
            }
            Item::Var {
                doc,
                is_pub,
                name,
                ty,
                ..
            } => {
                if !*is_pub && doc.is_none() {
                    return None;
                }
                let signature = self.const_signature("var", name, ty.as_ref(), item);
                Some((
                    self.make_item(
                        name,
                        DocKind::Var,
                        parent,
                        anchors,
                        signature,
                        doc.as_deref(),
                        Vec::new(),
                        Vec::new(),
                        None,
                    ),
                    None,
                ))
            }
            // Tests, comptime blocks: not documented.
            _ => None,
        }
    }

    /// Assembles a [`DocItem`] from its parts, extracting its examples and
    /// allocating a unique anchor.
    // Justified: this is the single assembly point for the seven independent,
    // already-computed `DocItem` fields (name/kind/parent/signature/doc/fields/
    // params/ret). Bundling them into a parameter struct would only move the same
    // arity to the struct's construction site with no readability gain.
    #[allow(clippy::too_many_arguments)]
    fn make_item(
        &self,
        name: &str,
        kind: DocKind,
        parent: &[String],
        anchors: &mut AnchorPool,
        signature: String,
        doc: Option<&str>,
        fields: Vec<DocField>,
        params: Vec<DocParam>,
        ret: Option<String>,
    ) -> DocItem {
        let doc_md = doc.map(render::doc_to_markdown).unwrap_or_default();
        let qualified = if parent.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", parent.join("."), name)
        };
        let anchor = anchors.unique(&qualified);
        let examples = doctest::extract_examples(&qualified, &doc_md);
        DocItem {
            name: name.to_string(),
            kind,
            module_path: parent.to_vec(),
            anchor,
            signature,
            doc_md,
            fields,
            params,
            ret,
            examples,
        }
    }

    /// Builds the namespace [`DocModule`] for a container const: its own doc plus
    /// its pub nested declarations (methods / member consts), each a [`DocItem`].
    fn container_module(
        &self,
        name: &str,
        parent: &[String],
        container: &Container,
        anchors: &mut AnchorPool,
    ) -> DocModule {
        let mut path = parent.to_vec();
        path.push(name.to_string());
        let mut items = Vec::new();
        for member in &container.members {
            if let Member::Decl(decl) = member {
                if let Some((doc_item, _sub)) = self.item_to_doc(decl, &path, anchors) {
                    // We flatten nested containers into the dotted path; their own
                    // submodule (if any) is dropped here to keep one level of
                    // namespacing (anchors stay unique via the pool).
                    items.push(doc_item);
                }
            }
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        DocModule {
            path,
            doc_md: String::new(),
            items,
            examples: Vec::new(),
        }
    }

    /// Renders a function's signature line + structured params + return type. Types
    /// come from the type checker when available, else from the AST.
    fn fn_signature(&self, item: &Item) -> (String, Vec<DocParam>, Option<String>) {
        let Item::Fn {
            is_pub,
            is_extern,
            is_export,
            is_inline,
            name,
            params,
            is_varargs,
            ret,
            ..
        } = item
        else {
            return (String::new(), Vec::new(), None);
        };

        // Try the type checker: item span → DefId → binding type → FnSig.
        let sig = self.fn_sig_of(item);

        let mut doc_params = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let ty = sig
                .as_ref()
                .and_then(|s| s.params.get(i))
                .map(|pi| self.typed.arena.fmt(pi.ty))
                .map(|checked| prefer_ast_if_deferred(checked, &p.ty))
                .unwrap_or_else(|| ast_type_string(&p.ty));
            doc_params.push(DocParam {
                name: if p.name.is_empty() {
                    "_".to_string()
                } else {
                    p.name.clone()
                },
                ty,
                is_comptime: p.is_comptime,
            });
        }
        let ret_str = sig
            .as_ref()
            .map(|s| self.typed.arena.fmt(s.ret))
            .map(|checked| prefer_ast_if_deferred(checked, ret))
            .unwrap_or_else(|| ast_type_string(ret));

        // Assemble the one-line signature.
        let mut line = String::new();
        if *is_pub {
            line.push_str("pub ");
        }
        if *is_extern {
            line.push_str("extern ");
        }
        if *is_export {
            line.push_str("export ");
        }
        if *is_inline {
            line.push_str("inline ");
        }
        line.push_str("fn ");
        line.push_str(name);
        line.push('(');
        let mut parts = Vec::new();
        for p in &doc_params {
            if p.is_comptime {
                parts.push(format!("comptime {}: {}", p.name, p.ty));
            } else {
                parts.push(format!("{}: {}", p.name, p.ty));
            }
        }
        if *is_varargs {
            parts.push("...".to_string());
        }
        line.push_str(&parts.join(", "));
        line.push_str(") ");
        line.push_str(&ret_str);

        (line, doc_params, Some(ret_str))
    }

    /// Looks up the [`k2_types::FnSig`] of a `fn` item via its DefId → binding type.
    fn fn_sig_of(&self, item: &Item) -> Option<k2_types::FnSig> {
        let def = self.def_for_span(item.span())?;
        let ty = *self.typed.binding_types.get(&def.id)?;
        if let Type::Fn(sigid) = self.typed.arena.get(ty) {
            self.typed.arena.fnsigs.get(sigid.0 as usize).cloned()
        } else {
            None
        }
    }

    /// Finds the item-level [`Def`] whose span equals `span` (resolver keys an item
    /// def by its full item span).
    fn def_for_span(&self, span: k2_syntax::Span) -> Option<&Def> {
        self.resolved.defs.iter().find(|d| {
            d.kind == DefKind::Item && d.span.start == span.start && d.span.end == span.end
        })
    }

    /// Renders a `const`/`var` signature: `const NAME: T` using the checker's type
    /// when available, else the AST type expression, else `comptime` if neither.
    fn const_signature(&self, kw: &str, name: &str, ty: Option<&Expr>, item: &Item) -> String {
        let rendered = self
            .def_for_span(item.span())
            .and_then(|d| self.typed.binding_types.get(&d.id))
            .map(|tid| self.typed.arena.fmt(*tid))
            .or_else(|| ty.map(ast_type_string));
        match rendered {
            Some(t) => format!("{kw} {name}: {t}"),
            None => format!("{kw} {name}"),
        }
    }

    /// Renders a container's signature line (`const T = struct` etc.).
    fn container_signature(&self, name: &str, container: &Container, kw: &str) -> String {
        let tail = match &container.kind {
            ContainerKind::Struct {
                is_extern,
                is_packed,
            } => {
                let pre = if *is_packed {
                    "packed "
                } else if *is_extern {
                    "extern "
                } else {
                    ""
                };
                format!("{pre}struct")
            }
            ContainerKind::Enum { tag } => match tag {
                Some(t) => format!("enum({})", ast_type_string(t)),
                None => "enum".to_string(),
            },
            ContainerKind::Union { tag } => match tag {
                UnionTag::None => "union".to_string(),
                UnionTag::Inferred => "union(enum)".to_string(),
                UnionTag::Typed(t) => format!("union({})", ast_type_string(t)),
            },
        };
        format!("{kw} {name} = {tail}")
    }

    /// Collects a container's documented fields/variants (pub-filtered for structs;
    /// enum variants are always public). Returns the kind + the field list.
    fn container_fields(&self, container: &Container) -> (DocKind, Vec<DocField>) {
        let kind = match container.kind {
            ContainerKind::Struct { .. } => DocKind::Struct,
            ContainerKind::Enum { .. } => DocKind::Enum,
            ContainerKind::Union { .. } => DocKind::Union,
        };
        let mut fields = Vec::new();
        for member in &container.members {
            if let Member::Field(field) = member {
                if let Some(f) = self.field_to_doc(field, kind) {
                    fields.push(f);
                }
            }
        }
        (kind, fields)
    }

    /// Converts one container field/variant to a [`DocField`], honoring `pub`
    /// visibility on struct/union fields (enum variants are always public).
    fn field_to_doc(&self, field: &Field, kind: DocKind) -> Option<DocField> {
        // Enum variants have no `ty`; they are always visible. A private struct/
        // union field is hidden UNLESS it carries a doc comment (mirroring the
        // private-but-documented item policy).
        let is_variant = matches!(kind, DocKind::Enum);
        let hidden = !is_variant && !field.is_pub && field.doc.is_none();
        if hidden {
            return None;
        }
        let ty = self.field_type_string(field);
        Some(DocField {
            name: field.name.clone(),
            ty,
            doc_md: field
                .doc
                .as_deref()
                .map(render::doc_to_markdown)
                .unwrap_or_default(),
        })
    }

    /// Renders a field's type: the checked type at the field's type-expr span when
    /// available, else the AST type expression (`void` for an enum variant).
    fn field_type_string(&self, field: &Field) -> String {
        match &field.ty {
            None => "void".to_string(),
            Some(expr) => self
                .typed
                .type_at(expr.span())
                .map(|tid| self.typed.arena.fmt(tid))
                .unwrap_or_else(|| ast_type_string(expr)),
        }
    }
}

/// The name of an item, if it has one.
fn item_name(item: &Item) -> Option<&str> {
    match item {
        Item::Const { name, .. } | Item::Var { name, .. } | Item::Fn { name, .. } => Some(name),
        _ => None,
    }
}

// =========================================================================
//  AST type-expression stringifier (the degraded path)
// =========================================================================

/// Prefers the AST type spelling over a type-checker rendering that leaked the
/// internal `deferred` placeholder.
///
/// For a generic free function whose parameter/return types mention a
/// `comptime T: type`, the checker renders the un-monomorphized component as
/// `deferred` (e.g. `deferred`, `?deferred`, `[]deferred`) — an internal compiler
/// token, not a user-facing type. In that case we fall back to the original source
/// spelling (`T`, `?T`, `[]T`), which the AST stringifier preserves. The check is
/// the bare token `deferred` delimited by non-identifier characters, so a real type
/// whose name merely contains the substring (e.g. `DeferredQueue`) is unaffected.
fn prefer_ast_if_deferred(checked: String, ast: &Expr) -> String {
    if contains_deferred_token(&checked) {
        ast_type_string(ast)
    } else {
        checked
    }
}

/// `true` iff `s` contains the bare identifier `deferred` (delimited by
/// non-identifier characters), as opposed to merely the substring.
fn contains_deferred_token(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut from = 0;
    while let Some(rel) = s[from..].find("deferred") {
        let start = from + rel;
        let end = start + "deferred".len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// `true` if `b` is an ASCII identifier byte (`[A-Za-z0-9_]`).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A total, recursive stringifier for a type [`Expr`], used when the type checker
/// has no resolved type (a file with type errors, or a generic-deferred type). It
/// mirrors the postfix-modifier shapes of the k2 type grammar. Never panics.
pub fn ast_type_string(expr: &Expr) -> String {
    match expr {
        Expr::Ident { name, .. } => name.clone(),
        Expr::AnyType { .. } => "anytype".to_string(),
        Expr::Optional { inner, .. } => format!("?{}", ast_type_string(inner)),
        Expr::Pointer {
            is_const, inner, ..
        } => format!("*{}{}", const_kw(*is_const), ast_type_string(inner)),
        Expr::Slice {
            is_const, inner, ..
        } => format!("[]{}{}", const_kw(*is_const), ast_type_string(inner)),
        Expr::ManyPtr {
            is_const, inner, ..
        } => format!("[*]{}{}", const_kw(*is_const), ast_type_string(inner)),
        Expr::ArrayType { len, inner, .. } => {
            format!("[{}]{}", ast_type_string(len), ast_type_string(inner))
        }
        Expr::ErrorUnion { err, ok, .. } => match err {
            Some(e) => format!("{}!{}", ast_type_string(e), ast_type_string(ok)),
            None => format!("!{}", ast_type_string(ok)),
        },
        Expr::ErrorSet { fields, .. } => format!("error{{{}}}", fields.join(", ")),
        Expr::Field { base, field, .. } => format!("{}.{}", ast_type_string(base), field),
        Expr::Call { callee, args, .. } => {
            let a: Vec<String> = args.iter().map(ast_type_string).collect();
            format!("{}({})", ast_type_string(callee), a.join(", "))
        }
        Expr::Builtin { name, args, .. } => {
            let a: Vec<String> = args.iter().map(ast_type_string).collect();
            format!("{name}({})", a.join(", "))
        }
        Expr::FnType { params, ret, .. } => {
            let p: Vec<String> = params.iter().map(|p| ast_type_string(&p.ty)).collect();
            format!("fn({}) {}", p.join(", "), ast_type_string(ret))
        }
        Expr::Container(c) => match c.kind {
            ContainerKind::Struct { .. } => "struct".to_string(),
            ContainerKind::Enum { .. } => "enum".to_string(),
            ContainerKind::Union { .. } => "union".to_string(),
        },
        Expr::Int { text, .. } | Expr::Float { text, .. } => text.clone(),
        // A non-type expression in type position (shouldn't normally happen): fall
        // back to a stable placeholder rather than panic.
        _ => "_".to_string(),
    }
}

/// The `const ` qualifier word for pointer/slice rendering.
fn const_kw(is_const: bool) -> &'static str {
    if is_const {
        "const "
    } else {
        ""
    }
}
