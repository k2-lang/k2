//! `textDocument/semanticTokens/full`: classify every token for richer editor
//! highlighting, encoded in the LSP delta format.
//!
//! The lexer gives token kinds and 1-based scalar `line`/`col`; this provider
//! reconstructs each token's scalar offset exactly the way the parser does
//! (`start = line_starts[line-1] + col - 1`, `end = start + chars().count()`),
//! classifies it, then converts the scalar offsets to UTF-16 `(line, char)` via
//! the [`PositionMap`](crate::position::PositionMap) for the delta encoding.
//!
//! Identifiers are classified by *resolution*, not by spelling: a `Use` of a `fn`
//! item is a `function`, a parameter is a `parameter`, a module is a `namespace`,
//! a member after `.` is a `property` (or `function`/`enumMember` once the type
//! checker resolved it). Comments (line + doc) are recovered from the lexer's
//! trivia channel and merged in. The legend index order is the const
//! [`SEMANTIC_TOKEN_TYPES`](crate::protocol::SEMANTIC_TOKEN_TYPES), shared with the
//! capability advertisement so the two can never drift.

use k2_lexer::{tokenize_with_trivia, TokenKind};
use k2_resolve::{DefKind, Resolution, Resolved};
use k2_syntax::Span;
use k2_types::{MemberRes, Type, Typed};

use crate::analysis::Analysis;
use crate::json::JsonValue;
use crate::protocol::{token_modifier_index, token_type_index};

/// One classified token before delta encoding.
struct SemTok {
    /// Scalar start offset.
    start: u32,
    /// Scalar end offset.
    end: u32,
    /// Index into the token-types legend.
    ty: u32,
    /// Modifier bitset.
    modifiers: u32,
}

/// Computes the `SemanticTokens` (`{ data: [...] }`) for `analysis`.
pub fn compute(analysis: &Analysis) -> JsonValue {
    let source = &analysis.source;
    let (tokens, trivia) = tokenize_with_trivia(source);

    // A scalar line-start table, identical to the parser's, so we can reconstruct
    // each token's scalar offset from its 1-based scalar line/col.
    let chars: Vec<char> = source.chars().collect();
    let line_starts = build_line_starts(&chars);

    let mut sem: Vec<SemTok> = Vec::new();

    for tok in &tokens {
        if tok.kind == TokenKind::Eof {
            continue;
        }
        let start = scalar_offset(&line_starts, tok.line, tok.col);
        let len = tok.text.chars().count() as u32;
        if len == 0 {
            continue;
        }
        let end = start + len;
        let span = Span::new(start, end, tok.line, tok.col);
        if let Some((ty, modifiers)) = classify(analysis, tok.kind, span) {
            sem.push(SemTok {
                start,
                end,
                ty,
                modifiers,
            });
        }
    }

    // Comments (line + doc) become `comment` tokens.
    let comment_ty = token_type_index("comment");
    for tr in &trivia {
        let _ = tr.kind; // both LineComment and DocComment map to `comment`.
        sem.push(SemTok {
            start: tr.start,
            end: tr.end,
            ty: comment_ty,
            modifiers: 0,
        });
    }

    // Sort by start (then end) so the delta encoding is monotonic. A multi-line
    // token is split per line so each delta entry stays on one line.
    sem.sort_by_key(|t| (t.start, t.end));

    let data = encode_deltas(&analysis.posmap, &chars, sem);
    JsonValue::obj(vec![("data", JsonValue::arr(data))])
}

/// Builds the scalar line-start table (offset of the first char of each line).
fn build_line_starts(chars: &[char]) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            starts.push((i + 1) as u32);
        }
    }
    starts
}

/// Reconstructs a token's scalar start from its 1-based scalar `line`/`col`,
/// matching the parser's `line_starts[line-1] + (col-1)`.
fn scalar_offset(line_starts: &[u32], line: u32, col: u32) -> u32 {
    let li = (line.saturating_sub(1)) as usize;
    let base = line_starts.get(li).copied().unwrap_or(0);
    base + col.saturating_sub(1)
}

/// Classifies one token to a `(type_index, modifier_bitset)`, or `None` to omit
/// it (whitespace-equivalent / non-highlighted punctuation we choose to skip).
fn classify(analysis: &Analysis, kind: TokenKind, span: Span) -> Option<(u32, u32)> {
    use TokenKind::*;
    // Keywords (including the literal keywords true/false/null/undefined).
    if kind.is_keyword() {
        return Some((token_type_index("keyword"), 0));
    }
    // Literals.
    match kind {
        StringLiteral | MultilineString | CharLiteral => {
            return Some((token_type_index("string"), 0))
        }
        IntLiteral | FloatLiteral => return Some((token_type_index("number"), 0)),
        // A `///` doc comment is recorded by the lexer in *both* channels: as a
        // retained `DocComment` token here AND as `TriviaKind::DocComment` trivia.
        // The trivia loop below emits the single `comment` token for it, so we
        // must NOT also emit one here — two tokens at the identical range would
        // overlap (deltaLine=0, deltaChar=0, length>0), violating the LSP
        // semantic-tokens spec. Returning `None` lets the doc comment flow
        // through the trivia channel exactly once, matching line comments.
        DocComment => return None,
        Builtin => return Some((token_type_index("function"), 0)),
        _ => {}
    }
    // Operators and structural punctuation we choose to mark as `operator`.
    if is_operator(kind) {
        return Some((token_type_index("operator"), 0));
    }
    // Identifiers (and escaped identifiers): classify by resolution.
    if matches!(kind, Ident | EscapedIdent) {
        return Some(classify_ident(analysis, span));
    }
    None
}

