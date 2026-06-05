//! The documentation RENDERER: a small, total, pure-`std` Markdown-in-doc-comment
//! → HTML core plus the HTML and Markdown site emitters.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! Everything here is deliberately a CommonMark *subset* chosen so the renderer is
//! TOTAL — it never panics, and every byte of generated HTML is correctly escaped
//! and tag-balanced. Three properties hold by construction:
//!
//! * **Well-formed.** HTML is emitted only through matched open/close helpers
//!   ([`tag`]); user-controlled text never becomes a raw tag.
//! * **Injection-free.** All text/attribute content is escaped ([`esc_html`] /
//!   [`esc_attr`]); a `[x](javascript:…)` link degrades to plain text.
//! * **Self-contained.** Each page inlines the one [`CSS`] constant — no external
//!   CSS/JS, no web fonts, no network fetch.
//!
//! The block scanner is shared with [`crate::doc::doctest`]: a fenced code block is
//! parsed once, here for rendering and there for doc-test extraction.

use std::collections::HashMap;

use super::{DocItem, DocKind, DocModel, DocModule};

/// The one inline stylesheet, injected verbatim into every page's `<style>`.
/// System font stack, a centered max-width column, monospace code with a light
/// background + border, bordered tables, a link color, and colored status pills.
/// No `@import`, no web fonts — fully offline and self-contained.
const CSS: &str = "\
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body {
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif;
  line-height: 1.55; margin: 0 auto; max-width: 52rem; padding: 1.5rem 1.25rem 4rem;
  color: #1b1f23; background: #fff;
}
h1, h2, h3, h4 { line-height: 1.25; }
h1 { font-size: 1.8rem; border-bottom: 1px solid #e1e4e8; padding-bottom: .3rem; }
h2 { font-size: 1.4rem; margin-top: 2rem; }
a { color: #0366d6; text-decoration: none; }
a:hover { text-decoration: underline; }
code, pre {
  font-family: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
  font-size: .92em;
}
code { background: #f3f4f6; border: 1px solid #e1e4e8; border-radius: 3px; padding: 0 .3em; }
pre { background: #f6f8fa; border: 1px solid #e1e4e8; border-radius: 6px; padding: .8rem 1rem; overflow-x: auto; }
pre code { background: none; border: none; padding: 0; }
table { border-collapse: collapse; margin: .6rem 0; }
th, td { border: 1px solid #d0d7de; padding: .3rem .7rem; text-align: left; vertical-align: top; }
th { background: #f6f8fa; }
section { margin: 1.4rem 0; padding-top: .4rem; border-top: 1px solid #eaecef; }
.sig { display: block; background: #f6f8fa; border: 1px solid #e1e4e8; border-radius: 6px; padding: .6rem .9rem; }
.kind { font-size: .7rem; font-weight: 600; text-transform: uppercase; letter-spacing: .04em;
        color: #57606a; border: 1px solid #d0d7de; border-radius: 999px; padding: .05rem .5rem; }
.summary { color: #57606a; }
.badge { font-size: .72rem; font-weight: 600; border-radius: 999px; padding: .05rem .55rem; margin-left: .4rem; }
.badge.run  { background: #ddf4ff; color: #0969da; }
.badge.norun{ background: #f0f0f4; color: #57606a; }
.badge.pass { background: #d3f5d8; color: #1a7f37; }
.badge.fail { background: #ffd7d5; color: #cf222e; }
.nav { margin-bottom: 1.2rem; font-size: .9rem; }
footer { margin-top: 3rem; color: #8b949e; font-size: .8rem; border-top: 1px solid #e1e4e8; padding-top: .6rem; }
";

// =========================================================================
//  Escaping (the foundation of well-formed, injection-free output)
// =========================================================================

/// Escapes text for HTML *element content*: `& < > " '` → entities. Applied to
/// every piece of text and code we emit, so no user byte can open a tag.
pub fn esc_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes a string for use inside a double-quoted HTML *attribute*. Same set as
/// [`esc_html`]; named separately so attribute sites read intentionally.
fn esc_attr(s: &str) -> String {
    esc_html(s)
}

/// Emits `<name attrs>inner</name>` with a guaranteed matching close tag. `attrs`
/// is appended verbatim (callers pass already-escaped attribute strings), so the
/// only way a tag is produced is through this helper — the source of the
/// well-formedness guarantee.
fn tag(name: &str, attrs: &str, inner: &str) -> String {
    if attrs.is_empty() {
        format!("<{name}>{inner}</{name}>")
    } else {
        format!("<{name} {attrs}>{inner}</{name}>")
    }
}

// =========================================================================
//  `///` → Markdown normalization
// =========================================================================

/// Recovers clean Markdown from the AST's stored doc string. The parser keeps the
/// `///` prefix per line and joins with `'\n'`; here each line drops a leading
/// `///` (exactly) and then ONE optional leading space, preserving any further
/// indentation (so an indented code block stays indented). Lines without the
/// prefix (already-stripped, e.g. file-level entries handled elsewhere) pass
/// through unchanged.
pub fn doc_to_markdown(stored: &str) -> String {
    let mut out = String::with_capacity(stored.len());
    let mut first = true;
    for line in stored.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        let body = match line.strip_prefix("///") {
            Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
            None => line,
        };
        out.push_str(body.trim_end());
    }
    out
}

// =========================================================================
//  Markdown → HTML (a total CommonMark subset)
// =========================================================================

/// A block produced by the line scanner. The fenced-code variant carries its info
/// string so the doc-test extractor can classify it; the renderer ignores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// `# … ######` heading: level (1..=6) + the inline text.
    Heading(u8, String),
    /// A paragraph of inline text (already joined with spaces).
    Paragraph(String),
    /// A bullet (`-`/`*`) list: the raw inline text of each item.
    BulletList(Vec<String>),
    /// An ordered (`N.`) list: the raw inline text of each item.
    OrderedList(Vec<String>),
    /// A fenced code block: `(info_string, verbatim_body)`.
    Code(String, String),
}

/// Splits Markdown text into [`Block`]s with a single, total line scan. Unknown
/// constructs degrade to paragraphs; an unterminated fence is closed at EOF. This
/// is the ONE scanner both the renderer and the doc-test extractor consume.
pub fn scan_blocks(md: &str) -> Vec<Block> {
    let lines: Vec<&str> = md.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // Fenced code block: ``` or ~~~ (>=3) optionally followed by an info string.
        if let Some(fence) = fence_marker(trimmed) {
            let info = trimmed[fence.len()..].trim().to_string();
            i += 1;
            let mut body = String::new();
            while i < lines.len() {
                let l = lines[i];
                if fence_marker(l.trim_start())
                    .map(|m| m.starts_with(&fence[..1]) && m.len() >= fence.len())
                    .unwrap_or(false)
                    && l.trim_start()[fence_marker(l.trim_start()).unwrap().len()..]
                        .trim()
                        .is_empty()
                {
                    i += 1; // consume the closing fence
                    break;
                }
                body.push_str(l);
                body.push('\n');
                i += 1;
            }
            blocks.push(Block::Code(info, body));
            continue;
        }

        // Blank line: a block separator.
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // ATX heading: 1..=6 `#` then a space.
        if let Some((level, text)) = atx_heading(trimmed) {
            blocks.push(Block::Heading(level, text.to_string()));
            i += 1;
            continue;
        }

        // Bullet list: a run of lines each starting `- ` or `* `.
        if bullet_item(trimmed).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                match bullet_item(t) {
                    Some(item) => {
                        items.push(item.to_string());
                        i += 1;
                    }
                    None => break,
                }
            }
            blocks.push(Block::BulletList(items));
            continue;
        }

        // Ordered list: a run of lines each starting `N. `.
        if ordered_item(trimmed).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                match ordered_item(t) {
                    Some(item) => {
                        items.push(item.to_string());
                        i += 1;
                    }
                    None => break,
                }
            }
            blocks.push(Block::OrderedList(items));
            continue;
        }

        // Otherwise: a paragraph, accumulated until a blank line / new block.
        let mut para = String::new();
        while i < lines.len() {
            let t = lines[i].trim_start();
            if t.is_empty()
                || fence_marker(t).is_some()
                || atx_heading(t).is_some()
                || bullet_item(t).is_some()
                || ordered_item(t).is_some()
            {
                break;
            }
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(lines[i].trim());
            i += 1;
        }
        blocks.push(Block::Paragraph(para));
    }
    blocks
}

/// If `line` opens/closes a fence, returns the fence marker (e.g. ```` ``` ````);
/// otherwise `None`. A fence is 3+ backticks or 3+ tildes at the line start.
fn fence_marker(line: &str) -> Option<&str> {
    for marker in ["```", "~~~"] {
        if line.starts_with(marker) {
            let ch = marker.chars().next().unwrap();
            let n = line.chars().take_while(|&c| c == ch).count();
            if n >= 3 {
                return Some(&line[..n]);
            }
        }
    }
    None
}

/// Parses an ATX heading `#`..`######` + a space, returning `(level, text)`.
fn atx_heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        if let Some(text) = rest.strip_prefix(' ') {
            return Some((hashes as u8, text.trim_end()));
        }
    }
    None
}

/// Parses a `- ` / `* ` bullet item, returning the item text.
fn bullet_item(line: &str) -> Option<&str> {
    line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
}

/// Parses an ordered item `N. ` / `N) `, returning the item text after the marker.
fn ordered_item(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &line[digits..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

/// Renders a doc-comment's Markdown to an HTML fragment. `links` maps a known item
/// name → its `#anchor`/page URL so an inline `` `Name` `` code span becomes a
/// cross-link. Total: any input yields balanced, escaped HTML.
pub fn md_to_html(md: &str, links: &HashMap<String, String>) -> String {
    let mut out = String::new();
    for block in scan_blocks(md) {
        match block {
            Block::Heading(level, text) => {
                let l = level.clamp(1, 6);
                let id = slug(&text);
                out.push_str(&tag(
                    &format!("h{l}"),
                    &format!("id=\"{}\"", esc_attr(&id)),
                    &inline_to_html(&text, links),
                ));
                out.push('\n');
            }
            Block::Paragraph(text) => {
                if !text.is_empty() {
                    out.push_str(&tag("p", "", &inline_to_html(&text, links)));
                    out.push('\n');
                }
            }
            Block::BulletList(items) => out.push_str(&render_list("ul", &items, links)),
            Block::OrderedList(items) => out.push_str(&render_list("ol", &items, links)),
            Block::Code(info, body) => {
                let lang = code_lang_class(&info);
                out.push_str(&format!(
                    "<pre><code{lang}>{}</code></pre>\n",
                    esc_html(body.trim_end_matches('\n'))
                ));
            }
        }
    }
    out
}

/// Renders a `<ul>`/`<ol>` from already-extracted item texts.
fn render_list(name: &str, items: &[String], links: &HashMap<String, String>) -> String {
    let mut inner = String::new();
    for it in items {
        inner.push_str(&tag("li", "", &inline_to_html(it, links)));
    }
    format!("{}\n", tag(name, "", &inner))
}

/// The `class="lang-k2"` attribute for a fenced block whose info string names k2
/// (or is empty); other languages get a class naming the language; truly bare
/// fences default to k2 (the doc corpus is k2). Always returns a leading-space
/// attribute or empty string.
fn code_lang_class(info: &str) -> String {
    let first = info.split([',', ' ']).next().unwrap_or("").trim();
    let lang = if first.is_empty() { "k2" } else { first };
    format!(" class=\"lang-{}\"", esc_attr(lang))
}

/// Applies inline Markdown to a single run of text: code spans, links, bold/italic.
/// Everything is escaped; code-span and link bodies are exempt from further inline
/// parsing. A code span whose text equals a known item name becomes a cross-link.
fn inline_to_html(text: &str, links: &HashMap<String, String>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                // A code span runs to the next backtick (or to EOL if unmatched).
                if let Some(end) = find_char(&chars, i + 1, '`') {
                    let span: String = chars[i + 1..end].iter().collect();
                    let escaped = esc_html(&span);
                    let code = tag("code", "", &escaped);
                    // Intra-doc cross-link: an exact item-name code span links out.
                    if let Some(href) = links.get(span.trim()) {
                        out.push_str(&tag("a", &format!("href=\"{}\"", esc_attr(href)), &code));
                    } else {
                        out.push_str(&code);
                    }
                    i = end + 1;
                    continue;
                }
                out.push_str("&#96;");
                i += 1;
            }
            '[' => {
                // A link `[text](url)`.
                if let Some((label, url, next)) = parse_link(&chars, i) {
                    if let Some(safe) = safe_url(&url) {
                        out.push_str(&tag(
                            "a",
                            &format!("href=\"{}\"", esc_attr(&safe)),
                            &inline_to_html(&label, links),
                        ));
                    } else {
                        // Unsafe scheme (e.g. `javascript:`): render as plain text.
                        out.push_str(&esc_html(&format!("[{label}]({url})")));
                    }
                    i = next;
                    continue;
                }
                out.push_str("&#91;");
                i += 1;
            }
            '*' => {
                // `**bold**` / `*em*`.
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    if let Some(end) = find_str(&chars, i + 2, "**") {
                        let inner: String = chars[i + 2..end].iter().collect();
                        out.push_str(&tag("strong", "", &inline_to_html(&inner, links)));
                        i = end + 2;
                        continue;
                    }
                } else if let Some(end) = find_char(&chars, i + 1, '*') {
                    let inner: String = chars[i + 1..end].iter().collect();
                    if !inner.is_empty() {
                        out.push_str(&tag("em", "", &inline_to_html(&inner, links)));
                        i = end + 1;
                        continue;
                    }
                }
                out.push_str("&#42;");
                i += 1;
            }
            _ => {
                out.push_str(&esc_html(&c.to_string()));
                i += 1;
            }
        }
    }
    out
}

/// Finds the next index `>= from` of `needle` in `chars`, or `None`.
fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == needle)
}

/// Finds the next index `>= from` where the 2-char `needle` begins, or `None`.
fn find_str(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || from + n.len() > chars.len() {
        return None;
    }
    (from..=chars.len() - n.len()).find(|&j| chars[j..j + n.len()] == n[..])
}

/// Parses a `[label](url)` link starting at `chars[start] == '['`, returning
/// `(label, url, index_past_close_paren)` or `None` if it is not a well-formed
/// link (in which case the caller renders the `[` literally).
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close_bracket = find_char(chars, start + 1, ']')?;
    if close_bracket + 1 >= chars.len() || chars[close_bracket + 1] != '(' {
        return None;
    }
    let close_paren = find_char(chars, close_bracket + 2, ')')?;
    let label: String = chars[start + 1..close_bracket].iter().collect();
    let url: String = chars[close_bracket + 2..close_paren].iter().collect();
    Some((label, url, close_paren + 1))
}

/// Returns the URL if its scheme is safe to put in an `href` (http/https/mailto, a
/// relative path, or an in-page `#anchor`); rejects everything else (notably
/// `javascript:`/`data:`) so a malicious doc cannot inject script.
fn safe_url(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    let lower = u.to_ascii_lowercase();
    let has_scheme = u
        .split_once(':')
        .map(|(s, _)| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        })
        .unwrap_or(false);
    if !has_scheme
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || u.starts_with('#')
        || u.starts_with('/')
        || u.starts_with("./")
        || u.starts_with("../")
    {
        Some(u.to_string())
    } else {
        None
    }
}

