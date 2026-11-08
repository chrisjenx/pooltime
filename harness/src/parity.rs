//! `docs/parity.md` must map every parity-inventory item (spec §16; Task 42). Test-only: runs
//! under `cargo test`, needs neither bun nor a bro checkout.
//!
//! An inventory item is, per numbered `## n.` section of
//! `docs/specs/2026-11-07-pooltime-parity-inventory.md`: every `- ` bullet, every row of a table
//! (the §1 response classification), every `**Name**:` paragraph, and every numbered §10 gap. Its
//! key is `§<n> <first four words>` (markdown stripped, `|` as `/`), `§1 row: <first cell words>`
//! for table rows, and `§10.<k>` for the gaps. `docs/parity.md` has one checklist row per key.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::scenarios;
use crate::workspace_root;

const STATUSES: &[&str] = &["ported", "changed", "dropped", "fixed", "partial"];

fn words(text: &str) -> String {
    text.replace(['*', '`'], "")
        .replace('|', "/")
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every inventory item's key, in document order.
fn inventory_keys(inv: &str) -> Vec<String> {
    let mut section: Option<String> = None;
    let mut keys = Vec::new();
    for line in inv.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            section = h
                .split_once(". ")
                .map(|(n, _)| n.to_string())
                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            continue;
        }
        let Some(s) = &section else { continue };
        if let Some(bullet) = line.strip_prefix("- ") {
            keys.push(format!("§{s} {}", words(bullet)));
        } else if let Some((n, _)) = line
            .split_once(". ")
            .filter(|(n, _)| s == "10" && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            keys.push(format!("§10.{n}"));
        } else if let Some(row) = line.strip_prefix("| ") {
            let cell = row.split('|').next().unwrap_or("");
            // The table's header row names the columns; it is not an item.
            if cell.trim() != "Upstream" {
                keys.push(format!("§{s} row: {}", words(cell)));
            }
        } else if let Some(rest) = line.strip_prefix("**") {
            if let Some((name, _)) = rest.split_once("**:") {
                keys.push(format!("§{s} {}", words(name)));
            }
        }
    }
    keys
}

/// A checklist row of `docs/parity.md`.
struct Row {
    key: String,
    status: String,
    notes: String,
    tests: Vec<String>,
}

fn checklist_rows(doc: &str) -> Vec<Row> {
    doc.lines()
        .filter(|l| l.starts_with("| `§"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let cell = |i: usize| cells.get(i).copied().unwrap_or("").to_string();
            let tests = cell(4)
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect();
            Row {
                key: cell(1).trim_matches('`').to_string(),
                status: cell(2),
                notes: cell(3),
                tests,
            }
        })
        .collect()
}

/// Names of every `#[test]` / `#[tokio::test]` function under `dirs`.
fn test_fn_names(root: &Path, dirs: &[&str]) -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for d in dirs {
        walk(&root.join(d), &mut files);
    }
    let mut names = BTreeSet::new();
    for f in files {
        let text = fs::read_to_string(&f).unwrap_or_default();
        let mut pending = false;
        for line in text.lines() {
            let t = line.trim_start();
            if t.starts_with("#[test]") || t.starts_with("#[tokio::test") {
                pending = true;
            } else if pending {
                if let Some((_, rest)) = t.split_once("fn ") {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    names.insert(name);
                    pending = false;
                }
            }
        }
    }
    names
}

fn read(rel: &str) -> String {
    fs::read_to_string(workspace_root().join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

#[test]
fn inventory_keys_cover_bullets_rows_paragraphs_and_gaps() {
    let inv = "intro\n- not in a section\n## 1. One\n**Group**\n- **Bold** `code` a|b c d e\n\
               | Upstream | Behavior |\n|---|---|\n| 429 **without** x | y |\n\
               **Exhaustion**: text\n## 10. Gaps\n1. first\n   continued\n2. second\n";
    assert_eq!(
        inventory_keys(inv),
        vec![
            "§1 Bold code a/b c",
            "§1 row: 429 without x",
            "§1 Exhaustion",
            "§10.1",
            "§10.2",
        ]
    );
}

#[test]
fn parity_doc_has_one_row_per_inventory_item() {
    let inv = inventory_keys(&read("docs/specs/2026-11-07-pooltime-parity-inventory.md"));
    let unique: BTreeSet<&String> = inv.iter().collect();
    assert_eq!(unique.len(), inv.len(), "inventory keys must be unique");
    assert!(
        inv.len() > 80,
        "inventory parse found only {} items",
        inv.len()
    );

    let rows = checklist_rows(&read("docs/parity.md"));
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &rows {
        *seen.entry(r.key.as_str()).or_default() += 1;
    }
    let missing: Vec<&String> = inv
        .iter()
        .filter(|k| !seen.contains_key(k.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "INV items without a parity row: {missing:#?}"
    );
    let stale: Vec<&&str> = seen
        .keys()
        .filter(|k| !unique.contains(&k.to_string()))
        .collect();
    assert!(
        stale.is_empty(),
        "parity rows matching no INV item: {stale:#?}"
    );
    let dup: Vec<(&&str, &usize)> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(dup.is_empty(), "INV items with several rows: {dup:?}");

    for r in &rows {
        assert!(
            STATUSES.contains(&r.status.as_str()),
            "{}: unknown status {:?}",
            r.key,
            r.status
        );
        let gap = r.key.starts_with("§10.");
        assert_eq!(
            gap,
            matches!(r.status.as_str(), "fixed" | "partial"),
            "{}: §10 gaps (and only they) are fixed/partial",
            r.key
        );
        if r.status != "dropped" {
            assert!(!r.tests.is_empty(), "{}: no pooltime test cited", r.key);
        }
        if r.status != "ported" {
            assert!(
                !r.notes.is_empty(),
                "{}: {} needs a reason",
                r.key,
                r.status
            );
        }
    }
}

#[test]
fn parity_doc_cites_only_existing_tests() {
    let root = workspace_root();
    let fns = test_fn_names(&root, &["src", "tests", "harness/src"]);
    let scenario_names: BTreeSet<&str> = scenarios::all().iter().map(|s| s.name).collect();
    let mut unknown = Vec::new();
    for r in checklist_rows(&read("docs/parity.md")) {
        for t in &r.tests {
            let ok = match t.strip_prefix("harness:") {
                Some(s) => scenario_names.contains(s),
                None => fns.contains(t),
            };
            if !ok {
                unknown.push(format!("{}: {t}", r.key));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "cited tests that do not exist: {unknown:#?}"
    );
}