/// `true` for an operator/structural-punctuation token kind we highlight as an
/// operator. Brackets/parens/braces are intentionally left unclassified (no token
/// emitted) so the highlight matches common editor expectations.
fn is_operator(kind: TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Plus | Minus
            | Star
            | Slash
            | Percent
            | Amp
            | Pipe
            | Caret
            | Tilde
            | Shl
            | Shr
            | EqEq
            | NotEq
            | Lt
            | LtEq
            | Gt
            | GtEq
            | Eq
            | PlusEq
            | MinusEq
            | StarEq
            | SlashEq
            | PercentEq
            | AmpEq
            | PipeEq
            | CaretEq
            | ShlEq
            | ShrEq
            | PlusPlus
            | Bang
            | Question
            | DotStar
            | DotQuestion
            | DotDot
            | DotDotDot
            | FatArrow
    )
}

/// Classifies an identifier token by its resolution, falling back to `variable`.
fn classify_ident(analysis: &Analysis, span: Span) -> (u32, u32) {
    let default = (token_type_index("variable"), 0u32);
    let resolved = match &analysis.resolved {
        Some(r) => r,
        None => return default,
    };

    // A declaration site? (e.g. the name in `const x = ...`, a param, a fn name.)
    // The resolver records an item's `Def.span` as the *whole* declaration, so the
    // name token is matched via its recovered name span, not the raw def span.
    if let Some(def) = decl_at_span(analysis, resolved, span) {
        let ty = def_token_type(analysis, def);
        let mut modifiers = bit(token_modifier_index("declaration"));
        if matches!(def.kind, DefKind::Item) && is_const_item(analysis, def) {
            modifiers |= bit(token_modifier_index("readonly"));
        }
        return (ty, modifiers);
    }

    // A reference (a recorded `Use` whose occurrence span equals this token)?
    if let Some(use_) = resolved.uses.at(span) {
        return match use_.res {
            Resolution::Def(id) | Resolution::Module(id) => {
                (def_token_type(analysis, &resolved.defs[id.index()]), 0)
            }
            Resolution::Predeclared(_) => (token_type_index("type"), 0),
            Resolution::DeferredMember => (classify_member(analysis, span), 0),
            Resolution::Error => default,
        };
    }

    // A member token after a `.` is recorded on the whole access span, not the
    // bare member token; check the type checker's member table by the member's
    // own position relative to the access.
    if let Some(ty) = member_token_at(analysis, span) {
        return (ty, 0);
    }

    default
}

/// The `Def` whose recovered name-token span equals this token span, if the token
/// *is* a declaration site. The name token is recovered from within the (whole-
/// declaration) `Def.span` via [`crate::features::def_name_span`], so the `add` in
/// `pub fn add(...)` matches even though the def span covers the entire function.
fn decl_at_span<'a>(
    analysis: &Analysis,
    resolved: &'a Resolved,
    span: Span,
) -> Option<&'a k2_resolve::Def> {
    resolved.defs.iter().find(|d| {
        if d.kind == DefKind::Predeclared || d.span.end <= d.span.start {
            return false;
        }
        let name = crate::features::def_name_span(d, &analysis.source);
        name.start == span.start && name.end == span.end
    })
}

/// The token-type index for a definition kind, refining `Item` to `function`,
/// `type`, or `variable` from its inferred type.
fn def_token_type(analysis: &Analysis, def: &k2_resolve::Def) -> u32 {
    match def.kind {
        DefKind::Param => token_type_index("parameter"),
        DefKind::Local | DefKind::Capture => token_type_index("variable"),
        DefKind::Module => token_type_index("namespace"),
        DefKind::Field => token_type_index("property"),
        DefKind::Predeclared => token_type_index("type"),
        DefKind::Item => item_token_type(analysis, def),
    }
}