/// Produces a stable, html-safe slug from arbitrary text: lowercase, non
/// `[a-z0-9]` runs → a single `-`, trimmed of leading/trailing `-`. Empty input
/// yields `"_"` so a slug is never empty (keeps anchors valid).
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed
    }
}

// =========================================================================
//  HTML site emission
// =========================================================================

/// One generated page: its filename and its full HTML/Markdown text.
pub struct Page {
    /// The output filename (a slug + extension; no path separators).
    pub filename: String,
    /// The full page contents.
    pub contents: String,
}

/// Builds the global `name → url` link map used for intra-doc cross-links. Each
/// pub item maps to `<prefix><page>#<anchor>`. The map is keyed by the bare item
/// name (and by its dotted path) so a `` `Name` `` code span anywhere resolves to
/// it. `prefix` is the per-file filename prefix (empty in single-file mode; a
/// `{slug}--` in directory mode) so the URL targets the ACTUAL written filename.
fn build_link_map(model: &DocModel, prefix: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for module in &model.modules {
        let page = module_page_name(module, prefix);
        for item in &module.items {
            let url = format!("{page}#{}", item.anchor);
            map.entry(item.name.clone()).or_insert_with(|| url.clone());
            map.insert(item.qualified_name(), url);
        }
    }
    map
}

