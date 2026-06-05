//! Semantic-version parsing, ordering, and constraint matching for the offline
//! package manager (v0.25).
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! This module is PURE: it performs no I/O and depends only on the Rust standard
//! library, so its output is a deterministic function of its string inputs. The
//! resolver ([`crate::pkg`]) feeds it the version directory names found under a
//! local registry and the constraints declared in a `k2.pkg` manifest, then asks
//! it to pick the HIGHEST registry version that satisfies a constraint.
//!
//! ## What is modeled
//!
//! * [`Version`] — a `MAJOR.MINOR.PATCH[-prerelease][+build]` semantic version,
//!   ordered per semver §11 (numeric core compared field-by-field; a prerelease
//!   sorts *below* the same core without one; build metadata is ignored for
//!   ordering and matching, per semver §10).
//! * [`Constraint`] — the union of the constraint syntaxes a `k2.pkg` may spell:
//!   caret (`^1.2.3`), tilde (`~1.2`), exact (`1.2.3`), partial-bare (`1.2`, `1`,
//!   treated as a tilde-style range), wildcard (`1.2.x`, `1.x`, `*`), and a chain
//!   of comparators (`>=1.0.0, <2.0.0`).
//!
//! A constraint is internally normalized to a half-open numeric range
//! `[lower, upper)`, which is the most robust representation for "the highest
//! version that satisfies all of these". An EXACT term (`1.2.3`, `=1.2.3`) is the
//! tightest such interval: its upper bound is the smallest version strictly
//! greater than the lower bound (see [`exact_upper`]), so the interval contains
//! exactly the named version — INCLUDING when it names a prerelease, where the
//! successor is the lower bound with a minimal extra prerelease field appended
//! rather than a patch bump. The prerelease policy is strict (cargo/npm style): a
//! prerelease version satisfies a range only when the range's own bounds name a
//! prerelease at the same numeric core, so `^1.0.0` never silently selects
//! `2.0.0-alpha`.

use std::cmp::Ordering;
use std::fmt;

/// A parsed semantic version: a numeric `major.minor.patch` core plus an optional
/// prerelease tag chain. Build metadata is parsed off and discarded (it does not
/// affect ordering or matching, per semver §10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// The major component.
    pub major: u64,
    /// The minor component.
    pub minor: u64,
    /// The patch component.
    pub patch: u64,
    /// The prerelease identifiers (`1.2.0-rc.1` → `["rc", "1"]`), empty for a
    /// stable release. Compared left-to-right per semver §11.
    pub pre: Vec<PreField>,
}

/// One dot-separated prerelease identifier, classified as numeric or alphanumeric
/// so the two compare per the semver §11 rules (numeric identifiers compare
/// numerically and always sort below alphanumeric ones).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreField {
    /// A purely numeric identifier (`1`, `42`), compared numerically.
    Num(u64),
    /// An alphanumeric / hyphenated identifier (`rc`, `alpha1`), compared
    /// lexically (ASCII).
    Alpha(String),
}