/// Refines a file/container `Item` to `function` (its type is a `fn`), `type`
/// (its value is a type), or `variable` otherwise.
fn item_token_type(analysis: &Analysis, def: &k2_resolve::Def) -> u32 {
    if let Some(typed) = &analysis.typed {
        if let Some(&tid) = typed.binding_types.get(&def.id) {
            match typed.arena.get(tid) {
                Type::Fn(_) => return token_type_index("function"),
                Type::TypeType
                | Type::Struct(_)
                | Type::Enum(_)
                | Type::Union(_)
                | Type::ErrorSet(_) => return token_type_index("type"),
                _ => {}
            }
        }
    }
    token_type_index("variable")
}

/// `true` if the item is a `const` (read-only) declaration.
fn is_const_item(analysis: &Analysis, def: &k2_resolve::Def) -> bool {
    use k2_syntax::Item;
    analysis.parse.file.items.iter().any(|item| match item {
        Item::Const { span, .. } => span.start <= def.span.start && def.span.end <= span.end,
        _ => false,
    })
}

/// Classifies a `DeferredMember` use (recorded on the whole `base.member` span)
/// by looking at the resolved member: a method → `function`, a variant →
/// `enumMember`, else `property`.
fn classify_member(analysis: &Analysis, span: Span) -> u32 {
    let typed = match &analysis.typed {
        Some(t) => t,
        None => return token_type_index("property"),
    };
    match typed.members.get(&(span.start, span.end)) {
        Some(MemberRes::Decl(def_id)) => {
            if let Some(&tid) = typed.binding_types.get(def_id) {
                if matches!(typed.arena.get(tid), Type::Fn(_)) {
                    return token_type_index("function");
                }
            }
            token_type_index("property")
        }
        Some(MemberRes::Variant(_)) => token_type_index("enumMember"),
        _ => token_type_index("property"),
    }
}

/// A member token (after `.`) is the tail of a member-access span. If a recorded
/// `DeferredMember`/member resolution covers a token positioned just after a `.`,
/// classify it; otherwise `None`.
fn member_token_at(analysis: &Analysis, span: Span) -> Option<u32> {
    let typed = analysis.typed.as_ref()?;
    // Find a member-access span whose tail member token matches `span` exactly.
    for (&(astart, aend), member) in &typed.members {
        if aend != span.end {
            continue;
        }
        // The access ends at `span.end`; the member token must start after a '.'.
        if span.start < astart {
            continue;
        }
        let chars: Vec<char> = analysis.source.chars().collect();
        if span.start == 0 || chars.get((span.start - 1) as usize) != Some(&'.') {
            continue;
        }
        return Some(member_type(typed, member));
    }
    None
}

/// The token type for an already-resolved member.
fn member_type(typed: &Typed, member: &MemberRes) -> u32 {
    match member {
        MemberRes::Decl(def_id) => {
            if let Some(&tid) = typed.binding_types.get(def_id) {
                if matches!(typed.arena.get(tid), Type::Fn(_)) {
                    return token_type_index("function");
                }
            }
            token_type_index("property")
        }
        MemberRes::Variant(_) => token_type_index("enumMember"),
        _ => token_type_index("property"),
    }
}

/// A 1-bit modifier mask from a legend index.
fn bit(index: u32) -> u32 {
    1u32 << index
}

/// Encodes the classified tokens into the LSP delta integer array, splitting any
/// multi-line token per line so every entry stays on a single line.
fn encode_deltas(
    posmap: &crate::position::PositionMap,
    chars: &[char],
    sem: Vec<SemTok>,
) -> Vec<JsonValue> {
    // Expand multi-line tokens into per-line pieces in scalar coordinates.
    let mut pieces: Vec<(u32, u32, u32, u32)> = Vec::new(); // (start, end, ty, mods)
    for t in sem {
        let mut seg_start = t.start;
        let mut i = t.start;
        while i < t.end {
            if chars.get(i as usize) == Some(&'\n') {
                if i > seg_start {
                    pieces.push((seg_start, i, t.ty, t.modifiers));
                }
                seg_start = i + 1;
            }
            i += 1;
        }
        if t.end > seg_start {
            pieces.push((seg_start, t.end, t.ty, t.modifiers));
        }
    }
    pieces.sort_by_key(|p| (p.0, p.1));

    let mut data: Vec<JsonValue> = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;
    for (start, end, ty, mods) in pieces {
        let (line, ch) = posmap.offset_to_position(start);
        let (_eline, ech) = posmap.offset_to_position(end);
        // Single-line by construction, so the UTF-16 length is the char delta.
        let length = ech.saturating_sub(ch);
        if length == 0 {
            continue;
        }
        let delta_line = line - prev_line;
        let delta_char = if delta_line == 0 { ch - prev_char } else { ch };
        data.push(JsonValue::num(i64::from(delta_line)));
        data.push(JsonValue::num(i64::from(delta_char)));
        data.push(JsonValue::num(i64::from(length)));
        data.push(JsonValue::num(i64::from(ty)));
        data.push(JsonValue::num(i64::from(mods)));
        prev_line = line;
        prev_char = ch;
    }
    data
}