/// The output filename for a module page, with the per-file `prefix` applied so the
/// link/nav targets match the ACTUAL written filenames. The root module lands in
/// `{prefix}index.html`; every other module gets `{prefix}module-<slug>.html`.
fn module_page_name(module: &DocModule, prefix: &str) -> String {
    if module.path.is_empty() {
        format!("{prefix}index.html")
    } else {
        format!("{prefix}module-{}.html", slug(&module.path.join("-")))
    }
}

/// Renders the whole model to a self-contained HTML site with NO filename prefix
/// (single-file mode). Equivalent to [`emit_html_prefixed`] with an empty prefix.
pub fn emit_html(model: &DocModel) -> Vec<Page> {
    emit_html_prefixed(model, "")
}

/// Renders the whole model to a self-contained HTML site: an index page plus one
/// page per non-root module. Deterministic (modules/items are pre-sorted by the
/// model builder). Returns the page list for the caller to write.
///
/// `prefix` is the per-file filename prefix (empty in single-file mode; `{slug}--`
/// in directory mode). It is applied to BOTH the page filenames AND every generated
/// URL (cross-links, the module index, and the `← Index` nav), so links resolve to
/// the actual written files rather than 404-ing on an un-prefixed name.
pub fn emit_html_prefixed(model: &DocModel, prefix: &str) -> Vec<Page> {
    let links = build_link_map(model, prefix);
    let mut pages = Vec::new();

    // The index page: title, file doc, the module index, and the root module's
    // items rendered inline.
    let mut body = String::new();
    body.push_str(&tag(
        "h1",
        "",
        &esc_html(&format!("{} — k2 documentation", model.label)),
    ));
    if !model.file_doc.is_empty() {
        body.push_str(&format!(
            "<div class=\"doc\">{}</div>\n",
            md_to_html(&model.file_doc, &links)
        ));
    }

    // Module index (a table of contents across every module).
    body.push_str(&tag("h2", "id=\"index\"", "Index"));
    body.push('\n');
    for module in &model.modules {
        let page = module_page_name(module, prefix);
        let title = if module.path.is_empty() {
            "(root)".to_string()
        } else {
            module.path.join(".")
        };
        body.push_str(&tag("h3", "", &esc_html(&title)));
        body.push('\n');
        let mut ul = String::new();
        for item in &module.items {
            let href = format!("{page}#{}", item.anchor);
            let entry = format!(
                "{} {} — {}",
                tag("span", "class=\"kind\"", item.kind.label()),
                tag(
                    "a",
                    &format!("href=\"{}\"", esc_attr(&href)),
                    &tag("code", "", &esc_html(&item.name))
                ),
                summary_html(item, &links),
            );
            ul.push_str(&tag("li", "", &entry));
        }
        if module.items.is_empty() {
            ul.push_str(&tag(
                "li",
                "class=\"summary\"",
                "(no documented public items)",
            ));
        }
        body.push_str(&tag("ul", "", &ul));
        body.push('\n');
    }

    // The root module's items, rendered in full on the index page.
    if let Some(root) = model.modules.iter().find(|m| m.path.is_empty()) {
        if !root.items.is_empty() {
            body.push_str(&tag("h2", "id=\"items\"", "Items"));
            body.push('\n');
            for item in &root.items {
                body.push_str(&render_item_section(item, &links));
            }
        }
    }

    pages.push(Page {
        filename: format!("{prefix}index.html"),
        contents: html_page(&model.label, &nav_back(false, prefix), &body),
    });

    // One page per non-root module.
    for module in &model.modules {
        if module.path.is_empty() {
            continue;
        }
        let mut mbody = String::new();
        mbody.push_str(&tag(
            "h1",
            "",
            &tag("code", "", &esc_html(&module.path.join("."))),
        ));
        if !module.doc_md.is_empty() {
            mbody.push_str(&format!(
                "<div class=\"doc\">{}</div>\n",
                md_to_html(&module.doc_md, &links)
            ));
        }
        for item in &module.items {
            mbody.push_str(&render_item_section(item, &links));
        }
        if module.items.is_empty() {
            mbody.push_str(&tag(
                "p",
                "class=\"summary\"",
                "No documented public members.",
            ));
        }
        pages.push(Page {
            filename: module_page_name(module, prefix),
            contents: html_page(&module.path.join("."), &nav_back(true, prefix), &mbody),
        });
    }

    pages
}