impl Version {
    /// Parses a `MAJOR.MINOR.PATCH[-pre][+build]` string into a [`Version`], or
    /// `None` if it is not a valid semver (which the registry listing simply
    /// skips — a non-semver directory name is not an error, it is ignored).
    pub fn parse(s: &str) -> Option<Version> {
        // Strip build metadata first (everything from the first '+'): it is
        // ignored for both ordering and matching.
        let s = s.split('+').next().unwrap_or(s);
        // Split off the prerelease (everything from the first '-').
        let (core, pre_str) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut it = core.split('.');
        let major = parse_num(it.next()?)?;
        let minor = parse_num(it.next()?)?;
        let patch = parse_num(it.next()?)?;
        // A core with a fourth dotted field is not a valid semver.
        if it.next().is_some() {
            return None;
        }
        let pre = match pre_str {
            None => Vec::new(),
            Some(p) => {
                // An empty prerelease (`1.2.3-`) is malformed.
                if p.is_empty() {
                    return None;
                }
                let mut out = Vec::new();
                for field in p.split('.') {
                    if field.is_empty() || !field.bytes().all(is_pre_byte) {
                        return None;
                    }
                    // A numeric identifier with a leading zero is malformed per
                    // semver §9; otherwise classify as numeric.
                    if field.bytes().all(|b| b.is_ascii_digit()) {
                        if field.len() > 1 && field.starts_with('0') {
                            return None;
                        }
                        out.push(PreField::Num(field.parse().ok()?));
                    } else {
                        out.push(PreField::Alpha(field.to_string()));
                    }
                }
                out
            }
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// `true` if this version carries a prerelease tag.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The stable `[major, minor, patch]` core as a tuple, for range comparisons
    /// that ignore the prerelease tag.
    fn core(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl Ord for Version {
    /// Total ordering per semver §11: numeric core first, then a release sorts
    /// ABOVE the same core's prereleases, then prerelease fields left-to-right.
    fn cmp(&self, other: &Self) -> Ordering {
        match self.core().cmp(&other.core()) {
            Ordering::Equal => cmp_pre(&self.pre, &other.pre),
            ord => ord,
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-")?;
            for (i, p) in self.pre.iter().enumerate() {
                if i > 0 {
                    write!(f, ".")?;
                }
                match p {
                    PreField::Num(n) => write!(f, "{n}")?,
                    PreField::Alpha(a) => write!(f, "{a}")?,
                }
            }
        }
        Ok(())
    }
}

/// Compares two prerelease field chains per semver §11. An EMPTY chain (a stable
/// release) sorts ABOVE any non-empty one; otherwise fields compare left-to-right
/// with numeric < alphanumeric, and a shorter prefix sorts below a longer one when
/// all shared fields are equal.
fn cmp_pre(a: &[PreField], b: &[PreField]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        // A release (empty pre) is GREATER than a prerelease.
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = match (x, y) {
                    (PreField::Num(n), PreField::Num(m)) => n.cmp(m),
                    (PreField::Alpha(s), PreField::Alpha(t)) => s.cmp(t),
                    // Numeric identifiers always have lower precedence than
                    // alphanumeric ones.
                    (PreField::Num(_), PreField::Alpha(_)) => Ordering::Less,
                    (PreField::Alpha(_), PreField::Num(_)) => Ordering::Greater,
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            // All shared fields equal: the chain with more fields wins.
            a.len().cmp(&b.len())
        }
    }
}

/// Parses a non-negative decimal component with no leading-zero ambiguity
/// (`0` is allowed, `01` is not — semver §2/§9).
fn parse_num(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

/// A byte permitted in a prerelease identifier: ASCII alphanumeric or hyphen.
fn is_pre_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

/// A parsed version constraint, normalized to a half-open numeric range
/// `[lower, upper)` over the [`Version`] ordering. A version satisfies the
/// constraint when `lower <= v < upper` under that ordering AND the strict
/// prerelease policy holds. An exact term collapses the interval to its tightest
/// form (`upper == exact_upper(&lower)`), so it admits exactly its own version —
/// see [`exact_upper`] for the prerelease successor that keeps an exact
/// prerelease pin from leaking into the stable release of the same core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    /// The original constraint text (for diagnostics).
    pub raw: String,
    /// The inclusive lower bound.
    lower: Version,
    /// The exclusive upper bound. A bound at `(M, m, p)` with no prerelease is the
    /// usual "less than this core".
    upper: Version,
}

impl Constraint {
    /// Parses a constraint string. Supports caret (`^`), tilde (`~`), exact
    /// (`1.2.3`), partial-bare (`1.2`, `1`), wildcard (`1.2.x`, `1.x`, `*`), and a
    /// comma/space-separated chain of comparators (`>=1.0.0, <2.0.0`). Returns
    /// `None` on a syntactically invalid constraint (the caller reports it).
    pub fn parse(raw: &str) -> Option<Constraint> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        // A comparator chain is detected by the presence of any comparator token
        // or an explicit separator; otherwise the whole string is a single
        // caret/tilde/exact/wildcard term.
        let looks_like_chain = trimmed.contains(',')
            || trimmed.contains(">=")
            || trimmed.contains("<=")
            || trimmed.starts_with('>')
            || trimmed.starts_with('<')
            || trimmed.starts_with('=');
        let (lower, upper) = if looks_like_chain {
            parse_comparator_chain(trimmed)?
        } else if let Some(rest) = trimmed.strip_prefix('^') {
            caret_range(rest)?
        } else if let Some(rest) = trimmed.strip_prefix('~') {
            tilde_range(rest)?
        } else if trimmed == "*" {
            (Version::parse("0.0.0").unwrap(), unbounded())
        } else if trimmed.contains(".x") || trimmed.contains(".X") || trimmed.ends_with('x') {
            wildcard_range(trimmed)?
        } else {
            bare_range(trimmed)?
        };
        Some(Constraint {
            raw: trimmed.to_string(),
            lower,
            upper,
        })
    }

    /// `true` if `v` satisfies this constraint. The numeric range test is
    /// `lower <= v < upper`; the prerelease policy then rejects a prerelease `v`
    /// unless the constraint itself admits a prerelease at the same core.
    pub fn matches(&self, v: &Version) -> bool {
        if v < &self.lower || v >= &self.upper {
            return false;
        }
        // Strict prerelease policy: a prerelease candidate is only acceptable when
        // a bound of this constraint names a prerelease at the SAME stable core.
        if v.is_prerelease() {
            let lower_pre_here = self.lower.is_prerelease() && self.lower.core() == v.core();
            let upper_pre_here = self.upper.is_prerelease() && self.upper.core() == v.core();
            if !lower_pre_here && !upper_pre_here {
                return false;
            }
        }
        true
    }

    /// Intersects this constraint with `other`, yielding the constraint satisfied
    /// by exactly the versions both admit (the tighter lower bound and tighter
    /// upper bound). Used to combine multiple constraints accumulated on one
    /// package; if the result is empty (`lower >= upper`), no version satisfies
    /// both — a version conflict.
    pub fn intersect(&self, other: &Constraint) -> Constraint {
        let lower = if self.lower >= other.lower {
            self.lower.clone()
        } else {
            other.lower.clone()
        };
        let upper = if self.upper <= other.upper {
            self.upper.clone()
        } else {
            other.upper.clone()
        };
        Constraint {
            raw: format!("{} , {}", self.raw, other.raw),
            lower,
            upper,
        }
    }

    /// `true` if NO version can satisfy this (normalized) constraint because its
    /// range is empty — the lower bound is at or above the upper bound. A genuine
    /// conflict from intersecting incompatible constraints lands here.
    pub fn is_empty(&self) -> bool {
        self.lower >= self.upper
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// An upper bound that admits every version (a `*` wildcard / open-ended `>=`).
fn unbounded() -> Version {
    Version {
        major: u64::MAX,
        minor: u64::MAX,
        patch: u64::MAX,
        pre: Vec::new(),
    }
}

/// A partially-supplied version: a required major, an optional minor/patch (a
/// missing field is `None`), and any prerelease tag. The output of
/// [`parse_version_ish`], shared by the caret/tilde/bare range builders.
type VersionIsh = (u64, Option<u64>, Option<u64>, Vec<PreField>);

/// Parses a partial `MAJOR[.MINOR[.PATCH[-pre]]]` "version-ish", returning the
/// supplied numeric fields (missing ones reported as `None`) and any prerelease.
/// Used by the caret/tilde/bare range builders.
fn parse_version_ish(s: &str) -> Option<VersionIsh> {
    let s = s.split('+').next().unwrap_or(s);
    let (core, pre_str) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (s, None),
    };
    let mut it = core.split('.');
    let major = parse_num(it.next()?)?;
    let minor = match it.next() {
        Some(m) => Some(parse_num(m)?),
        None => None,
    };
    let patch = match it.next() {
        Some(p) => Some(parse_num(p)?),
        None => None,
    };
    if it.next().is_some() {
        return None;
    }
    // A prerelease is only meaningful with a full core; reject `1.2-rc`.
    let pre = match pre_str {
        None => Vec::new(),
        Some(_) if patch.is_none() => return None,
        Some(p) => {
            Version::parse(&format!(
                "{}.{}.{}-{}",
                major,
                minor.unwrap(),
                patch.unwrap(),
                p
            ))?
            .pre
        }
    };
    Some((major, minor, patch, pre))
}

/// Builds the caret `^` range: pin the left-most NON-ZERO numeric field, allow
/// everything up to the next increment of that field. `^1.2.3` ⇒ `[1.2.3, 2.0.0)`,
/// `^0.2.3` ⇒ `[0.2.3, 0.3.0)`, `^0.0.3` ⇒ `[0.0.3, 0.0.4)`.
fn caret_range(s: &str) -> Option<(Version, Version)> {
    let (major, minor, patch, pre) = parse_version_ish(s)?;
    let minor_v = minor.unwrap_or(0);
    let patch_v = patch.unwrap_or(0);
    let lower = Version {
        major,
        minor: minor_v,
        patch: patch_v,
        pre,
    };
    let upper = if major > 0 {
        ver(major + 1, 0, 0)
    } else if minor_v > 0 {
        ver(0, minor_v + 1, 0)
    } else {
        ver(0, 0, patch_v + 1)
    };
    Some((lower, upper))
}

/// Builds the tilde `~` range: allow patch-level changes when a minor is given,
/// or minor-level changes when only a major is given. `~1.2.3`/`~1.2` ⇒
/// `[…, 1.3.0)`; `~1` ⇒ `[1.0.0, 2.0.0)`.
fn tilde_range(s: &str) -> Option<(Version, Version)> {
    let (major, minor, patch, pre) = parse_version_ish(s)?;
    let lower = Version {
        major,
        minor: minor.unwrap_or(0),
        patch: patch.unwrap_or(0),
        pre,
    };
    let upper = match minor {
        // `~1.2` / `~1.2.3` → allow patch bumps within the minor.
        Some(m) => ver(major, m + 1, 0),
        // `~1` → allow minor bumps within the major.
        None => ver(major + 1, 0, 0),
    };
    Some((lower, upper))
}

/// Builds the range for a bare version-ish: an EXACT match for a full `1.2.3`, or
/// a tilde-style range for a partial `1.2` / `1` (spec §3.2 table).
fn bare_range(s: &str) -> Option<(Version, Version)> {
    let (major, minor, patch, pre) = parse_version_ish(s)?;
    match (minor, patch) {
        // A full version is an exact match: `[1.2.3, 1.2.3+ε)` — the smallest
        // half-open interval containing exactly this version (incl. prerelease).
        (Some(m), Some(p)) => {
            let lower = Version {
                major,
                minor: m,
                patch: p,
                pre: pre.clone(),
            };
            let upper = exact_upper(&lower);
            Some((lower, upper))
        }
        // `1.2` → `[1.2.0, 1.3.0)`.
        (Some(m), None) => Some((ver(major, m, 0), ver(major, m + 1, 0))),
        // `1` → `[1.0.0, 2.0.0)`.
        (None, _) => Some((ver(major, 0, 0), ver(major + 1, 0, 0))),
    }
}

/// Builds the wildcard range for `1.2.x` / `1.x` / `1.x.x` (an `x`/`X` field is a
/// "any" placeholder pinning the prefix before it).
fn wildcard_range(s: &str) -> Option<(Version, Version)> {
    let mut major: Option<u64> = None;
    let mut minor: Option<u64> = None;
    let mut seen_wild = false;
    for (i, field) in s.split('.').enumerate() {
        let is_wild = field == "x" || field == "X" || field == "*";
        if is_wild {
            seen_wild = true;
            continue;
        }
        // A fixed field after a wildcard (`1.x.3`) is malformed.
        if seen_wild {
            return None;
        }
        let n = parse_num(field)?;
        match i {
            0 => major = Some(n),
            1 => minor = Some(n),
            // A wildcard in the patch slot is the only one that can be fixed at a
            // deeper index; `1.2.x` lands here with i==2 → but that field is the
            // wildcard, handled above. A fixed 3rd field means it is not a
            // wildcard constraint.
            _ => return None,
        }
    }
    match (major, minor) {
        // `1.2.x` → `[1.2.0, 1.3.0)`.
        (Some(m), Some(n)) => Some((ver(m, n, 0), ver(m, n + 1, 0))),
        // `1.x` / `1.x.x` → `[1.0.0, 2.0.0)`.
        (Some(m), None) => Some((ver(m, 0, 0), ver(m + 1, 0, 0))),
        // `*` is handled by the caller; a bare `x` is treated as any.
        (None, _) => Some((ver(0, 0, 0), unbounded())),
    }
}

/// Parses a `>=1.0.0, <2.0.0`-style comparator chain into the intersected
/// `[lower, upper)` range. Comparators are separated by commas and/or whitespace.
fn parse_comparator_chain(s: &str) -> Option<(Version, Version)> {
    let mut lower = ver(0, 0, 0);
    let mut upper = unbounded();
    // Tokenize on commas and whitespace, keeping the operator glued to its
    // version (`>= 1.0.0` and `>=1.0.0` both work because we strip the op prefix).
    for token in s.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
        let (op, rest) = split_op(token);
        let (major, minor, patch, pre) =
            parse_version_ish(rest).or_else(|| parse_version_ish(token))?;
        let v = Version {
            major,
            minor: minor.unwrap_or(0),
            patch: patch.unwrap_or(0),
            pre,
        };
        match op {
            ">=" => {
                if v > lower {
                    lower = v;
                }
            }
            ">" => {
                let nv = exact_upper(&v);
                if nv > lower {
                    lower = nv;
                }
            }
            "<=" => {
                let nv = exact_upper(&v);
                if nv < upper {
                    upper = nv;
                }
            }
            "<" => {
                if v < upper {
                    upper = v;
                }
            }
            "=" | "" => {
                // An `=1.2.3` (or a bare term inside a chain) pins both bounds.
                if v > lower {
                    lower = v.clone();
                }
                let nv = exact_upper(&v);
                if nv < upper {
                    upper = nv;
                }
            }
            _ => return None,
        }
    }
    Some((lower, upper))
}

/// Splits a comparator token into its `(operator, version-ish)` parts. Recognizes
/// `>=`, `<=`, `>`, `<`, `=`; an unprefixed token yields the empty operator.
fn split_op(token: &str) -> (&str, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = token.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", token)
}

/// The smallest version strictly greater than `v`, used as the exclusive upper
/// bound of an exact / `<=` / `>` constraint so the half-open interval
/// `[lower, exact_upper(v))` contains EXACTLY `v` (per semver ordering) and
/// nothing above it.
///
/// Two cases, because the successor of a version depends on whether it carries a
/// prerelease tag:
///
/// * **Stable `v`** (`1.2.4`): its successor is `(major, minor, patch+1)` with no
///   prerelease (`1.2.5`). There is no version strictly between `1.2.4` and
///   `1.2.5` except `1.2.5`'s own prereleases, which the strict prerelease policy
///   in [`Constraint::matches`] already excludes — so `[1.2.4, 1.2.5)` pins
///   exactly the stable `1.2.4`.
/// * **Prerelease `v`** (`1.2.4-rc.1`): bumping the patch would yield `1.2.5` and
///   wrongly admit the stable `1.2.4`, every other `1.2.4` prerelease, AND
///   `1.2.4` itself (all sort below `1.2.5`). Instead, append one MINIMAL
///   prerelease identifier (`PreField::Num(0)`): `1.2.4-rc.1` → `1.2.4-rc.1.0`,
///   which sorts immediately ABOVE `1.2.4-rc.1` (a longer prerelease chain with
///   an equal prefix wins, semver §11) but BELOW `1.2.4-rc.2` and stable `1.2.4`.
///   So `[1.2.4-rc.1, 1.2.4-rc.1.0)` contains exactly `1.2.4-rc.1`.
fn exact_upper(v: &Version) -> Version {
    if v.is_prerelease() {
        let mut pre = v.pre.clone();
        pre.push(PreField::Num(0));
        Version {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
            pre,
        }
    } else {
        ver(v.major, v.minor, v.patch + 1)
    }
}

/// Constructs a stable [`Version`] (no prerelease) from a numeric core.
fn ver(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
        pre: Vec::new(),
    }
}

/// Picks the HIGHEST version in `candidates` that satisfies `constraint`, or
/// `None` if none do. The candidate slice is sorted internally, so the result
/// never depends on the order the registry directory was listed in — the core of
/// the resolver's determinism.
pub fn highest_match<'a>(
    candidates: &'a [Version],
    constraint: &Constraint,
) -> Option<&'a Version> {
    candidates
        .iter()
        .filter(|v| constraint.matches(v))
        .max_by(|a, b| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn c(s: &str) -> Constraint {
        Constraint::parse(s).unwrap()
    }

    #[test]
    fn parses_basic_versions() {
        assert_eq!(v("1.2.3"), ver(1, 2, 3));
        assert_eq!(v("0.0.0"), ver(0, 0, 0));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("01.2.3").is_none());
        assert!(Version::parse("v1.2.3").is_none());
    }

    #[test]
    fn parses_and_orders_prereleases() {
        assert!(v("1.0.0-alpha") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-alpha.beta") < v("1.0.0-beta"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0") < v("2.0.0"));
    }

    #[test]
    fn build_metadata_ignored() {
        assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
    }

    #[test]
    fn caret_constraint() {
        let con = c("^1.2.3");
        assert!(con.matches(&v("1.2.3")));
        assert!(con.matches(&v("1.9.0")));
        assert!(!con.matches(&v("2.0.0")));
        assert!(!con.matches(&v("1.2.2")));
        // 0.x caret is narrow.
        let con0 = c("^0.2.3");
        assert!(con0.matches(&v("0.2.5")));
        assert!(!con0.matches(&v("0.3.0")));
    }

    #[test]
    fn tilde_constraint() {
        let con = c("~1.2.3");
        assert!(con.matches(&v("1.2.9")));
        assert!(!con.matches(&v("1.3.0")));
        let con2 = c("~1.2");
        assert!(con2.matches(&v("1.2.0")));
        assert!(!con2.matches(&v("1.3.0")));
        let con1 = c("~1");
        assert!(con1.matches(&v("1.9.9")));
        assert!(!con1.matches(&v("2.0.0")));
    }

    #[test]
    fn exact_and_wildcard() {
        let exact = c("1.2.3");
        assert!(exact.matches(&v("1.2.3")));
        assert!(!exact.matches(&v("1.2.4")));
        let wild = c("1.2.x");
        assert!(wild.matches(&v("1.2.9")));
        assert!(!wild.matches(&v("1.3.0")));
        let any = c("*");
        assert!(any.matches(&v("9.9.9")));
    }

    #[test]
    fn comparator_chain() {
        let con = c(">=1.0.0, <2.0.0");
        assert!(con.matches(&v("1.5.0")));
        assert!(!con.matches(&v("2.0.0")));
        assert!(!con.matches(&v("0.9.0")));
    }

    #[test]
    fn prerelease_policy_strict() {
        // ^1.0.0 must NOT admit 2.0.0-alpha (different core, prerelease).
        let con = c("^1.0.0");
        assert!(!con.matches(&v("2.0.0-alpha")));
        // A constraint that names a prerelease admits prereleases at that core.
        let pre = c(">=1.0.0-alpha, <1.0.0");
        assert!(pre.matches(&v("1.0.0-beta")));
    }

    #[test]
    fn intersection_conflict() {
        let a = c("^1.0.0");
        let b = c("^2.0.0");
        let i = a.intersect(&b);
        assert!(i.is_empty(), "incompatible carets must not intersect");
        let a2 = c(">=1.2.0");
        let b2 = c("<1.5.0");
        let i2 = a2.intersect(&b2);
        assert!(!i2.is_empty());
        assert!(i2.matches(&v("1.3.0")));
    }

    #[test]
    fn highest_match_picks_max() {
        let cands = vec![v("1.0.0"), v("1.2.0"), v("2.0.0")];
        let con = c("^1.0.0");
        assert_eq!(highest_match(&cands, &con), Some(&v("1.2.0")));
    }

    #[test]
    fn exact_prerelease_pin_matches_only_itself() {
        // Regression (finding 1): an exact constraint that NAMES a prerelease must
        // match ONLY that prerelease — not the stable release of the same core, and
        // not any higher prerelease. Before the fix `exact_upper` bumped the patch
        // and the interval `[1.2.4-rc.1, 1.2.5)` swallowed `1.2.4` and `1.2.4-rc.2`.
        let con = c("1.2.4-rc.1");
        assert!(con.matches(&v("1.2.4-rc.1")), "must match itself");
        assert!(
            !con.matches(&v("1.2.4")),
            "must NOT leak to the stable core"
        );
        assert!(
            !con.matches(&v("1.2.4-rc.2")),
            "must NOT admit a higher prerelease"
        );
        assert!(
            !con.matches(&v("1.2.4-rc.1.0")),
            "the exclusive successor itself is out of range"
        );
    }

    #[test]
    fn explicit_equals_and_le_prerelease_bounds() {
        // `=1.2.4-rc.1` behaves like the bare exact pin.
        let eq = c("=1.2.4-rc.1");
        assert!(eq.matches(&v("1.2.4-rc.1")));
        assert!(!eq.matches(&v("1.2.4")));
        // `<=1.2.4-rc.1` must NOT select the stable `1.2.4` (an upper-bound
        // violation: stable sorts ABOVE the prerelease ceiling).
        let le = c("<=1.2.4-rc.1");
        assert!(le.matches(&v("1.2.4-rc.1")));
        assert!(
            !le.matches(&v("1.2.4")),
            "stable exceeds the prerelease ceiling"
        );
        assert!(!le.matches(&v("1.2.4-rc.2")), "above the ceiling");
        assert!(le.matches(&v("1.2.4-rc.0")), "below the ceiling is fine");
    }

    #[test]
    fn exact_prerelease_highest_match_selects_prerelease() {
        // With both the prerelease and the stable present, the exact prerelease pin
        // resolves to the prerelease (not the higher stable release).
        let cands = vec![
            v("1.2.4-alpha"),
            v("1.2.4-rc.1"),
            v("1.2.4-rc.2"),
            v("1.2.4"),
        ];
        assert_eq!(
            highest_match(&cands, &c("1.2.4-rc.1")),
            Some(&v("1.2.4-rc.1"))
        );
        // With the stable removed, an exact `1.2.4-alpha` picks `1.2.4-alpha`, NOT
        // the higher `1.2.4-rc.2` (proving the pin is truly exact, not a range over
        // the whole 1.2.4 core).
        let cands2 = vec![v("1.2.4-alpha"), v("1.2.4-rc.1"), v("1.2.4-rc.2")];
        assert_eq!(
            highest_match(&cands2, &c("1.2.4-alpha")),
            Some(&v("1.2.4-alpha"))
        );
    }

    #[test]
    fn exact_stable_pin_still_works() {
        // The stable exact path is unchanged: `1.2.4` pins exactly `1.2.4`.
        let con = c("1.2.4");
        assert!(con.matches(&v("1.2.4")));
        assert!(!con.matches(&v("1.2.5")));
        assert!(!con.matches(&v("1.2.3")));
        // And does NOT admit a same-core prerelease (strict policy).
        assert!(!con.matches(&v("1.2.4-rc.1")));
    }
}
