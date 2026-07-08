//! Structural documentation gate: the CLI surface (`tsm --help`, recursively)
//! must stay in sync with `docs/command-reference.md`. This catches the two
//! mechanical drift kinds — a command/flag that exists but is undocumented, and
//! a command that was removed but still appears in the docs — deterministically,
//! so they fail in CI/pre-push rather than slipping to review. Semantic drift
//! (prose that contradicts behavior) is out of scope here; see the `verify-docs`
//! skill.

use std::collections::BTreeMap;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tsm");
const DOC: &str = include_str!("../docs/command-reference.md");

/// Global/universal flags that are not part of any single command's documented
/// surface (the global `--project-root`, and clap's built-in help/version).
const GLOBAL_FLAGS: &[&str] = &["--help", "--version", "--project-root"];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Whole-token containment: `phrase` occurs in `doc` bounded by non-word bytes on
/// both sides. Substring matching is too loose — `--file` would match inside
/// `--files-from-stdin`, and `tsm reload` inside `tsm reloads`.
fn mentions(doc: &str, phrase: &str) -> bool {
    let bytes = doc.as_bytes();
    let len = phrase.len();
    doc.match_indices(phrase).any(|(i, _)| {
        let left_ok = i == 0 || !is_word_byte(bytes[i - 1]);
        let right_ok = i + len >= bytes.len() || !is_word_byte(bytes[i + len]);
        left_ok && right_ok
    })
}

/// `tsm <path...> --help`, captured. Help is pure (no DB/config), so this is a
/// safe, side-effect-free way to read the live CLI surface.
fn help_for(path: &[String]) -> String {
    let out = Command::new(BIN)
        .args(path)
        .arg("--help")
        .output()
        .unwrap_or_else(|e| panic!("running `tsm {} --help`: {e}", path.join(" ")));
    String::from_utf8(out.stdout).expect("help output is utf-8")
}

/// The indented entries of a `Header:` block (e.g. `Commands:` / `Options:`):
/// the lines after the header up to the next blank line. Output is captured (a
/// pipe), so clap does not wrap — one entry per line.
fn section_entries<'a>(help: &'a str, header: &str) -> Vec<&'a str> {
    let mut lines = help.lines().skip_while(|l| l.trim_end() != header);
    lines.next(); // consume the header line
    lines
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .collect()
}

/// Subcommand names under a command (excludes clap's auto-added `help`).
fn subcommands(help: &str) -> Vec<String> {
    section_entries(help, "Commands:")
        .iter()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .map(String::from)
        .collect()
}

/// Command-specific long flags (`--name`), excluding the global/universal ones.
fn flags(help: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in section_entries(help, "Options:") {
        for tok in line.split_whitespace() {
            if tok.starts_with("--") {
                let name = tok.trim_end_matches(',').to_string();
                if !GLOBAL_FLAGS.contains(&name.as_str()) && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// The full command tree, discovered by walking `--help` from the root.
struct CliTree {
    /// path (space-joined) -> subcommand names; empty vec = leaf command.
    nodes: BTreeMap<String, Vec<String>>,
    /// path (space-joined) -> command-specific flags.
    flags: BTreeMap<String, Vec<String>>,
}

fn discover() -> CliTree {
    let mut nodes = BTreeMap::new();
    let mut flag_map = BTreeMap::new();
    let mut stack: Vec<Vec<String>> = vec![vec![]];
    while let Some(path) = stack.pop() {
        let help = help_for(&path);
        let subs = subcommands(&help);
        let key = path.join(" ");
        flag_map.insert(key.clone(), flags(&help));
        for sub in &subs {
            let mut child = path.clone();
            child.push(sub.clone());
            stack.push(child);
        }
        nodes.insert(key, subs);
    }
    CliTree {
        nodes,
        flags: flag_map,
    }
}

#[test]
fn every_command_is_documented() {
    let tree = discover();
    let mut missing = Vec::new();
    for path in tree.nodes.keys() {
        if path.is_empty() {
            continue; // the root itself has no `tsm <path>` heading
        }
        // The reference documents commands as `tsm <path>` (headings/examples).
        if !mentions(DOC, &format!("tsm {path}")) {
            missing.push(path.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "commands in `tsm --help` but absent from docs/command-reference.md: {missing:?}"
    );
}

#[test]
fn every_flag_is_documented() {
    let tree = discover();
    let mut missing = Vec::new();
    for (path, flags) in &tree.flags {
        for flag in flags {
            if !mentions(DOC, flag) {
                missing.push(format!("`{flag}` (on `tsm {path}`)"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "flags in `tsm --help` but absent from docs/command-reference.md: {missing:?}"
    );
}

#[test]
fn doc_command_examples_resolve() {
    let tree = discover();
    let mut invalid = Vec::new();
    for example in tsm_examples(DOC) {
        if let Err(bad) = resolve(&example, &tree) {
            invalid.push(format!(
                "`{}` — unknown command path `{bad}`",
                example.join(" ")
            ));
        }
    }
    assert!(
        invalid.is_empty(),
        "command examples in docs/command-reference.md that don't resolve in `tsm --help` \
         (removed/renamed command still documented?): {invalid:?}"
    );
}

/// Tokenized `tsm …` invocations found inside fenced code blocks. Prose
/// mentions and non-command tokens (`tsm.toml`, `tsm-v0.7.0…`) are ignored: the
/// first whitespace token must be exactly `tsm`.
fn tsm_examples(doc: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            continue;
        }
        let mut toks = line.split_whitespace();
        if toks.next() == Some("tsm") {
            out.push(toks.map(String::from).collect());
        }
    }
    out
}

/// Walk an example's tokens against the command tree. While the current node is
/// a group, the next token must be a known subcommand; a flag/positional or the
/// end stops the walk (valid). A non-subcommand token where a subcommand is
/// required is drift. Returns `Err(bad_path)` on the offending prefix.
fn resolve(example: &[String], tree: &CliTree) -> Result<(), String> {
    let mut path: Vec<String> = vec![];
    for tok in example {
        if tok.starts_with('-') {
            break; // a flag: positionals/flags follow, command path is complete
        }
        let subs = match tree.nodes.get(&path.join(" ")) {
            Some(s) => s,
            None => break,
        };
        if subs.is_empty() {
            break; // leaf reached; remaining tokens are positionals
        }
        if subs.contains(tok) {
            path.push(tok.clone());
        } else {
            // A group was given a token that is not one of its subcommands.
            let mut bad = path.clone();
            bad.push(tok.clone());
            return Err(bad.join(" "));
        }
    }
    Ok(())
}