/// The first-paragraph summary of an item, rendered inline (for the index list).
fn summary_html(item: &DocItem, links: &HashMap<String, String>) -> String {
    match scan_blocks(&item.doc_md).into_iter().find_map(|b| match b {
        Block::Paragraph(p) if !p.is_empty() => Some(p),
        _ => None,
    }) {
        Some(p) => tag("span", "class=\"summary\"", &inline_to_html(&p, links)),
        None => tag("span", "class=\"summary\"", "<em>(undocumented)</em>"),
    }
}

/// Renders one item as a `<section id="anchor">`: a kind pill, the signature, the
/// rendered doc, a params/fields/variants table, and any examples (with badges).
fn render_item_section(item: &DocItem, links: &HashMap<String, String>) -> String {
    let mut s = String::new();
    let head = format!(
        "{} {}",
        tag("span", "class=\"kind\"", item.kind.label()),
        tag("code", "class=\"sig\"", &esc_html(&item.signature)),
    );
    s.push_str(&tag(
        "h2",
        &format!("id=\"{}\"", esc_attr(&item.anchor)),
        &head,
    ));
    s.push('\n');

    if item.doc_md.is_empty() {
        s.push_str(&tag("p", "class=\"summary\"", "<em>(undocumented)</em>"));
        s.push('\n');
    } else {
        s.push_str(&format!(
            "<div class=\"doc\">{}</div>\n",
            md_to_html(&item.doc_md, links)
        ));
    }

    // Parameters table (functions).
    if matches!(item.kind, DocKind::Fn) && !item.params.is_empty() {
        s.push_str(&tag("h3", "", "Parameters"));
        let mut rows = String::from("<tr><th>name</th><th>type</th></tr>");
        for p in &item.params {
            let name = if p.is_comptime {
                format!("comptime {}", p.name)
            } else {
                p.name.clone()
            };
            rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                tag("code", "", &esc_html(&name)),
                tag("code", "", &esc_html(&p.ty)),
            ));
        }
        s.push_str(&tag("table", "", &rows));
        s.push('\n');
    }
    if matches!(item.kind, DocKind::Fn) {
        if let Some(ret) = &item.ret {
            s.push_str(&format!(
                "<p><strong>Returns:</strong> {}</p>\n",
                tag("code", "", &esc_html(ret))
            ));
        }
    }

    // Fields (struct/union) or variants (enum).
    if !item.fields.is_empty() {
        let heading = match item.kind {
            DocKind::Enum => "Variants",
            _ => "Fields",
        };
        s.push_str(&tag("h3", "", heading));
        let mut rows = String::from("<tr><th>name</th><th>type</th><th>docs</th></tr>");
        for f in &item.fields {
            rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                tag("code", "", &esc_html(&f.name)),
                tag("code", "", &esc_html(&f.ty)),
                if f.doc_md.is_empty() {
                    String::new()
                } else {
                    inline_summary(&f.doc_md, links)
                },
            ));
        }
        s.push_str(&tag("table", "", &rows));
        s.push('\n');
    }

    // Examples (fenced code blocks), with mode/test badges.
    if !item.examples.is_empty() {
        s.push_str(&tag("h3", "", "Examples"));
        s.push('\n');
        for ex in &item.examples {
            let badge = example_badge(ex);
            s.push_str(&format!(
                "<div class=\"example\">{badge}<pre><code class=\"lang-k2\">{}</code></pre></div>\n",
                esc_html(ex.code.trim_end_matches('\n'))
            ));
        }
    }

    format!(
        "{}\n",
        tag(
            "section",
            &format!("id=\"sec-{}\"", esc_attr(&item.anchor)),
            &s
        )
    )
}

