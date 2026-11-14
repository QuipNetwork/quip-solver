//! The module dependency rules of `docs/ARCHITECTURE.md`, checked against the
//! source. Every `crate::` path (and every `super::` path that climbs to the
//! crate root) in production code is an edge from the top-level module that
//! holds the file to the top-level module the path names. Test modules, inline
//! or in their own file, are skipped, because a test may reach across layers
//! to build its fixtures.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "helpers outside #[test] read the source tree and fail the test on I/O errors"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The layers each top-level module may import, besides itself. Every
/// top-level module of `src/` is its own layer. `testing` holds test doubles,
/// built only for tests, and may import any module but `cli`.
const MAY_IMPORT: &[(&str, &[&str])] = &[
    ("types", &[]),
    ("observe", &["types"]),
    ("config", &["types"]),
    ("host", &["types"]),
    ("chain", &["types", "observe", "host"]),
    ("miner", &["types", "observe", "config"]),
    ("coordinator", &["types", "observe", "miner"]),
    ("dashboard", &["types", "observe"]),
    (
        "app",
        &[
            "types",
            "observe",
            "config",
            "host",
            "chain",
            "miner",
            "coordinator",
            "dashboard",
        ],
    ),
    ("cli", &["app", "chain", "config", "types"]),
    (
        "testing",
        &[
            "types",
            "observe",
            "config",
            "host",
            "chain",
            "miner",
            "coordinator",
            "dashboard",
            "app",
        ],
    ),
];

/// Paths the layer rules forbid but the design keeps on purpose: the importing
/// top-level module, the imported path, and why. A path matches when it starts
/// with the listed one.
const EXCEPTIONS: &[(&str, &str, &str)] = &[(
    "config",
    "chain::registration::funding",
    "the config parser reads its funding defaults from the module that uses them",
)];

/// One production use of another top-level module.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Use {
    /// The file, relative to `src/`.
    file: String,
    /// The path used, from the top-level module on, such as `chain::registration::funding`.
    path: String,
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The item after a `#[cfg(test)]` at line `at`, without visibility: the
/// line index and the item text.
fn item_after_cfg_test<'a>(lines: &[&'a str], at: usize) -> (usize, &'a str, &'a str) {
    let mut j = at + 1;
    while lines.get(j).is_some_and(|l| l.trim().starts_with("#[")) {
        j += 1;
    }
    let item = lines.get(j).copied().unwrap_or_default();
    let indent = &item[..item.len() - item.trim_start().len()];
    let head = item.trim_start();
    let head = head
        .strip_prefix("pub(crate) ")
        .or_else(|| head.strip_prefix("pub "))
        .unwrap_or(head);
    (j, indent, head)
}

/// Drop inline `#[cfg(test)]` modules, out-of-line test module declarations,
/// and comment lines. The source is rustfmt-formatted, so an inline module
/// closes on the first line that is its own indentation followed by `}`.
fn production_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&line) = lines.get(i) {
        if line.trim() == "#[cfg(test)]" {
            let (j, indent, head) = item_after_cfg_test(&lines, i);
            if head.starts_with("mod ") && head.ends_with(';') {
                i = j + 1;
                continue;
            }
            if head.starts_with("mod ") && head.ends_with('{') {
                let close = format!("{indent}}}");
                let mut k = j + 1;
                while lines.get(k).is_some_and(|l| *l != close) {
                    k += 1;
                }
                i = k + 1;
                continue;
            }
        }
        if !line.trim_start().starts_with("//") {
            out.push(line);
        }
        i += 1;
    }
    out
}

/// Files that hold an out-of-line `#[cfg(test)] mod name;` of `file`.
fn test_module_files(file: &Path, text: &str) -> Vec<PathBuf> {
    let lines: Vec<&str> = text.lines().collect();
    let dir = if file.file_name().is_some_and(|n| n == "mod.rs") {
        file.parent().map(Path::to_path_buf)
    } else {
        file.parent()
            .zip(file.file_stem())
            .map(|(parent, stem)| parent.join(stem))
    };
    let Some(dir) = dir else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let (_, _, head) = item_after_cfg_test(&lines, i);
        if let Some(name) = head
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            out.push(dir.join(format!("{name}.rs")));
            out.push(dir.join(name).join("mod.rs"));
        }
    }
    out
}

/// Paths that follow each occurrence of `prefix` in `line`, as far as they
/// run through `ident::` segments: `crate::chain::registration::funding::X` gives
/// `chain::registration::funding::X`.
fn paths_after(line: &str, prefix: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find(prefix) {
        let before = rest[..at].chars().next_back();
        let tail = &rest[at + prefix.len()..];
        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':') {
            let end = tail
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                .unwrap_or(tail.len());
            paths.push(tail[..end].trim_end_matches(':').to_owned());
        }
        rest = tail;
    }
    paths
}

