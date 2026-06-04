//! The build lockfile: a deterministic, reproducible record of the resolved
//! build graph and its input fingerprints.
//!
//! k2 — *Kardashev Type II*: total control over the machine, with zero waste.
//!
//! After `build(b)` records the [`BuildGraph`], the driver serializes the graph
//! together with the content hashes of every resolved `.k2` input into
//! `build.lock` in the build root. The format is line-oriented and FULLY SORTED,
//! so identical inputs and identical `-D` flags always produce a byte-identical
//! lock — the offline, local realization of the §08.7 content-addressed lockfile
//! (no network fetch in this milestone). A changed input flips its content hash,
//! which flips `graph_hash`, making drift visible.
//!
//! The content hash is a small, dependency-free FNV-1a over file bytes (std-only,
//! offline) — sufficient for change detection and reproducibility, not a
//! cryptographic integrity claim (that is the post-0.12 networked package
//! manager's job).

use std::path::Path;

use k2_vm::BuildGraph;

use crate::multi::InputFiles;

/// FNV-1a over bytes, rendered as 16-hex. The same hash the multi-file merge uses
/// for module names; here it fingerprints file contents and the canonical lock
/// serialization.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Serializes the resolved graph + input fingerprints into the canonical lock
/// text. Every collection is emitted in sorted order, so the result is
/// deterministic for identical inputs. The `graph_hash` line is the FNV-1a of the
/// canonical body (everything below it), so two runs over identical inputs yield
/// a byte-identical lock with a matching hash.
pub fn serialize(graph: &BuildGraph, inputs: &InputFiles, dopts: &[(String, String)]) -> String {
    // Build the canonical body first (without the graph_hash line), then prepend
    // its hash. Module content hashes are read from disk.
    let mut body = String::new();

    body.push_str(&format!("target = {}\n", graph.target.triple()));
    body.push_str(&format!("optimize = {}\n", graph.optimize.name()));

    // [dopts] — the command-line -D map, sorted by key.
    let mut sorted_dopts: Vec<&(String, String)> = dopts.iter().collect();
    sorted_dopts.sort_by(|a, b| a.0.cmp(&b.0));
    body.push_str("[dopts]\n");
    for (k, v) in sorted_dopts {
        body.push_str(&format!("{k} = {v}\n"));
    }

    // [options] — declared options, sorted by name.
    let mut opts: Vec<&k2_vm::DeclaredOption> = graph.options.iter().collect();
    opts.sort_by(|a, b| a.name.cmp(&b.name));
    body.push_str("[options]\n");
    for o in opts {
        body.push_str(&format!("{} {}\n", o.name, o.kind));
    }

    // [modules] — every resolved input file, sorted by relative path, with its
    // content hash.
    body.push_str("[modules]\n");
    for (rel, abs) in &inputs.files {
        let h = match std::fs::read(abs) {
            Ok(bytes) => fnv1a_hex(&bytes),
            Err(_) => "missing".to_string(),
        };
        body.push_str(&format!("{rel} h={h}\n"));
    }

    // [artifacts] — in creation order (already deterministic).
    body.push_str("[artifacts]\n");
    for a in &graph.artifacts {
        let root = a.root_source.clone().unwrap_or_default();
        let mut mods: Vec<String> = a
            .modules
            .iter()
            .map(|(n, id)| format!("{n}->{id}"))
            .collect();
        mods.sort();
        let mut opts: Vec<String> = a
            .options
            .iter()
            .map(|(n, v)| format!("{n}={}", v.display()))
            .collect();
        opts.sort();
        let exe = a.exe_id.map(|e| format!(" exe={e}")).unwrap_or_default();
        body.push_str(&format!(
            "{} {} root={} modules=[{}] options=[{}]{}\n",
            a.kind.keyword(),
            a.name,
            root,
            mods.join(","),
            opts.join(","),
            exe,
        ));
    }

    // [steps] — named steps sorted by name, with their dep step ids.
    body.push_str("[steps]\n");
    let mut steps: Vec<&k2_vm::StepNode> =
        graph.steps.iter().filter(|s| s.name.is_some()).collect();
    steps.sort_by(|a, b| a.name.cmp(&b.name));
    for s in steps {
        let mut deps: Vec<String> = s.deps.iter().map(|d| d.to_string()).collect();
        deps.sort();
        body.push_str(&format!(
            "{} deps=[{}]\n",
            s.name.as_deref().unwrap_or(""),
            deps.join(",")
        ));
    }

    // [install] — installed artifact ids, sorted.
    body.push_str("[install]\n");
    let mut install: Vec<u32> = graph.install.clone();
    install.sort_unstable();
    let install_str: Vec<String> = install.iter().map(|i| i.to_string()).collect();
    body.push_str(&format!("ids=[{}]\n", install_str.join(",")));

    let graph_hash = fnv1a_hex(body.as_bytes());
    format!("# k2 build lock v1\ngraph_hash = {graph_hash}\n{body}")
}

/// Writes the lock to `path`, but ONLY if it differs from the existing file (no
/// mtime churn on a reproducible rebuild, like `fmt --write`). Returns `true` if
/// the lock was already up to date (a reproducible rebuild), `false` if it was
/// (re)written.
pub fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == contents {
            return Ok(true);
        }
    }
    std::fs::write(path, contents)?;
    Ok(false)
}