/// Renders a doc string's first paragraph inline (no `<p>` wrapper) for a table
/// cell.
fn inline_summary(md: &str, links: &HashMap<String, String>) -> String {
    match scan_blocks(md).into_iter().find_map(|b| match b {
        Block::Paragraph(p) if !p.is_empty() => Some(p),
        _ => None,
    }) {
        Some(p) => inline_to_html(&p, links),
        None => String::new(),
    }
}

/// The mode/status badge(s) for an example: its mode pill plus, if doc-tests ran, a
/// pass/fail pill.
fn example_badge(ex: &super::DocExample) -> String {
    use super::doctest::ExMode;
    let mode = match ex.mode {
        ExMode::Run => tag("span", "class=\"badge run\"", "run"),
        ExMode::NoRun => tag("span", "class=\"badge norun\"", "no-run"),
        ExMode::CompileFail => tag("span", "class=\"badge norun\"", "compile-fail"),
        ExMode::Ignore => tag("span", "class=\"badge norun\"", "ignore"),
    };
    let status = match ex.passed {
        Some(true) => tag("span", "class=\"badge pass\"", "pass"),
        Some(false) => tag("span", "class=\"badge fail\"", "fail"),
        None => String::new(),
    };
    format!("<div>{mode}{status}</div>")
}