/// Production uses between top-level modules, by `(from, to)`.
fn module_edges() -> BTreeMap<(String, String), BTreeSet<Use>> {
    let src = src_dir();
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let tops: BTreeSet<String> = std::fs::read_dir(&src)
        .expect("read src")
        .map(|entry| entry.expect("directory entry").path())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            if path.is_dir() {
                Some(name)
            } else {
                name.strip_suffix(".rs").map(str::to_owned)
            }
        })
        .filter(|name| name != "lib" && name != "main")
        .collect();
    let texts: BTreeMap<PathBuf, String> = files
        .iter()
        .map(|file| {
            let text = std::fs::read_to_string(file).expect("read source");
            (file.clone(), text)
        })
        .collect();
    let test_files: BTreeSet<PathBuf> = texts
        .iter()
        .flat_map(|(file, text)| test_module_files(file, text))
        .collect();

    let mut edges: BTreeMap<(String, String), BTreeSet<Use>> = BTreeMap::new();
    for (file, text) in &texts {
        if test_files.contains(file) {
            continue;
        }
        let rel = file.strip_prefix(&src).expect("file under src");
        let parts: Vec<String> = rel
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        let Some(first) = parts.first() else {
            continue;
        };
        let top = first.trim_end_matches(".rs").to_owned();
        if top == "lib" || top == "main" {
            continue;
        }
        // Module depth below the crate root: `a.rs` and `a/mod.rs` are 1,
        // `a/b.rs` is 2. That many `super::` segments reach the crate root.
        let depth = parts.len() - usize::from(parts.last().is_some_and(|p| p == "mod.rs"));
        let to_root = "super::".repeat(depth);
        for line in production_lines(text) {
            let mut used = paths_after(line, "crate::");
            used.extend(paths_after(line, &to_root));
            for path in used {
                let name = path.split("::").next().unwrap_or_default();
                if name != top && tops.contains(name) {
                    let _ = edges
                        .entry((top.clone(), name.to_owned()))
                        .or_default()
                        .insert(Use {
                            file: rel.display().to_string(),
                            path,
                        });
                }
            }
        }
    }
    edges
}

fn allowed(from: &str, to: &str) -> bool {
    let may = MAY_IMPORT
        .iter()
        .find(|(name, _)| *name == from)
        .map_or_else(
            || panic!("top-level module {from} has no row in MAY_IMPORT"),
            |(_, may)| *may,
        );
    may.contains(&to)
}

fn excepted(from: &str, path: &str) -> bool {
    EXCEPTIONS
        .iter()
        .any(|(f, prefix, _)| *f == from && path.starts_with(prefix))
}

#[test]
fn every_module_edge_follows_the_layer_rules() {
    let edges = module_edges();
    let mut violations = Vec::new();
    for ((from, to), uses) in &edges {
        if allowed(from, to) {
            continue;
        }
        for u in uses.iter().filter(|u| !excepted(from, &u.path)) {
            violations.push(format!("{from} -> {} in {}", u.path, u.file));
        }
    }
    assert!(
        violations.is_empty(),
        "forbidden module edges:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_listed_exception_is_still_used() {
    let edges = module_edges();
    let stale: Vec<String> = EXCEPTIONS
        .iter()
        .filter(|(from, prefix, _)| {
            !edges
                .iter()
                .any(|((f, _), uses)| f == from && uses.iter().any(|u| u.path.starts_with(prefix)))
        })
        .map(|(from, prefix, _)| format!("{from} -> {prefix}"))
        .collect();
    assert!(
        stale.is_empty(),
        "listed exceptions no longer occur; remove them from the list:\n{}",
        stale.join("\n")
    );
}

/// Depth-first search with an explicit path: a successor already on the path
/// closes a cycle, which is returned.
fn find_cycle<'a>(
    node: &'a str,
    graph: &BTreeMap<&'a str, Vec<&'a str>>,
    path: &mut Vec<&'a str>,
    done: &mut BTreeSet<&'a str>,
) -> Option<Vec<&'a str>> {
    if done.contains(node) {
        return None;
    }
    if let Some(start) = path.iter().position(|n| *n == node) {
        let mut cycle: Vec<&str> = path.iter().skip(start).copied().collect();
        cycle.push(node);
        return Some(cycle);
    }
    path.push(node);
    for next in graph.get(node).into_iter().flatten() {
        if let Some(cycle) = find_cycle(next, graph, path, done) {
            return Some(cycle);
        }
    }
    let _ = path.pop();
    let _ = done.insert(node);
    None
}

#[test]
fn the_module_graph_has_no_cycles() {
    let edges = module_edges();
    let mut graph: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in edges.keys() {
        graph.entry(from.as_str()).or_default().push(to.as_str());
    }
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        if let Some(cycle) = find_cycle(node, &graph, &mut Vec::new(), &mut done) {
            panic!("module cycle: {}", cycle.join(" -> "));
        }
    }
}