/// The "← back to index" nav line. `is_module` links back to THIS FILE's own index
/// page (`{prefix}index.html`, not the directory-level listing); the index page
/// itself links to its own in-page table of contents. `prefix` is the per-file
/// filename prefix so the link targets the actual written index in directory mode.
fn nav_back(is_module: bool, prefix: &str) -> String {
    if is_module {
        let href = format!("href=\"{}index.html\"", esc_attr(prefix));
        tag("a", &href, "← Index")
    } else {
        tag("a", "href=\"#index\"", "Jump to index ↓")
    }
}

/// Wraps a body fragment in a complete, standalone HTML document with the inline
/// stylesheet. Every page produced this way is self-contained.
fn html_page(title: &str, nav: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         <style>\n{CSS}</style>\n\
         </head>\n<body>\n\
         <nav class=\"nav\">{nav}</nav>\n\
         {body}\
         <footer>Generated by <code>k2c doc</code>.</footer>\n\
         </body>\n</html>\n",
        esc_html(title)
    )
}

// =========================================================================
//  Markdown site emission (bonus)
// =========================================================================

/// Renders the model to a Markdown site: `index.md` plus one `<slug>.md` per
/// non-root module. Signatures go in fenced ```` ```k2 ```` blocks; doc text is
/// passed through verbatim (it is already Markdown); cross-links are `[name](url)`.
pub fn emit_markdown(model: &DocModel) -> Vec<Page> {
    emit_markdown_prefixed(model, "")
}

/// Like [`emit_markdown`], but with a per-file filename `prefix` (directory mode)
/// threaded into BOTH the page filenames and the `[name](url)` cross-link targets so
/// links resolve to the actual written `.md` files.
pub fn emit_markdown_prefixed(model: &DocModel, prefix: &str) -> Vec<Page> {
    let mut pages = Vec::new();

    let mut idx = String::new();
    idx.push_str(&format!("# {} — k2 documentation\n\n", model.label));
    if !model.file_doc.is_empty() {
        idx.push_str(&model.file_doc);
        idx.push_str("\n\n");
    }
    idx.push_str("## Index\n\n");
    for module in &model.modules {
        let page = md_page_name(module, prefix);
        let title = if module.path.is_empty() {
            "(root)".to_string()
        } else {
            module.path.join(".")
        };
        idx.push_str(&format!("### {title}\n\n"));
        if module.items.is_empty() {
            idx.push_str("_(no documented public items)_\n\n");
        }
        for item in &module.items {
            idx.push_str(&format!(
                "- **{}** [`{}`]({}#{})\n",
                item.kind.label(),
                item.name,
                page,
                item.anchor
            ));
        }
        idx.push('\n');
    }
    if let Some(root) = model.modules.iter().find(|m| m.path.is_empty()) {
        for item in &root.items {
            idx.push_str(&md_item(item));
        }
    }
    pages.push(Page {
        filename: format!("{prefix}index.md"),
        contents: idx,
    });

    for module in &model.modules {
        if module.path.is_empty() {
            continue;
        }
        let mut body = String::new();
        body.push_str(&format!("# `{}`\n\n", module.path.join(".")));
        if !module.doc_md.is_empty() {
            body.push_str(&module.doc_md);
            body.push_str("\n\n");
        }
        for item in &module.items {
            body.push_str(&md_item(item));
        }
        pages.push(Page {
            filename: md_page_name(module, prefix),
            contents: body,
        });
    }

    pages
}

/// The Markdown page filename for a module, with the per-file `prefix` applied.
fn md_page_name(module: &DocModule, prefix: &str) -> String {
    if module.path.is_empty() {
        format!("{prefix}index.md")
    } else {
        format!("{prefix}module-{}.md", slug(&module.path.join("-")))
    }
}

/// Escapes a string for safe interpolation into a GFM (pipe) table cell: a literal
/// `|` becomes `\|` (otherwise it splits the row into extra columns and corrupts the
/// table), and any newline becomes a space (a table cell is single-line). Applied at
/// every `| … |` interpolation site in [`md_item`].
fn md_cell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '|' => out.push_str("\\|"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// Renders one item as a Markdown section.
fn md_item(item: &DocItem) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "\n### <a id=\"{}\"></a>`{}`\n\n",
        item.anchor, item.name
    ));
    s.push_str(&format!("```k2\n{}\n```\n\n", item.signature));
    if !item.doc_md.is_empty() {
        s.push_str(&item.doc_md);
        s.push_str("\n\n");
    }
    if matches!(item.kind, DocKind::Fn) && !item.params.is_empty() {
        s.push_str("| param | type |\n|---|---|\n");
        for p in &item.params {
            let name = if p.is_comptime {
                format!("comptime {}", p.name)
            } else {
                p.name.clone()
            };
            s.push_str(&format!(
                "| `{}` | `{}` |\n",
                md_cell_escape(&name),
                md_cell_escape(&p.ty)
            ));
        }
        s.push('\n');
    }
    if !item.fields.is_empty() {
        let h = if matches!(item.kind, DocKind::Enum) {
            "variant"
        } else {
            "field"
        };
        s.push_str(&format!("| {h} | type | docs |\n|---|---|---|\n"));
        for f in &item.fields {
            let doc = scan_blocks(&f.doc_md)
                .into_iter()
                .find_map(|b| match b {
                    Block::Paragraph(p) if !p.is_empty() => Some(p),
                    _ => None,
                })
                .unwrap_or_default();
            s.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                md_cell_escape(&f.name),
                md_cell_escape(&f.ty),
                md_cell_escape(&doc)
            ));
        }
        s.push('\n');
    }
    for ex in &item.examples {
        s.push_str(&format!(
            "```k2\n{}\n```\n\n",
            ex.code.trim_end_matches('\n')
        ));
    }
    s
}

// =========================================================================
//  Test helper: tag-balance checker
// =========================================================================

/// Asserts that `html` is tag-balanced over the element set the emitter produces:
/// every `<tag>` has a matching `</tag>` in proper nesting order. Void/self-closing
/// elements (`meta`, `br`, `<a id…></a>` are explicit pairs here) are not used as
/// unbalanced singletons by our emitter, so a strict stack check is correct. Used
/// only by tests; returns `Ok(())` or a descriptive error.
#[cfg(test)]
pub fn assert_balanced(html: &str) -> Result<(), String> {
    // Elements we emit that are intentionally void or whose content we never
    // re-scan as markup.
    let void = ["meta", "br", "hr", "!doctype"];
    let mut stack: Vec<String> = Vec::new();
    let bytes: Vec<char> = html.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '<' {
            i += 1;
            continue;
        }
        // Find the end of the tag.
        let end = match (i + 1..bytes.len()).find(|&j| bytes[j] == '>') {
            Some(e) => e,
            None => return Err("unterminated `<`".to_string()),
        };
        let raw: String = bytes[i + 1..end].iter().collect();
        i = end + 1;
        let raw = raw.trim();
        if raw.starts_with('!') {
            continue; // doctype / comment
        }
        if let Some(name) = raw.strip_prefix('/') {
            let name = tag_name(name);
            match stack.pop() {
                Some(open) if open == name => {}
                Some(open) => {
                    return Err(format!("closing </{name}> does not match open <{open}>"))
                }
                None => return Err(format!("stray closing </{name}>")),
            }
        } else {
            let name = tag_name(raw);
            if void.contains(&name.as_str()) {
                continue;
            }
            stack.push(name);
        }
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err(format!("unclosed tags: {stack:?}"))
    }
}

/// Extracts the lowercase element name from a tag's inner text (stops at the first
/// whitespace or `/`).
#[cfg(test)]
fn tag_name(raw: &str) -> String {
    raw.trim()
        .trim_end_matches('/')
        .split([' ', '\t', '\n'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
