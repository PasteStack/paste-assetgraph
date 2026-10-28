// Copyright (c) 2017–present Tomshley LLC
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use walkdir::WalkDir;

use crate::config::Config;
use crate::jam::{AssetType, OutputDirs};
use crate::manifest::{AssetEntry, AssetUpdate, Manifest};
use crate::storage::StorageBackend;

/// The comment spans oxc's real lexer recorded while parsing `content`
/// as a classic script — the only source of JS declarations, as
/// `(byte offset, text)` pairs. A lexer that cannot tell a regex literal
/// or character class from a comment opener invents declarations
/// (`/[// @module x]/` is a regex, not a comment), so no hand scanner
/// may substitute: even on a source oxc cannot fully parse the trivia it
/// collected is still real comment text, and a source with no lexed
/// comments simply has no declarations — guessing at the surrounding
/// tokens could manufacture one. When the parser gives up (`panicked`) it
/// returns no comments at all, so the spans are unknown: None.
fn js_comment_spans(content: &str) -> Option<Vec<(usize, &str)>> {
    use oxc::allocator::Allocator;
    use oxc::parser::Parser;
    use oxc::span::SourceType;

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, content, SourceType::cjs()).parse();
    if parsed.panicked {
        return None;
    }
    Some(
        parsed
            .program
            .comments
            .iter()
            .filter_map(|comment| {
                content
                    .get(comment.span.start as usize..comment.span.end as usize)
                    .map(|text| (comment.span.start as usize, text))
            })
            .collect(),
    )
}

/// Every `//` line comment and `/* ... */` block comment in a stylesheet,
/// as `(byte offset, text)` pairs, extracted while skipping
/// `'...'`/`"..."` strings and unquoted `url(...)` content — an unquoted
/// URL's `//` is address data, not a comment opener, so `url(//cdn/x)`
/// cannot manufacture a declaration. Plain `.css` sources must declare in
/// `/* … */` comments: grass's plain-CSS parser rejects `//` comments, so a
/// `.css` file written with one fails to compile whatever it declares.
fn style_comment_spans(content: &str) -> Vec<(usize, &str)> {
    let bytes = content.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                spans.push((start, &content[start..i]));
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                spans.push((start, &content[start..i]));
            }
            // `url(` with an unquoted argument consumes raw bytes to its
            // closing paren — `//` or `/*` inside are URL data. A quoted
            // argument falls through to the string branch on the quote.
            b'u' | b'U'
                if (i > 0
                    && !matches!(bytes[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'))
                    || i == 0 =>
            {
                // Byte-level recognition only: `i + 3` is not a known UTF-8
                // boundary (`u€` ends the range inside the euro sign), so
                // no &str slice may be constructed here.
                if bytes[i..].len() >= 3 && bytes[i..i + 3].eq_ignore_ascii_case(b"url") {
                    let mut j = i + 3;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'(') {
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if !matches!(bytes.get(j), Some(b'"' | b'\'')) {
                            while j < bytes.len() && bytes[j] != b')' {
                                j += if bytes[j] == b'\\' { 2 } else { 1 };
                            }
                            i = (j + 1).min(bytes.len());
                            continue;
                        }
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    spans
}

/// The 1-based line number `offset` lands on in `content` — the position
/// a declaration error reports.
fn line_of(content: &str, offset: usize) -> usize {
    content[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}

/// Whether `at` begins a recognized annotation keyword — `@module` or
/// `@requires` — on a keyword boundary (`@modules` is not `@module`).
fn annotation_at(at: &str) -> Option<(&'static str, usize)> {
    for kw in ["@module", "@requires"] {
        if let Some(rest) = at.strip_prefix(kw) {
            // `-`, `.`, `/` and `:` continue a word too: `@module-federation`,
            // `@module.exports` and `@requires:` are text about other
            // tools, not declarations.
            if !rest.chars().next().is_some_and(|c| {
                regex_syntax::is_word_character(c) || matches!(c, '-' | '.' | '/' | ':')
            }) {
                return Some((kw, kw.len()));
            }
        }
    }
    None
}

/// One parsed declaration occurrence inside a real comment: the byte
/// offset of its `@` in the comment's inner text, the keyword, and the
/// declared name as written (slashes still in it — normalization and
/// name validation are the caller's job, after syntactic parsing).
struct ParsedDecl<'a> {
    pos: usize,
    keyword: &'static str,
    name: &'a str,
}

/// A declaration occurrence that failed the grammar: the `@`'s offset in
/// the inner text and the keyword it attempted, so the error can name
/// both the annotation's real line and its complete text.
struct MalformedDecl {
    pos: usize,
    keyword: &'static str,
}

/// The exact annotation grammar applied to one real comment's inner
/// text (markers already stripped by the caller): each `@module` or
/// `@requires` occurrence must be `KEY <name>` — at least one whitespace
/// after the keyword, then exactly one name token, then nothing but
/// whitespace to the end of the line, the end of the comment, or the
/// next recognized annotation. `@module` alone keeps the legacy quoted
/// spelling, but a quote opened must close before the token boundary.
/// EVERY occurrence is validated — a malformed later one fails even when
/// an earlier one parsed, and material trailing a name is never dropped.
fn parse_annotations(inner: &str) -> std::result::Result<Vec<ParsedDecl<'_>>, MalformedDecl> {
    let bytes = inner.as_bytes();
    let mut decls = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let Some((keyword, kwlen)) = annotation_at(&inner[i..]) else {
            i += 1;
            continue;
        };
        let start = i;
        // The argument requires whitespace after the keyword — bare
        // `@module` and `@module(foo)` are malformed, not "no declaration".
        i += kwlen;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == start + kwlen || i >= bytes.len() {
            return Err(MalformedDecl {
                pos: start,
                keyword,
            });
        }
        // The name token: `[^\s"']+`, or a `"`-quoted one with a required
        // closing quote (`@module` compatibility spelling).
        let name;
        if keyword == "@module" && bytes[i] == b'"' {
            i += 1;
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b'"' | b'\'')
            {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'"' {
                return Err(MalformedDecl {
                    pos: start,
                    keyword,
                });
            }
            name = &inner[name_start..i];
            i += 1;
        } else {
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b'"' | b'\'')
            {
                i += 1;
            }
            name = &inner[name_start..i];
        }
        // After the argument, only horizontal whitespace may follow up to
        // the end of the line, the end of the comment, or the next
        // recognized annotation — `extra`, `trailing`, a stray quote, or
        // an unknown @tag here is material that must not be dropped.
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r') {
            i += 1;
        }
        // One optional `;` closes the declaration — the spelling released
        // paste-elements stylesheets use (`@module "paste.ui.autogrow";`).
        if i < bytes.len() && bytes[i] == b';' {
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r') {
                i += 1;
            }
        }
        if i < bytes.len() && bytes[i] != b'\n' && annotation_at(&inner[i..]).is_none() {
            return Err(MalformedDecl {
                pos: start,
                keyword,
            });
        }
        decls.push(ParsedDecl {
            pos: start,
            keyword,
            name,
        });
    }
    Ok(decls)
}

/// The comment's interior text and its byte offset within the comment —
/// `//x` → `x` at 2, `/*x*/` → `x` at 2, unterminated `/*x` → `x` at 2.
/// Stripping markers is what keeps a closing `*/` from reading as
/// material after a one-line block's declaration.
fn comment_inner(comment: &str) -> (&str, usize) {
    if let Some(rest) = comment.strip_prefix("//") {
        (rest, 2)
    } else if let Some(rest) = comment.strip_prefix("/*") {
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        (rest, 2)
    } else {
        (comment, 0)
    }
}

/// The complete offending annotation text for an error message: from the
/// `@` through the end of its line (or the comment's end).
fn segment_at(inner: &str, pos: usize) -> &str {
    let end = inner[pos..].find('\n').map_or(inner.len(), |n| pos + n);
    inner[pos..end].trim_end()
}

/// Whether one string or template literal's raw source text carries a JS
/// `\uXXXX` or `\u{...}` escape naming a lone surrogate. The escape is
/// legal JavaScript but has no Unicode scalar value, so a parser that
/// decodes it either mangles the character or reprints a changed escape;
/// the minifier must pass the source through untouched instead. A real
/// surrogate pair `\uD8..\uDC..` is a single code point and does not count.
fn literal_has_lone_surrogate(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'u' {
                if let Some((consumed, lone)) = surrogate_escape(&raw[i..]) {
                    if lone {
                        return true;
                    }
                    i += consumed;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    false
}

/// The `\uXXXX` or `\u{X...}` escape at the start of `at` (which begins
/// with the backslash): `(bytes consumed, whether it names a lone
/// surrogate)`. A high surrogate immediately followed by a low-surrogate
/// escape is a valid pair — consumed as one unit, never lone.
fn surrogate_escape(at: &str) -> Option<(usize, bool)> {
    let rest = at.strip_prefix("\\u")?;
    if let Some(inner) = rest.strip_prefix('{') {
        let end = inner.find('}')?;
        let value = u32::from_str_radix(inner.get(..end)?, 16).ok()?;
        return Some((2 + 1 + end + 1, (0xD800..=0xDFFF).contains(&value)));
    }
    let value = u32::from_str_radix(rest.get(..4)?, 16).ok()?;
    if !(0xD800..=0xDBFF).contains(&value) {
        return Some((6, (0xDC00..=0xDFFF).contains(&value)));
    }
    // A high surrogate: valid only paired with a low-surrogate escape.
    match surrogate_escape_low(at.get(6..).unwrap_or("")) {
        Some(consumed) => Some((6 + consumed, false)),
        None => Some((6, true)),
    }
}

/// The immediately-following `\uXXXX` low-surrogate escape that pairs a
/// high surrogate, else None.
fn surrogate_escape_low(at: &str) -> Option<usize> {
    let rest = at.strip_prefix("\\u")?;
    let value = u32::from_str_radix(rest.get(..4)?, 16).ok()?;
    (0xDC00..=0xDFFF).contains(&value).then_some(6)
}

/// Whether `name` is a module name as the build writes it: MODULE_NAME_RE's
/// alphabet after `/`→`.` normalization — nonempty dot-separated segments of
/// Unicode word characters and `-`. Empty segments ("a..b", ".a", "a.") are
/// rejected: they would let a declared name reach outside the output tree.
/// A segmented character predicate, not a compiled regex — compiling `\w`'s
/// Unicode classes recurses deeper than a foreign runtime's worker stack
/// holds (a Node FFI worker runs `combo` on one, and the first call into
/// the process may be there).
pub fn is_module_name(name: &str) -> bool {
    name.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|ch| ch == '-' || regex_syntax::is_word_character(ch))
    })
}

/// Statement separator for concatenated JavaScript. A bare newline lets one
/// file's trailing expression merge with the next file's leading parenthesis
/// into a call; an explicit empty statement terminates it. Matches the
/// separator the Scala combo handler uses for the same reason.
pub const JS_CONCAT_SEPARATOR: &str = "\n;\n";

/// Directory names that hold build output or vendored dependencies rather than
/// sources.
///
/// The project's own output directories come from its config and are filtered by
/// path, but an include path points at another project, whose output directory
/// this build knows nothing about. Walking into it turns compiled assets into
/// modules — a minified copy of a module is registered a second time under a
/// name derived from its output path — so the graph gains entries no source
/// declares.
const BUILD_OUTPUT_DIRS: [&str; 5] = [
    "target",
    "dist",
    "node_modules",
    ".git",
    // The conventional beside-source build directory is never a source, even
    // when `build-dir-name` has since been configured to something else.
    "_paste_build",
];

/// True for a directory inside the walk that holds build output — a vendored
/// or build directory, or a `build-dir-name` directory that beside-source
/// output lands in (skipped in either layout, like the original's
/// `excluded_dirs + [build_prefix]`). The walk root itself is never skipped:
/// a caller may deliberately point an include path at a built distribution.
fn is_build_output(entry: &walkdir::DirEntry, build_dir_name: &str) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| BUILD_OUTPUT_DIRS.contains(&name) || name == build_dir_name)
}

/// Which asset pipeline a discovered source file feeds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Js,
    Scss,
    Css,
}

/// A typed failure only source edits can remedy. Watch mode may remain
/// active for it iff the offending path (or missing ancestry) is actually
/// observed; manifest/output/ownership failures remain fatal.
#[derive(Debug, thiserror::Error)]
#[error("watched input requires correction: {0:?}")]
struct RecoverableWatchInput(PathBuf);

/// Physical identity for an observed input, including an import candidate
/// that does not exist yet. Resolve the longest existing path *before*
/// simplifying any remaining components: `link/../name` crosses the target
/// of `link`, not the directory containing that symlink. Lexically removing
/// `..` first can register a watch on a completely different directory.
fn observed_input_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut current = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut physical) = fs::canonicalize(current) {
            for part in missing.iter().rev() {
                physical.push(part);
            }
            return physical;
        }
        let Some(parent) = current.parent() else {
            return absolute;
        };
        let component = match current.components().next_back() {
            Some(std::path::Component::Normal(name)) => name.to_os_string(),
            Some(std::path::Component::ParentDir) => std::ffi::OsString::from(".."),
            Some(std::path::Component::CurDir) => std::ffi::OsString::from("."),
            _ => return absolute,
        };
        missing.push(component);
        current = parent;
    }
}

/// The physical spelling of a directory entry, without following a
/// symlink in the final component. Filesystem notifications about an
/// import symlink's replacement name its entry, not its old target.
fn observed_entry_path(path: &Path) -> PathBuf {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(parent) = fs::canonicalize(parent) {
            return parent.join(name);
        }
    }
    observed_input_path(path)
}

/// Negative Sass probes retain their original expected operation, not
/// just the path. An `is_dir` lookup can fail on a valid file and must not
/// spuriously recover merely because that path already exists.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SassProbeKind {
    File,
    Directory,
    Read,
    Canonicalize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SassNegativeProbe {
    path: PathBuf,
    kind: SassProbeKind,
}

impl SassNegativeProbe {
    fn now_succeeds(&self) -> bool {
        match self.kind {
            SassProbeKind::File => self.path.is_file(),
            SassProbeKind::Directory => self.path.is_dir(),
            SassProbeKind::Read => fs::read(&self.path).is_ok(),
            SassProbeKind::Canonicalize => fs::canonicalize(&self.path).is_ok(),
        }
    }
}

/// Reads and failed existence/read probes from Sass compilations that did
/// not complete. Watch installs registrations for the attempted inputs and
/// compares this snapshot before it blocks on another filesystem event.
#[derive(Default)]
struct FailedSassSnapshot {
    reads: HashMap<PathBuf, Vec<u8>>,
    symlink_targets: HashMap<PathBuf, PathBuf>,
    negative_probes: BTreeSet<SassNegativeProbe>,
}

/// A `grass::Fs` that serves the entry file's declaration-scan bytes
/// verbatim and captures the bytes of every file the compiler reads —
/// the entry and each resolved `@import` alike — so the build can verify
/// the emitted CSS describes one coherent input state.
#[derive(Debug)]
struct SnapshotFs {
    /// (canonical path, bytes) for the file `from_path` was called on.
    entry: Option<(PathBuf, Vec<u8>)>,
    reads: std::cell::RefCell<HashMap<PathBuf, Vec<u8>>>,
    // The compiler may canonicalize a file symlink before reading it.
    // Retain the directory entry it traversed as an additional input:
    // replacing the link leaves the old physical target unchanged.
    aliases: std::cell::RefCell<HashMap<PathBuf, BTreeSet<PathBuf>>>,
    // Negative is_file/is_dir probes matter when a newly added import
    // fails to compile: creating the missing partial must wake watch even
    // though no successful read ever captured that file.
    negative_probes: std::cell::RefCell<BTreeSet<SassNegativeProbe>>,
}

impl SnapshotFs {
    fn new(entry_path: &Path, entry_bytes: Option<String>) -> Self {
        let entry = entry_bytes.map(|bytes| {
            (
                fs::canonicalize(entry_path).unwrap_or_else(|_| entry_path.to_path_buf()),
                bytes.into_bytes(),
            )
        });
        SnapshotFs {
            entry,
            reads: std::cell::RefCell::new(HashMap::new()),
            aliases: std::cell::RefCell::new(HashMap::new()),
            negative_probes: std::cell::RefCell::new(BTreeSet::new()),
        }
    }

    /// Every (path, bytes) the compiler read. Physical files are keyed by
    /// canonical path, so repeated reads use the first bytes. Direct file
    /// symlink entries also appear with those same bytes, to verify and
    /// watch their directory entry independently from the physical file.
    fn reads(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let reads = self.reads.borrow();
        let aliases = self.aliases.borrow();
        let mut observed: Vec<_> = reads
            .iter()
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect();
        for (target, paths) in aliases.iter() {
            if let Some(bytes) = reads.get(target) {
                observed.extend(paths.iter().map(|path| (path.clone(), bytes.clone())));
            }
        }
        observed
    }

    fn note_alias(&self, path: &Path) {
        // Capture links in every traversed path component, not only the
        // final filename. A directory symlink can be replaced while all
        // files under its old target keep their bytes, so watching those
        // files alone cannot discover the new target's later edits.
        for candidate in path.ancestors() {
            if !fs::symlink_metadata(candidate).is_ok_and(|meta| meta.file_type().is_symlink()) {
                continue;
            }
            if let Ok(target) = fs::canonicalize(candidate) {
                let alias = observed_entry_path(candidate);
                if alias != target {
                    self.aliases
                        .borrow_mut()
                        .entry(target)
                        .or_default()
                        .insert(alias);
                }
            }
        }
    }

    /// An import's link spelling and the exact physical file resolved at
    /// compile time. Equal source bytes do not imply equal dependencies:
    /// after a same-content link retarget, future target edits would go
    /// unnoticed if only the old physical file remained registered.
    fn symlink_targets(&self) -> HashMap<PathBuf, PathBuf> {
        let mut targets = HashMap::new();
        for (target, aliases) in self.aliases.borrow().iter() {
            for alias in aliases {
                targets.insert(alias.clone(), target.clone());
            }
        }
        targets
    }

    fn missing(&self) -> BTreeSet<PathBuf> {
        self.negative_probes
            .borrow()
            .iter()
            .map(|probe| probe.path.clone())
            .collect()
    }

    fn negative_probes(&self) -> BTreeSet<SassNegativeProbe> {
        self.negative_probes.borrow().clone()
    }

    fn note_failed_probe(&self, path: &Path, kind: SassProbeKind) {
        let mut negative = self.negative_probes.borrow_mut();
        negative.insert(SassNegativeProbe {
            path: observed_input_path(path),
            kind,
        });
        // A dangling file *or directory* symlink can point to a missing
        // target outside the alias parent. Creating the target does not
        // notify the alias directory, so follow each traversed symlink's
        // referent, including the unresolved suffix, as another probe.
        for link in path.ancestors() {
            let (Some(parent), Ok(meta), Ok(target)) = (
                link.parent(),
                fs::symlink_metadata(link),
                fs::read_link(link),
            ) else {
                continue;
            };
            if !meta.file_type().is_symlink() {
                continue;
            }
            let referent = if target.is_absolute() {
                target
            } else {
                parent.join(target)
            };
            let suffix = path.strip_prefix(link).unwrap_or(Path::new(""));
            negative.insert(SassNegativeProbe {
                path: observed_input_path(&referent.join(suffix)),
                kind,
            });
        }
    }

    fn check_path(&self, path: &Path, present: bool, kind: SassProbeKind) -> bool {
        if !present {
            self.note_failed_probe(path, kind);
        }
        present
    }
}

impl grass::Fs for SnapshotFs {
    fn is_file(&self, path: &Path) -> bool {
        self.check_path(path, path.is_file(), SassProbeKind::File)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.check_path(path, path.is_dir(), SassProbeKind::Directory)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.note_alias(path);
        if let Some((entry, bytes)) = &self.entry {
            if &canonical == entry {
                self.reads
                    .borrow_mut()
                    .entry(canonical)
                    .or_insert_with(|| bytes.clone());
                return Ok(bytes.clone());
            }
        }
        if let Some(bytes) = self.reads.borrow().get(&canonical) {
            return Ok(bytes.clone());
        }
        let bytes = fs::read(path).inspect_err(|_| {
            self.note_failed_probe(path, SassProbeKind::Read);
        })?;
        self.reads.borrow_mut().insert(canonical, bytes.clone());
        Ok(bytes)
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        let canonical = fs::canonicalize(path).inspect_err(|_| {
            self.note_failed_probe(path, SassProbeKind::Canonicalize);
        })?;
        self.note_alias(path);
        Ok(canonical)
    }
}

/// The set of manifest `source` labels a configured root can write —
/// used to prove whether a vanished root could have contributed a
/// recorded asset before that contribution is attributed to it.
enum LabelSpace {
    /// `base_dir`-relative labels resolving under exactly this root.
    Project(PathBuf),
    /// `<basename>` and `<basename>/…` — external include roots.
    Prefixed(String),
    /// Bare file names — external source roots and basenameless include
    /// roots.
    Bare,
}

/// How a configured source or include root resolves on disk — the shared
/// classification the preflight guard and discovery agree on, so a root
/// can never be optional to one check and mandatory to the other.
enum SourceRoot {
    /// Absent — optional on a first build, an outage once the manifest
    /// still serves assets sourced under it.
    Missing,
    /// Present and a directory — the only walkable state.
    Directory,
}

/// Classify one configured root. A `NotFound` stat is the optional-missing
/// case; a successful stat naming anything that is not a directory is a
/// named error — walking an ordinary file parked at the root's path would
/// scan zero sources and retire every module the manifest still serves.
/// Every other stat failure propagates: a root that cannot be examined
/// is not optional. (`fs::metadata` resolves a symlinked root to its
/// target, so a link to a real directory stays walkable.)
fn classify_root(root: &Path) -> Result<SourceRoot> {
    match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => Ok(SourceRoot::Directory),
        Ok(_) => anyhow::bail!(
            "configured source root is not a directory: {}",
            root.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SourceRoot::Missing),
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "cannot stat configured source root {}",
            root.display()
        ))),
    }
}

/// One asset file found by [`AssetGraph::discover_sources`].
struct DiscoveredSource {
    path: PathBuf,
    /// The walk root the file was found under — path-derived names and
    /// style asset keys are relative to it.
    root: PathBuf,
    /// Whether `root` is a configured source dir rather than an include
    /// path — include-path files never write into a source tree.
    from_source_dir: bool,
    kind: SourceKind,
}

/// The ordering of one asset type's module/dependency map, as written to
/// `sorted_deps`.
pub struct LayeredOrder {
    /// Modules released in dependency layers: each layer holds every node
    /// whose remaining dependencies were all released earlier, name-sorted
    /// within the layer. Dependencies come before their dependents.
    pub sorted: Vec<String>,
    /// Cycle members and anything depending on them, name-sorted. They are
    /// left out of `sorted` — no layer can release them — but still build.
    pub cyclic: Vec<String>,
}

/// Layer a module→declared-dependencies map for `sorted_deps`.
///
/// Self-dependencies are dropped. A dependency naming no known module joins
/// the layering as a leaf so the dependent still sorts, but the unknown name
/// never appears in the output. Each pass releases every node with no
/// remaining dependencies (name-sorted) and removes it from the other nodes'
/// remaining sets; when a pass releases nothing, whatever is left is cyclic.
fn layered_order(deps: &BTreeMap<String, Vec<String>>) -> LayeredOrder {
    let mut remaining: BTreeMap<String, Vec<String>> = deps
        .iter()
        .map(|(name, declared)| {
            (
                name.clone(),
                declared.iter().filter(|d| *d != name).cloned().collect(),
            )
        })
        .collect();
    let unknown: Vec<String> = remaining
        .values()
        .flatten()
        .filter(|d| !remaining.contains_key(*d))
        .cloned()
        .collect();
    for name in unknown {
        remaining.insert(name, Vec::new());
    }

    let mut sorted = Vec::new();
    loop {
        // BTreeMap iterates keys in name order, so the layer is sorted.
        let layer: Vec<String> = remaining
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        if layer.is_empty() {
            break;
        }
        for name in &layer {
            remaining.remove(name);
        }
        for deps in remaining.values_mut() {
            deps.retain(|d| !layer.contains(d));
        }
        sorted.extend(layer.into_iter().filter(|name| deps.contains_key(name)));
    }
    // Leftover unknown names are impossible — they carry no dependencies and
    // release in the first layer — but the filter keeps the guarantee local.
    let cyclic: Vec<String> = remaining
        .into_keys()
        .filter(|name| deps.contains_key(name))
        .collect();
    LayeredOrder { sorted, cyclic }
}

/// One module's full dependency list for its manifest entry: the declared
/// deps in declaration order — deduplicated, self removed — followed by the
/// transitive deps appended in pre-order, first occurrence wins. The visited
/// guard ends recursion on the first revisit, so a cyclic graph still
/// terminates.
fn backfilled_deps(deps: &BTreeMap<String, Vec<String>>, module: &str) -> Vec<String> {
    expand_declared_deps(
        deps,
        module,
        deps.get(module).map(Vec::as_slice).unwrap_or(&[]),
    )
}

/// The dependency list a file's own `declared` set expands to — the same
/// traversal `backfilled_deps` performs, but seeded by an explicit declared
/// list rather than a map lookup, for a file whose name is not a module in
/// the map (a plain path-keyed asset). `self_name` only seeds the
/// self-dependency guard.
fn expand_declared_deps(
    deps: &BTreeMap<String, Vec<String>>,
    self_name: &str,
    declared: &[String],
) -> Vec<String> {
    fn walk<'a>(
        node: &'a str,
        deps: &'a BTreeMap<String, Vec<String>>,
        seen: &mut HashSet<&'a str>,
        walked: &mut HashSet<&'a str>,
        result: &mut Vec<String>,
    ) {
        if !walked.insert(node) {
            return;
        }
        if let Some(declared) = deps.get(node) {
            for dep in declared {
                if seen.insert(dep) {
                    result.push(dep.clone());
                }
                walk(dep, deps, seen, walked, result);
            }
        }
    }

    let mut seen: HashSet<&str> = HashSet::from([self_name]);
    let mut result: Vec<String> = declared
        .iter()
        .filter(|dep| seen.insert(dep.as_str()))
        .cloned()
        .collect();
    let mut walked = HashSet::new();
    for dep in declared {
        walk(dep, deps, &mut seen, &mut walked, &mut result);
    }
    result
}

/// One declared CSS module collected during the style pass — everything
/// sorted_deps emission needs afterwards.
struct CssModule {
    name: String,
    output: String,
    version: u32,
}

/// One style file whose `@module`/`@requires` were registered before the
/// compile pass — the record each expanded-dependency update is built from.
struct StyleScan<'a> {
    source: &'a DiscoveredSource,
    /// The asset key the file compiles under (`css/{path}.css`)
    key: String,
    /// Its declared module name, when the file carries an @module comment
    module_name: Option<String>,
    /// Its declared @requires list
    declared: Vec<String>,
}

/// Warn once about the modules that sorted as cyclic for one asset type.
fn warn_cyclic(asset_type: &str, cyclic: &[String]) {
    if !cyclic.is_empty() {
        eprintln!(
            "Warning: circular dependency among {} modules (built, but excluded from sorted_deps): {}",
            asset_type,
            cyclic.join(", ")
        );
    }
}

/// The last-modified stamp for one source file: its mtime, clamped to
/// SOURCE_DATE_EPOCH when the build read one. A reproducible build sets the
/// variable so the same source checkout stamps identically on every host;
/// an unreadable mtime falls back to now rather than failing the build.
fn source_stamp(source_path: &Path, epoch_clamp: Option<i64>) -> DateTime<Utc> {
    // A checked conversion: chrono's `From<SystemTime>` panics on an mtime
    // outside its range (a file touched to the far future).
    let mtime: DateTime<Utc> = fs::metadata(source_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_secs())
                .ok()
                .and_then(|secs| DateTime::from_timestamp(secs, after.subsec_nanos())),
            Err(before) => i64::try_from(before.duration().as_secs())
                .ok()
                .and_then(|secs| DateTime::from_timestamp(-secs, 0)),
        })
        .unwrap_or_else(Utc::now);
    match epoch_clamp.and_then(|secs| DateTime::from_timestamp(secs, 0)) {
        Some(clamp) => mtime.min(clamp),
        None => mtime,
    }
}

/// SOURCE_DATE_EPOCH, when the build environment sets it — the
/// reproducible-build clamp read once per build rather than inside the
/// manifest's per-asset update path. A value that is set but is not an
/// epoch chrono can represent fails the build: silently building with raw
/// mtimes would defeat the reproducibility the variable asks for.
fn source_date_epoch() -> Result<Option<i64>> {
    let Ok(value) = std::env::var("SOURCE_DATE_EPOCH") else {
        return Ok(None);
    };
    let epoch = value
        .parse::<i64>()
        .ok()
        .filter(|secs| DateTime::from_timestamp(*secs, 0).is_some());
    match epoch {
        Some(secs) => Ok(Some(secs)),
        None => {
            anyhow::bail!("SOURCE_DATE_EPOCH must be an integer epoch in seconds, got {value:?}")
        }
    }
}

pub struct AssetGraph {
    config: Config,
    js_deps: BTreeMap<String, Vec<String>>,
    /// Module name → (source file, whether it was found under a source dir).
    module_map: HashMap<String, (PathBuf, bool)>,
    /// The configured backend, or the construction error verbatim — a
    /// config naming an unsupported backend must never hold a storage
    /// that silently behaves as local. `build` validates the config
    /// first, so the stored error surfaces through the backend accessor.
    storage: Result<StorageBackend>,
    /// The resolver every read and write of a compiled file goes through —
    /// flat layout or beside-source, per `config.output.layout`.
    output_dirs: OutputDirs,
    /// The build's stamp clamp — SOURCE_DATE_EPOCH, read once per build.
    stamp_clamp: Option<i64>,
    /// The exact source bytes the declaration scan read, per path — the
    /// compile pass compiles these bytes, never a re-read, so the emitted
    /// output and the recorded dependency metadata always describe one
    /// coherent input state.
    source_snapshots: HashMap<PathBuf, String>,
    /// Every source byte the build consumed, keyed by canonical path for
    /// files and by entry spelling for imported symlinks. Verified
    /// unchanged immediately before the manifest publishes, so an edit
    /// landing after its asset's own compile still aborts the build.
    captured_inputs: std::cell::RefCell<HashMap<PathBuf, Vec<u8>>>,
    /// Sass import symlink identities, separate from read bytes: changing
    /// the target to an equal-content file changes future watch ownership.
    captured_symlink_targets: std::cell::RefCell<HashMap<PathBuf, PathBuf>>,
    /// Files Sass attempted to consume during the current build, including
    /// successful reads that later failed coherence verification and
    /// failed candidate probes. Only watch uses these; none are published.
    attempted_sass_inputs: std::cell::RefCell<BTreeSet<PathBuf>>,
    /// Exact failed-compilation reads and negative filesystem probes.
    /// These are private watch observations, never part of the manifest.
    failed_sass_snapshot: std::cell::RefCell<FailedSassSnapshot>,
    /// Compiled files the current build created — names that did not exist
    /// before its write. Each is also recorded in the pending-output
    /// journal before the write, so a killed build's files are known too.
    created_outputs: std::cell::RefCell<Vec<PathBuf>>,
    /// Versions this build retired but could not unlink — carried in the
    /// pending-output journal so the next build retries them.
    unpruned_outputs: std::cell::RefCell<Vec<PathBuf>>,
    /// The files the manifest this build publishes names, as
    /// `forget_published` dropped them from the journal, and whether that
    /// manifest is now the one on disk. Once it is, the closing settle never
    /// takes them back — not through a transient read failure, not through
    /// a manifest deleted right after the publish.
    published_outputs: std::cell::RefCell<HashSet<PathBuf>>,
    publication_done: std::cell::Cell<bool>,
    /// Test hook run between a source's declaration-scan snapshot and its
    /// post-compile verification — simulates an edit landing mid-build so
    /// the coherence check can be exercised deterministically.
    #[cfg(test)]
    mid_build_edit: Option<MidBuildEdit>,
    /// Test hook run once after the missing-roots preflight and before
    /// discovery — the seam where a contributing root can vanish between
    /// the guard and the walk that must still refuse it.
    #[cfg(test)]
    pre_discovery_edit: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// When the first build in this process began — the line between staging
/// residue a killed earlier process left and files this process may still
/// be writing.
static BUILD_PROCESS_START: std::sync::OnceLock<std::time::SystemTime> = std::sync::OnceLock::new();

/// The pending-output journal for `config`'s manifest: one path per line
/// (project-relative inside the project) of versioned files no published
/// manifest named — names a build created, recorded before each write,
/// and versions a build retired but could not unlink. It lives at the
/// project root beside `manifest.lock`, outside every output tree, named
/// for the manifest so two configurations sharing a project root keep
/// separate journals, and exists only while something is unsettled.
pub(crate) fn pending_journal_path(config: &Config) -> PathBuf {
    // Named by the manifest's physical path relative to the physical
    // project root, so every spelling of one project — a symlinked `--cwd`,
    // an absolute or relative manifest — finds the same journal, and a
    // project moved or renamed as a whole keeps it. A manifest outside the
    // project is named by its physical path.
    let manifest = observed_input_path(&config.manifest_path);
    let base = observed_input_path(&config.base_dir);
    let manifest = manifest
        .strip_prefix(&base)
        .map(Path::to_path_buf)
        .unwrap_or(manifest);
    let digest = manifest
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    config
        .base_dir
        .join(format!(".paste-pending.{digest:016x}"))
}

/// How old a staging file must be before a build may treat it as residue.
const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);

/// Whether the process that named a staging file may still be writing it:
/// this process, a live process on this host, or — where liveness cannot
/// be asked — anyone.
fn staging_writer_may_be_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return true;
        };
        !matches!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        )
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The mid-build edit hook's signature — see `AssetGraph::mid_build_edit`.
#[cfg(test)]
type MidBuildEdit = std::sync::Arc<dyn Fn(&Path) + Send + Sync>;

impl AssetGraph {
    pub fn new(config: Config) -> Self {
        let mut type_dirs = std::collections::HashMap::new();
        type_dirs.insert("js".to_string(), config.output.js.clone());
        type_dirs.insert("css".to_string(), config.output.css.clone());
        type_dirs.insert("images".to_string(), config.output.images.clone());
        let storage = StorageBackend::from_config(
            &config.storage.backend,
            &config.output.base,
            config.storage.keep_versions,
            type_dirs,
        )
        .map_err(|e| anyhow::anyhow!("{e:#}"));
        let output_dirs = config.output_dirs();
        AssetGraph {
            config,
            js_deps: BTreeMap::new(),
            module_map: HashMap::new(),
            storage,
            output_dirs,
            stamp_clamp: None,
            source_snapshots: HashMap::new(),
            captured_inputs: std::cell::RefCell::new(HashMap::new()),
            captured_symlink_targets: std::cell::RefCell::new(HashMap::new()),
            attempted_sass_inputs: std::cell::RefCell::new(BTreeSet::new()),
            failed_sass_snapshot: std::cell::RefCell::new(FailedSassSnapshot::default()),
            created_outputs: std::cell::RefCell::new(Vec::new()),
            unpruned_outputs: std::cell::RefCell::new(Vec::new()),
            published_outputs: std::cell::RefCell::new(HashSet::new()),
            publication_done: std::cell::Cell::new(false),
            #[cfg(test)]
            mid_build_edit: None,
            #[cfg(test)]
            pre_discovery_edit: None,
        }
    }

    /// The configured storage backend — `Err` when `storage.backend`
    /// names something unsupported (the error `from_config` recorded at
    /// construction, never a fallback backend).
    fn backend(&self) -> Result<&StorageBackend> {
        self.storage.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))
    }

    /// How many history entries a rebuilt asset retains: the configured
    /// bound on dist — a retained version's pinned URL must still serve —
    /// and none on local, where only the current file is kept on disk and
    /// recorded history could never resolve to a file.
    fn history_bound(&self) -> Result<usize> {
        Ok(if self.backend()?.is_dist() {
            self.config.storage.keep_versions
        } else {
            0
        })
    }

    /// The write-lock contract: one build at a time per project. The
    /// whole build — discovery, compilation, cleanup and publication —
    /// runs under an exclusive advisory lock on `<base_dir>/manifest.lock`;
    /// a second writer fails fast instead of interleaving torn outputs
    /// with the first build's. The lock is advisory, so it only binds
    /// AssetGraph writers, and the OS releases it if a holder dies, so
    /// a crashed build never strands the project. It lives at the project
    /// root, never inside a served output tree.
    fn acquire_build_lock(&self) -> Result<std::fs::File> {
        use fs2::FileExt;
        let lock_path = self.config.base_dir.join("manifest.lock");
        // The lock is opened relative to the pinned project directory —
        // the earlier pathname-level symlink check had a check/open
        // window through which a swapped lock name could point two
        // "exclusive" holders at different inodes. `open_or_create` is
        // atomic no-follow at the syscall: a symlinked lock name refuses
        // instead of being checked then opened. A lock file is never
        // ours to truncate either — the call never truncates, so an
        // earlier lock's bytes are tooling state and a foreign file's
        // are data.
        let dir =
            crate::dirfd::OwnedDir::open_anchor(&self.config.base_dir).with_context(|| {
                format!(
                    "cannot open the project root for build lock {}",
                    lock_path.display()
                )
            })?;
        let file = match dir.open_or_create(std::ffi::OsStr::new("manifest.lock")) {
            Ok(file) => file,
            Err(e) if crate::dirfd::is_unsafe_component(&e) => {
                anyhow::bail!(
                    "build lock {} is a symlink — refusing to open through it",
                    lock_path.display()
                )
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("cannot create build lock {}", lock_path.display())))
            }
        };
        if !crate::dirfd::OwnedDir::opened_file_is_regular(&file)? {
            anyhow::bail!(
                "build lock {} is not a regular file — refusing to lock it",
                lock_path.display()
            );
        }
        // Only contention means another writer; any other errno (a
        // filesystem without flock support) is reported as itself.
        file.try_lock_exclusive().map_err(|e| {
            if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                anyhow::anyhow!(
                    "another build holds the write lock on {} — refusing to interleave",
                    lock_path.display()
                )
            } else {
                anyhow::Error::new(e).context(format!("cannot lock {}", lock_path.display()))
            }
        })?;
        Ok(file)
    }

    pub fn build(&mut self) -> Result<Manifest> {
        BUILD_PROCESS_START.get_or_init(std::time::SystemTime::now);
        self.attempted_sass_inputs.borrow_mut().clear();
        *self.failed_sass_snapshot.borrow_mut() = FailedSassSnapshot::default();
        self.created_outputs.borrow_mut().clear();
        self.unpruned_outputs.borrow_mut().clear();
        self.published_outputs.borrow_mut().clear();
        self.publication_done.set(false);
        let _write_lock = self.acquire_build_lock()?;
        // Journal upkeep runs under the same lock as the build: no other
        // build can adopt a file between one build writing it and the next
        // removing it. A killed or failed earlier build's files and an
        // earlier failed prune are settled first, then this build's own.
        self.settle_pending();
        let result = self.build_locked();
        self.settle_pending();
        result
    }

    /// The pending-output journal: see [`pending_journal_path`].
    fn pending_journal(&self) -> PathBuf {
        pending_journal_path(&self.config)
    }

    /// One journal line for `file`: project-relative inside the project,
    /// absolute elsewhere; None for a name a line cannot carry.
    fn journal_line(&self, file: &Path) -> Option<String> {
        file.strip_prefix(&self.config.base_dir)
            .unwrap_or(file)
            .to_str()
            .filter(|line| !line.is_empty() && !line.contains('\n'))
            .map(str::to_string)
    }

    /// Append `file` to the pending-output journal. Best effort: a journal
    /// that cannot be written only means a killed build's files stay.
    fn journal_pending(&self, file: &Path) {
        use std::io::Write;
        let Some(line) = self.journal_line(file) else {
            return;
        };
        let journal = self.pending_journal();
        let result = (|| -> Result<()> {
            let root = crate::dirfd::OwnedDir::open_anchor(&self.config.base_dir)?;
            let mut handle = root.open_or_create(journal.file_name().unwrap_or_default())?;
            anyhow::ensure!(
                crate::dirfd::OwnedDir::opened_file_is_regular(&handle)?,
                "it is not a regular file"
            );
            std::io::Seek::seek(&mut handle, std::io::SeekFrom::End(0))?;
            handle.write_all(format!("{line}\n").as_bytes())?;
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!(
                "Warning: could not record {} in {}: {e:#}",
                file.display(),
                journal.display()
            );
        }
    }

    /// Replace the journal with exactly `entries` — staged and renamed into
    /// place, never appended to — or remove it when there are none.
    fn write_pending(&self, entries: &[PathBuf]) {
        use std::io::Write;
        let journal = self.pending_journal();
        let name = journal.file_name().unwrap_or_default();
        let mut text = String::new();
        for line in entries.iter().filter_map(|file| self.journal_line(file)) {
            text.push_str(&line);
            text.push('\n');
        }
        let result = (|| -> Result<()> {
            let root = crate::dirfd::OwnedDir::open_anchor(&self.config.base_dir)?;
            if text.is_empty() {
                return match root.remove_file(name) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                    _ => Ok(()),
                };
            }
            let tmp = crate::storage::staged_tmp_component(
                &journal,
                "pending",
                &format!("{}.0", std::process::id()),
            );
            let _ = root.remove_file(&tmp);
            let mut handle = root.create_new(&tmp)?;
            handle.write_all(text.as_bytes())?;
            handle.sync_all()?;
            drop(handle);
            if let Err(e) = root.rename(&tmp, name) {
                let _ = root.remove_file(&tmp);
                return Err(e.into());
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("Warning: could not rewrite {}: {e:#}", journal.display());
        }
    }

    /// The entries the journal holds, resolved against base_dir and
    /// deduplicated. The journal is read through the project root without
    /// following a link in its place.
    fn read_pending(&self) -> Vec<PathBuf> {
        let journal = self.pending_journal();
        let text = (|| -> std::io::Result<String> {
            let root = crate::dirfd::OwnedDir::open_anchor(&self.config.base_dir)?;
            let file = root.open_file(journal.file_name().unwrap_or_default())?;
            if !crate::dirfd::OwnedDir::opened_file_is_regular(&file)? {
                return Err(std::io::Error::other("not a regular file"));
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(file, 16 << 20), &mut text)?;
            Ok(text)
        })();
        let text = match text {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                eprintln!("Warning: ignoring {}: {e}", journal.display());
                return Vec::new();
            }
        };
        let mut entries: Vec<PathBuf> = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| self.config.base_dir.join(line))
            .collect();
        entries.sort();
        entries.dedup();
        entries
    }

    /// The resolved file of every output `manifest` records.
    fn named_outputs(&self, manifest: &Manifest) -> HashSet<PathBuf> {
        let mut named = HashSet::new();
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                named.extend(self.output_dirs.compiled_file(output, kind));
            }
        }
        named
    }

    /// Just before publication: every journal entry and created file the
    /// manifest about to be published names leaves the journal. A build
    /// killed after the publish therefore never leaves a published version
    /// listed — the journal only ever holds files no published manifest
    /// named, which is what makes settling it safe against a manifest later
    /// replaced by an older copy.
    fn forget_published(&self, manifest: &Manifest) {
        let named = self.named_outputs(manifest);
        let mut entries = self.read_pending();
        {
            let mut published = self.published_outputs.borrow_mut();
            published.extend(entries.iter().filter(|file| named.contains(*file)).cloned());
            published.extend(
                self.created_outputs
                    .borrow()
                    .iter()
                    .filter(|file| named.contains(*file))
                    .cloned(),
            );
        }
        entries.retain(|file| !named.contains(file));
        // The in-memory list keeps every created file: if publication then
        // fails, the closing settle judges them against the manifest still
        // on disk and removes what it does not name.
        entries.extend(
            self.created_outputs
                .borrow()
                .iter()
                .filter(|file| !named.contains(*file))
                .cloned(),
        );
        entries.sort();
        entries.dedup();
        self.write_pending(&entries);
    }

    /// Settle the pending-output journal: remove each listed versioned
    /// file — plus this build's created and unpruned files — that the
    /// manifest on disk does not name, and keep only the ones a later build
    /// can still settle. The authority is the manifest on disk, re-read
    /// rather than assumed. Without one, nothing is removed and the entries
    /// are forgotten — a lost manifest proves nothing about what saved
    /// pages hold, and the manifest the build then publishes starts a new
    /// lineage that never judged them. An unreadable manifest settles
    /// nothing and keeps the entries; the journal is rewritten, never
    /// appended to, so it cannot grow. An
    /// entry no build may ever remove — outside the project and every
    /// output root, not a versioned name, or behind a symlinked
    /// component — is dropped with the reason, its file left in place.
    fn settle_pending(&self) {
        let created = std::mem::take(&mut *self.created_outputs.borrow_mut());
        let mut candidates = self.read_pending();
        candidates.extend(created.iter().cloned());
        candidates.append(&mut self.unpruned_outputs.borrow_mut());
        candidates.sort();
        candidates.dedup();
        if self.publication_done.get() {
            let published = self.published_outputs.borrow();
            candidates.retain(|file| !published.contains(file));
        }
        if candidates.is_empty() {
            return;
        }
        candidates.retain(|file| {
            let reason = if !crate::storage::is_versioned_file(file) {
                Some("it is not a versioned output name")
            } else if !file.components().all(|c| {
                !matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            }) || (!file.starts_with(&self.config.base_dir)
                && self.established_root_for(file, &[]).is_none())
            {
                Some("it is outside the project and every output root")
            } else {
                None
            };
            if let Some(reason) = reason {
                eprintln!(
                    "Warning: dropping pending entry {} — {reason}; a build never removes it",
                    file.display()
                );
            }
            reason.is_none()
        });
        let manifest_present = !matches!(
            fs::symlink_metadata(&self.config.manifest_path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        );
        if !manifest_present {
            // The manifest that judged the journal's entries is gone, and
            // the next one starts a new lineage: none of them may be removed
            // by it. This build's own new files are different — no manifest
            // ever named them, and the lock kept anyone else from adopting
            // them — so a failed build still takes them with it.
            let (own, earlier): (Vec<PathBuf>, Vec<PathBuf>) = candidates
                .into_iter()
                .partition(|file| created.contains(file));
            if !earlier.is_empty() {
                eprintln!(
                    "Warning: the manifest is missing — leaving {} pending output(s) in place and forgetting them",
                    earlier.len()
                );
            }
            let kept = self.remove_unnamed(own, &HashSet::new());
            self.write_pending(&kept);
            return;
        }
        let published = match Manifest::load(&self.config.manifest_path) {
            Ok(manifest) => manifest,
            Err(e) => {
                eprintln!(
                    "Warning: keeping {} pending output(s) — the manifest could not be read: {e:#}",
                    candidates.len()
                );
                self.write_pending(&candidates);
                return;
            }
        };
        let named = self.named_outputs(&published);
        let kept = self.remove_unnamed(candidates, &named);
        self.write_pending(&kept);
    }

    /// Remove each of `files` that `named` does not list; return the ones a
    /// later build should retry.
    fn remove_unnamed(&self, files: Vec<PathBuf>, named: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let mut kept = Vec::new();
        for file in files {
            if named.contains(&file) {
                continue;
            }
            let root = self
                .established_root_for(&file, &[])
                .unwrap_or_else(|| self.config.base_dir.clone());
            match self.deletion_dir(&file, &root) {
                Ok(crate::dirfd::Descend::Anchored(dir)) => {
                    match dir.remove_file(file.file_name().unwrap_or_default()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            eprintln!(
                                "Warning: could not remove unpublished output {}: {e} — the next build retries",
                                file.display()
                            );
                            kept.push(file);
                        }
                    }
                }
                Ok(crate::dirfd::Descend::Missing) => {}
                Ok(crate::dirfd::Descend::Unsafe) => eprintln!(
                    "Warning: leaving {} in place — it resolves through a symlinked component",
                    file.display()
                ),
                Err(e) => {
                    eprintln!(
                        "Warning: could not reach {}: {e:#} — the next build retries",
                        file.display()
                    );
                    kept.push(file);
                }
            }
        }
        kept
    }

    /// Write one compiled file through the backend, recording it as
    /// created by this build when its name did not exist beforehand.
    fn write_output(&self, target: &Path, content: &[u8]) -> Result<()> {
        let boundary = self.write_boundary(target)?;
        let existed = !matches!(
            fs::symlink_metadata(target),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        );
        if !existed {
            // Recorded before the write: a build killed between the two
            // leaves an entry for a file that never appeared — harmless —
            // never a file with no entry.
            self.journal_pending(target);
        }
        self.backend()?.write_file_in(
            target,
            content,
            &boundary,
            boundary == self.config.base_dir,
        )?;
        if !existed {
            self.created_outputs.borrow_mut().push(target.to_path_buf());
        }
        Ok(())
    }

    /// Remove staging files a killed build left behind. The build lock only
    /// excludes builds of this project, while an output directory can be
    /// shared — another project, a container on a bind mount — so a file
    /// is removed only when all of these hold:
    /// - its name has the exact `.paste-staged.{kind}.{pid}.{n}.{digest}`
    ///   shape and it is a regular file;
    /// - it sits in a directory builds of this project write into — the
    ///   manifest's directory, the configured output roots, a recorded
    ///   generated directory, the project root (the journal's staging) —
    ///   that resolves beneath the project root;
    /// - it was last modified before this process started and at least
    ///   [`STALE_STAGING_AGE`] ago, far longer than any write takes;
    /// - the pid its name records is not a live process on this host
    ///   (Unix only — elsewhere nothing is removed).
    ///
    /// A writer in another pid namespace whose clock runs more than the
    /// age floor behind this one can still lose an in-flight file; see
    /// ARCHITECTURE.md. Failures warn; residue is inert either way.
    fn remove_stale_staging(&self, known_dirs: &[PathBuf]) {
        let Some(started) = BUILD_PROCESS_START.get().copied() else {
            return;
        };
        let Some(floor) = std::time::SystemTime::now().checked_sub(STALE_STAGING_AGE) else {
            return;
        };
        let cutoff = started.min(floor);
        thread_local! {
            static STAGED_RE: regex::Regex = regex::Regex::new(
                r"^\.paste-staged\.(file|json|bin|pending)\.([0-9]+)\.[0-9]+\.[0-9a-f]{16}$"
            )
            .unwrap();
        }
        let mut dirs: BTreeSet<PathBuf> = known_dirs.iter().cloned().collect();
        dirs.extend([
            self.config.output.base.clone(),
            self.config.output.js.clone(),
            self.config.output.css.clone(),
            self.config.output.images.clone(),
            crate::dirfd::parent_dir(&self.config.manifest_path).to_path_buf(),
            // The journal's own staging sits at the project root.
            self.config.base_dir.clone(),
        ]);
        // Containment is physical: a directory spelled beneath the project
        // whose path resolves elsewhere (a symlinked output root) is not
        // the project's.
        let Ok(project) = fs::canonicalize(&self.config.base_dir) else {
            return;
        };
        for dir in dirs {
            if !fs::canonicalize(&dir).is_ok_and(|real| real.starts_with(&project)) {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|name| {
                    STAGED_RE.with(|re| {
                        re.captures(name)
                            .and_then(|caps| caps.get(2)?.as_str().parse::<u32>().ok())
                    })
                }) else {
                    continue;
                };
                // `DirEntry::metadata` does not follow a link.
                let stale = entry.metadata().is_ok_and(|meta| {
                    meta.is_file() && meta.modified().is_ok_and(|modified| modified < cutoff)
                });
                if !stale || staging_writer_may_be_alive(pid) {
                    continue;
                }
                let path = entry.path();
                let root = self
                    .established_root_for(&path, known_dirs)
                    .unwrap_or_else(|| self.config.base_dir.clone());
                if let Ok(crate::dirfd::Descend::Anchored(owned)) = self.deletion_dir(&path, &root)
                {
                    match owned.remove_file(&name) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => eprintln!(
                            "Warning: could not remove stale staging file {}: {e}",
                            path.display()
                        ),
                    }
                }
            }
        }
    }

    fn build_locked(&mut self) -> Result<Manifest> {
        self.config.validate()?;
        // Inert-key notices live here, not in validate(): config load
        // validates too, and one build must not print them twice.
        self.config.emit_compat_notes();
        // Every version stamp this build writes shares the one clamp.
        self.stamp_clamp = source_date_epoch()?;

        // A symlinked manifest path reads through to its target while the
        // publication rename would replace the link itself — and a
        // dangling link reads as absent, silently resetting history.
        // Refuse before the load can misinterpret either.
        crate::manifest::validate_writable_manifest_path(&self.config.manifest_path)?;

        // Load existing manifest for version history. A load failure —
        // corrupt JSON, an unreadable file — fails the build rather than
        // silently resetting the history the deployable URLs depend on.
        let previous_manifest_exists = self.config.manifest_path.is_file();
        let mut manifest = Manifest::load(&self.config.manifest_path)?;
        // The authoritative state this build may replace — an unchanged
        // effective manifest keeps the old generation instead of
        // republishing identical bytes under a new timestamp.
        let previous_manifest = previous_manifest_exists.then(|| manifest.clone());

        // A configured root that disappeared is not a deletion event:
        // when the previous manifest still serves assets sourced under
        // it, rebuilding would retire every one of them and publish a
        // hollow manifest. Fail before any write — the explicit way to
        // retire a root is removing it from the configuration.
        self.guard_missing_roots(&manifest.assets)?;

        // Loader pins that no longer verify against the loaded state are
        // dead weight — drop them before anything mutates. Then re-record
        // the previous build's own fingerprint from the state just loaded:
        // a page that saved that URL still names it, and the record is
        // immutable so an unchanged state is a no-op. On the first upgrade
        // from a manifest with no records at all, the legacy fingerprint
        // joins it — the segment builds emitted before pins existed — so
        // pages rendered against one keep resolving. Nothing older is
        // synthesized: a fingerprint no build emitted is not derivable.
        manifest.prune_packages_segments();
        let needs_legacy = !manifest.assets.is_empty() && manifest.packages_segments.is_empty();
        if !manifest.assets.is_empty() {
            manifest.record_packages_segment(&crate::jam::packages_segment(&manifest))?;
            if needs_legacy {
                manifest
                    .record_packages_segment(&crate::jam::legacy_packages_segment(&manifest))?;
            }
        }

        manifest.generated = Utc::now().to_rfc3339();

        // Snapshot the previous manifest's world before rebuilding the
        // active map: a deleted source's retired entry may still need its
        // module alias, recorded generated directories keep their discovery
        // exclusion across config changes, and the old outputs anchor the
        // history migration below.
        let previous_modules = manifest.modules.clone();
        let previous_generated_dirs = manifest.generated_dirs.clone();
        let previous_output_roots = manifest.output_roots.clone();
        let old_assets = manifest.assets.clone();

        // A manifest written before logical-name pins existed loads the map
        // empty: seed it from the state the old name→key→version resolution
        // could serve — every current or aliased binding, at every retained
        // version — so the pins pages already hold keep resolving once the
        // records exist. Must run before modules/sorted_deps are rebuilt.
        if manifest.historical_names.is_empty() && !manifest.assets.is_empty() {
            manifest.seed_historical_names();
        }

        // Modules and sorted_deps are rebuilt fresh each build: `modules`
        // holds only the names current now — a renamed module's old
        // spelling lives in `historical_names` for its recorded pins, not
        // here as a live alias.
        manifest.modules.clear();
        manifest.sorted_deps.clear();

        // Directories this build must never treat as sources: the
        // configured output roots, every recorded generated directory, and
        // the parent of every existing file the old manifest's versioned
        // outputs resolve to — an old flat or beside-source location
        // otherwise re-enters discovery the moment the layout or
        // build-dir name changes.
        let known_dirs: Vec<PathBuf> = previous_generated_dirs
            .iter()
            .map(|label| self.generated_dir_path(label))
            .collect::<Result<_>>()?;
        // Index each recorded generated directory by file basename once.
        // Every recorded output is searched for through these directories
        // — discovery exclusion, history materialization, stale-location
        // pruning — and joining every recorded basename against every
        // recorded directory turns one directory scan per output into
        // directories × outputs stat calls. One read_dir per directory
        // answers every lookup this build makes.
        let mut known_index: HashMap<std::ffi::OsString, Vec<PathBuf>> = HashMap::new();
        for dir in &known_dirs {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    known_index
                        .entry(entry.file_name())
                        .or_default()
                        .push(entry.path());
                }
            }
        }
        self.remove_stale_staging(&known_dirs);
        let mut excluded_dirs: Vec<PathBuf> = vec![
            self.config.output.base.clone(),
            self.config.output.js.clone(),
            self.config.output.css.clone(),
            self.config.output.images.clone(),
        ];
        // A recorded directory that is (or contains) a configured input
        // root is mixed territory, not generated output — excluding it
        // would block that root forever. Discovery's versioned-filename
        // check keeps retained files there from re-entering as sources.
        for dir in &known_dirs {
            if self.holds_input_root(dir) {
                eprintln!(
                    "Warning: recorded generated directory {} holds a configured input root; it is no longer treated as generated output",
                    dir.display()
                );
            } else {
                excluded_dirs.push(dir.clone());
            }
        }
        for (key, entry) in &old_assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                for candidate in self.existing_output_candidates(output, kind, &known_index) {
                    if candidate.is_file() {
                        if let Some(parent) = candidate.parent() {
                            if !self.holds_input_root(parent) {
                                excluded_dirs.push(parent.to_path_buf());
                            }
                        }
                    }
                }
            }
        }
        excluded_dirs.sort();
        excluded_dirs.dedup();

        // Track seen asset keys to prune or retire stale assets after build
        let mut seen_asset_keys: HashSet<String> = HashSet::new();

        // Discover every asset file: each source dir, then each include
        // path, walked recursively and classified by extension. One dir may
        // hold JS and styles together — the folder layout means nothing.
        #[cfg(test)]
        if let Some(edit) = &self.pre_discovery_edit {
            // The post-guard/pre-discovery seam — a test removes a
            // contributing root here to prove the walk's own
            // classification still refuses the outage.
            edit();
        }
        let discovered = self.discover_sources(&excluded_dirs, &old_assets)?;

        // Register JS modules
        self.js_deps.clear();
        self.module_map.clear();
        self.source_snapshots.clear();
        self.captured_inputs.borrow_mut().clear();
        self.captured_symlink_targets.borrow_mut().clear();
        for source in discovered.iter().filter(|s| s.kind == SourceKind::Js) {
            self.register_js_module(&source.path, &source.root, source.from_source_dir)?;
        }
        let js_order = layered_order(&self.js_deps);
        warn_cyclic("js", &js_order.cyclic);

        // Backfill each module's transitive dependencies
        let expanded_deps: HashMap<String, Vec<String>> = self
            .js_deps
            .keys()
            .map(|module| (module.clone(), backfilled_deps(&self.js_deps, module)))
            .collect();

        // Build sorted_deps for JS
        let mut js_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        let js_sorted: HashSet<&String> = js_order.sorted.iter().collect();

        // Cyclic modules build like the rest, after the layered ones; only
        // sorted names are written to sorted_deps.
        for module in js_order.sorted.iter().chain(&js_order.cyclic) {
            if let Some((source_path, from_source_dir)) = self.module_map.get(module) {
                // Add to modules map: declared module name -> manifest asset key.
                // Consumers (the combo resolver, the language runtimes)
                // look the value up in `assets`, so it must be a key of
                // that map and never a source path.
                let key = format!("js/{}.js", module.replace('.', "/"));
                record_module(&mut manifest, module, &key, "js");

                // Use expanded dependencies
                let deps = expanded_deps.get(module).cloned().unwrap_or_default();
                self.process_js_with_deps(
                    source_path.clone(),
                    *from_source_dir,
                    module,
                    deps,
                    &mut manifest,
                )?;

                // Add to sorted_deps and track seen keys
                seen_asset_keys.insert(key.clone());
                if js_sorted.contains(module) {
                    if let Some(entry) = manifest.assets.get(&key) {
                        js_sorted_deps.push((module.clone(), entry.output.clone(), entry.version));
                    }
                }
            }
        }

        // Store JS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("js".to_string(), js_sorted_deps);

        // Register every style file's @module/@requires before compiling
        // anything: the expanded dependency vector has to exist before a
        // version decision, exactly like the JS registration above. The
        // first file to declare a module name in scan order keeps it; a
        // later duplicate warns and is skipped entirely — compiled under its
        // own key it would still serve as a path-keyed asset, so the first
        // declaration would not hold.
        let mut css_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen_style_keys: HashMap<String, PathBuf> = HashMap::new();
        let mut styles: Vec<StyleScan<'_>> = Vec::new();

        for source in discovered
            .iter()
            .filter(|s| matches!(s.kind, SourceKind::Scss | SourceKind::Css))
        {
            let path = &source.path;
            // Skip partials — only `_`-prefixed SCSS is a partial; a plain
            // `.css` file is always an asset in its own right.
            if source.kind == SourceKind::Scss
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('_'))
            {
                continue;
            }
            // Track every style asset key to prevent pruning, even without @module
            let rel_path = path.strip_prefix(&source.root)?;
            let base_name = source_stem(rel_path).to_string_lossy().replace('\\', "/");
            let css_key = format!("css/{}.css", base_name);
            // `x.min.min.scss` would keep a `.min` in its key after the one
            // the stem drops — never part of a key or module name. A style's
            // key comes from its path, so only a rename fixes it.
            if base_name.ends_with(".min") {
                return Err(min_style_key_error(&css_key, path)
                    .context(RecoverableWatchInput(path.to_path_buf())));
            }
            if let Some(first) = seen_style_keys.get(&css_key) {
                if is_min_source(first) || is_min_source(path) {
                    let module = base_name.replace('/', ".");
                    return Err(min_twin_error(&module, first, path));
                }
                eprintln!(
                    "Warning: {} duplicates style asset {}; keeping the first",
                    path.display(),
                    css_key
                );
                continue;
            }
            seen_style_keys.insert(css_key.clone(), path.clone());
            let content = fs::read_to_string(path)
                .with_context(|| RecoverableWatchInput(path.to_path_buf()))?;
            // The compile pass consumes these exact bytes for the entry —
            // the manifest's declarations and the emitted CSS describe
            // the same read.
            self.source_snapshots.insert(path.clone(), content.clone());
            self.capture_input(
                fs::canonicalize(path).unwrap_or_else(|_| path.clone()),
                content.clone().into_bytes(),
            )?;
            let (module_name, declared) = self
                .style_declarations(&content, path)
                .with_context(|| RecoverableWatchInput(path.to_path_buf()))?;
            if let Some(name) = module_name.as_deref() {
                if name.ends_with(".min") {
                    return Err(min_module_name_error(name, path, true)
                        .context(RecoverableWatchInput(path.to_path_buf())));
                }
            }
            if let Some(dep) = declared.iter().find(|dep| dep.ends_with(".min")) {
                return Err(min_requirement_error(dep, path)
                    .context(RecoverableWatchInput(path.to_path_buf())));
            }
            match &module_name {
                // A duplicate declaration keeps the first file only: the
                // later one is not compiled, recorded, or counted as seen —
                // a dist build retires a previously recorded entry at its key.
                Some(name) if css_deps.contains_key(name) => {
                    eprintln!(
                        "Warning: {} redeclares css module {}; keeping the first",
                        path.display(),
                        name
                    );
                    continue;
                }
                Some(name) => {
                    css_deps.insert(name.clone(), declared.clone());
                }
                None => {}
            }
            seen_asset_keys.insert(css_key.clone());
            styles.push(StyleScan {
                source,
                key: css_key,
                module_name,
                declared,
            });
        }

        // A style without @module still joins the module world: it takes a
        // path-derived name — the relative path under its root, extension
        // stripped, separators as dots, exactly like JS — as long as the
        // name is valid and unclaimed. A name that collides with a declared
        // module or an earlier derived name, or that is not a module name
        // at all, warns and leaves the file unnamed: it still compiles
        // under its path key, just outside the named-module index.
        for style in &mut styles {
            if style.module_name.is_some() {
                continue;
            }
            let derived = source_stem(style.source.path.strip_prefix(&style.source.root)?)
                .to_string_lossy()
                .replace(['/', '\\'], ".");
            if derived.ends_with(".min") {
                return Err(min_module_name_error(&derived, &style.source.path, false)
                    .context(RecoverableWatchInput(style.source.path.clone())));
            }
            if !is_module_name(&derived) {
                eprintln!(
                    "Warning: {} has no @module and derives invalid module name {}; leaving it unnamed",
                    style.source.path.display(),
                    derived
                );
                continue;
            }
            if css_deps.contains_key(&derived) {
                eprintln!(
                    "Warning: {} derives css module name {} that is already claimed; leaving it unnamed",
                    style.source.path.display(),
                    derived
                );
                continue;
            }
            if !style.source.from_source_dir {
                eprintln!(
                    "Warning: {} in an include path has no @module; using path-derived name {}",
                    style.source.path.display(),
                    derived
                );
            }
            css_deps.insert(derived.clone(), style.declared.clone());
            style.module_name = Some(derived);
        }

        // Layer the declared CSS dependencies — the layering consumes the
        // declarations; the stored dependencies are the backfilled list.
        let css_order = layered_order(&css_deps);
        warn_cyclic("css", &css_order.cyclic);

        let mut css_modules: Vec<CssModule> = Vec::new();
        for style in &styles {
            let expanded = expand_declared_deps(
                &css_deps,
                style.module_name.as_deref().unwrap_or(&style.key),
                &style.declared,
            );

            if let Some(name) = &style.module_name {
                // The surviving file owns its module name — a later duplicate
                // declaration never reaches this pass.
                // A declared module whose source moved to another
                // source-relative key carries its entry — version sequence
                // and history included — into the new key, as long as the
                // destination is not already occupied. A plain style keeps
                // its path-based identity. The carry also leaves a retired
                // clone at the old key: a source rename must not delete the
                // recorded outputs a page may hold — the old current
                // version's own URL keeps resolving to its file, and the
                // old key's history stays put under its recorded stems
                // rather than being renamed after the new source.
                if let Some(old_key) = previous_modules.get(name) {
                    // A live source owning the old key in this same build
                    // keeps it — the carry must never steal a key a current
                    // stylesheet compiles into.
                    if old_key != &style.key
                        && old_key.starts_with("css/")
                        && !manifest.assets.contains_key(&style.key)
                        && !seen_asset_keys.contains(old_key)
                    {
                        if let Some(entry) = manifest.assets.remove(old_key) {
                            let mut shell = entry.clone();
                            shell.retired = true;
                            manifest.assets.insert(style.key.clone(), entry);
                            manifest.assets.insert(old_key.clone(), shell);
                        }
                    }
                }
                record_module(&mut manifest, name, &style.key, "css");
            }

            if let Some(record) = self.process_scss(
                &style.source.path,
                &style.source.root,
                style.source.from_source_dir,
                style.module_name.as_deref(),
                expanded,
                &mut manifest,
            )? {
                css_modules.push(record);
            }
            #[cfg(test)]
            if let Some(edit) = &self.mid_build_edit {
                // Fires a second time for each stylesheet, after its
                // captured inputs merged — the seam where an edit to a
                // shared import would split one build into two source
                // generations.
                edit(&style.source.path);
            }
        }

        let mut css_sorted_deps: Vec<(String, String, u32)> = Vec::new();
        for name in &css_order.sorted {
            if let Some(m) = css_modules.iter().find(|m| &m.name == name) {
                css_sorted_deps.push((name.clone(), m.output.clone(), m.version));
            }
        }

        // Store CSS sorted_deps in manifest
        manifest
            .sorted_deps
            .insert("css".to_string(), css_sorted_deps);

        // Unseen assets leave the active set: a local build drops them —
        // nothing local serves an old pin — while a dist build retires
        // them: their output and history stay recorded and materialized so
        // exact pinned URLs keep resolving to the original bytes.
        if self.backend()?.is_dist() {
            for (key, entry) in manifest.assets.iter_mut() {
                if !seen_asset_keys.contains(key) {
                    entry.retired = true;
                }
            }
            // update_asset_with_history enforces the retention bound only
            // for entries a build still sees; a source that stays removed
            // never reaches it, so a bound lowered after retirement would
            // leave the whole history behind. Apply it here too — an
            // entry's current version is not history and always survives.
            let keep = self.config.storage.keep_versions;
            for entry in manifest.assets.values_mut() {
                if entry.retired && entry.history.len() > keep {
                    entry.history.drain(..entry.history.len() - keep);
                }
            }
        } else {
            manifest
                .assets
                .retain(|key, _| seen_asset_keys.contains(key));
        }

        // Record the pin every resolvable name carries this build — the
        // identity a fresh `name+vN` URL is issued under — then drop the
        // records whose versions retention just evicted. The modules map
        // above holds only current names; this is the historical record a
        // renamed or retired spelling's saved pins resolve through.
        manifest.record_name_pins()?;
        manifest.prune_name_pins();

        // Materialize every recorded version at the file this build's
        // resolver maps its recorded output to: the migration is a copy
        // verified against the recorded hash, never a rebuild of old
        // bytes, and a retired entry's current version must exist with
        // its source gone. A recorded output is the URL a page may hold —
        // it is never rewritten: when a layout or output-root move would
        // change the path a version serves at, the version itself rolled
        // forward in update_asset_with_history and the old one kept its
        // recorded output in history; a move that leaves the recorded path
        // unchanged only changes where the resolver places the file. An
        // active entry's current version was freshly written by the
        // compile pass above; a retired entry's current version and every
        // history version are what this pass supplies. Besides the recorded
        // generated directories, the directories this build just wrote
        // into are searched by basename too: a beside-source folder renamed
        // since the last build carries its retained files with it, and the
        // recorded hash still decides whether a copy found there is real.
        let mut materialize_index = known_index.clone();
        let mut current_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for (key, entry) in manifest.assets.iter().filter(|(_, e)| !e.retired) {
            if let Some(dir) = Self::key_asset_type(key)
                .and_then(|kind| self.output_dirs.compiled_file(&entry.output, kind))
                .and_then(|path| path.parent().map(Path::to_path_buf))
            {
                if !known_dirs.contains(&dir) {
                    current_dirs.insert(dir);
                }
            }
        }
        for dir in &current_dirs {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    materialize_index
                        .entry(entry.file_name())
                        .or_default()
                        .push(entry.path());
                }
            }
        }
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            if entry.retired {
                self.materialize_version(
                    key,
                    entry.version,
                    &entry.hash,
                    &entry.output,
                    kind,
                    &materialize_index,
                )?;
            }
            for history in &entry.history {
                self.materialize_version(
                    key,
                    history.version,
                    &history.hash,
                    &history.output,
                    kind,
                    &materialize_index,
                )?;
            }
        }

        // Record where generated files live for the next build's scanner
        // and for future output-root moves: the configured output roots and
        // the parent of every recorded file, project-relative labels only,
        // carried forward so a label outlives the config that created it.
        let mut generated: BTreeSet<String> = previous_generated_dirs
            .iter()
            .filter(|label| {
                self.generated_dir_path(label)
                    .map(|dir| !self.holds_input_root(&dir))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        for dir in [
            &self.config.output.base,
            &self.config.output.js,
            &self.config.output.css,
            &self.config.output.images,
        ] {
            if let Some(label) = generated_label(&self.config.base_dir, dir) {
                generated.insert(label);
            }
        }
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                if let Some(parent) = self
                    .output_dirs
                    .compiled_file(output, kind)
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                {
                    // A retained version landing in a source root (a flat →
                    // beside-source move with `source.dirs = ["js"]`) must
                    // not mark that root as generated output.
                    if self.holds_input_root(&parent) {
                        continue;
                    }
                    if let Some(label) = generated_label(&self.config.base_dir, &parent) {
                        generated.insert(label);
                    }
                }
            }
        }
        manifest.generated_dirs = generated.into_iter().collect();
        // The type roots this build wrote to are the only provenance a
        // later build accepts for removing a legacy assetgraph.{type} at
        // a historical location — recorded as labels and carried forward
        // like generated_dirs so a root survives the config move that
        // replaced it.
        let mut output_roots = previous_output_roots.clone();
        for (asset_type, dir) in [
            ("js", self.backend()?.resolve_dir("js")),
            ("css", self.backend()?.resolve_dir("css")),
        ] {
            if let Some(label) = generated_label(&self.config.base_dir, &dir) {
                let roots = output_roots.entry(asset_type.to_string()).or_default();
                if !roots.contains(&label) {
                    roots.push(label);
                }
            }
        }
        for roots in output_roots.values_mut() {
            roots.sort();
        }
        manifest.output_roots = output_roots;
        // The manifest carries the URL policy so adapters need no setting of
        // their own; a toggle changes rendered tokens only — never hashes,
        // versions, timestamps, or history.
        manifest.hash_urls = self.config.jam.hash_urls;
        // The build's default URL prefix rides the manifest: a runtime
        // with no cdnBase of its own emits it, one that sets it overrides.
        manifest.cdn_base = if self.config.cdn.enabled {
            Some(self.config.cdn.url.clone())
        } else {
            None
        };

        if self.backend()?.is_dist() {
            manifest.make_deterministic();
            // Builds once wrote an unversioned all-modules file per type —
            // js/assetgraph.js and css/assetgraph.css — under the type
            // output roots. Nothing served them (combos read per-module
            // files) and they were never manifest entries, so they are
            // removed by their exact names here — but only at established
            // type roots. A root is established two ways, both provenance
            // rather than inference: the currently configured root, and a
            // root the previous manifest recorded as that type's root.
            // Anything else is ownership the build cannot prove — a copy
            // of a compiled file sitting in another directory is not
            // evidence, and generated_dirs also records output.base, the
            // images root, and per-module subdirectories — so a stale name
            // found in unestablished recorded territory is reported for
            // explicit migration, never deleted.
            let recorded_dirs: HashSet<PathBuf> = manifest
                .generated_dirs
                .iter()
                .map(|label| self.generated_dir_path(label))
                .collect::<Result<_>>()?;
            // Ownership protection consults the same roots discovery
            // walks — the effective source dirs (configured, legacy, or
            // the src/main/assets defaults) plus every include path — so
            // an overlapping recorded root without a generated-directory
            // record is untrusted, never cleaned.
            let source_roots: Vec<PathBuf> = self
                .config
                .source_dirs()
                .into_iter()
                .chain(self.config.include_paths.iter().cloned())
                .collect();
            for (asset_type, name) in [("js", "assetgraph.js"), ("css", "assetgraph.css")] {
                let mut roots = BTreeSet::from([self.backend()?.resolve_dir(asset_type)]);
                let mut unestablished: BTreeSet<PathBuf> = BTreeSet::new();
                if let Some(labels) = previous_output_roots.get(asset_type) {
                    for label in labels {
                        let root = self.generated_dir_path(label)?;
                        // Output nested beneath a source root is supported —
                        // discovery excludes the generated directories it
                        // writes. A recorded root under sources is only
                        // trusted when the manifest also records it as
                        // generated territory; a label under sources
                        // without that record names real source directories
                        // and is reported, never cleaned.
                        if source_roots.iter().any(|source| root.starts_with(source))
                            && !recorded_dirs.contains(&root)
                        {
                            unestablished.insert(root);
                            continue;
                        }
                        roots.insert(root);
                    }
                }
                for root in &roots {
                    let target = root.join(name);
                    // The unlink must act inside physically owned output:
                    // a symlinked component between the trusted boundary
                    // and the file means the root now resolves into a
                    // tree this build never owned.
                    match self.deletion_dir(&target, root)? {
                        crate::dirfd::Descend::Anchored(dir) => {
                            match dir.remove_file(target.file_name().unwrap_or_default()) {
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                result => result?,
                            }
                        }
                        crate::dirfd::Descend::Missing => {}
                        crate::dirfd::Descend::Unsafe => {
                            let configured = self.backend()?.resolve_dir(asset_type);
                            anyhow::ensure!(
                                root != &configured,
                                "unsafe cleanup target: {} resolves through a symlinked component — refusing to delete {} or publish a new manifest",
                                root.display(),
                                target.display()
                            );
                            eprintln!(
                                "paste-assetgraph: keeping {} — {} is not physically owned output; resolve the symlinked component before cleaning",
                                target.display(),
                                root.display()
                            );
                            continue;
                        }
                    }
                }
                unestablished.extend(
                    recorded_dirs
                        .iter()
                        .filter(|dir| !roots.contains(*dir))
                        .cloned(),
                );
                for dir in unestablished {
                    let stale = dir.join(name);
                    if stale.is_file() {
                        eprintln!(
                            "paste-assetgraph: keeping {} — {} is not an established {} output root; remove it manually if it is a legacy aggregate",
                            stale.display(),
                            dir.display(),
                            asset_type
                        );
                    }
                }
            }
        }

        // A saved page's loader URL names the packages fingerprint it was
        // rendered with: record this build's pins under this build's
        // segment so those URLs keep resolving to their original bytes
        // across rebuilds. Done after history, retirement, and migration
        // are settled so the pin map and the retained files agree — dead
        // pins are dropped first so no record outlives what it serves.
        manifest.prune_packages_segments();
        manifest.record_packages_segment(&crate::jam::packages_segment(&manifest))?;

        // Prune old file versions from storage. Retention comes from the
        // manifest itself: `new_referenced` is the resolved file of every
        // current and history entry still referenced — a count of what
        // happens to be on disk cannot substitute, since a stray file
        // sharing a version number would displace a version a pinned URL
        // still serves. Deletion goes one step further: only a file the
        // loaded manifest recorded may go (`old_recorded`, below), so a
        // missing or replaced manifest never turns the versions on disk
        // into prune candidates — the new manifest names only its own.
        // What a failed unlink or a killed build leaves behind is carried
        // in the pending-output journal instead, and the next build
        // removes it (`settle_pending`).
        //
        // The retained set is manifest-wide, not per asset: two live
        // assets can share one (dir, stem, extension) group — a renamed
        // module keeps the source-derived stem under beside-source, so
        // the retired key's output and the new key's output sit in the
        // same group — and a per-asset scan would delete the sibling's
        // files as strays. One directory scan per group covers that and
        // shares with the recorded-location pass below: every referenced
        // version in a group resolves to it, so scanning it again there —
        // or once per recorded output — would multiply read_dir calls by
        // the retained history. Keep-all never scans: it retires nothing.
        let mut new_referenced: HashSet<PathBuf> = HashSet::new();
        let mut pruned_groups: BTreeSet<(PathBuf, String, String)> = BTreeSet::new();
        let mut group_anchors: Vec<PathBuf> = Vec::new();
        for (key, entry) in &manifest.assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&entry.output)
                .chain(entry.history.iter().map(|history| &history.output))
            {
                if let Some(path) = self.output_dirs.compiled_file(output, kind) {
                    if let Some((stem, ext)) = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(StorageBackend::split_versioned_name)
                    {
                        let group = (
                            path.parent().map(Path::to_path_buf).unwrap_or_default(),
                            stem.to_string(),
                            ext.to_string(),
                        );
                        if pruned_groups.insert(group) {
                            group_anchors.push(path.clone());
                        }
                    }
                    new_referenced.insert(path);
                }
            }
        }

        // Every recorded-location file the migration pass below may
        // unlink: the old outputs' own anchors plus the directories their
        // stems would prune.
        let mut recorded_anchors: Vec<PathBuf> = Vec::new();
        for (key, old_entry) in &old_assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&old_entry.output)
                .chain(old_entry.history.iter().map(|history| &history.output))
            {
                recorded_anchors.extend(
                    self.existing_output_candidates(output, kind, &known_index)
                        .into_iter()
                        .filter(|candidate| candidate.is_file()),
                );
            }
        }
        // The files the loaded manifest recorded, where this build resolves
        // them and where they were found: the only prune candidates.
        let mut old_recorded: HashSet<PathBuf> = recorded_anchors.iter().cloned().collect();
        for (key, old_entry) in &old_assets {
            let Some(kind) = Self::key_asset_type(key) else {
                continue;
            };
            for output in std::iter::once(&old_entry.output)
                .chain(old_entry.history.iter().map(|history| &history.output))
            {
                old_recorded.extend(self.output_dirs.compiled_file(output, kind));
            }
        }

        // Destructive preflight, before the manifest publishes: every
        // directory these passes will unlink from must sit inside
        // physically owned output — no symlinked component may redirect
        // the deletion into an unrelated tree. A refusal keeps the last
        // valid manifest and every retained version in place. The guard
        // re-runs at each unlink site below — a preflight alone cannot
        // eliminate a hostile swap between check and delete.
        let established_root = |path: &Path| {
            self.established_root_for(path, &known_dirs)
                .unwrap_or_else(|| self.config.base_dir.clone())
        };
        for anchor in group_anchors.iter().chain(&recorded_anchors) {
            let root = established_root(anchor);
            anyhow::ensure!(
                self.deletion_safe(anchor, &root)?,
                "unsafe cleanup target: {} resolves through a symlinked component — refusing to prune or publish a new manifest",
                anchor.display()
            );
        }

        // Final input sweep, still pre-publish: every byte the build
        // consumed — entries and imports alike — must still be what it
        // was compiled from.
        self.verify_captured_inputs()?;
        // A contributing root that vanished mid-walk can leave no
        // captured input to fail the sweep — the outage check re-runs
        // here against the prior manifest's attribution so an emptied
        // source set still cannot publish.
        self.guard_missing_roots(&old_assets)?;

        // The `generated` stamp alone is volatile: a rebuild whose
        // assembled state matches the previous authoritative manifest on
        // every effective field keeps the old stamp and never republishes
        // — otherwise a no-op local build would mint a new
        // manifest-generation token and retire pages mid-render.
        let unchanged = previous_manifest.as_ref().is_some_and(|previous| {
            let mut comparison = manifest.clone();
            comparison.generated = previous.generated.clone();
            comparison.canonical_json().ok() == previous.canonical_json().ok()
        });
        if unchanged {
            manifest.generated = previous_manifest.as_ref().unwrap().generated.clone();
        }

        // Essential fallible preparation, all of it while the previous
        // manifest is still authoritative. Cache upkeep is decoupled
        // from publication: a `.bin` is disposable — the JSON is always
        // authoritative — so a no-op build still repairs a missing or
        // stale local cache and still clears a dist stray, as
        // best-effort maintenance that warns rather than fails. When a
        // publication does follow, staging stays essential pre-publish
        // work whose failure leaves the old manifest untouched.
        let manifest_json = manifest.canonical_json()?;
        // The manifest's directory is created and checked by the same
        // ownership rule as every output directory: a symlinked component
        // between the boundary and the manifest is refused, never followed.
        // A manifest outside both the project and every output root is a
        // configured location of its own: its directory is the anchor.
        let manifest_dir = crate::dirfd::parent_dir(&self.config.manifest_path);
        let manifest_boundary = if self.config.manifest_path.starts_with(&self.config.base_dir)
            || self
                .established_root_for(&self.config.manifest_path, &[])
                .is_some()
        {
            self.write_boundary(&self.config.manifest_path)?
        } else {
            manifest_dir.to_path_buf()
        };
        crate::storage::ensure_owned_parent_in(
            manifest_dir,
            &manifest_boundary,
            manifest_boundary == self.config.base_dir,
        )?;
        let backend = self.backend()?;
        let staged_cache = if backend.is_dist() {
            // The cache sits beside the manifest — a configured
            // location, so the same ownership rule covers its removal.
            // A `.bin` entry must not survive any dist build, changed
            // or not; the entry check sees a dangling link too.
            let cache = self.config.manifest_path.with_extension("bin");
            if !matches!(
                fs::symlink_metadata(&cache),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            ) {
                let removal = (|| -> Result<()> {
                    if let Some(root) = cache.parent() {
                        anyhow::ensure!(
                            self.deletion_safe(&cache, root)?,
                            "unsafe cleanup target: {} resolves through a symlinked component — refusing to publish a new manifest",
                            cache.display()
                        );
                    }
                    Manifest::remove_cache(&self.config.manifest_path)
                })();
                match removal {
                    Ok(()) => {}
                    Err(e) if unchanged => {
                        eprintln!("Warning: could not remove the stray manifest cache: {e:#}");
                    }
                    Err(e) => return Err(e),
                }
            }
            None
        } else if unchanged {
            // No publication boundary is coming: a missing, corrupt,
            // or stale-hash cache is re-staged and installed in place —
            // the authoritative JSON and its generation never move.
            if !Manifest::cache_is_current(&self.config.manifest_path, &manifest_json) {
                let repair = manifest
                    .prepare_cache(&self.config.manifest_path, &manifest_json)
                    .map(|staged| {
                        let result = Manifest::commit_cache(&staged, &self.config.manifest_path);
                        (staged, result)
                    });
                match repair {
                    Ok((staged, Ok(()))) => staged.disarm(),
                    Ok((_, Err(e))) | Err(e) => eprintln!(
                        "Warning: could not repair the manifest cache: {e:#} — \
                         loads fall back to the authoritative JSON"
                    ),
                }
            }
            None
        } else {
            Some(manifest.prepare_cache(&self.config.manifest_path, &manifest_json)?)
        };

        // THE PUBLISH BOUNDARY: one atomic swap — the moment the new
        // manifest becomes the one readers see. Everything essential
        // already happened; nothing below may fail the build. An
        // unchanged manifest never crosses it — the authoritative bytes
        // stay exactly as readers and pinned pages already see them. A
        // staged cache that never got installed — publication failing
        // after it was armed, or the install below warning — is removed
        // by the armed guard, not left for a later run.
        self.forget_published(&manifest);
        if !unchanged {
            Manifest::publish_json(&self.config.manifest_path, &manifest_json)?;
        }
        // The manifest on disk now names everything `forget_published`
        // dropped — published just now, or unchanged and already there.
        self.publication_done.set(true);
        if !unchanged {
            // Post-publish maintenance: diagnostics, never build errors.
            // The published manifest is authoritative, every file it
            // references was written pre-publish, and the next build
            // retries this same cleanup — a stray cache or orphaned
            // version degrades to a warning, not a failed build over a
            // live manifest. A version the bound retired stays in a group
            // the published manifest records, so the next build's scan
            // removes it once the failure clears.
            if let Some(staged) = staged_cache {
                match Manifest::commit_cache(&staged, &self.config.manifest_path) {
                    Ok(()) => staged.disarm(),
                    Err(e) => eprintln!(
                        "Warning: could not install the manifest cache: {e:#} — \
                         loads fall back to the authoritative JSON"
                    ),
                }
            }
        }

        let boundary = |path: &Path| -> PathBuf {
            if path.starts_with(&self.config.base_dir) {
                self.config.base_dir.clone()
            } else {
                established_root(path)
            }
        };
        // A failed unlink is recorded for the next build — the version
        // is no longer in the manifest just published, so nothing else
        // would ever name it again.
        let prune = |anchor: &Path, anchor_boundary: &Path| match backend.prune_versions_set_in(
            anchor,
            &new_referenced,
            Some(&old_recorded),
            anchor_boundary,
            anchor_boundary == self.config.base_dir,
        ) {
            Ok(failed) => {
                for path in failed {
                    eprintln!(
                        "Warning: could not prune {} — the next build retries",
                        path.display()
                    );
                    self.unpruned_outputs.borrow_mut().push(path);
                }
            }
            Err(e) => {
                eprintln!(
                    "Warning: version pruning skipped {}: {e:#} — \
                     the next build retries",
                    anchor.display()
                );
                // The scan never ran: every recorded file of this group
                // the new manifest dropped is still due.
                let group = |path: &Path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .and_then(StorageBackend::split_versioned_name)
                        .map(|(stem, ext)| {
                            (
                                path.parent().map(Path::to_path_buf),
                                stem.to_string(),
                                ext.to_string(),
                            )
                        })
                };
                let due = group(anchor);
                self.unpruned_outputs.borrow_mut().extend(
                    old_recorded
                        .iter()
                        .filter(|path| !new_referenced.contains(*path) && group(path) == due)
                        .cloned(),
                );
            }
        };
        for anchor in &group_anchors {
            prune(anchor, &boundary(anchor));
        }

        // Prune the locations the migration just moved away from: each old
        // output's directory is pruned by its exact stem and extension, and
        // the old file itself is removed only when the new manifest no
        // longer references it. Missing files are harmless; unrelated
        // siblings are never touched.
        for anchor in &recorded_anchors {
            if let Some((stem, ext)) = anchor
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(StorageBackend::split_versioned_name)
            {
                let group = (
                    anchor.parent().map(Path::to_path_buf).unwrap_or_default(),
                    stem.to_string(),
                    ext.to_string(),
                );
                if pruned_groups.insert(group) {
                    prune(anchor, &boundary(anchor));
                }
            }
            // A case-only spelling change names the same physical file on a
            // case-insensitive filesystem — identity, not spelling, decides.
            let still_referenced = new_referenced.contains(anchor)
                || bound_identity(anchor).is_some_and(|id| {
                    new_referenced
                        .iter()
                        .any(|path| bound_identity(path) == Some(id))
                });
            if !still_referenced {
                // The pre-publish sweep already refused unsafe targets; a
                // refusal here can only be a mid-build swap, so leave the
                // file and warn rather than fail over a live manifest.
                let anchor_name = anchor.file_name().unwrap_or_default();
                match match self.deletion_dir(anchor, &established_root(anchor)) {
                    Ok(crate::dirfd::Descend::Anchored(dir)) => dir.remove_file(anchor_name),
                    Ok(crate::dirfd::Descend::Missing) => Ok(()),
                    Ok(crate::dirfd::Descend::Unsafe) | Err(_) => Err(std::io::Error::other(
                        "resolves through a symlinked component",
                    )),
                } {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        eprintln!(
                            "Warning: could not unlink migrated output {}: {e} — \
                             it stays until a later build retries",
                            anchor.display()
                        );
                        self.unpruned_outputs.borrow_mut().push(anchor.clone());
                    }
                }
            }
        }

        Ok(manifest)
    }

    /// Record one source input under its canonical path. The first
    /// capture wins: a later capture of the same path may only repeat
    /// those bytes — anything else means the input changed between two
    /// reads this build made, and publishing would describe two source
    /// generations at once.
    fn capture_input(&self, path: PathBuf, bytes: Vec<u8>) -> Result<()> {
        let mut captured = self.captured_inputs.borrow_mut();
        match captured.get(&path) {
            Some(existing) if *existing == bytes => {}
            Some(_) => {
                // A second Sass entry may observe different bytes for a
                // shared import. Preserve both generations so a watch-gap
                // rescan is not suppressed by the latest failed read. JS
                // reads are not Sass candidates and need no Sass snapshot.
                if self.attempted_sass_inputs.borrow().contains(&path) {
                    self.failed_sass_snapshot
                        .borrow_mut()
                        .reads
                        .insert(path.clone(), bytes);
                }
                return Err(
                    anyhow::anyhow!("source {} changed during the build", path.display())
                        .context(RecoverableWatchInput(path)),
                );
            }
            None => {
                captured.insert(path, bytes);
            }
        }
        Ok(())
    }

    /// Record the symlink used to resolve a consumed Sass import. Its
    /// physical target is as important as its bytes for future watches.
    fn capture_symlink_target(&self, alias: PathBuf, target: PathBuf) -> Result<()> {
        let mut captured = self.captured_symlink_targets.borrow_mut();
        if captured
            .get(&alias)
            .is_some_and(|previous| previous != &target)
        {
            return Err(anyhow::anyhow!(
                "Sass import symlink {} changed during the build",
                alias.display()
            )
            .context(RecoverableWatchInput(alias)));
        }
        captured.insert(alias, target);
        Ok(())
    }

    /// Re-verify every source input the build consumed — entry files,
    /// resolved imports and imported symlink entries — immediately before
    /// the manifest publishes. Per-asset checks abort early; this sweep
    /// closes the window between an asset's own check and the swap.
    fn verify_captured_inputs(&self) -> Result<()> {
        #[cfg(test)]
        if let Some(edit) = &self.mid_build_edit {
            edit(&self.config.manifest_path);
        }
        for (path, bytes) in self.captured_inputs.borrow().iter() {
            match fs::read(path) {
                Ok(now) if now == *bytes => {}
                _ => {
                    return Err(anyhow::anyhow!(
                        "source {} changed during the build — refusing to publish an inconsistent manifest",
                        path.display()
                    ).context(RecoverableWatchInput(path.clone())));
                }
            }
        }
        for (alias, target) in self.captured_symlink_targets.borrow().iter() {
            if fs::canonicalize(alias).map_or(true, |current| current != *target) {
                return Err(anyhow::anyhow!(
                    "Sass import symlink {} changed during the build — refusing to publish an inconsistent manifest",
                    alias.display()
                )
                .context(RecoverableWatchInput(alias.clone())));
            }
        }
        Ok(())
    }

    /// Project-relative label for an asset's source file, with forward slashes.
    ///
    /// The label identifies the file rather than the machine that built it, so a
    /// manifest built in one checkout matches a manifest built in another, and a
    /// consumer that reads sources (a debug response, an inlined bootstrap) can
    /// resolve it from the project root.
    ///
    /// Every configured source and include root, tagged by whether it
    /// came from a source dir — the one list the preflight guard and
    /// discovery both walk, so the two can never disagree about what is
    /// configured.
    fn configured_roots(&self) -> Vec<(PathBuf, bool)> {
        self.config
            .source_dirs()
            .into_iter()
            .map(|root| (root, true))
            .chain(
                self.config
                    .include_paths
                    .iter()
                    .cloned()
                    .map(|root| (root, false)),
            )
            .collect()
    }

    /// Whether `root` contributed an asset the given manifest state still
    /// serves — the provenance check that separates an optional absent
    /// root from an outage, shared by the preflight guard, the discovery
    /// walk, and the pre-publish recheck.
    fn root_contributed(
        &self,
        previous_assets: &HashMap<String, AssetEntry>,
        root: &Path,
        from_source_dir: bool,
    ) -> bool {
        previous_assets.values().any(|entry| {
            !entry.retired && self.label_could_come_from(&entry.source, root, from_source_dir)
        })
    }

    /// Refuse to build when a configured root vanished while still
    /// represented in `previous_assets`. An absent root that never
    /// contributed (an optional root configured before it was populated)
    /// is not an outage, and deleting one file inside a present root is
    /// an ordinary retirement — only the root's own disappearance with
    /// recorded active sources blocks the build.
    fn guard_missing_roots(&self, previous_assets: &HashMap<String, AssetEntry>) -> Result<()> {
        for (root, from_source_dir) in self.configured_roots() {
            if matches!(classify_root(&root)?, SourceRoot::Missing) {
                self.check_vanished_root(previous_assets, &root, from_source_dir)?;
            }
        }
        Ok(())
    }

    /// Whether a recorded `source` label could have been written for a
    /// file discovered under `root` — the inversion of `source_label` for
    /// whatever root shape was configured. A file under any root inside
    /// base_dir — source dir or include path — records the
    /// base_dir-relative path, so contribution is the resolved path's
    /// containment. A file under an external source root records the bare
    /// file name; one under an external include root records
    /// `<include dir name>/<relative>`.
    fn label_could_come_from(&self, label: &str, root: &Path, from_source_dir: bool) -> bool {
        // The writer no longer emits an absolute or `..` label, but an older
        // manifest may carry one; its origin cannot be proven, so it fails
        // closed — counting as a contribution keeps a vanished root an
        // outage instead of silently retiring what it served.
        let label_path = Path::new(label);
        if label_path.is_absolute()
            || label_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return true;
        }
        if self.project_relative(root).is_some() {
            return collapse_parents(&self.config.base_dir.join(label))
                .starts_with(collapse_parents(root));
        }
        if from_source_dir {
            return !label.contains('/');
        }
        match root.file_name() {
            Some(name) => {
                let name = name.to_string_lossy();
                label == name.as_ref() || label.starts_with(&format!("{name}/"))
            }
            // An include root with no basename (the filesystem root)
            // falls through source_label to the bare file name too.
            None => !label.contains('/'),
        }
    }

    /// `path` relative to base_dir when it lies inside it by plain
    /// components only — a lexical `base/../x` spelling is outside.
    fn project_relative(&self, path: &Path) -> Option<PathBuf> {
        project_relative(path, &self.config.base_dir)
    }

    fn label_space(root: &Path, from_source_dir: bool, base_dir: &Path) -> LabelSpace {
        if project_relative(root, base_dir).is_some() {
            return LabelSpace::Project(root.to_path_buf());
        }
        if !from_source_dir {
            if let Some(name) = root.file_name() {
                return LabelSpace::Prefixed(name.to_string_lossy().into_owned());
            }
        }
        LabelSpace::Bare
    }

    /// Whether two configured roots can produce a label the other could
    /// also have written — where they do, a vanished root's contribution
    /// is unprovable and the build must refuse rather than guess. Two
    /// `Project` spaces never overlap undecidably: the recorded label
    /// resolves to one concrete path, which either sat inside the
    /// vanished root or did not.
    fn label_spaces_overlap(&self, a: &LabelSpace, b: &LabelSpace) -> bool {
        match (a, b) {
            (LabelSpace::Project(_), LabelSpace::Project(_)) => false,
            (LabelSpace::Project(root), LabelSpace::Prefixed(prefix))
            | (LabelSpace::Prefixed(prefix), LabelSpace::Project(root)) => {
                let resolved = self.config.base_dir.join(prefix);
                resolved.starts_with(root) || root.starts_with(&resolved)
            }
            // Only base_dir itself produces bare labels — every other
            // in-project root labels below its own relative path.
            (LabelSpace::Project(root), LabelSpace::Bare)
            | (LabelSpace::Bare, LabelSpace::Project(root)) => root == &self.config.base_dir,
            (LabelSpace::Prefixed(a), LabelSpace::Prefixed(b)) => a == b,
            (LabelSpace::Bare, LabelSpace::Bare) => true,
            (LabelSpace::Prefixed(_), LabelSpace::Bare)
            | (LabelSpace::Bare, LabelSpace::Prefixed(_)) => false,
        }
    }

    /// Fail when a vanished root still claims recorded assets. An absent
    /// root no label could have come from stays optional; one a label
    /// does name is an outage — and where another configured root
    /// produces indistinguishable labels, the contribution cannot be
    /// proven either way and the build refuses rather than guess.
    fn check_vanished_root(
        &self,
        previous_assets: &HashMap<String, AssetEntry>,
        root: &Path,
        from_source_dir: bool,
    ) -> Result<()> {
        if !self.root_contributed(previous_assets, root, from_source_dir) {
            return Ok(());
        }
        let missing = Self::label_space(root, from_source_dir, &self.config.base_dir);
        if let Some((other, _)) = self
            .configured_roots()
            .into_iter()
            .filter(|(other, _)| other != root)
            .find(|(other, other_from_source)| {
                self.label_spaces_overlap(
                    &missing,
                    &Self::label_space(other, *other_from_source, &self.config.base_dir),
                )
            })
        {
            return Err(anyhow::anyhow!(
                "cannot attribute recorded sources when {} is missing: {} produces indistinguishable source labels — restore the root or rename one so each root labels uniquely",
                root.display(),
                other.display()
            ).context(RecoverableWatchInput(root.to_path_buf())));
        }
        Err(anyhow::anyhow!(
            "configured source root disappeared: {} — the previous manifest still serves assets sourced under it; restore the root or remove it from the configuration",
            root.display()
        ).context(RecoverableWatchInput(root.to_path_buf())))
    }

    /// Project-relative label for an asset's source file, with forward slashes.
    ///
    /// The label identifies the file rather than the machine that built it, so a
    /// manifest built in one checkout matches a manifest built in another, and a
    /// consumer that reads sources (a debug response, an inlined bootstrap) can
    /// resolve it from the project root.
    ///
    /// A file pulled in from an include path outside the project is labelled
    /// relative to that include path, prefixed with the include directory's own
    /// name, so it stays both stable and recognizable.
    fn source_label(&self, path: &Path) -> String {
        let relative = self
            .project_relative(path)
            .or_else(|| self.include_path_label(path))
            .or_else(|| path.file_name().map(PathBuf::from))
            .unwrap_or_default();
        relative.to_string_lossy().replace('\\', "/")
    }

    fn include_path_label(&self, path: &Path) -> Option<PathBuf> {
        self.config.include_paths.iter().find_map(|include_path| {
            let relative = project_relative(path, include_path)?;
            let root = include_path.file_name().map(PathBuf::from)?;
            Some(root.join(relative))
        })
    }

    /// Walk every source root — the configured `source.dirs` plus the legacy
    /// js/scss roots, then every include path — and classify each file by
    /// extension. One dir may hold JS and styles together: the folder layout
    /// means nothing to discovery, only the extension does. `sort_by_file_name`
    /// keeps walk order deterministic, and the first root to claim a module
    /// name or style asset key wins. `generated_dirs` — the configured output
    /// roots, the manifest's recorded generated directories, and the parents
    /// of the old manifest's existing outputs — are never sources, and a
    /// missing optional root is skipped. Walk errors fail the build rather
    /// than silently publishing a reduced manifest.
    /// `previous_assets` carries the prior manifest's attribution: a root
    /// that vanishes between the preflight guard and this walk is the same
    /// outage, so the walk's own classification re-applies it rather than
    /// skipping the root into an empty source set.
    fn discover_sources(
        &self,
        generated_dirs: &[PathBuf],
        previous_assets: &HashMap<String, AssetEntry>,
    ) -> Result<Vec<DiscoveredSource>> {
        let mut found = Vec::new();
        for (root, from_source_dir) in self.configured_roots() {
            // An optional root that never contributed may be absent; every
            // other classification failure — a non-directory parked at the
            // root's path, a stat error — propagates rather than silently
            // producing a reduced manifest. An unreadable directory still
            // walks: the walker surfaces the permission error itself.
            match classify_root(&root)? {
                SourceRoot::Directory => {}
                SourceRoot::Missing => {
                    self.check_vanished_root(previous_assets, &root, from_source_dir)?;
                    continue;
                }
            }
            // A configured root equal to or nested inside a directory the
            // build writes — current output roots, or generated locations
            // the previous manifest recorded before an output relocation —
            // would be pruned by the walk's own filter at depth zero. Fail
            // naming it rather than publish a manifest that silently lost
            // the modules that root was configured to contribute.
            if let Some(generated) = generated_dirs
                .iter()
                .find(|directory| root.starts_with(directory))
            {
                let kind = if from_source_dir {
                    "source dir"
                } else {
                    "include path"
                };
                anyhow::bail!(
                    "configured {kind} {} is inside generated output directory {} — an input root cannot also be generated output; move it or adjust the output configuration",
                    root.display(),
                    generated.display()
                );
            }
            for entry in WalkDir::new(&root)
                .sort_by_file_name()
                .into_iter()
                // Generated trees are pruned, not just filtered after the
                // fact: descending into them would walk every retained
                // version on every build.
                .filter_entry(|e| {
                    !is_build_output(e, &self.config.output.build_dir_name)
                        && !generated_dirs
                            .iter()
                            .any(|directory| e.path().starts_with(directory))
                })
            {
                let entry = entry.with_context(|| RecoverableWatchInput(root.clone()))?;
                if entry.file_type().is_symlink() {
                    // Links inside a root are never followed — say so rather
                    // than drop a source without a trace.
                    eprintln!(
                        "Warning: skipping symlinked source {} — links inside a source root are not followed",
                        entry.path().display()
                    );
                    continue;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                // A generated versioned filename is never a source under a
                // configured root — a stale build directory whose configured
                // name has since changed cannot feed its output back in. An
                // include path may deliberately point at a built
                // distribution, so the check applies to source roots only.
                if from_source_dir && crate::storage::is_versioned_file(path) {
                    continue;
                }
                let kind = match path.extension().and_then(|ext| ext.to_str()) {
                    Some("js") => SourceKind::Js,
                    Some("scss") => SourceKind::Scss,
                    Some("css") => SourceKind::Css,
                    _ => continue,
                };
                found.push(DiscoveredSource {
                    path: path.to_path_buf(),
                    root: root.clone(),
                    from_source_dir,
                    kind,
                });
            }
        }
        Ok(found)
    }

    /// Register one JS source file's module name and dependencies.
    /// Files with @module comments use that name; others derive from the
    /// path relative to their walk root. The first root to claim a name
    /// wins; later files with the same name are skipped.
    fn register_js_module(
        &mut self,
        path: &Path,
        root: &Path,
        from_source_dir: bool,
    ) -> Result<()> {
        // A source that cannot be read fails the build — silently dropping
        // it would publish a manifest missing a module it promises.
        let content =
            fs::read_to_string(path).with_context(|| RecoverableWatchInput(path.to_path_buf()))?;
        // The compile pass consumes these exact bytes — the manifest's
        // dependency metadata and the emitted output describe the same
        // read, never two reads straddling an edit.
        self.source_snapshots
            .insert(path.to_path_buf(), content.clone());
        self.capture_input(
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
            content.clone().into_bytes(),
        )?;
        // Declarations live in comments — a `@module` inside a string
        // literal or a regex is data, not a module claim. Real comment
        // spans come from the parser, not a scanner that cannot tell a
        // regex literal from `//`.
        // A file oxc cannot parse yields no comment spans; if it declares
        // anything, falling back to a path-derived name with no
        // dependencies would silently publish the wrong module.
        let comments = match js_comment_spans(&content) {
            Some(comments) => comments,
            None if content.contains("@module") || content.contains("@requires") => {
                return Err(anyhow::anyhow!(
                    "{}: cannot read its @module/@requires declarations — the JavaScript does not parse",
                    path.display()
                ))
                .with_context(|| RecoverableWatchInput(path.to_path_buf()));
            }
            None => Vec::new(),
        };
        let (declared_name, deps) = self
            .declarations(&comments, &content, path)
            .with_context(|| RecoverableWatchInput(path.to_path_buf()))?;
        if let Some(dep) = deps.iter().find(|dep| dep.ends_with(".min")) {
            return Err(
                min_requirement_error(dep, path).context(RecoverableWatchInput(path.to_path_buf()))
            );
        }

        // Find module name from @module comment
        let (module_name, derived) = if let Some(name) = declared_name {
            (name, false)
        } else {
            // Derive from path (fallback)
            (
                source_stem(path.strip_prefix(root).unwrap_or(path))
                    .to_string_lossy()
                    .replace(['/', '\\'], "."),
                true,
            )
        };

        // A name that is not dot-separated word segments could join a
        // path outside the output tree (`@module /tmp` normalizes to
        // `.tmp`, an absolute tail) — refuse the file outright.
        if !is_module_name(&module_name) {
            return Err(anyhow::anyhow!(
                "invalid module name {:?} in {}",
                module_name,
                path.display()
            )
            .context(RecoverableWatchInput(path.to_path_buf())));
        }
        // The paste runtime reads a name whose last segment is `js` as a
        // script URL (`paste.require('chart.js')` fetches a page-relative
        // file and never runs the module's factory), so such a JS module
        // name could never load.
        if module_name.rsplit('.').next() == Some("js") {
            return Err(anyhow::anyhow!(
                "module name {:?} in {} ends in `.js` — the paste runtime treats such a name as a script URL; declare a different @module name",
                module_name,
                path.display()
            )
            .context(RecoverableWatchInput(path.to_path_buf())));
        }

        if module_name.ends_with(".min") {
            return Err(min_module_name_error(&module_name, path, !derived)
                .context(RecoverableWatchInput(path.to_path_buf())));
        }

        // Don't overwrite a module already found in an earlier root — but a
        // `.min` copy and its plain source are one module, never a choice.
        if let Some((first, _)) = self.module_map.get(&module_name) {
            if is_min_source(first) || is_min_source(path) {
                return Err(min_twin_error(&module_name, first, path)
                    .context(RecoverableWatchInput(path.to_path_buf())));
            }
            return Ok(());
        }

        // An include-path file's module name is invisible at a glance —
        // say when the fallback supplies it.
        if derived && !from_source_dir {
            eprintln!(
                "Warning: {} in an include path has no @module; using path-derived name {}",
                path.display(),
                module_name
            );
        }

        self.js_deps.insert(module_name.clone(), deps);
        self.module_map
            .insert(module_name, (path.to_path_buf(), from_source_dir));
        Ok(())
    }

    /// The prefix an asset entry's versioned `output` is built on — the
    /// manifest records it and `OutputDirs::compiled_file` resolves it back
    /// to a file.
    ///
    /// In `flat` layout every output is `/{type}/{rel}` and lands under the
    /// type's own output directory. In `beside-source` layout a source-dir
    /// file inside base_dir compiles into `{file dir}/{build-dir-name}/{file
    /// stem}` beside its source; a file from an include path — never written
    /// into someone else's tree — keeps flat placement under output.base.
    /// Either way a beside-source output is recorded relative to base_dir,
    /// which is also the output root the runtime is handed.
    fn output_base_for(
        &self,
        source_path: &Path,
        from_source_dir: bool,
        flat_rel: &str,
        asset_type: &str,
    ) -> String {
        if self.config.output.layout != "beside-source" {
            return format!("/{}/{}", asset_type, flat_rel);
        }
        if from_source_dir {
            // Public output paths derive from the file's physical home —
            // the spelled source path may carry `..` or a symlinked
            // component an alias introduced, and neither belongs in a
            // recorded URL. The containing directory resolves; the file
            // name stays spelled so a linked file keeps its declared slot.
            let physical = source_path.parent().map(|p| {
                let dir = canonical_identity(p);
                match source_path.file_name() {
                    Some(name) => dir.join(name),
                    None => dir,
                }
            });
            let stem = source_path.file_name().and_then(|name| {
                source_stem(Path::new(name))
                    .file_name()
                    .map(|n| n.to_os_string())
            });
            if let (Some(physical), Some(stem)) = (physical.as_ref(), stem) {
                if let Ok(rel) = physical.strip_prefix(canonical_identity(&self.config.base_dir)) {
                    let dir = rel
                        .parent()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    let stem = stem.to_string_lossy();
                    return if dir.is_empty() {
                        format!("/{}/{}", self.config.output.build_dir_name, stem)
                    } else {
                        format!("/{}/{}/{}", dir, self.config.output.build_dir_name, stem)
                    };
                }
            }
        }
        let rel_base = self
            .config
            .output
            .base
            .strip_prefix(&self.config.base_dir)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|s| !s.is_empty());
        match rel_base {
            Some(rel) => format!("/{}/{}/{}", rel, asset_type, flat_rel),
            None => format!("/{}/{}", asset_type, flat_rel),
        }
    }

    fn process_js_with_deps(
        &self,
        source_path: PathBuf,
        from_source_dir: bool,
        module: &str,
        deps: Vec<String>,
        manifest: &mut Manifest,
    ) -> Result<()> {
        // Compile the bytes the declaration scan read — not a second read
        // that could carry an edit made mid-build into the output while
        // the manifest records the old declarations.
        let content = match self.source_snapshots.get(&source_path) {
            Some(snapshot) => snapshot.clone(),
            None => fs::read_to_string(&source_path)
                .with_context(|| RecoverableWatchInput(source_path.clone()))?,
        };

        // Skip minification in debug mode or when minify=false
        let output_content = if self.config.debug_mode || !self.config.minify {
            content.clone()
        } else {
            // One pass-through for every minifier failure, by deliberate
            // contract: a parse diagnostic cannot tell malformed input
            // from syntax newer than the parser, and neither warrants
            // anything but the author's own bytes — the emitted file is
            // byte-exact what the source held, so a page serving it is
            // never worse off than minify=false. An optional
            // optimization must never gate publication; the warning
            // keeps the fallback visible instead.
            match minify_js(&content) {
                Ok(minified) => minified,
                Err(e) => {
                    eprintln!("Warning: JS minification failed for {}: {}", module, e);
                    content.clone()
                }
            }
        };

        // Hash the emitted bytes, not the source: only an output change may
        // roll the version — a comment edit that the printer drops produces
        // identical bytes and keeps the name, while a whitespace change that
        // reaches the output gets a new one.
        let hash = format!("{:x}", md5::compute(output_content.as_bytes()));

        // The emitted bytes and the recorded declarations both describe
        // `content` — if the file moved since that read, publishing would
        // record a graph for a source state that no longer exists. Abort
        // so the next build (a watch event already queued on the same
        // edit) compiles the real source.
        #[cfg(test)]
        if let Some(edit) = &self.mid_build_edit {
            edit(&source_path);
        }
        match fs::read(&source_path) {
            Ok(now) if now == content.as_bytes() => {}
            _ => {
                return Err(anyhow::anyhow!(
                    "source {} changed during the build",
                    source_path.display()
                )
                .context(RecoverableWatchInput(source_path.clone())));
            }
        }

        let key = format!("js/{}.js", module.replace('.', "/"));
        let rel_path = module.replace('.', "/");
        let output_base = self.output_base_for(&source_path, from_source_dir, &rel_path, "js");

        let entry = manifest.update_asset_with_history(AssetUpdate {
            key: &key,
            source: &self.source_label(&source_path),
            hash: &hash,
            output_base: &output_base,
            ext: "js",
            dependencies: deps, // Use expanded transitive dependencies
            max_history: self.history_bound()?,
            stamp: source_stamp(&source_path, self.stamp_clamp),
            module_name: Some(module),
        })?;

        // Resolve the target through the same resolver every read uses, and
        // write it unconditionally: existence is not correctness — the
        // backend compares bytes, so an unchanged output keeps its mtime
        // while a tampered or truncated one is repaired.
        let target = self
            .output_dirs
            .compiled_file(&entry.output, AssetType::Js)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", entry.output))?;
        self.write_output(&target, output_content.as_bytes())?;

        Ok(())
    }

    /// One source file's declarations — an optional `@module` name and the
    /// `@requires` list — read strictly from real comment spans: the shared
    /// contract for JS and stylesheet sources. Each annotation occurrence is
    /// parsed against the exact grammar by [`parse_annotations`]: a comment
    /// carrying the keyword but no parseable argument, or an argument with
    /// material trailing it, is a malformed declaration; a token that is not
    /// a module name is an invalid one; two `@module` occurrences naming
    /// different names conflict. All of it fails the build naming the file,
    /// the annotation's own line (not the comment's first line), the keyword,
    /// and the full offending annotation text — never silently dropping into
    /// path-derived naming.
    fn declarations(
        &self,
        comments: &[(usize, &str)],
        content: &str,
        source_path: &Path,
    ) -> Result<(Option<String>, Vec<String>)> {
        let mut module_name: Option<String> = None;
        let mut deps: Vec<String> = Vec::new();
        for &(offset, comment) in comments {
            let (inner, inner_base) = comment_inner(comment);
            let decls = match parse_annotations(inner) {
                Ok(decls) => decls,
                Err(MalformedDecl { pos, keyword }) => {
                    let segment = segment_at(inner, pos);
                    anyhow::bail!(
                        "{}:{}: malformed {} declaration {:?} — expected `{} <name>`",
                        source_path.display(),
                        line_of(content, offset + inner_base + pos),
                        keyword,
                        segment,
                        keyword,
                    );
                }
            };
            for decl in decls {
                let line = line_of(content, offset + inner_base + decl.pos);
                // Convert slash to dot after parsing: paste/ui/heroscroll
                // -> paste.ui.heroscroll
                let name = decl.name.replace('/', ".");
                if !is_module_name(&name) {
                    anyhow::bail!(
                        "{}:{}: invalid {} name {:?} in declaration {:?}",
                        source_path.display(),
                        line,
                        decl.keyword,
                        name,
                        segment_at(inner, decl.pos),
                    );
                }
                match decl.keyword {
                    "@module" => match &module_name {
                        Some(existing) if *existing != name => anyhow::bail!(
                            "{}:{}: conflicting @module declarations {:?} and {:?}",
                            source_path.display(),
                            line,
                            existing,
                            name,
                        ),
                        Some(_) => {}
                        None => module_name = Some(name),
                    },
                    _ => deps.push(name),
                }
            }
        }
        Ok((module_name, deps))
    }

    /// Parse one style file's `@module` name and `@requires` list from its
    /// content, validating both — shared by the registration scan and the
    /// compile pass so a declared name is checked exactly once per shape.
    /// Declared names land in the manifest and in output paths — refuse
    /// anything that is not dot-separated word segments.
    fn style_declarations(
        &self,
        content: &str,
        source_path: &Path,
    ) -> Result<(Option<String>, Vec<String>)> {
        // Declarations live in comments — a `@module` inside a CSS string
        // or an unquoted url() is data, not a module claim.
        let comments = style_comment_spans(content);
        self.declarations(&comments, content, source_path)
    }

    /// Process one style file — `.scss` or plain `.css`, which is valid
    /// SCSS — and return its module record for the layering and
    /// sorted_deps emission that follow the walk. `module_name` is the
    /// effective name the registration pass resolved — declared or
    /// path-derived, `None` when neither applies — and `dependencies` is
    /// the expanded list the caller computed from the registration map —
    /// like JS, the version decision sees the full dependency metadata, so
    /// a transitive-dependency edit still versions the file.
    fn process_scss(
        &self,
        source_path: &Path,
        base_dir: &Path,
        from_source_dir: bool,
        module_name: Option<&str>,
        dependencies: Vec<String>,
        manifest: &mut Manifest,
    ) -> Result<Option<CssModule>> {
        // Compile through grass with every source dir and include path as a
        // load path — an @import may resolve against any of them. Use
        // from_path so grass knows the file's location for relative @import
        // resolution; plain .css is valid SCSS and takes the same path.
        //
        // The Fs layer serves the entry file's declaration-scan bytes
        // verbatim and captures every other file the compiler reads — the
        // compiled output describes the input state the declarations were
        // parsed from, then each captured read is verified below so an
        // import edited mid-build aborts rather than publish a graph that
        // half-describes two source states.
        let snapshot_fs =
            SnapshotFs::new(source_path, self.source_snapshots.get(source_path).cloned());
        let mut options = grass::Options::default().fs(&snapshot_fs);
        for dir in self
            .config
            .source_dirs()
            .iter()
            .chain(&self.config.include_paths)
        {
            options = options.load_path(dir);
        }

        let compiled = grass::from_path(source_path, &options);
        // A compilation can fail after successfully reading external Sass
        // files (including when one changes during our verification). Keep
        // these attempted inputs even if the graph never reaches capture_input.
        // A failed lookup also records negative candidate probes, so creating
        // a brand-new partial alone can repair the build.
        let attempted_reads = snapshot_fs.reads();
        // Symlinked ancestors of the project directory are path plumbing,
        // not dependencies: retargeting one relocates the whole project,
        // not an input inside it. Only links beneath the project — or in
        // an external import's own ancestry — can be input identities
        // worth watching. Compare against both the configured and the
        // physical base spelling, since an ancestor's observed entry
        // keeps its lexical name while a deeper one's parent resolves.
        let observed_base = observed_input_path(base_dir);
        let attempted_links: HashMap<PathBuf, PathBuf> = snapshot_fs
            .symlink_targets()
            .into_iter()
            .filter(|(alias, _)| !base_dir.starts_with(alias) && !observed_base.starts_with(alias))
            .collect();
        {
            let mut attempted = self.attempted_sass_inputs.borrow_mut();
            attempted.extend(attempted_reads.iter().map(|(path, _)| path.clone()));
            attempted.extend(attempted_links.keys().cloned());
        }
        if compiled.is_err() {
            let mut failed = self.failed_sass_snapshot.borrow_mut();
            failed.reads.extend(attempted_reads);
            failed.symlink_targets.extend(attempted_links.clone());
            failed.negative_probes.extend(snapshot_fs.negative_probes());
            self.attempted_sass_inputs
                .borrow_mut()
                .extend(snapshot_fs.missing());
        }
        let css_content =
            compiled.with_context(|| RecoverableWatchInput(source_path.to_path_buf()))?;
        #[cfg(test)]
        if let Some(edit) = &self.mid_build_edit {
            edit(source_path);
        }
        // Every file the compiler consumed — the entry snapshot and each
        // resolved @import — joins the build-wide input set under its
        // canonical path plus any imported symlink entry spelling. The
        // pre-publish sweep re-verifies the whole
        // set regardless of the spelling each read arrived by. Verified
        // unchanged here for an early abort, and once more before the
        // manifest publishes.
        let captured = snapshot_fs.reads();
        for (path, bytes) in &captured {
            match fs::read(path) {
                Ok(now) if now == *bytes => {}
                _ => {
                    // Grass compiled successfully, but its import changed
                    // before byte verification. This is still a failed
                    // attempt that can introduce an as-yet-unwatched
                    // external input. Keep its actual read snapshot so
                    // watch can detect the registration-gap repair.
                    let mut failed = self.failed_sass_snapshot.borrow_mut();
                    failed.reads.extend(captured.iter().cloned());
                    failed.symlink_targets.extend(attempted_links.clone());
                    return Err(anyhow::anyhow!(
                        "source {} changed during the build",
                        path.display()
                    )
                    .context(RecoverableWatchInput(path.clone())));
                }
            }
        }
        // Merge insert-or-compare, never blind extend: a later entrypoint
        // importing a file an earlier one already captured must see the
        // same bytes — a different second capture means the input moved
        // mid-build and one manifest must not describe both generations.
        for (path, bytes) in captured {
            self.capture_input(path, bytes)?;
        }
        for (alias, target) in attempted_links {
            if fs::canonicalize(&alias).map_or(true, |current| current != target) {
                self.failed_sass_snapshot
                    .borrow_mut()
                    .symlink_targets
                    .insert(alias.clone(), target.clone());
                return Err(anyhow::anyhow!(
                    "Sass import symlink {} changed during compilation",
                    alias.display()
                )
                .context(RecoverableWatchInput(alias)));
            }
            self.capture_symlink_target(alias, target)?;
        }

        // Skip minification in debug mode or when minify=false
        let output_content = if self.config.debug_mode || !self.config.minify {
            css_content
        } else {
            // Use lightningcss for minification
            match minify_css(&css_content) {
                Ok(minified) => minified,
                Err(e) => {
                    eprintln!("Warning: CSS minification failed: {}", e);
                    css_content
                }
            }
        };

        // Hash the emitted bytes, not the source — same contract as the JS path.
        let hash = format!("{:x}", md5::compute(output_content.as_bytes()));

        let rel_path = source_path.strip_prefix(base_dir)?;
        let base_name = source_stem(rel_path).to_string_lossy().replace('\\', "/");
        let key = format!("css/{}.css", base_name);
        let output_base = self.output_base_for(source_path, from_source_dir, &base_name, "css");

        let entry = manifest.update_asset_with_history(AssetUpdate {
            key: &key,
            source: &self.source_label(source_path),
            hash: &hash,
            output_base: &output_base,
            ext: "css",
            dependencies,
            max_history: self.history_bound()?,
            stamp: source_stamp(source_path, self.stamp_clamp),
            module_name,
        })?;

        // Capture entry info before moving
        let output = entry.output.clone();
        let version = entry.version;

        // Resolve the target through the same resolver every read uses, and
        // write it unconditionally: existence is not correctness — the
        // backend compares bytes, so an unchanged output keeps its mtime
        // while a tampered or truncated one is repaired.
        let target = self
            .output_dirs
            .compiled_file(&entry.output, AssetType::Css)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", entry.output))?;
        self.write_output(&target, output_content.as_bytes())?;

        // Return the module record when the file is named; sorted_deps
        // emission consumes it.
        Ok(module_name.map(|name| CssModule {
            name: name.to_string(),
            output,
            version,
        }))
    }

    /// The asset type a manifest key belongs to (`js/…`, `css/…`), or None
    /// for a key outside the compiled set.
    /// Whether `dir` is, or contains, a configured source or include root.
    fn holds_input_root(&self, dir: &Path) -> bool {
        self.configured_roots()
            .iter()
            .any(|(root, _)| root.starts_with(dir))
    }

    fn key_asset_type(key: &str) -> Option<AssetType> {
        match key.split_once('/').map(|(t, _)| t) {
            Some("js") => Some(AssetType::Js),
            Some("css") => Some(AssetType::Css),
            _ => None,
        }
    }

    /// The deepest established output root `path` resolves under: a
    /// configured type directory or the output base first, then the
    /// recorded generated dirs. `None` means the path claims territory no
    /// established root covers — unsafe by definition.
    fn established_root_for(&self, path: &Path, known_dirs: &[PathBuf]) -> Option<PathBuf> {
        for root in [
            &self.output_dirs.js,
            &self.output_dirs.css,
            &self.output_dirs.base,
        ] {
            if path.starts_with(root) {
                return Some(root.clone());
            }
        }
        known_dirs
            .iter()
            .filter(|dir| path.starts_with(dir.as_path()))
            .max_by_key(|dir| dir.as_os_str().len())
            .cloned()
    }

    /// Whether `path` may be unlinked as generated output: it must sit
    /// beneath `root` (the established root the caller derived it under)
    /// and every component between the trusted boundary and the file's
    /// parent must be a real directory. The boundary is base_dir when
    /// `root` nests inside it — components above base_dir are the
    /// deployment's own aliases, not evidence of a swap; for a root
    /// outside base_dir the root itself is the boundary and must be a
    /// real directory, never a link. A residual gap remains: a symlinked
    /// ancestor *above* an out-of-base_dir root cannot be told apart from
    /// the deployment's own naming — physical ownership there relies on
    /// the root's own identity.
    fn deletion_safe(&self, path: &Path, root: &Path) -> Result<bool> {
        Ok(!matches!(
            self.deletion_dir(path, root)?,
            crate::dirfd::Descend::Unsafe
        ))
    }

    /// `deletion_safe`'s check as a pinned handle: on Unix the returned
    /// [`crate::dirfd::Descend::Anchored`] descriptor is the directory the
    /// unlink must run against — a component swapped for a link after this
    /// point changes the name, never the pinned inode, so the delete
    /// cannot cross out of the owned tree no matter when it lands.
    /// `Missing` means the parent chain is gone — nothing to delete.
    fn deletion_dir(&self, path: &Path, root: &Path) -> Result<crate::dirfd::Descend> {
        if !path.starts_with(root) {
            return Ok(crate::dirfd::Descend::Unsafe);
        }
        let parent = path.parent().unwrap_or(path);
        let boundary = if root.starts_with(&self.config.base_dir) {
            &self.config.base_dir
        } else {
            root
        };
        crate::storage::owned_dir_beneath_in(parent, boundary, boundary == self.config.base_dir)
    }

    /// The ownership anchor a generated-output write must stay beneath:
    /// base_dir when the file's established output root nests inside it
    /// (components above base_dir are the deployment's own aliases), the
    /// root itself otherwise — and an outside root must be a real
    /// directory, never a link. Mirrors `deletion_safe`'s boundary rule.
    fn write_boundary(&self, file: &Path) -> Result<PathBuf> {
        if let Some(root) = self.established_root_for(file, &[]) {
            if root.starts_with(&self.config.base_dir) {
                return Ok(self.config.base_dir.clone());
            }
            match fs::symlink_metadata(&root) {
                Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                    "refusing to write {} — output root {} is a symlink",
                    file.display(),
                    root.display()
                ),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(root);
        }
        Ok(self.config.base_dir.clone())
    }

    /// Resolve one recorded `generated_dirs` label to a path under
    /// base_dir. A label must be a relative path of ordinary components —
    /// anything else cannot name a generated directory and fails the build
    /// rather than silently dropping the exclusion.
    fn generated_dir_path(&self, label: &str) -> Result<PathBuf> {
        let rel = Path::new(label);
        anyhow::ensure!(
            !label.is_empty()
                && !label.contains(['\\', ':'])
                && rel.components().count() > 0
                && rel
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "manifest generated_dirs entry {:?} is not a project-relative directory",
            label
        );
        Ok(self.config.base_dir.join(rel))
    }

    /// The file locations a recorded `output` may already exist at, in
    /// resolver order: the current layout first, then the explicit flat
    /// layout, then the project-relative beside-source layout — after which
    /// the recorded generated directories are tried by basename through
    /// `known_index`. These are the locations this build and its
    /// predecessors can have written, not a filesystem search; every
    /// resolver candidate passed `compiled_file`'s path validation, and
    /// the basename fallback applies only to an output at least one
    /// resolver accepted — and only to a directory the index actually
    /// listed the basename in.
    fn existing_output_candidates(
        &self,
        output: &str,
        kind: AssetType,
        known_index: &HashMap<std::ffi::OsString, Vec<PathBuf>>,
    ) -> Vec<PathBuf> {
        let resolvers = [
            self.output_dirs.clone(),
            OutputDirs {
                base: self.config.output.base.clone(),
                js: self.config.output.js.clone(),
                css: self.config.output.css.clone(),
            },
            OutputDirs::under(&self.config.base_dir),
        ];
        let mut candidates = Vec::new();
        let mut validated = false;
        for resolver in &resolvers {
            if let Some(path) = resolver.compiled_file(output, kind) {
                validated = true;
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }
        }
        if validated {
            if let Some(basename) = Path::new(output.trim_start_matches('/')).file_name() {
                for path in known_index.get(basename).into_iter().flatten() {
                    if !candidates.contains(path) {
                        candidates.push(path.clone());
                    }
                }
            }
        }
        candidates
    }

    /// Make the file a recorded version resolves to exist at `output`,
    /// copying the bytes of whichever existing candidate still holds them —
    /// the recorded hash decides which candidate is real, so a tampered or
    /// stale copy is never mistaken for the retained version. A version
    /// with no existing candidate fails the build naming the version and
    /// path; candidates that exist but never match fail as corrupted
    /// historical output. Current bytes are never substituted for a
    /// historical version.
    fn materialize_version(
        &self,
        key: &str,
        version: u32,
        hash: &str,
        output: &str,
        kind: AssetType,
        known_index: &HashMap<std::ffi::OsString, Vec<PathBuf>>,
    ) -> Result<()> {
        let target = self
            .output_dirs
            .compiled_file(output, kind)
            .ok_or_else(|| anyhow::anyhow!("invalid output path: {}", output))?;
        let mut found = false;
        for candidate in self.existing_output_candidates(output, kind, known_index) {
            if !candidate.is_file() {
                continue;
            }
            found = true;
            // One bounded, hash-verified read per retained version; a
            // version already at its target needs no second read or write.
            if let Some(bytes) = crate::jam::verified_bytes(&candidate, hash) {
                if candidate == target {
                    return Ok(());
                }
                self.write_output(&target, &bytes)?;
                return Ok(());
            }
        }
        if found {
            anyhow::bail!(
                "corrupted retained output for {} v{}: no candidate matches recorded hash {}",
                key,
                version,
                hash
            );
        }
        anyhow::bail!(
            "retained output for {} v{} is missing: {} resolves to no existing file",
            key,
            version,
            output
        );
    }
}

/// `path` relative to `base` once `.` and `..` collapse — `src/../assets`
/// labels as `assets/…`, and `base/../shared` (lexically under `base`,
/// physically beside it) is not project-relative at all.
fn project_relative(path: &Path, base: &Path) -> Option<PathBuf> {
    collapse_parents(path)
        .strip_prefix(collapse_parents(base))
        .ok()
        .filter(|rel| {
            rel.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        })
        .map(Path::to_path_buf)
}

/// The project-relative label a generated directory is recorded under —
/// forward-slashed and ordinary components only — or None when the
/// directory is outside base_dir or is the project root itself, since
/// neither can be recorded safely.
fn generated_label(base_dir: &Path, dir: &Path) -> Option<String> {
    let rel = dir.strip_prefix(base_dir).ok()?;
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    // Only a label `generated_dir_path` will read back is recorded: forward
    // slashes between plain UTF-8 components, none containing `\` or `:`.
    let label = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join("/");
    (!label.contains(['\\', ':'])).then_some(label)
}

/// Record a declared module name against its manifest asset key.
///
/// A source path with its extension removed and, after it, one `.min`
/// marker: `lib/vendor.min.js` → `lib/vendor`. `.min` is an output and URL
/// suffix, never part of a module name or asset key, so a minified source
/// names the same module as its plain spelling would.
fn source_stem(rel: &Path) -> PathBuf {
    let stem = rel.with_extension("");
    match stem
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".min"))
    {
        Some(plain) if !plain.is_empty() => stem.with_file_name(plain),
        _ => stem,
    }
}

/// Whether `path`'s file name carries the `.min` marker before its
/// extension (`vendor.min.js`).
fn is_min_source(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with(".min") && stem.len() > 4)
}

/// The refusal for a module name whose last segment is `min`: `.min` is
/// the output and URL suffix, so `name.min.js` could never request it.
fn min_module_name_error(name: &str, path: &Path, declared: bool) -> anyhow::Error {
    let fix = if declared {
        "declare it without `.min`"
    } else {
        "rename the file or declare an @module name"
    };
    anyhow::anyhow!(
        "module name {:?} in {} ends in `.min` — `.min` is the output and URL suffix, never part of a module name; {fix}",
        name,
        path.display()
    )
}

/// The refusal for a style whose path-derived key still ends in `.min`
/// after the stem dropped one: a style's key comes from its path, so no
/// `@module` declaration changes it.
fn min_style_key_error(key: &str, path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "style asset key {:?} from {} ends in `.min` — `.min` is the output and URL suffix, never part of a key or module name; rename the file",
        key,
        path.display()
    )
}

/// The refusal for a dependency named `X.min`: no module can carry that
/// name, so the requirement always means `X`.
fn min_requirement_error(dep: &str, path: &Path) -> anyhow::Error {
    let plain = dep.strip_suffix(".min").unwrap_or(dep);
    anyhow::anyhow!(
        "{} requires {:?} — `.min` is the output and URL suffix, never part of a module name; require `{}`",
        path.display(),
        dep,
        plain
    )
}

/// The refusal for two sources that name one module because one of them is
/// the other's `.min` spelling: neither can be picked silently.
fn min_twin_error(module: &str, first: &Path, second: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} and {} both name module {:?} — `.min` is not part of a module name, so a minified copy beside its source is the same module; remove one",
        first.display(),
        second.display(),
        module
    )
}

/// The map holds one entry per name, and the same name can be declared by a
/// script and by a stylesheet. Keep the entry a consumer cannot reconstruct: a
/// key that follows the convention is derivable from the name alone, while a
/// stylesheet declaring a name unrelated to its path is not, and losing that
/// entry would make the asset unreachable by name. Type-aware resolution
/// (Manifest::asset_key_for_module) falls back to the convention key, so the
/// entry that is dropped here is still resolvable.
fn record_module(manifest: &mut Manifest, module: &str, key: &str, asset_type: &str) {
    let conventional = format!("{}/{}.{}", asset_type, module.replace('.', "/"), asset_type);
    let derivable = key == conventional;
    match manifest.modules.get(module) {
        Some(existing) if existing == key => {}
        Some(_) if derivable => {}
        _ => {
            manifest.modules.insert(module.to_string(), key.to_string());
        }
    }
}

/// Watch for file changes and rebuild. One owner manages both configured
/// roots and exact Sass dependencies reached outside them; neither creates
/// a new public artifact, event protocol, or independent watcher.
pub fn watch(config: Config) -> Result<()> {
    watch_with(config, WatchOptions::default())
}

/// How [`watch_with`] recovers from a file watcher backend thread that died
/// (see [`install_watch_backend_monitor`]).
#[derive(Default)]
pub struct WatchOptions {
    /// Replace the whole process instead of only the watcher: the dead
    /// thread's registrations keep one descriptor per watched file open,
    /// and only a fresh process image releases them. Called when the
    /// backend dies; it returns only if it did not restart, with the
    /// reason, and the watcher is then replaced in place. The library never
    /// restarts a process on its own — that is the program's decision.
    pub restart_process: Option<Box<dyn Fn() -> String>>,
}

/// [`watch`] with recovery options.
pub fn watch_with(config: Config, options: WatchOptions) -> Result<()> {
    let (tx, rx) = channel();
    let watcher: RecommendedWatcher = Watcher::new(
        tx.clone(),
        notify::Config::default().with_poll_interval(Duration::from_secs(1)),
    )?;
    let registration = if WatchSet::polls_directories() {
        // The kqueue backend's event thread can die on a queued event for a
        // registration it already retired (notify 6.1.1 over kqueue 1.1.1
        // unwraps it); the long-lived sender below keeps the channel open,
        // so the loop would otherwise wait forever on a dead watcher. The
        // monitor that notices is the program's to install.
        Some(register_watch_wake(tx.clone()))
    } else {
        None
    };
    let mut watches = WatchSet {
        watcher,
        polled: BTreeMap::new(),
        event_tx: WatchSet::polls_directories().then_some(tx),
        physical: BTreeMap::new(),
        bound: BTreeMap::new(),
        restart_process: options.restart_process,
        died: registration.as_ref().map(|r| r.died.clone()),
    };
    let roots: Vec<PathBuf> = config
        .source_dirs()
        .into_iter()
        .chain(config.include_paths.iter().cloned())
        .collect();

    println!("Watching for changes in:");
    for dir in &roots {
        println!("  {:?}", dir);
    }
    println!("Press Ctrl+C to stop");

    let mut consumed_external = BTreeSet::new();
    let mut provisional_external = BTreeSet::new();
    // Observe roots before the first build, so missing roots and inputs
    // edited during compilation have a repair path from the outset.
    update_watch_registrations(
        &mut watches,
        &roots,
        &BTreeSet::new(),
        false,
        &config.base_dir,
    )?;
    for root in &roots {
        if !root.exists() {
            eprintln!(
                "Note: root {:?} does not exist — watching its ancestor",
                root
            );
        }
    }

    let mut graph = AssetGraph::new(config.clone());
    let mut build_ok = false;
    // The manifest's directory as each build found it, bookkeeping aside:
    // publishing there shows up on some backends (kqueue) as an event on
    // the directory alone, which must not start another build.
    let manifest_dir = crate::dirfd::parent_dir(&config.manifest_path).to_path_buf();
    let mut manifest_dir_listing = bookkeeping_free_listing(&config, &manifest_dir);
    match graph.build() {
        Ok(_) => {
            build_ok = true;
            consumed_external = external_build_inputs(&graph, &config, &roots);
            println!("Initial build complete");
        }
        Err(error) => {
            provisional_external.extend(external_attempted_inputs(&graph, &config, &roots));
            let inputs = watched_external_inputs(&consumed_external, &provisional_external);
            update_watch_registrations(&mut watches, &roots, &inputs, false, &config.base_dir)?;
            if !is_recoverable_watch_failure(&error, &roots, &inputs, &watches.physical) {
                return Err(
                    error.context("initial build cannot recover from observed source changes")
                );
            }
            eprintln!("Initial build waiting for a watched input repair: {error:#}");
        }
    }
    let mut inputs = watched_external_inputs(&consumed_external, &provisional_external);
    update_watch_registrations(&mut watches, &roots, &inputs, false, &config.base_dir)?;
    // Sass imports discovered during compilation were not necessarily
    // observed *during* that build. Close the publication-to-registration
    // gap by rechecking their snapshots after the watches are installed;
    // a missed edit becomes an immediate synthetic rescan.
    let mut pending_rescan = external_inputs_changed_since_build(&graph, &consumed_external)
        || failed_external_attempts_changed_since_build(&graph, &config, &roots);

    let debounce_duration = Duration::from_millis(200);
    let mut last_poll = std::time::Instant::now();
    loop {
        let mut batch = WatchBatch::default();
        if pending_rescan {
            batch.rescan = true;
            pending_rescan = false;
        } else if watches.polled.is_empty() {
            let message = next_watch_message(&rx)?;
            accumulate_with_inputs(&mut batch, &config, &roots, &inputs, message);
        } else {
            // Polled directories are checked once a second whether or not
            // native events keep arriving — steady traffic elsewhere must
            // not starve a root replacement only the poll can see.
            loop {
                let wait = DIRECTORY_POLL_INTERVAL.saturating_sub(last_poll.elapsed());
                let received = match rx.recv_timeout(wait) {
                    Ok(message) => {
                        accumulate_with_inputs(&mut batch, &config, &roots, &inputs, message);
                        true
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        anyhow::bail!("file watcher notification channel disconnected")
                    }
                };
                let mut polled_any = false;
                if last_poll.elapsed() >= DIRECTORY_POLL_INTERVAL {
                    last_poll = std::time::Instant::now();
                    for event in watches.poll_changes() {
                        polled_any = true;
                        accumulate_with_inputs(&mut batch, &config, &roots, &inputs, Ok(event));
                    }
                }
                if received || polled_any {
                    break;
                }
            }
        }
        loop {
            match rx.recv_timeout(debounce_duration) {
                Ok(message) => {
                    accumulate_with_inputs(&mut batch, &config, &roots, &inputs, message)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("file watcher notification channel disconnected during debounce")
                }
            }
        }
        for error in &batch.errors {
            eprintln!("Watch error: {}", error);
        }
        confirm_ambiguous_external_entry(
            &mut batch,
            &graph,
            &config,
            &roots,
            &consumed_external,
            build_ok,
        );
        drop_bookkeeping_directory_event(&mut batch, &config, &manifest_dir, &manifest_dir_listing);
        // A directory from a missing-import chain may now exist. Update
        // the observation point before building, without broadening the
        // event filter to unrelated files beneath that directory.
        update_watch_registrations(
            &mut watches,
            &roots,
            &inputs,
            batch.rescan,
            &config.base_dir,
        )?;

        let mut build_succeeded = false;
        if batch.rescan || batch.rebuild {
            if batch.paths.is_empty() {
                if batch.rescan {
                    println!("\nRescan requested — rebuilding");
                } else {
                    println!("\nExternal Sass input changed — rebuilding");
                }
            } else {
                println!("\nFile changed: {:?}", batch.paths);
            }
            manifest_dir_listing = bookkeeping_free_listing(&config, &manifest_dir);
            match graph.build() {
                Ok(_) => {
                    build_ok = true;
                    consumed_external = external_build_inputs(&graph, &config, &roots);
                    provisional_external.clear();
                    build_succeeded = true;
                    println!("Rebuild complete");
                }
                Err(error) => {
                    build_ok = false;
                    // Preserve last successful dependencies, but replace
                    // provisional candidates with this failed attempt's
                    // observations. A formerly missing import no longer
                    // referenced by the edited source needs no watch.
                    provisional_external = external_attempted_inputs(&graph, &config, &roots);
                    let next = watched_external_inputs(&consumed_external, &provisional_external);
                    update_watch_registrations(
                        &mut watches,
                        &roots,
                        &next,
                        false,
                        &config.base_dir,
                    )?;
                    if !is_recoverable_watch_failure(&error, &roots, &next, &watches.physical) {
                        return Err(
                            error.context("rebuild cannot recover from observed source changes")
                        );
                    }
                    eprintln!("Build waiting for a watched input repair: {error:#}");
                }
            }
        }
        // The authoritative graph and its derived watches move together
        // after success; on failure only additions survive until repair.
        let next = watched_external_inputs(&consumed_external, &provisional_external);
        update_watch_registrations(&mut watches, &roots, &next, false, &config.base_dir)?;
        // A successful replacement drops obsolete import filters; a
        // failure retains both the prior good and provisional inputs.
        inputs = next;
        if build_succeeded {
            pending_rescan = captured_inputs_changed_since_build(&graph, &config);
        } else if batch.rescan || batch.rebuild {
            pending_rescan = failed_external_attempts_changed_since_build(&graph, &config, &roots);
        }
    }
}

/// Root and include-tree sources share the publication-to-registration
/// gap external Sass imports have: a write that lands after the build's
/// read but before the directory's (re)registration is an event nobody
/// saw — under a replaced root link the whole tree re-arms on a new
/// target and that window is wide. Re-compare every captured input once
/// the registrations settle; the external subset is already inside it.
fn captured_inputs_changed_since_build(graph: &AssetGraph, config: &Config) -> bool {
    let mut inputs: BTreeSet<PathBuf> = graph
        .captured_inputs
        .borrow()
        .keys()
        .filter(|path| !is_generated_watch_path(config, path))
        .cloned()
        .collect();
    inputs.extend(graph.captured_symlink_targets.borrow().keys().cloned());
    external_inputs_changed_since_build(graph, &inputs)
}

/// Content comparison after watches are registered closes the only gap
/// where an out-of-root Sass import can change after publication but
/// before its newly discovered directory is being observed. Once watched,
/// later edits are delivered as ordinary filesystem notifications.
fn external_inputs_changed_since_build(graph: &AssetGraph, inputs: &BTreeSet<PathBuf>) -> bool {
    let captured = graph.captured_inputs.borrow();
    let links = graph.captured_symlink_targets.borrow();
    inputs.iter().any(|path| {
        if let Some(bytes) = captured.get(path) {
            fs::read(path).map_or(true, |now| now != *bytes)
        } else {
            // A directory symlink is an input by identity, not readable
            // file bytes. It belongs to the symlink-target observation.
            !links.contains_key(path)
        }
    }) || links.iter().any(|(alias, target)| {
        inputs.contains(alias) && fs::canonicalize(alias).map_or(true, |current| current != *target)
    })
}

/// Failed Sass compilation can discover imports outside the watched roots.
/// Files repaired after compilation but before provisional watches are
/// installed may not produce an event. Compare failed reads byte-for-byte,
/// and retry when a previously failed filesystem operation now succeeds.
/// A still-missing input or a still-failing probe never causes polling.
fn failed_external_attempts_changed_since_build(
    graph: &AssetGraph,
    config: &Config,
    roots: &[PathBuf],
) -> bool {
    let failed = graph.failed_sass_snapshot.borrow();
    let captured = graph.captured_inputs.borrow();
    let attempted = external_attempted_inputs(graph, config, roots);
    attempted.iter().any(|path| {
        let failed_bytes = failed.reads.get(path);
        let captured_bytes = captured.get(path);
        if failed_bytes.is_none() && captured_bytes.is_none() {
            return false;
        }
        // A shared dependency may have been read under two different
        // generations before the build rejected it. Checking only the
        // latest failed read would suppress recovery when disk still
        // matches that read but disagrees with an earlier capture.
        fs::read(path).map_or(true, |current| {
            failed_bytes.is_some_and(|bytes| current != *bytes)
                || captured_bytes.is_some_and(|bytes| current != *bytes)
        })
    }) || failed.symlink_targets.iter().any(|(alias, target)| {
        attempted.contains(alias)
            && fs::canonicalize(alias).map_or(true, |current| current != *target)
    }) || graph
        .captured_symlink_targets
        .borrow()
        .iter()
        .any(|(alias, target)| {
            attempted.contains(alias)
                && fs::canonicalize(alias).map_or(true, |current| current != *target)
        })
        || failed.negative_probes.iter().any(|probe| {
            !within_declared_root(roots, &probe.path)
                && !is_generated_watch_path(config, &probe.path)
                && probe.now_succeeds()
        })
}

fn next_watch_message(
    rx: &std::sync::mpsc::Receiver<std::result::Result<notify::Event, notify::Error>>,
) -> Result<std::result::Result<notify::Event, notify::Error>> {
    rx.recv()
        .context("file watcher notification channel disconnected")
}

/// Only an error explicitly wrapped by a source-reading/parsing or
/// missing-contributor operation is a recoverable watch failure. A log
/// string that happens to mention a source is not sufficient.
fn is_recoverable_watch_failure(
    error: &anyhow::Error,
    roots: &[PathBuf],
    extra: &BTreeSet<PathBuf>,
    physical: &BTreeMap<PathBuf, RecursiveMode>,
) -> bool {
    // The tag is attached through `.context`, so it must be recovered by
    // downcasting the error itself — anyhow resolves context layers at
    // any depth — never by downcasting `chain()` items, which hide
    // context types behind anyhow's own wrapper.
    let Some(RecoverableWatchInput(path)) = error.downcast_ref::<RecoverableWatchInput>() else {
        return false;
    };
    let physical_path = observed_input_path(path);
    // Captured inputs carry canonical paths while roots keep their spelled
    // form (`/tmp/…` vs `/private/tmp/…`): compare both in observed form.
    let logical = roots.iter().any(|root| path.starts_with(root))
        || within_declared_root(roots, path)
        || extra.contains(&physical_path);
    logical
        && physical.iter().any(|(target, _mode)| {
            // Recursive watches cover sources directly. A missing
            // path watched from its existing ancestor recovers one
            // component at a time as registrations are reconciled.
            physical_path.starts_with(target) || path.starts_with(target)
        })
}

fn watched_external_inputs(
    consumed: &BTreeSet<PathBuf>,
    missing: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
    consumed.union(missing).cloned().collect()
}

/// The manifest contains no watch metadata; these are private observations
/// from the compiler's most recent successful and failed read attempts.
fn external_build_inputs(
    graph: &AssetGraph,
    config: &Config,
    roots: &[PathBuf],
) -> BTreeSet<PathBuf> {
    let mut inputs: BTreeSet<PathBuf> = graph.captured_inputs.borrow().keys().cloned().collect();
    inputs.extend(graph.captured_symlink_targets.borrow().keys().cloned());
    inputs
        .into_iter()
        .filter(|path| !within_declared_root(roots, path) && !is_generated_watch_path(config, path))
        .collect()
}

fn external_attempted_inputs(
    graph: &AssetGraph,
    config: &Config,
    roots: &[PathBuf],
) -> BTreeSet<PathBuf> {
    graph
        .attempted_sass_inputs
        .borrow()
        .iter()
        .filter(|path| !within_declared_root(roots, path) && !is_generated_watch_path(config, path))
        .cloned()
        .collect()
}

/// Captured and attempted inputs are stored canonicalized; configured
/// roots are not — a lexical `starts_with` splits one real directory
/// into inside/outside whenever a root travels through a symlink
/// (every `tempdir` on macOS). Compare both sides in observed form.
fn within_declared_root(roots: &[PathBuf], path: &Path) -> bool {
    let observed = if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        observed_entry_path(path)
    } else {
        observed_input_path(path)
    };
    roots
        .iter()
        .any(|root| observed.starts_with(observed_input_path(root)))
}

/// Output paths arrive both as raw event paths and as canonicalized
/// captured inputs; compare in observed form so a symlinked prefix
/// cannot smuggle a generated file into the input set. The build's own
/// bookkeeping is generated too wherever it sits — the manifest, its
/// `.bin` cache, the project's `manifest.lock`, and any `.paste-staged.` or
/// `.paste-pending.` file — so a manifest kept inside a watched root
/// cannot trigger a rebuild by being published.
fn is_generated_watch_path(config: &Config, path: &Path) -> bool {
    let observed = observed_input_path(path);
    let bookkeeping = [
        config.manifest_path.clone(),
        config.manifest_path.with_extension("bin"),
        config.base_dir.join("manifest.lock"),
    ];
    [&config.output.base, &config.output.js, &config.output.css]
        .iter()
        .any(|dir| observed.starts_with(observed_input_path(dir)))
        || path
            .components()
            .any(|c| c.as_os_str() == config.output.build_dir_name.as_str())
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(".paste-staged.") || name.starts_with(".paste-pending.")
            })
        || bookkeeping
            .iter()
            .any(|file| path == file || observed == observed_input_path(file))
}

/// One external Sass import watches its containing directory for
/// replacement/creation; existing imported files also receive exact-file
/// registrations to observe in-place writes on file-oriented backends.
/// A missing import is followed from its nearest existing parent directory.
fn supplemental_watch(path: &Path) -> Option<(PathBuf, RecursiveMode)> {
    path.parent()
        .and_then(|parent| parent.ancestors().find(|ancestor| ancestor.is_dir()))
        .map(|parent| (parent.to_path_buf(), RecursiveMode::NonRecursive))
}

/// The identity a notification backend binds to when a watch registers:
/// every symlink on the path resolves at that moment, so the same
/// spelled target can name a different directory after a retarget.
/// Registrations keyed on this identity become stale entries the
/// reconcile can see instead of an unchanged `(path, mode)` pair.
fn canonical_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The filesystem object a registration binds to — a directory replaced
/// at the same pathname is a different inode, and the backend keeps
/// reporting on the dead one while the map key stays identical. `None`
/// when the object cannot be identified right now.
#[cfg(unix)]
pub(crate) fn bound_identity(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .ok()
        .map(|m| m.ino() ^ m.dev().rotate_left(32))
}

#[cfg(windows)]
pub(crate) fn bound_identity(path: &Path) -> Option<u64> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    // A directory handle exists only through CreateFileW — `File::open`
    // refuses directories — and zero desired access still answers
    // GetFileInformationByHandle. The index identifies the object within
    // its volume; folding in the serial keeps two volumes' same-index
    // objects distinct.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | u64::from(info.nFileIndexLow);
    Some(index ^ u64::from(info.dwVolumeSerialNumber).rotate_left(32))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn bound_identity(_path: &Path) -> Option<u64> {
    None
}

fn desired_watches_with_inputs(
    roots: &[PathBuf],
    extras: &BTreeSet<PathBuf>,
    base: &Path,
) -> BTreeMap<PathBuf, RecursiveMode> {
    let mut desired = desired_watches(roots, base);
    for input in extras {
        if within_declared_root(roots, input)
            || desired.iter().any(|(target, mode)| {
                *mode == RecursiveMode::Recursive && input.starts_with(target)
            })
        {
            continue;
        }
        if let Some((target, mode)) = supplemental_watch(input) {
            desired.entry(canonical_identity(&target)).or_insert(mode);
        }
        // On kqueue a nonrecursive directory watch observes directory
        // entries, not writes to the files within it. Observe an existing
        // import as a file as well, without traversing every sibling in
        // its directory. The parent handles replacement/creation; the
        // file registration handles edits in place.
        if input.is_file() {
            desired
                .entry(canonical_identity(input))
                .or_insert(RecursiveMode::NonRecursive);
        }
    }
    desired
}

/// `unwatch` on a registration that is already gone is not a failure —
/// it is the goal state. Notify reports it differently per backend: a
/// `WatchNotFound` kind, a not-found io error (kqueue removes a walked
/// root a second time through its recursive walk), or — since kqueue
/// flattens inner errors to a generic message — the backend's own
/// "was not being watched" string.
fn is_watch_already_gone(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::WatchNotFound => true,
        // inotify answers EINVAL for a watch the kernel already retired (a
        // deleted subdirectory's descriptor) — the goal state, not a fault.
        notify::ErrorKind::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
        ),
        _ => error.to_string().contains("was not being watched"),
    }
}

/// One watcher's live registration state — the watcher, the channel its
/// registrations post events to (recreated kqueue watchers share it, so
/// the receiver never changes), the recorded physical registrations, and
/// the filesystem identity each registration was bound to.
struct WatchSet {
    watcher: RecommendedWatcher,
    /// Non-recursive DIRECTORY targets (a root's parent, a missing root's
    /// ancestor, an external import's directory) on the kqueue backend,
    /// with the entry snapshot each was last checked against. kqueue turns
    /// a NonRecursive directory watch recursive on the first write — one fd
    /// per file of whatever sibling tree it reaches — so these directories
    /// are compared by entry identity from the watch loop instead. Always
    /// empty on other backends.
    polled: BTreeMap<PathBuf, DirEntries>,
    /// A sender for recreating the watcher — kept only on kqueue, the one
    /// backend recreated in place. Elsewhere no long-lived sender is held,
    /// so a backend thread that dies disconnects the channel and the loop
    /// reports it instead of waiting forever.
    event_tx: Option<Sender<std::result::Result<notify::Event, notify::Error>>>,
    physical: BTreeMap<PathBuf, RecursiveMode>,
    bound: BTreeMap<PathBuf, u64>,
    /// The program's process restart for a dead backend, if it gave one.
    restart_process: Option<Box<dyn Fn() -> String>>,
    /// Set by the backend monitor when a backend thread died.
    died: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// One registered watch: the flag its loop checks, and its loop's sender.
type WatchMonitorSlot = (
    u64,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    Sender<std::result::Result<notify::Event, notify::Error>>,
);

/// Every watch running in this process. A dying notify thread cannot say
/// which watcher it served — every backend thread has the same name — so
/// the monitor flags and wakes them all; a healthy watcher restarted this
/// way costs one rescan.
static WATCH_MONITORS: std::sync::Mutex<Vec<WatchMonitorSlot>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// Set around a drop whose panic is expected and caught.
    static QUIET_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A watch's place in [`WATCH_MONITORS`], released when the watch ends.
struct WatchRegistration {
    id: u64,
    died: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for WatchRegistration {
    fn drop(&mut self) {
        if let Ok(mut slots) = WATCH_MONITORS.lock() {
            slots.retain(|(id, _, _)| *id != self.id);
        }
    }
}

/// Register a watch loop's sender with the backend monitor.
fn register_watch_wake(
    tx: Sender<std::result::Result<notify::Event, notify::Error>>,
) -> WatchRegistration {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let died = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Ok(mut slots) = WATCH_MONITORS.lock() {
        slots.push((id, died.clone(), tx));
    }
    WatchRegistration { id, died }
}

/// Install the file watcher's backend monitor: a process-wide panic hook
/// that reports a panic on a notify backend thread (`notify-rs …`) to the
/// watch loop — it sets the restart flag, wakes a loop blocked on its
/// channel, and prints one line naming where the thread failed. Any other
/// panic goes to the previous hook unchanged. Without it a dead kqueue
/// backend thread goes unnoticed (the loop's own sender keeps the channel
/// open); with it [`watch`] replaces the dead watcher. A panic hook belongs
/// to the program, so the library never installs it itself: the CLI does,
/// and an embedding host may. Installed once per process.
pub fn install_watch_backend_monitor() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("notify-rs"))
            {
                if let Ok(slots) = WATCH_MONITORS.lock() {
                    for (_, died, tx) in slots.iter() {
                        died.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = tx.send(Err(notify::Error::generic(
                            "the file watcher's event thread stopped",
                        )));
                    }
                }
                let at = info
                    .location()
                    .map(|l| format!("{}:{}", l.file(), l.line()))
                    .unwrap_or_default();
                eprintln!("Watch: the file watcher's event thread panicked at {at}");
                return;
            }
            if QUIET_PANIC.with(|quiet| quiet.get()) {
                return;
            }
            previous(info);
        }));
    });
}

/// How often polled directories are compared while watching.
const DIRECTORY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One directory's entries by name: each entry's file identity and, for a
/// symlink, its target — what a create, remove, rename, replacement, or
/// link retarget changes. `None` when the directory itself is unreadable.
type DirEntries = Option<BTreeMap<std::ffi::OsString, (Option<u64>, Option<PathBuf>)>>;

fn dir_entries(dir: &Path) -> DirEntries {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let link = fs::read_link(&path).ok();
        let identity = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                fs::symlink_metadata(&path)
                    .ok()
                    .map(|m| m.ino() ^ m.dev().rotate_left(32))
            }
            #[cfg(not(unix))]
            {
                None
            }
        };
        entries.insert(entry.file_name(), (identity, link));
    }
    Some(entries)
}

impl WatchSet {
    /// Replace a watcher whose event thread died with a fresh one on the
    /// same channel. Every registration is dropped; the next
    /// `update_watch_registrations` binds the desired set again, and the
    /// caller rescans for whatever changed while nothing was watching.
    fn restart(&mut self) -> Result<()> {
        let Some(event_tx) = self.event_tx.clone() else {
            anyhow::bail!("the file watcher stopped and this backend cannot be restarted");
        };
        let fresh: RecommendedWatcher = Watcher::new(
            event_tx,
            notify::Config::default().with_poll_interval(Duration::from_secs(1)),
        )?;
        // Dropping the dead watcher messages its gone thread and unwraps
        // the failure, so the drop runs with that one expected panic caught
        // and kept quiet. The dead thread's registrations are not released
        // by it: each in-place replacement leaves their descriptors open
        // (one per watched file), which is why the CLI restarts the process
        // instead whenever it can.
        let dead = std::mem::replace(&mut self.watcher, fresh);
        QUIET_PANIC.with(|quiet| quiet.set(true));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(dead)));
        QUIET_PANIC.with(|quiet| quiet.set(false));
        self.polled.clear();
        self.physical.clear();
        self.bound.clear();
        Ok(())
    }

    fn polls_directories() -> bool {
        <RecommendedWatcher as Watcher>::kind() == notify::WatcherKind::Kqueue
    }

    /// Register `target`: a non-recursive directory on kqueue is polled by
    /// entry identity; files and recursive roots stay native (a kqueue file
    /// watch is one descriptor and never expands).
    fn watch(&mut self, target: &Path, mode: RecursiveMode) -> notify::Result<()> {
        if Self::polls_directories() && mode == RecursiveMode::NonRecursive && target.is_dir() {
            self.polled
                .insert(target.to_path_buf(), dir_entries(target));
            return Ok(());
        }
        self.watcher.watch(target, mode)
    }

    fn unwatch(&mut self, target: &Path) -> notify::Result<()> {
        if self.polled.remove(target).is_some() {
            return Ok(());
        }
        self.watcher.unwatch(target)
    }

    /// The entry changes in polled directories since the last check, as
    /// the events a native backend would have reported for them.
    fn poll_changes(&mut self) -> Vec<notify::Event> {
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
        let mut events = Vec::new();
        for (dir, recorded) in self.polled.iter_mut() {
            let current = dir_entries(dir);
            if current == *recorded {
                continue;
            }
            let empty = BTreeMap::new();
            let before = recorded.as_ref().unwrap_or(&empty);
            let after = current.as_ref().unwrap_or(&empty);
            for (name, state) in after {
                let kind = match before.get(name) {
                    None => notify::EventKind::Create(CreateKind::Any),
                    Some(old) if old != state => {
                        notify::EventKind::Modify(ModifyKind::Name(RenameMode::Any))
                    }
                    Some(_) => continue,
                };
                events.push(notify::Event::new(kind).add_path(dir.join(name)));
            }
            for name in before.keys().filter(|name| !after.contains_key(*name)) {
                events.push(
                    notify::Event::new(notify::EventKind::Remove(RemoveKind::Any))
                        .add_path(dir.join(name)),
                );
            }
            if recorded.is_some() != current.is_some() {
                let kind = if current.is_some() {
                    notify::EventKind::Create(CreateKind::Folder)
                } else {
                    notify::EventKind::Remove(RemoveKind::Folder)
                };
                events.push(notify::Event::new(kind).add_path(dir.clone()));
            }
            *recorded = current;
        }
        events
    }
}

/// Restart the watcher when its backend thread died, before anything asks
/// the dead one to watch or unwatch. The program's process restart runs
/// first when it gave one (it returns only when it did not restart);
/// otherwise, or then, the watcher is replaced in process. The wake-up
/// error the monitor sent is still queued, so the next batch rescans.
fn restart_dead_watcher(set: &mut WatchSet) -> Result<bool> {
    if !set
        .died
        .as_ref()
        .is_some_and(|died| died.swap(false, std::sync::atomic::Ordering::SeqCst))
    {
        return Ok(false);
    }
    eprintln!("The file watcher's event thread stopped — restarting it and rescanning every root");
    if let Some(restart) = &set.restart_process {
        let reason = restart();
        eprintln!("Watch: not restarting the process ({reason}); replacing the watcher in place");
    }
    set.restart()?;
    Ok(true)
}

fn update_watch_registrations(
    set: &mut WatchSet,
    roots: &[PathBuf],
    extra: &BTreeSet<PathBuf>,
    rescan: bool,
    base: &Path,
) -> Result<()> {
    let restarted = restart_dead_watcher(set)?;
    match apply_watch_registrations(set, roots, extra, rescan || restarted, base) {
        // The thread can also die while this call talks to it: restart
        // and apply again once.
        Err(_) if restart_dead_watcher(set)? => {
            apply_watch_registrations(set, roots, extra, true, base)
        }
        result => result,
    }
}

fn apply_watch_registrations(
    set: &mut WatchSet,
    roots: &[PathBuf],
    extra: &BTreeSet<PathBuf>,
    rescan: bool,
    base: &Path,
) -> Result<()> {
    let desired = desired_watches_with_inputs(roots, extra, base);
    let (mut drop, mut add) = reconcile_watches(&desired, &set.physical, rescan);
    // A pathname reused by a replacement directory keeps its map entry —
    // same key, same mode — while the registration stays bound to the
    // dead filesystem object. The recorded identity forces re-registration
    // where the live object no longer matches the one `.watch` pinned.
    for (target, mode) in set.physical.iter() {
        if desired.get(target) != Some(mode) {
            continue;
        }
        let live = bound_identity(target);
        match (set.bound.get(target), live) {
            (Some(&recorded), Some(current)) if current != recorded => {
                drop.push(target.clone());
                add.push((target.clone(), *mode));
            }
            _ => {}
        }
    }
    // The kqueue backend reports and auto-retires registrations on vnode
    // delete/rename itself, and its remove path is unsafe under churn:
    // it can close one fd while queued events still name it, which the
    // event loop dereferences unconditionally. Instead of unwatching one
    // by one, the watcher is recreated from exactly the desired set —
    // stale registrations disappear with the old watcher, and `physical`
    // records only what is actually bound.
    if let (true, Some(event_tx)) = (
        <RecommendedWatcher as Watcher>::kind() == notify::WatcherKind::Kqueue && !drop.is_empty(),
        set.event_tx.clone(),
    ) {
        // The fresh watcher shares the same channel — the receiver never
        // changes, queued events from the retiring watcher still land,
        // and no send ever hits a dropped receiver.
        let fresh: RecommendedWatcher = Watcher::new(
            event_tx,
            notify::Config::default().with_poll_interval(Duration::from_secs(1)),
        )?;
        set.watcher = fresh;
        set.polled.clear();
        set.physical.clear();
        set.bound.clear();
        for (target, mode) in &desired {
            set.watch(target, *mode).with_context(|| {
                format!("could not register file watch on {}", target.display())
            })?;
            set.physical.insert(target.clone(), *mode);
            if let Some(identity) = bound_identity(target) {
                set.bound.insert(target.clone(), identity);
            }
        }
        return Ok(());
    }
    for target in drop {
        match set.unwatch(&target) {
            Ok(()) => {}
            // A native watcher may have dropped the registration on
            // deletion or overflow. The tracked logical entry must be
            // released even if the physical watch is already gone.
            Err(error) if is_watch_already_gone(&error) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not remove file watch on {}", target.display())
                });
            }
        }
        set.physical.remove(&target);
        set.bound.remove(&target);
    }
    for (target, mode) in add {
        set.watch(&target, mode)
            .with_context(|| format!("could not register file watch on {}", target.display()))?;
        if let Some(identity) = bound_identity(&target) {
            set.bound.insert(target.clone(), identity);
        }
        set.physical.insert(target, mode);
    }
    Ok(())
}

/// The physical watch a configured root currently requires: the root
/// itself recursively while it exists; while it does not, its nearest
/// existing ancestor NON-recursively — the ancestor's only job is to
/// announce the next path component's appearance, so its whole subtree
/// is not a watch target. `None` when no ancestor exists either. The
/// registration names the resolved target: a backend binds whatever the
/// spelled path resolves to at watch time, so a retargeted symlink must
/// surface as a different registration, not an unchanged pathname.
fn desired_watch(root: &Path) -> Option<(PathBuf, RecursiveMode)> {
    if root.exists() {
        Some((canonical_identity(root), RecursiveMode::Recursive))
    } else {
        root.ancestors()
            .skip(1)
            .find(|ancestor| ancestor.exists())
            .map(|ancestor| (canonical_identity(ancestor), RecursiveMode::NonRecursive))
    }
}

/// The physical spellings a missing root may materialize as: for every
/// symlink component on its chain, the referent with the remaining
/// suffix appended. A dangling `project/src -> /outside/target`
/// appears under `/outside`'s events, not `project`'s — the referent
/// spellings are how the watcher both observes the ancestor that can
/// create the chain and scopes the events it reports.
fn missing_root_spellings(root: &Path) -> Vec<PathBuf> {
    root.ancestors()
        .filter_map(|link| {
            let (parent, meta, target) = (
                link.parent()?,
                fs::symlink_metadata(link).ok()?,
                fs::read_link(link).ok()?,
            );
            if !meta.file_type().is_symlink() {
                return None;
            }
            let referent = if target.is_absolute() {
                target
            } else {
                parent.join(target)
            };
            Some(collapse_parents(&referent.join(
                root.strip_prefix(link).unwrap_or_else(|_| Path::new("")),
            )))
        })
        .collect()
}

/// Collapse `.` and `..` lexically — a referent spelling is built by
/// joining a link target to the link's parent, and the `..` the target
/// carried must resolve before any prefix comparison sees the real
/// chain. No filesystem calls: a referent may not exist yet.
fn collapse_parents(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The complete physical watch set `roots` require right now: one entry
/// per resolved target, deduplicated — two missing roots under the same
/// ancestor share its one registration, and a target Recursive mode was
/// already claimed for stays Recursive (a non-recursive need is covered
/// by it). Every symlink component on a root's own chain is additionally
/// observed through its parent: retargeting the link only replaces an
/// entry in that directory, and without the parent's report the stale
/// physical registration is never re-examined. Links that are proper
/// ancestors of `base` are path plumbing — retargeting one relocates the
/// spelled project itself, not one watched tree.
fn desired_watches(roots: &[PathBuf], base: &Path) -> BTreeMap<PathBuf, RecursiveMode> {
    let mut desired = BTreeMap::new();
    for root in roots {
        if let Some((target, mode)) = desired_watch(root) {
            desired
                .entry(target)
                .and_modify(|held| {
                    if mode == RecursiveMode::Recursive {
                        *held = RecursiveMode::Recursive;
                    }
                })
                .or_insert(mode);
        }
        // The root's own parent observes the name being replaced: a
        // directory swapped at the same pathname produces nothing inside
        // the dead registration — the parent entry is the only reporter
        // that can announce the swap.
        if let Some(parent) = root.parent().filter(|p| p.is_dir()) {
            desired
                .entry(canonical_identity(parent))
                .or_insert(RecursiveMode::NonRecursive);
        }
        // A missing root spelling that resolves through a dangling link
        // materializes at the REFERENT, not under this spelling — its
        // creation lands in the referent's ancestor's events. Watch that
        // ancestor too; as directories appear the next reconciliation
        // advances the registration down the chain.
        if !root.exists() {
            for spelling in missing_root_spellings(root) {
                if let Some(ancestor) = spelling.ancestors().skip(1).find(|a| a.exists()) {
                    desired
                        .entry(canonical_identity(ancestor))
                        .or_insert(RecursiveMode::NonRecursive);
                }
            }
        }
        for parent in root
            .ancestors()
            .filter(|alias| *alias == base || !base.starts_with(alias))
            .filter(|alias| {
                fs::symlink_metadata(alias).is_ok_and(|meta| meta.file_type().is_symlink())
            })
            .filter_map(|alias| alias.parent())
        {
            desired
                .entry(canonical_identity(parent))
                .or_insert(RecursiveMode::NonRecursive);
        }
    }
    desired
}

/// The unwatch/watch calls that bring the live `physical` set to
/// `desired`: first the targets to release (absent entirely or present
/// under a different mode), then the targets to register. Applying drops
/// before adds means a mode change is a clean re-registration. `refresh`
/// replaces every desired registration after a rescan. The old one
/// may still exist and must not be blindly registered a second time.
/// Missing physical watches are ignored when releasing them.
fn reconcile_watches(
    desired: &BTreeMap<PathBuf, RecursiveMode>,
    physical: &BTreeMap<PathBuf, RecursiveMode>,
    refresh: bool,
) -> (Vec<PathBuf>, Vec<(PathBuf, RecursiveMode)>) {
    let drop = physical
        .iter()
        .filter(|(target, mode)| refresh || desired.get(*target) != Some(*mode))
        .map(|(target, _)| target.clone())
        .collect();
    let add = desired
        .iter()
        .filter(|(target, mode)| refresh || physical.get(*target) != Some(*mode))
        .map(|(target, mode)| (target.clone(), *mode))
        .collect();
    (drop, add)
}

/// What one debounce batch accumulates across every message it holds:
/// whether a rebuild is owed, whether the backend asked for a rescan,
/// every distinct watcher error, and the deduplicated paths that earned
/// the rebuild line.
#[derive(Default)]
struct WatchBatch {
    rebuild: bool,
    rescan: bool,
    // Some backends report a directory-entry change against a sibling
    // path instead of the created/removed file. Check only our tracked
    // external Sass inputs before deciding whether a rebuild is owed.
    ambiguous_external_entry: bool,
    errors: Vec<String>,
    paths: Vec<PathBuf>,
}

#[cfg(test)]
fn accumulate(
    batch: &mut WatchBatch,
    config: &Config,
    roots: &[PathBuf],
    message: std::result::Result<notify::Event, notify::Error>,
) {
    accumulate_with_inputs(batch, config, roots, &BTreeSet::new(), message)
}

/// Fold one notify channel message into the batch. A rescan flag forces
/// a rebuild with no usable path list; every watcher error is surfaced,
/// not just the first; each event path is judged against the configured
/// scope — and against the recorded external Sass `inputs` — and recorded
/// once.
fn accumulate_with_inputs(
    batch: &mut WatchBatch,
    config: &Config,
    roots: &[PathBuf],
    extras: &BTreeSet<PathBuf>,
    message: std::result::Result<notify::Event, notify::Error>,
) {
    match message {
        Ok(event) => {
            if event.need_rescan() {
                batch.rescan = true;
            }
            for path in &event.paths {
                if !is_source_change_with_inputs(config, roots, extras, path, &event.kind) {
                    batch.ambiguous_external_entry |=
                        is_ambiguous_supplemental_entry(roots, extras, path, &event.kind);
                    continue;
                }
                batch.rebuild = true;
                if !batch.paths.contains(path) {
                    batch.paths.push(path.clone());
                }
            }
        }
        Err(e) => {
            // A watcher error means events may have been lost — the
            // backend could not deliver them. That is the rescan flag's
            // own contract, so the batch must rebuild even when the
            // error is its only content; logging alone would leave
            // changed sources unbuilt.
            batch.rescan = true;
            let error = e.to_string();
            if !batch.errors.contains(&error) {
                batch.errors.push(error);
            }
        }
    }
}

/// A directory's entries, with the build's own bookkeeping left out: name →
/// (file identity, length, modified time, is a directory). Any source
/// change in the directory — an in-place write, an atomic replace, an entry
/// added, removed or renamed — changes it; publishing the manifest beside
/// it does not.
type DirListing = BTreeMap<std::ffi::OsString, (u64, u64, Option<std::time::SystemTime>, bool)>;

fn bookkeeping_free_listing(config: &Config, dir: &Path) -> Option<DirListing> {
    let mut listing = BTreeMap::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        if is_generated_watch_path(config, &entry.path()) {
            continue;
        }
        // `DirEntry::metadata` does not follow a link.
        let meta = entry.metadata().ok()?;
        #[cfg(unix)]
        let identity = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let identity = 0;
        listing.insert(
            entry.file_name(),
            (identity, meta.len(), meta.modified().ok(), meta.is_dir()),
        );
    }
    Some(listing)
}

/// A batch whose only event is the manifest directory itself, while that
/// directory's non-bookkeeping entries are exactly as they were when the
/// last build started, came from the build's own publication: kqueue
/// reports a staged rename or `.bin` removal as a change to the directory.
/// The snapshot is taken before each build, so a source edit landing during
/// or after it still differs and still rebuilds.
fn drop_bookkeeping_directory_event(
    batch: &mut WatchBatch,
    config: &Config,
    manifest_dir: &Path,
    listing: &Option<DirListing>,
) {
    if !batch.rebuild || batch.rescan || batch.paths.is_empty() || listing.is_none() {
        return;
    }
    let observed = observed_input_path(manifest_dir);
    if !batch
        .paths
        .iter()
        .all(|path| observed_input_path(path) == observed)
    {
        return;
    }
    if bookkeeping_free_listing(config, manifest_dir) == *listing {
        batch.rebuild = false;
        batch.paths.clear();
    }
}

/// Some notification backends cannot associate a directory-entry event
/// with the actual newly created or removed child. A sibling-looking
/// entry event in a nonrecursive supplemental Sass parent is therefore
/// ambiguous, not evidence that a rebuild is needed. The watcher checks
/// the observed Sass input/probe snapshots before rebuilding.
fn is_ambiguous_supplemental_entry(
    roots: &[PathBuf],
    extras: &BTreeSet<PathBuf>,
    event_path: &Path,
    kind: &notify::EventKind,
) -> bool {
    let entry = observed_entry_path(event_path);
    // A Modify on the supplemental directory itself (a poller or kqueue
    // reporting that its entries changed) is the same ambiguity.
    if matches!(kind, notify::EventKind::Modify(_)) && event_path.is_dir() {
        let observed = observed_input_path(event_path);
        return extras.iter().any(|file| {
            !within_declared_root(roots, file)
                && supplemental_watch(file)
                    .is_some_and(|(target, _)| observed_input_path(&target) == observed)
        });
    }
    if !matches!(
        kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) {
        return false;
    }
    let Some(parent) = entry.parent() else {
        return false;
    };
    extras.iter().any(|file| {
        !within_declared_root(roots, file)
            && supplemental_watch(file)
                .is_some_and(|(target, _)| observed_input_path(&target) == parent)
    })
}

/// Avoid rebuilding for an actual unrelated sibling. Compare the
/// previously published external Sass bytes and any failed compilation
/// probes only when an ambiguous directory-entry notification occurred.
fn confirm_ambiguous_external_entry(
    batch: &mut WatchBatch,
    graph: &AssetGraph,
    config: &Config,
    roots: &[PathBuf],
    consumed_external: &BTreeSet<PathBuf>,
    build_ok: bool,
) {
    if !batch.ambiguous_external_entry || batch.rebuild || batch.rescan {
        return;
    }
    // A failed build cleared its current capture map. Do not treat a
    // missing capture of a last-successful import as evidence that an
    // unrelated sibling edit changed it; the last successful *file*
    // watcher remains registered. Failed Sass reads/probes are the
    // applicable snapshots until a complete build succeeds again.
    batch.rebuild = if build_ok {
        external_inputs_changed_since_build(graph, consumed_external)
    } else {
        failed_external_attempts_changed_since_build(graph, config, roots)
    };
}

#[cfg(test)]
fn is_source_change(
    config: &Config,
    roots: &[PathBuf],
    path: &Path,
    kind: &notify::EventKind,
) -> bool {
    is_source_change_with_inputs(config, roots, &BTreeSet::new(), path, kind)
}

/// Whether a filesystem event path is a build input for `config`. The
/// event must land inside a configured source root or include path —
/// watching the ancestor of a not-yet-existing root makes sibling
/// subtrees visible, and their `.js` files must NOT rebuild — or be an
/// ancestor step on the chain to a root that does not exist yet (the
/// create/rename that can carry the root into existence). Inside scope,
/// JS, SCSS and CSS files count; so do directory lifecycle events, since
/// a created, removed, or renamed directory can carry or delete sources
/// without touching one file. Nothing under a configured output
/// directory counts, so an output tree nested inside a source tree
/// cannot loop the watcher. A `build-dir-name` directory is generated
/// output for the same reason and is excluded in either layout. Outside
/// the roots, the recorded external Sass `extras` count by exact path or
/// by an ancestor's lifecycle event.
fn is_source_change_with_inputs(
    config: &Config,
    roots: &[PathBuf],
    extras: &BTreeSet<PathBuf>,
    path: &Path,
    kind: &notify::EventKind,
) -> bool {
    if is_generated_watch_path(config, path) {
        return false;
    }
    // Supplemental parents are watched non-recursively. Only the exact
    // consumed/candidate file, or an ancestor directory lifecycle event
    // that could remove/create it, may trigger a rebuild. An unrelated
    // sibling in the watched directory has no effect.
    // A watch registered through a symlink reports events under the
    // resolved path — a supplemental alias, a source root behind a link,
    // or the ancestor standing in for a missing root. Compare the event
    // path in observed form as well as lexically wherever scope decides.
    let observed = observed_input_path(path);
    let entry = observed_entry_path(path);
    let lifecycle = matches!(
        kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    if !extras.is_empty() {
        // A Modify on the watched directory itself names no child; it is an
        // ambiguous entry event, confirmed against the recorded inputs by
        // `is_ambiguous_supplemental_entry` rather than rebuilt blindly.
        let extras_lifecycle = lifecycle;
        if extras.iter().any(|file| {
            observed == observed_input_path(file)
                || entry == observed_entry_path(file)
                || (extras_lifecycle && (file.starts_with(&observed) || file.starts_with(&entry)))
        }) {
            return true;
        }
    }
    // Outside every configured root, an event matters only when it lands
    // on the ancestry chain of a root — the event that can create or
    // remove the path leading to it, or the directory-entry event that
    // replaces a link the root resolves through. Events inside an
    // existing root are judged by shape below; the scope check is what
    // keeps an ancestor watch's unrelated subtrees from rebuilding. A
    // root (or the ancestor standing in for a missing one) behind a
    // symlink reports under its resolved path, so both sides compare in
    // observed and entry form as well as by spelling.
    let in_scope = roots.iter().any(|root| {
        let observed_root = observed_input_path(root);
        let entry_root = observed_entry_path(root);
        path.starts_with(root)
            || observed.starts_with(&observed_root)
            || entry.starts_with(&entry_root)
            || root.starts_with(path)
            || entry_root.starts_with(&entry)
            || (!root.exists()
                && missing_root_spellings(root).iter().any(|referent| {
                    referent.starts_with(path)
                        || referent.starts_with(&observed)
                        || path.starts_with(referent)
                        || observed.starts_with(observed_input_path(referent))
                }))
    });
    if !in_scope {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("js" | "scss" | "css") => true,
        // A directory's own path has no source extension — but a source
        // extension that is merely a directory NAME ("css/site") must
        // not count either: it changes nothing the build reads. A path
        // still on disk answers directly; a removed or renamed one can
        // no longer report `is_dir`, so its lifecycle event counts on
        // the event kind alone — whatever it held left with it.
        Some(_) => path.is_dir() || (lifecycle && !path.exists()),
        // An extensionless regular file (an editor's `4913` write probe)
        // is not a source; a removed entry may have been a directory that
        // held sources, so removal still counts.
        None => path.is_dir() || (lifecycle && !path.exists()),
    }
}

/// Minify JavaScript using oxc's full compressor plus local-name mangling.
///
/// The compressor runs with an ES5 target so it never emits syntax newer than
/// the input: `function () {}` stays a function and is NEVER rewritten into an
/// arrow function, which avoids the minify-js 0.6 bug where constructor
/// functions became arrows and `new` threw "e is not a constructor", breaking
/// paste.js's OOP system.
///
/// Top-level names are not mangled (`top_level: false`): separately built
/// files share the global scope, so a `var` or `function` declared at the top
/// of one file may be referenced by another. Property names and string
/// literals are never renamed either, so `module['name']`-style exports and
/// `paste.define('name', ...)` module names are preserved.
///
/// The printer must never raise the syntax level of the input either: in
/// minify mode oxc's codegen prints plain string literals as template
/// literals (`` `x` `` is one byte shorter to escape than `"x"`), which is
/// ES2015 syntax and a hard parse error on the ES5 engines paste.js ships
/// polyfills for. `restore_string_literals` rewrites every substitution-free,
/// untagged template literal in the printed output back to a quoted string.
/// For the same reason `restore_longhand_object_properties` rewrites the
/// `{key}` shorthand the codegen emits for a `{key: key}` property back to
/// longhand.
///
/// On any parse error (or panic) we return Err so the caller falls back to the
/// original, unminified source instead of emitting broken output.
fn minify_js(content: &str) -> Result<String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
        let printed = print_js(content)?;
        let printed = restore_string_literals(&printed)?;
        restore_longhand_object_properties(&printed)
    }));

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!("JS minifier panicked (unsupported syntax)")),
    }
}

/// Parse `content` as a classic script, compress and mangle it, and print it
/// through oxc's codegen in minify mode.
fn print_js(content: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::codegen::{Codegen, CodegenOptions};
    use oxc::minifier::{CompressOptions, MangleOptions, Minifier, MinifierOptions};
    use oxc::parser::Parser;
    use oxc::span::SourceType;
    use oxc::syntax::es_target::ESTarget;

    let allocator = Allocator::default();
    // paste.js modules are classic scripts (global `paste.define(...)`),
    // not ES modules, so parse in script mode.
    let parsed = Parser::new(&allocator, content, SourceType::cjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "JS parse produced {} diagnostic(s); refusing to emit minified output",
            parsed.errors.len()
        ));
    }
    // oxc keeps the shorthand flag when it mangles a `__proto__` binding,
    // and its codegen then emits the renamed binding as the shorthand key —
    // `{__proto__}` would print as `{e:e}` and change the property name.
    // Reject the shorthand on the pre-mangled AST so the caller falls back
    // to unminified source; longhand `__proto__: x` and every other
    // shorthand key print correctly.
    // A lone-surrogate escape in a string or template literal cannot be
    // reprinted as written either; checking the literals' own raw source
    // spans keeps comments and regex literals out of the decision.
    use oxc::ast::ast::{ObjectProperty, StringLiteral, TemplateElement};
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    struct Unsafe<'s> {
        source: &'s str,
        proto_shorthand: bool,
        lone_surrogate: bool,
    }
    impl<'a> Visit<'a> for Unsafe<'_> {
        fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
            self.proto_shorthand |=
                property.shorthand && property.key.is_specific_static_name("__proto__");
            walk::walk_object_property(self, property);
        }
        fn visit_string_literal(&mut self, literal: &StringLiteral<'a>) {
            let raw = self
                .source
                .get(literal.span.start as usize..literal.span.end as usize)
                .unwrap_or("");
            self.lone_surrogate |= literal_has_lone_surrogate(raw);
        }
        fn visit_template_element(&mut self, element: &TemplateElement<'a>) {
            self.lone_surrogate |= literal_has_lone_surrogate(element.value.raw.as_str());
        }
    }
    let mut checks = Unsafe {
        source: content,
        proto_shorthand: false,
        lone_surrogate: false,
    };
    checks.visit_program(&parsed.program);
    anyhow::ensure!(
        !checks.proto_shorthand,
        "__proto__ shorthand requires unminified output to preserve its property name"
    );
    anyhow::ensure!(
        !checks.lone_surrogate,
        "cannot safely minify JavaScript containing an unpaired surrogate escape"
    );
    let mut program = parsed.program;
    let minified = Minifier::new(MinifierOptions {
        compress: Some(CompressOptions {
            target: ESTarget::ES5,
            drop_debugger: true,
            drop_console: false,
        }),
        mangle: Some(MangleOptions {
            top_level: false,
            debug: false,
        }),
    })
    .build(&allocator, &mut program);
    let printed = Codegen::new()
        .with_options(CodegenOptions {
            minify: true,
            ..CodegenOptions::default()
        })
        .with_symbol_table(minified.symbol_table)
        .build(&program);
    Ok(printed.code)
}

/// Rewrite template literals that carry no substitutions and no tag back to
/// double-quoted string literals. Such a literal is exactly equivalent to a
/// string whose value is its cooked text, so the rewrite is semantics-preserving;
/// tagged templates are left alone because their tag observes the raw text.
///
/// One position needs care: a template literal that is a whole expression
/// statement is not a directive, but the quoted string replacing it would be —
/// `` `use strict` `` is a no-op statement while `"use strict"` at the head of a
/// function body switches that function to strict mode. Such a replacement is
/// parenthesized so it stays an ordinary expression statement.
fn restore_string_literals(code: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::ast::ast::{ExpressionStatement, TaggedTemplateExpression, TemplateLiteral};
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    use oxc::parser::Parser;
    use oxc::span::{GetSpan, SourceType};

    struct Collector {
        replacements: Vec<(u32, u32, String)>,
        statement_expressions: HashSet<u32>,
    }

    impl<'a> Visit<'a> for Collector {
        fn visit_expression_statement(&mut self, it: &ExpressionStatement<'a>) {
            self.statement_expressions
                .insert(it.expression.span().start);
            walk::walk_expression_statement(self, it);
        }

        fn visit_tagged_template_expression(&mut self, it: &TaggedTemplateExpression<'a>) {
            // Visit the tag (it may contain ordinary strings) but not the quasi.
            self.visit_expression(&it.tag);
        }

        fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
            if it.expressions.is_empty() && it.quasis.len() == 1 {
                if let Some(cooked) = &it.quasis[0].value.cooked {
                    let literal = quote_js_string(cooked);
                    let replacement = if self.statement_expressions.contains(&it.span.start) {
                        format!("({literal})")
                    } else {
                        literal
                    };
                    self.replacements
                        .push((it.span.start, it.span.end, replacement));
                    return;
                }
            }
            walk::walk_template_literal(self, it);
        }
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::cjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "minified output failed to re-parse; refusing to emit it"
        ));
    }
    let mut collector = Collector {
        replacements: Vec::new(),
        statement_expressions: HashSet::new(),
    };
    collector.visit_program(&parsed.program);

    let mut out = code.to_string();
    collector
        .replacements
        .sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, literal) in collector.replacements {
        out.replace_range(start as usize..end as usize, &literal);
    }
    Ok(out)
}

/// Rewrite ES2015 shorthand object properties back to `key: value` longhand.
///
/// In minify mode oxc's codegen prints `{key: value}` as `{value}` whenever
/// the property key name equals the value's identifier — including for
/// properties the source wrote in longhand — which is ES2015 syntax and a
/// parse error on the ES5 engines paste.js ships polyfills for. The rewrite is
/// semantics-preserving: the shorthand and longhand forms denote the same
/// property — with one exception.
///
/// `__proto__` is not an ordinary property name: in an object literal the
/// longhand `__proto__: <expr>` is a prototype setter, while the shorthand
/// `{__proto__}` creates an own data property holding the identifier's value.
/// Rewriting the shorthand to `__proto__:__proto__` would therefore turn an
/// own property into a prototype link — a semantic change, not a spelling
/// change. There is no longhand spelling of the own-property form, so the
/// function returns Err and the caller falls back to the unminified source.
///
/// Binding and assignment patterns get no such rewrite: destructuring is
/// already ES2015 syntax in any form, so expanding their shorthand would buy
/// nothing.
fn restore_longhand_object_properties(code: &str) -> Result<String> {
    use oxc::allocator::Allocator;
    use oxc::ast::ast::ObjectProperty;
    use oxc::ast::visit::walk;
    use oxc::ast::Visit;
    use oxc::parser::Parser;
    use oxc::span::{GetSpan, SourceType};

    struct Collector<'c> {
        code: &'c str,
        replacements: Vec<(u32, u32, String)>,
        unwritable: bool,
    }

    impl<'a> Visit<'a> for Collector<'_> {
        fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
            if it.shorthand {
                // A shorthand property's key and value are the same identifier
                // spanning the property itself, e.g. `{value}`.
                let key_span = it.key.span();
                let value_span = it.value.span();
                let key = &self.code[key_span.start as usize..key_span.end as usize];
                let value = &self.code[value_span.start as usize..value_span.end as usize];
                if key == "__proto__" {
                    self.unwritable = true;
                    return;
                }
                self.replacements
                    .push((it.span.start, it.span.end, format!("{key}:{value}")));
                return;
            }
            walk::walk_object_property(self, it);
        }
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::cjs()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(anyhow::anyhow!(
            "minified output failed to re-parse; refusing to emit it"
        ));
    }
    let mut collector = Collector {
        code,
        replacements: Vec::new(),
        unwritable: false,
    };
    collector.visit_program(&parsed.program);
    if collector.unwritable {
        return Err(anyhow::anyhow!(
            "shorthand __proto__ cannot be written in ES5 longhand without turning an own property into a prototype link"
        ));
    }

    let mut out = code.to_string();
    collector
        .replacements
        .sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, replacement) in collector.replacements {
        out.replace_range(start as usize..end as usize, &replacement);
    }
    Ok(out)
}

/// Print `value` as an ES5 double-quoted string literal.
fn quote_js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Line terminators that ES5 forbids inside a string literal.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Minify CSS using lightningcss
fn minify_css(content: &str) -> Result<String> {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    let stylesheet = StyleSheet::parse(content, ParserOptions::default())
        .map_err(|e| anyhow::anyhow!("CSS parse error: {:?}", e))?;

    let result = stylesheet
        .to_css(PrinterOptions {
            minify: true,
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("CSS print error: {:?}", e))?;

    Ok(result.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dist project under `dir` with these files written beneath it.
    fn min_project(dir: &Path, files: &[(&str, &str)]) -> Config {
        for (path, body) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        dist_config(dir, false)
    }

    #[test]
    fn test_min_sources_name_their_plain_module() {
        // `.min` is stripped with the extension: `vendor.min.js` is module
        // `vendor` at key `js/vendor.js`, `theme.min.scss` is `theme` at
        // `css/theme.css`, and a dependent requires the plain name.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = min_project(
            dir.path(),
            &[
                ("src/js/vendor.min.js", "var vendor = 1;"),
                ("src/js/app.js", "// @requires vendor\nvar app = vendor;"),
                ("src/scss/theme.min.scss", ".t { color: red; }"),
            ],
        );
        let manifest = AssetGraph::new(config).build().unwrap();
        assert_eq!(
            manifest.modules.get("vendor").map(String::as_str),
            Some("js/vendor.js")
        );
        assert!(
            manifest.assets.contains_key("css/theme.css"),
            "{:?}",
            manifest.assets.keys()
        );
        assert_eq!(
            manifest.modules.get("theme").map(String::as_str),
            Some("css/theme.css")
        );
        assert!(!manifest.modules.keys().any(|name| name.ends_with(".min")));
        assert_eq!(
            manifest.assets["js/app.js"].dependencies,
            vec!["vendor".to_string()]
        );
    }

    #[test]
    fn test_a_min_copy_beside_its_source_fails_the_build() {
        // `vendor.js` and `vendor.min.js` both name `vendor`; the build
        // names both files instead of picking one.
        use tempfile::tempdir;
        for (plain, min) in [
            ("src/js/vendor.js", "src/js/vendor.min.js"),
            ("src/scss/theme.scss", "src/scss/theme.min.scss"),
        ] {
            let dir = tempdir().unwrap();
            let body = if plain.ends_with(".js") {
                "var x;"
            } else {
                ".x{}"
            };
            let config = min_project(dir.path(), &[(plain, body), (min, body)]);
            let error = format!("{:#}", AssetGraph::new(config).build().unwrap_err());
            let name = |p: &str| {
                Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            };
            assert!(
                error.contains(&name(plain)) && error.contains(&name(min)),
                "{error}"
            );
            assert!(error.contains("both name module"), "{error}");
        }
    }

    #[test]
    fn test_a_module_name_ending_in_min_is_rejected() {
        // A declared `@module x.min` — script or style — and a path that
        // derives one (`vendor/min.js`) are configuration errors.
        use tempfile::tempdir;
        for (path, body) in [
            ("src/js/lib.js", "// @module vendor.min\nvar v;"),
            ("src/scss/theme.scss", "/* @module theme.min */\n.t{}"),
            ("src/js/vendor/min.js", "var v;"),
        ] {
            let dir = tempdir().unwrap();
            let config = min_project(dir.path(), &[(path, body)]);
            let error = format!("{:#}", AssetGraph::new(config).build().unwrap_err());
            assert!(error.contains("ends in `.min`"), "{path}: {error}");
        }
        // A module that is just `min` is fine: `min.min.js` names it.
        let dir = tempdir().unwrap();
        let config = min_project(dir.path(), &[("src/js/min.js", "var m;")]);
        assert!(AssetGraph::new(config)
            .build()
            .unwrap()
            .modules
            .contains_key("min"));
    }

    #[test]
    fn test_requiring_a_min_name_names_the_fix() {
        // `X.min` can never be a module, so `@requires vendor.min` is
        // always a mistake for `vendor` — in a script or a style.
        use tempfile::tempdir;
        for (path, body) in [
            ("src/js/app.js", "// @requires vendor.min\nvar app;"),
            ("src/scss/site.scss", "/* @requires theme.min */\n.s{}"),
        ] {
            let dir = tempdir().unwrap();
            let config = min_project(
                dir.path(),
                &[
                    (path, body),
                    ("src/js/vendor.js", "var vendor;"),
                    ("src/scss/theme.scss", ".t{}"),
                ],
            );
            let error = format!("{:#}", AssetGraph::new(config).build().unwrap_err());
            let plain = if path.ends_with(".js") {
                "vendor"
            } else {
                "theme"
            };
            assert!(error.contains(&format!("require `{plain}`")), "{error}");
        }
    }

    #[test]
    fn test_a_style_whose_key_or_derived_name_ends_in_min_is_rejected() {
        // Styles follow the script rule: `x.min.min.scss` keeps a `.min`
        // in its key, `vendor/min.scss` derives `vendor.min` — both errors.
        use tempfile::tempdir;
        for (path, body) in [
            ("src/scss/x.min.min.scss", ".x{}"),
            ("src/scss/x.min.min.scss", "/* @module ok */\n.x{}"),
            ("src/scss/vendor/min.scss", ".x{}"),
        ] {
            let dir = tempdir().unwrap();
            let config = min_project(dir.path(), &[(path, body)]);
            let error = format!("{:#}", AssetGraph::new(config).build().unwrap_err());
            assert!(error.contains("ends in `.min`"), "{path}: {error}");
            if path.contains("x.min.min") {
                // The key comes from the path: only a rename helps.
                assert!(
                    error.contains("rename the file") && !error.contains("@module"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn test_is_module_name_accepts_the_build_alphabet() {
        assert!(is_module_name("paste.dom"));
        assert!(is_module_name("site.cafe\u{301}"));
        assert!(is_module_name("a\u{203F}b"));
        assert!(is_module_name("site.café"));
    }

    #[test]
    fn test_is_module_name_rejects_non_names() {
        assert!(!is_module_name(""));
        assert!(!is_module_name("../etc"));
        assert!(!is_module_name("a/b"));
        assert!(!is_module_name("a b"));
        assert!(!is_module_name("a+v1"));
        // Empty dot segments would let a name reach outside the output tree.
        assert!(!is_module_name("."));
        assert!(!is_module_name(".."));
        assert!(!is_module_name(".name"));
        assert!(!is_module_name("name."));
        assert!(!is_module_name("a..b"));
        assert!(!is_module_name("a\\b"));
    }

    #[test]
    fn test_is_module_name_matches_the_existing_unicode_pattern() {
        let reference = regex::Regex::new(r"^[\w-]+(?:\.[\w-]+)*$").unwrap();
        for name in [
            "",
            ".a",
            "a.",
            "a..b",
            "a/b",
            "a b",
            "a\n",
            "-",
            "a-b.c_2",
            "site.café",
            "a.\u{301}b",
            "a.\u{200c}",
            "a.\u{203f}",
            "a.²",
            "a.🙂",
        ] {
            assert_eq!(is_module_name(name), reference.is_match(name), "{name:?}");
        }
    }

    #[test]
    fn test_is_module_name_works_on_a_small_foreign_stack() {
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(|| {
                assert!(is_module_name("site.café"));
                assert!(is_module_name("a.\u{301}b"));
                assert!(!is_module_name("a..b"));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // ============================================================
    // JS minification (oxc) — regression for the minify-js arrow bug
    // ============================================================

    #[test]
    fn test_minify_js_preserves_constructor_functions() {
        // paste.js OOP uses `function`s invoked with `new`. The old minify-js
        // crate rewrote these into arrow functions, which cannot be `new`'d
        // ("e is not a constructor"). oxc codegen must keep them as functions.
        let input = r#"
            var oop = {};
            oop.Class = {
                create: function (proto) {
                    var Ctor = function () {
                        if (this.init) { this.init.apply(this, arguments); }
                    };
                    Ctor.prototype = proto;
                    return Ctor;
                }
            };
            var Animal = oop.Class.create({ init: function (name) { this.name = name; } });
            var a = new Animal("rex");
        "#;

        let out = minify_js(input).expect("minification should succeed");

        assert!(
            out.contains("function"),
            "functions must be preserved: {}",
            out
        );
        assert!(
            !out.contains("=>"),
            "constructor functions must NOT become arrow functions: {}",
            out
        );
        assert!(
            out.contains("new Animal"),
            "`new` call must be preserved: {}",
            out
        );
        assert!(
            out.len() < input.len(),
            "minified output should be smaller than input"
        );
    }

    #[test]
    fn test_minify_js_output_is_valid_js() {
        // Re-minifying the output must also succeed, proving the emitted code
        // is syntactically valid JavaScript.
        let input = "var f = function (x) { return x * 2; }; var y = f(21);";
        let out = minify_js(input).expect("first minification should succeed");
        let out2 = minify_js(&out).expect("re-minifying valid output should succeed");
        assert!(out2.contains("function"));
        assert!(!out2.contains("=>"));
    }

    #[test]
    fn test_minify_js_removes_whitespace_and_comments() {
        let input = "var x = 1; // a line comment\n\n   var y = 2;   /* block */  var z = x + y;";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains("// a line comment"),
            "line comment should be removed: {}",
            out
        );
        assert!(
            !out.contains("/* block */"),
            "block comment should be removed: {}",
            out
        );
        assert!(
            !out.contains("  "),
            "runs of spaces should be collapsed: {}",
            out
        );
    }

    #[test]
    fn test_minify_js_never_raises_syntax_level_of_es5_input() {
        // oxc's minify-mode printer prefers backticks for plain strings; on the
        // ES5 engines paste.js supports a template literal is a parse error for
        // the whole file. ES5 in must mean ES5 out.
        let input = r#"paste.define('paste.oop', ['paste.util'], function (oop, util) {
            "use strict";
            var msg = 'He said "hi"', path = "a\\b", multi = 'line1\nline2';
            return { msg: msg, path: path, multi: multi, sep: "\u2028" };
        });"#;
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains('`'),
            "no template literal may be introduced: {}",
            out
        );
        assert!(
            !out.contains("=>"),
            "no arrow function may be introduced: {}",
            out
        );
        assert!(
            out.contains(r#""paste.oop""#) || out.contains("'paste.oop'"),
            "module name stays a string: {}",
            out
        );
        assert!(
            !out.contains('\u{2028}'),
            "U+2028 must be escaped, not emitted raw: {}",
            out
        );
        // The restored strings must still be exactly the same values.
        let reparsed = minify_js(&out).expect("output must re-parse");
        assert_eq!(
            reparsed, out,
            "restoring string literals must be idempotent"
        );
    }

    #[test]
    fn test_minify_js_keeps_genuine_template_literals() {
        // Sources that really use ES2015 keep it: substitutions and tags are
        // semantics the rewrite cannot express as a plain string.
        let input = "var a = `x${y}z`; var b = tag`raw\\n`; var c = `plain`;";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("`x${y}z`"),
            "substitution template must be kept: {}",
            out
        );
        assert!(
            out.contains("tag`raw\\n`"),
            "tagged template must be kept verbatim: {}",
            out
        );
        assert!(
            out.contains(r#""plain""#),
            "substitution-free untagged template becomes a string: {}",
            out
        );
    }

    #[test]
    fn test_minify_js_does_not_introduce_a_strict_mode_directive() {
        // `use strict` as a template literal is an ordinary no-op statement.
        // Printing it as a bare string would make it a directive and switch the
        // enclosing function to strict mode — a semantic change, not a rewrite.
        let input = "function f() { `use strict`; return this === undefined; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains('`'),
            "template literal should be rewritten: {out}"
        );
        assert!(
            !out.contains("{\"use strict\"") && !out.contains("{ \"use strict\""),
            "the rewrite must not become a directive: {out}"
        );
        // The compressor drops the no-op statement outright, which is equally
        // safe: a removed statement cannot become a directive either.
        assert!(
            !out.contains("\"use strict\""),
            "the dead statement should be removed, not printed: {out}"
        );
        let reparsed = minify_js(&out).expect("output must re-parse");
        assert_eq!(reparsed, out, "the rewrite must be idempotent");
    }

    #[test]
    fn test_minify_js_keeps_an_existing_strict_mode_directive() {
        // A real directive must survive untouched.
        let input = "function f() { \"use strict\"; return 1; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("\"use strict\""),
            "existing directive must be preserved: {out}"
        );
        assert!(
            !out.contains("(\"use strict\")"),
            "an existing directive must not be parenthesized: {out}"
        );
    }

    #[test]
    fn test_minify_js_mangles_locals_inside_define() {
        // YUI-style compression: local variables are shortened while every
        // property name and string literal is preserved, so module names,
        // named exports, and dot/bracket member access all survive.
        let input = "paste.define('paste.example', ['paste.util'], function (mymodule, util) { 'use strict'; var longLocalName = 1; function privateHelper(someArgument) { return someArgument + longLocalName; } mymodule.publicMethod = function (anotherArgument) { return privateHelper(anotherArgument) + util.value; }; mymodule['namedExport'] = mymodule.publicMethod; mymodule['delete'] = privateHelper; });";
        let out = minify_js(input).expect("minification should succeed");

        for local in [
            "longLocalName",
            "privateHelper",
            "someArgument",
            "anotherArgument",
            "mymodule",
        ] {
            assert!(
                !out.contains(local),
                "local name {local} must be mangled: {out}"
            );
        }
        for kept in [
            "\"paste.example\"",
            "\"paste.util\"",
            "publicMethod",
            "namedExport",
            "delete",
            ".value",
            "\"use strict\"",
        ] {
            assert!(out.contains(kept), "{kept} must be preserved: {out}");
        }
        assert!(!out.contains("=>"), "no arrow may be introduced: {out}");
        assert!(
            !out.contains('`'),
            "no template literal may be introduced: {out}"
        );
        minify_js(&out).expect("minified output must re-minify");
    }

    #[test]
    fn test_minify_js_keeps_top_level_names() {
        // Separately built files share the global scope, so top-level
        // declarations must keep their names; only locals are mangled.
        let input = "var pasteGlobal = 1; function topLevelFn(localValue) { var inner = pasteGlobal + localValue; return inner; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            out.contains("pasteGlobal"),
            "top-level var must not be mangled: {out}"
        );
        assert!(
            out.contains("topLevelFn"),
            "top-level function must not be mangled: {out}"
        );
        assert!(
            !out.contains("localValue"),
            "parameter must be mangled: {out}"
        );
        assert!(!out.contains("inner"), "local var must be mangled: {out}");
    }

    #[test]
    fn test_minify_js_keeps_string_keyed_globals() {
        // Bracket-string member access is how bootstrap code installs globals;
        // the names inside the brackets are strings and must be preserved.
        let input = "(function (w) { w['paste'] = w['paste'] || {}; w['paste']['define'] = function () {}; window['PasteIOFormData'] = function () {}; })(window);";
        let out = minify_js(input).expect("minification should succeed");
        for kept in ["paste", "define", "PasteIOFormData"] {
            assert!(out.contains(kept), "{kept} must be preserved: {out}");
        }
    }

    #[test]
    fn test_minify_js_does_not_emit_es2015_object_shorthand() {
        // The ES5 target must keep `key: value` longhand: `{value}` shorthand
        // is a parse error on ES5 engines.
        let input = "var value = 1; var o = { value: value }; function f(value) { return { value: value }; }";
        let out = minify_js(input).expect("minification should succeed");
        assert!(
            !out.contains("{value}"),
            "object shorthand is ES2015: {out}"
        );
        assert!(
            !out.contains("{value,") && !out.contains(",value}"),
            "object shorthand is ES2015: {out}"
        );
        assert!(out.contains("value:"), "the key must remain: {out}");
    }

    #[test]
    fn test_minify_js_lone_surrogate_escapes_pass_through_verbatim() {
        // A `\uD800`-style escape is legal JavaScript with no scalar
        // value — whatever the parser stores, reprinting cannot
        // reproduce the author's escape. The pass reports it like any
        // other unminifiable input so the caller's fallback warning
        // fires; the emitted bytes are still the exact source.
        for content in [
            "var s = '\\uD800 lone high';",
            "var s = '\\uDC00 lone low';",
            "var s = '\\u{D83F} braced';",
            "var s = 'paired \\uD800 tail';",
            // Quotes in comments and regex literals must not hide a real one.
            "// don't touch\nvar s = '\\uD800';",
            "var r = /\"/g; var s = \"\\uD800\";",
            "var t = `\\uD800`;",
        ] {
            let err = minify_js(content).expect_err(&format!(
                "a lone-surrogate escape must refuse minification: {content:?}"
            ));
            assert!(
                err.to_string().contains("surrogate"),
                "the diagnostic must name the cause: {err}"
            );
        }

        // A real surrogate pair is one code point — it minifies like any
        // other string (a fallback would return the source verbatim).
        let paired = "var s = '\\uD83D\\uDE00 emoji pair';";
        let printed = minify_js(paired).unwrap();
        assert_ne!(printed, paired, "a valid pair still minifies: {printed:?}");

        // An escape in a comment is comment text — the printer drops
        // comments, nothing decodes it.
        let commented = "var s = 1; // \\uD800 in a comment\nvar t = 2;";
        assert!(
            minify_js(commented).is_ok(),
            "a surrogate escape in a comment does not force a fallback"
        );

        // Through the build the diagnostic reaches the caller's warn
        // path, and the emitted file carries the source bytes verbatim —
        // the surrogate escape survives unedited.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = "// @module app\nvar s = '\\uD800 lone';\nvar keep = '\\u0041';\n";
        fs::write(js_dir.join("app.js"), source).unwrap();
        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        let emitted = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/')),
        )
        .unwrap();
        assert_eq!(emitted, source, "the fallback emits the source verbatim");
    }

    #[test]
    fn test_minify_js_proto_shorthand_falls_back_to_source() {
        use tempfile::tempdir;

        // `{__proto__}` shorthand is an own data property; there is no ES5
        // longhand spelling for it (`__proto__: value` is a prototype
        // setter). The pass refuses to rewrite it, minify_js fails, and the
        // emitted file is the unminified source — shorthand intact.
        let source = "// @module app\nvar __proto__ = {marker: 1};\nvar object = {__proto__};\n";
        let err = minify_js(source).unwrap_err();
        assert!(err.to_string().contains("__proto__"), "{err}");

        // The escaped spelling `\u005f_proto__` decodes to `__proto__` and
        // must be rejected just the same — key.name sees the cooked name.
        let escaped = r#"var \u005f_proto__ = {marker: 1}; var object = {\u005f_proto__};"#;
        let err = minify_js(escaped).unwrap_err();
        assert!(err.to_string().contains("__proto__"), "{err}");

        // Longhand `__proto__: value` is the prototype setter, not an own
        // property shorthand — it must not be misclassified.
        minify_js("var object = {__proto__: {marker: 1}};")
            .expect("longhand __proto__ minifies normally");

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), source).unwrap();
        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        let emitted = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/')),
        )
        .unwrap();
        assert_eq!(emitted, source, "the fallback must emit the source bytes");
        assert!(emitted.contains("{__proto__}"), "{emitted}");
    }

    #[test]
    fn test_quote_js_string_escapes_for_es5() {
        assert_eq!(quote_js_string("plain"), r#""plain""#);
        assert_eq!(quote_js_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_js_string("a\\b"), r#""a\\b""#);
        assert_eq!(quote_js_string("l1\nl2\r\t"), r#""l1\nl2\r\t""#);
        assert_eq!(
            quote_js_string("\u{2028}\u{2029}\u{1}"),
            r#""\u2028\u2029\u0001""#
        );
    }

    // ============================================================
    // Dependency ordering and backfill
    // ============================================================

    fn deps_map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(name, deps)| {
                (
                    name.to_string(),
                    deps.iter().map(|d| d.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_layered_order_releases_leaves_first() {
        // b and c have no deps, so both release in layer 1 (name order); a
        // follows once c is gone.
        let order = layered_order(&deps_map(&[("a", &["c"]), ("b", &[]), ("c", &[])]));
        assert_eq!(order.sorted, vec!["b", "c", "a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_diamond() {
        let order = layered_order(&deps_map(&[
            ("a", &["b", "c"]),
            ("b", &["d"]),
            ("c", &["d"]),
            ("d", &[]),
        ]));
        assert_eq!(order.sorted, vec!["d", "b", "c", "a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_ignores_self_dependencies() {
        let order = layered_order(&deps_map(&[("a", &["a"]), ("b", &["a"])]));
        assert_eq!(order.sorted, vec!["a", "b"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_unknown_deps_layer_but_never_sort() {
        // `x` names no module: it joins the layering as a leaf so `a` still
        // sorts, but it must never appear in the output.
        let order = layered_order(&deps_map(&[("a", &["x"])]));
        assert_eq!(order.sorted, vec!["a"]);
        assert!(order.cyclic.is_empty());
    }

    #[test]
    fn test_layered_order_cycle_traps_its_dependents() {
        // a↔b is a cycle; c depends on a and can never release either. d is
        // the only node that sorts.
        let order = layered_order(&deps_map(&[
            ("a", &["b"]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &[]),
        ]));
        assert_eq!(order.sorted, vec!["d"]);
        assert_eq!(order.cyclic, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_backfilled_deps_declared_first_then_preorder() {
        // Declared order first (b, c), then the pre-order walk appends d.
        let deps = deps_map(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &[]), ("d", &[])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "c", "d"]);
    }

    #[test]
    fn test_backfilled_deps_dedupes_and_drops_self() {
        let deps = deps_map(&[("a", &["a", "b", "b", "c"]), ("b", &["c"]), ("c", &[])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "c"]);
    }

    #[test]
    fn test_backfilled_deps_keeps_unknown_and_ends_on_cycles() {
        // Unknown dep stays in the list; the a↔b cycle terminates instead of
        // recursing forever.
        let deps = deps_map(&[("a", &["b"]), ("b", &["a", "x"])]);
        assert_eq!(backfilled_deps(&deps, "a"), vec!["b", "x"]);
        assert_eq!(backfilled_deps(&deps, "b"), vec!["a", "x"]);
    }

    #[test]
    fn test_build_js_cycle_builds_every_module_but_sorts_none() {
        use tempfile::tempdir;

        // a↔b is circular: the build must succeed, record and emit both
        // modules, and leave them out of sorted_deps — only c sorts.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("a.js"),
            "// @module a\n// @requires b\nvar a = 1;",
        )
        .unwrap();
        fs::write(
            js_dir.join("b.js"),
            "// @module b\n// @requires a\nvar b = 1;",
        )
        .unwrap();
        fs::write(js_dir.join("c.js"), "// @module c\nvar c = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        for module in ["a", "b", "c"] {
            let key = format!("js/{module}.js");
            assert!(
                manifest.assets.contains_key(&key),
                "missing asset for cyclic module {module}"
            );
            assert!(
                manifest.modules.contains_key(module),
                "missing modules entry for {module}"
            );
            let output = &manifest.assets[&key].output;
            assert!(
                dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "missing output file {output}"
            );
        }
        let sorted: Vec<&String> = manifest.sorted_deps["js"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["c"], "cyclic modules must not sort");
    }

    #[test]
    fn test_css_sorted_deps_follow_layered_order_not_file_order() {
        use tempfile::tempdir;

        // The file walk reads atheme.scss before zbase.scss; @requires still
        // puts base before theme.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("atheme.scss"),
            "// @module theme\n// @requires base\n.t { color: red; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("zbase.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        let sorted: Vec<&String> = manifest.sorted_deps["css"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["base", "theme"]);

        let theme_key = &manifest.modules["theme"];
        assert_eq!(
            manifest.assets[theme_key].dependencies,
            vec!["base".to_string()],
            "css asset deps should be backfilled too"
        );
    }

    // ============================================================
    // Styles without @module take a path-derived module name
    // ============================================================

    #[test]
    fn test_unnamed_style_takes_path_derived_module_name() {
        use tempfile::tempdir;

        // src/scss/site/theme.scss declares nothing: it is named
        // "site.theme" like a JS file at that path would be, records the
        // module, and sorts into css sorted_deps.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss/site");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("theme.scss"),
            "// @requires base\n.t { color: red; }",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/scss/base.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("site.theme"),
            Some(&"css/site/theme.css".to_string())
        );
        assert!(manifest.assets.contains_key("css/site/theme.css"));
        let sorted: Vec<&String> = manifest.sorted_deps["css"]
            .iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(sorted, vec!["base", "site.theme"]);
        assert_eq!(
            manifest.assets["css/site/theme.css"].dependencies,
            vec!["base".to_string()]
        );
    }

    #[test]
    fn test_derived_style_name_loses_to_a_declared_name() {
        use tempfile::tempdir;

        // declared.scss declares @module theme; theme.scss derives the same
        // name — the declaration keeps it and the deriving file stays
        // unnamed but still compiles under its path key.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("declared.scss"),
            "// @module theme\n.d { color: red; }",
        )
        .unwrap();
        fs::write(scss_dir.join("theme.scss"), ".t { color: blue; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("theme"),
            Some(&"css/declared.css".to_string()),
            "the declared owner keeps the name"
        );
        assert!(manifest.assets.contains_key("css/declared.css"));
        assert!(
            manifest.assets.contains_key("css/theme.css"),
            "the unnamed file still builds under its path key"
        );
        let names: Vec<&String> = manifest.modules.keys().collect();
        assert_eq!(
            names.iter().filter(|n| ***n == "theme").count(),
            1,
            "one owner for the name"
        );
    }

    #[test]
    fn test_invalid_derived_style_name_stays_unnamed_and_builds() {
        use tempfile::tempdir;

        // A filename that cannot be a module name (a space) warns, leaves
        // the file unnamed, and does not fail the build.
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(scss_dir.join("in valid.scss"), ".t { color: red; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert!(
            manifest.assets.contains_key("css/in valid.css"),
            "the file compiles under its path key"
        );
        assert!(
            manifest
                .modules
                .values()
                .all(|key| key != "css/in valid.css"),
            "no module name claims the invalid file"
        );
    }

    #[test]
    fn test_include_path_style_derives_a_module_name() {
        use tempfile::tempdir;

        // A style an include path provides is named from its path like a
        // source-dir style (the fallback warning fires on stderr).
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        let include_dir = dir.path().join("vendor");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::create_dir_all(&include_dir).unwrap();
        fs::write(
            scss_dir.join("base.scss"),
            "// @module base\n.b { color: blue; }",
        )
        .unwrap();
        fs::write(include_dir.join("widget.scss"), ".w { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.include_paths = vec![include_dir];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("widget"),
            Some(&"css/widget.css".to_string())
        );
        assert!(manifest.assets.contains_key("css/widget.css"));
    }

    #[test]
    fn test_include_path_js_derives_a_module_name() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let include_dir = dir.path().join("vendor");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&include_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        fs::write(include_dir.join("widget.js"), "var w = 1;").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.include_paths = vec![include_dir];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.modules.get("widget"),
            Some(&"js/widget.js".to_string())
        );
        assert!(manifest.assets.contains_key("js/widget.js"));
    }

    // ============================================================
    // SCSS files without @module are tracked and not pruned
    // ============================================================

    #[test]
    fn test_scan_clears_stale_modules_between_builds() {
        // Verify that deleted source files don't persist as phantom modules
        // when build() is called multiple times on the same AssetGraph
        use crate::config::Config;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let output_js = dir.path().join("output/js");
        let output_css = dir.path().join("output/css");
        let output_images = dir.path().join("output/images");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&output_js).unwrap();
        fs::create_dir_all(&output_css).unwrap();
        fs::create_dir_all(&output_images).unwrap();

        // Write a JS file
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let config = Config {
            base_dir: dir.path().to_path_buf(),
            debug_mode: true,
            minify: false,
            source_maps: false,
            source: crate::config::SourcePaths {
                dirs: vec![],
                js: js_dir.clone(),
                scss: dir.path().join("src/scss"),
                images: None,
            },
            output: crate::config::OutputPaths {
                base: dir.path().join("output"),
                js: output_js,
                css: output_css,
                images: output_images,
                layout: "flat".into(),
                build_dir_name: "_paste_build".into(),
            },
            manifest_path: dir.path().join("manifest.json"),
            include_paths: vec![],
            storage: crate::config::StorageConfig {
                // Local: a deleted source leaves the manifest entirely
                // (dist would keep it as a retired entry).
                backend: "local".into(),
                ..crate::config::StorageConfig::default()
            },
            jam: crate::config::JamConfig {
                enabled: None,
                base_path: Some("/paste".into()),
                // Legacy pins — these fixtures request `+vN.0` URLs; the
                // qualified form is covered by the hash_url tests below.
                hash_urls: false,
                inline_threshold: Some(1024),
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: None,
            },
            templates: None,
        };

        let mut graph = AssetGraph::new(config);
        let manifest1 = graph.build().unwrap();
        assert!(
            manifest1.assets.contains_key("js/a.js"),
            "First build should have js/a.js"
        );

        // Delete the file and rebuild on same instance
        fs::remove_file(js_dir.join("a.js")).unwrap();
        let manifest2 = graph.build().unwrap();
        assert!(
            !manifest2.assets.contains_key("js/a.js"),
            "Second build should NOT have js/a.js after file deleted"
        );
    }

    /// Config for a dist-backend build rooted at `dir`, with sources in
    /// src/js + src/scss and output in dist/.
    fn dist_config(dir: &Path, minify: bool) -> Config {
        let output = dir.join("dist");
        Config {
            base_dir: dir.to_path_buf(),
            debug_mode: false,
            minify,
            source_maps: false,
            source: crate::config::SourcePaths {
                dirs: vec![],
                js: dir.join("src/js"),
                scss: dir.join("src/scss"),
                images: None,
            },
            output: crate::config::OutputPaths {
                base: output.clone(),
                js: output.join("js"),
                css: output.join("css"),
                images: output.join("images"),
                layout: "flat".into(),
                build_dir_name: "_paste_build".into(),
            },
            manifest_path: output.join("manifest.json"),
            include_paths: vec![],
            storage: crate::config::StorageConfig {
                backend: "dist".into(),
                // Keep-all, the same default a parsed config gets —
                // eviction tests set an explicit bound.
                keep_versions: usize::MAX,
            },
            jam: crate::config::JamConfig {
                enabled: None,
                base_path: Some("/paste".into()),
                // Legacy pins — these fixtures request `+vN.0` URLs; the
                // qualified form is covered by the hash_url tests below.
                hash_urls: false,
                inline_threshold: Some(1024),
            },
            cdn: crate::config::CdnConfig {
                enabled: false,
                url: String::new(),
                fallback_to_local: None,
            },
            templates: None,
        }
    }

    #[test]
    fn test_dist_build_writes_no_rollup() {
        // Serving assembles a page's group from individual module files —
        // there is no all-modules aggregate for a build to write.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();

        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n.main { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        assert!(!dir.path().join("dist/js/assetgraph.js").exists());
        assert!(!dir.path().join("dist/css/assetgraph.css").exists());
        assert!(
            !manifest.assets.keys().any(|key| key.contains("assetgraph")),
            "no manifest asset is an aggregate"
        );
    }

    #[test]
    fn test_dist_build_removes_an_existing_rollup() {
        // A dist tree built by a version that still emitted the
        // unversioned all-modules files loses them on the next build:
        // they were never manifest entries, so no recorded output can
        // retire them.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let stale_js = dir.path().join("dist/js/assetgraph.js");
        let stale_css = dir.path().join("dist/css/assetgraph.css");
        fs::create_dir_all(stale_js.parent().unwrap()).unwrap();
        fs::create_dir_all(stale_css.parent().unwrap()).unwrap();
        fs::write(&stale_js, "var stale = 1;").unwrap();
        fs::write(&stale_css, ".stale { color: red; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();

        assert!(!stale_js.exists(), "a stale js aggregate is removed");
        assert!(!stale_css.exists(), "a stale css aggregate is removed");
        assert!(
            dir.path().join("dist/js").exists()
                && fs::read_dir(dir.path().join("dist/js")).unwrap().any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("a.")),
            "the module's own versioned file is untouched"
        );
    }

    #[test]
    fn test_dist_build_records_cdn_base_only_when_enabled() {
        // cdn.enabled writes the configured origin into the manifest as
        // the render-time default; disabled leaves the field unset so
        // emitted tags stay origin-relative until a runtime says
        // otherwise.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.cdn.enabled = true;
        config.cdn.url = "https://cdn.example".to_string();
        let manifest = AssetGraph::new(config).build().unwrap();
        assert_eq!(manifest.cdn_base.as_deref(), Some("https://cdn.example"));

        // The persisted JSON carries it too — a runtime reads the
        // manifest, not the build configuration.
        let loaded = Manifest::load_json(&dir.path().join("dist/manifest.json")).unwrap();
        assert_eq!(loaded.cdn_base.as_deref(), Some("https://cdn.example"));

        // The same configuration rebuilds the same default — the value
        // is a config string, not environment state.
        let mut config = dist_config(dir.path(), true);
        config.cdn.enabled = true;
        config.cdn.url = "https://cdn.example".to_string();
        let rebuilt = AssetGraph::new(config).build().unwrap();
        assert_eq!(rebuilt.cdn_base, manifest.cdn_base);
    }

    #[test]
    fn test_build_rejects_malformed_cdn_authority_before_mutating() {
        // A malformed authority is a configuration error caught in
        // Config::validate — before a single output or manifest write.
        // The prior publication stays byte-exact and every retained
        // versioned file is untouched, exactly like the other cdn.url
        // rejections.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.cdn.enabled = true;
        config.cdn.url = "https://cdn.example".to_string();
        AssetGraph::new(config.clone()).build().unwrap();

        // Snapshot the published state: every file under the output
        // root, manifest included.
        let dist = dir.path().join("dist");
        let snapshot = |root: &Path| -> BTreeMap<PathBuf, Vec<u8>> {
            let mut files = BTreeMap::new();
            let mut pending = vec![root.to_path_buf()];
            while let Some(dir) = pending.pop() {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        files.insert(path.clone(), fs::read(&path).unwrap());
                    }
                }
            }
            files
        };
        let before = snapshot(&dist);
        assert!(
            before.values().any(|b| !b.is_empty()),
            "the first build must have produced versioned outputs"
        );

        config.cdn.url = "https://cdn.example:bad/assets".to_string();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a malformed authority must fail");
        assert!(
            format!("{error:#}").contains("cdn.url"),
            "the failure must name cdn.url, got: {error:#}"
        );

        // Nothing moved: the authoritative JSON and every previously
        // emitted file keep their recorded bytes, and no new file
        // appeared.
        assert_eq!(
            snapshot(&dist),
            before,
            "the rejected build left the published tree byte-identical"
        );
    }

    #[test]
    fn test_dist_build_leaves_cdn_base_unset_when_disabled() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        assert_eq!(manifest.cdn_base, None);

        // The JSON carries no cdn_base key at all — absent means
        // origin-relative URLs; only the positional bincode cache keeps
        // the field, and the load reads it back as None either way.
        let raw = fs::read_to_string(dir.path().join("dist/manifest.json")).unwrap();
        assert!(!raw.contains("\"cdn_base\""), "{raw}");
        let loaded = Manifest::load_json(&dir.path().join("dist/manifest.json")).unwrap();
        assert_eq!(loaded.cdn_base, None);
    }

    #[test]
    fn test_dist_build_preserves_same_named_files_outside_type_roots() {
        // The legacy pair lived at the js/css type roots — nowhere else.
        // A consumer file that merely shares the name must survive: under
        // output.base, the images root, a nested module directory, and
        // the wrong type's root (assetgraph.js was never written under the
        // css root), on a fresh build and after a rebuild alike.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();

        let config = dist_config(dir.path(), true);
        AssetGraph::new(config.clone()).build().unwrap();

        let preserved = [
            "dist/assetgraph.js",        // output.base — never a type root
            "dist/assetgraph.css",       // output.base
            "dist/images/assetgraph.js", // the images root
            "dist/js/sub/assetgraph.js", // a nested directory, not the root
            "dist/css/assetgraph.js",    // the wrong type's root
            "dist/js/assetgraph.css",    // the wrong name for the js root
        ];
        for rel in preserved {
            let file = dir.path().join(rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "consumer-owned").unwrap();
        }
        // The owned pair at the real type roots — present when an old
        // build emitted them — is still removed.
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();
        fs::write(dir.path().join("dist/css/assetgraph.css"), ".stale{}").unwrap();

        AssetGraph::new(config).build().unwrap();

        for rel in preserved {
            let file = dir.path().join(rel);
            assert!(
                file.exists() && fs::read_to_string(&file).unwrap() == "consumer-owned",
                "{rel} is outside any established rollup location and must survive"
            );
        }
        assert!(!dir.path().join("dist/js/assetgraph.js").exists());
        assert!(!dir.path().join("dist/css/assetgraph.css").exists());
    }

    #[test]
    fn test_beside_source_roots_named_like_types_preserve_same_named_sources() {
        // With `source.dirs = ["js", "css"]` under beside-source layout the
        // build records outputs like /js/_paste_build/a.min.js — a label
        // that looks flat but whose leading "js" is a source directory.
        // The files js/assetgraph.js and css/assetgraph.css are inputs the
        // build itself compiles; mistaking the source dir for a former
        // rollup root would delete them.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("js")).unwrap();
        fs::create_dir_all(dir.path().join("css")).unwrap();
        fs::write(dir.path().join("js/a.js"), "var a = 1;").unwrap();
        fs::write(dir.path().join("js/assetgraph.js"), "var mine = 1;").unwrap();
        fs::write(dir.path().join("css/site.css"), ".site { color: red; }").unwrap();
        fs::write(
            dir.path().join("css/assetgraph.css"),
            ".mine { color: red; }",
        )
        .unwrap();

        let config = beside_source_config(dir.path(), &["js", "css"], true);
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        assert!(manifest.assets.contains_key("js/assetgraph.js"));
        assert!(manifest.assets.contains_key("css/assetgraph.css"));

        // The second build is where recorded beside-source labels meet the
        // stale-file pass.
        AssetGraph::new(config).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("js/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "a source file is never a deletion target"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("css/assetgraph.css")).unwrap(),
            ".mine { color: red; }"
        );
    }

    #[test]
    fn test_spelled_parent_components_never_reach_an_output_pointer() {
        // A spelled root carrying `..` — plain or reached through a
        // symlinked component — discovers sources normally, but a public
        // output pointer derives from the file's physical home:
        // `src/../src` compiles into `/src/_paste_build/…`, never a URL
        // with `..` inside it.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = beside_source_config(dir.path(), &["src/../src"], false);
        let manifest = AssetGraph::new(config).build().unwrap();
        let output = &manifest.assets["js/app.js"].output;
        assert!(
            output.starts_with("/src/_paste_build/") && !output.contains(".."),
            "a spelled parent must not reach the recorded pointer: {output}"
        );

        // `link/../src` resolves physically through `link -> other` to
        // the same `src` — the alias spelling must not leak either.
        if symlink(&dir.path().join("link"), &dir.path().join("other")) {
            fs::create_dir_all(dir.path().join("other")).unwrap();
            let config = beside_source_config(dir.path(), &["link/../src"], false);
            let manifest = AssetGraph::new(config).build().unwrap();
            let output = &manifest.assets["js/app.js"].output;
            assert!(
                output.starts_with("/src/_paste_build/") && !output.contains(".."),
                "a linked parent must not reach the recorded pointer: {output}"
            );
        }

        // Flat layout never borrowed the spelling either — the same root
        // compiles under the type directory it always did.
        let config = dirs_config(dir.path(), &["src/../src"], false);
        let manifest = AssetGraph::new(config).build().unwrap();
        let output = &manifest.assets["js/app.js"].output;
        assert!(
            output.starts_with("/js/") && !output.contains(".."),
            "flat output is spelled-path-free already: {output}"
        );
    }

    #[test]
    fn test_beside_source_folder_rename_keeps_retained_versions() {
        // Renaming a beside-source folder carries its retained files into
        // the new folder's build directory: the next build finds them there
        // by exact name and recorded hash and restores each recorded path,
        // instead of failing the version as missing. Bytes truly gone
        // still fail closed.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/a")).unwrap();
        fs::write(
            dir.path().join("config.json"),
            r#"{"source":{"dirs":["src"]},"output":{"base":"dist","js":"dist/js","css":"dist/css","images":"dist/images","layout":"beside-source"},"manifest_path":"dist/manifest.json","storage":{"backend":"dist","keep_versions":"all"},"minify":false,"source_maps":false}"#,
        )
        .unwrap();
        let config = Config::load_with_base_dir(Path::new("config.json"), dir.path()).unwrap();
        for i in 1..=2 {
            fs::write(dir.path().join("src/a/app.js"), format!("var a = {i};\n")).unwrap();
            AssetGraph::new(config.clone()).build().unwrap();
        }
        fs::rename(dir.path().join("src/a"), dir.path().join("src/b")).unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let old = &manifest.assets["js/a/app.js"];
        assert!(old.retired);
        for output in std::iter::once(&old.output).chain(old.history.iter().map(|h| &h.output)) {
            assert!(
                dir.path().join(output.trim_start_matches('/')).is_file(),
                "{output} restored"
            );
        }

        // With every copy gone the build still refuses.
        fs::remove_dir_all(dir.path().join("src/a")).unwrap();
        for entry in fs::read_dir(dir.path().join("src/b/_paste_build")).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        assert!(AssetGraph::new(config).build().is_err());
    }

    #[test]
    fn test_beside_source_mixed_type_directory_names_roundtrip_and_serve() {
        // Beside-source output lands beside its source, so a JS module
        // under a css/ source root records /css/_paste_build/…min.js and
        // a stylesheet under a js/ root records /js/_paste_build/…min.css:
        // an output pointer's first segment names the source directory,
        // not the asset's type. The manifest still saves and reloads, an
        // unchanged rebuild is byte-stable, retained history keeps
        // serving its exact pins, and a per-module render names the same
        // pointers.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("css")).unwrap();
        fs::create_dir_all(dir.path().join("js")).unwrap();
        fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/features/paste.js"),
            dir.path().join("css/paste.js"),
        )
        .unwrap();
        fs::write(
            dir.path().join("css/app.js"),
            "// @module app\nvar app = 1;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("js/theme.scss"),
            "// @module theme\n.theme { color: red; }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("config.json"),
            r#"{"source":{"dirs":["css","js"]},"output":{"base":"dist","js":"dist/js","css":"dist/css","images":"dist/images","layout":"beside-source"},"manifest_path":"dist/manifest.json","storage":{"backend":"dist","keep_versions":"all"},"minify":false,"source_maps":false}"#,
        )
        .unwrap();
        let config = Config::load_with_base_dir(Path::new("config.json"), dir.path()).unwrap();
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let manifest_bytes_v1 = fs::read(&config.manifest_path).unwrap();

        let app_v1 = manifest.assets["js/app.js"].output.clone();
        let paste_v1 = manifest.assets["js/paste.js"].output.clone();
        let theme_v1 = manifest.assets["css/theme.css"].output.clone();
        assert!(app_v1.starts_with("/css/_paste_build/"), "{app_v1}");
        assert!(paste_v1.starts_with("/css/_paste_build/"), "{paste_v1}");
        assert!(theme_v1.starts_with("/js/_paste_build/"), "{theme_v1}");

        // The exact pins serve the bytes just built.
        let dirs = graph.output_dirs.clone();
        let body = |manifest: &Manifest, path: &str| {
            let mut response = crate::jam::combo(manifest, &dirs, "/paste", path, None);
            for _ in 0..5 {
                if response.status != 302 {
                    break;
                }
                response = crate::jam::combo(
                    manifest,
                    &dirs,
                    "/paste",
                    &response.location.clone().unwrap(),
                    None,
                );
            }
            assert_eq!(response.status, 200, "{path}");
            response.body
        };
        let app_pin = pin(&manifest, "js/app.js", "app", 1, "js");
        let theme_pin = pin(&manifest, "css/theme.css", "theme", 1, "css");
        let app_body_v1 = body(&manifest, &app_pin);
        let theme_body_v1 = body(&manifest, &theme_pin);
        assert_eq!(
            app_body_v1,
            fs::read_to_string(dir.path().join(app_v1.trim_start_matches('/'))).unwrap()
        );
        assert_eq!(
            theme_body_v1,
            fs::read_to_string(dir.path().join(theme_v1.trim_start_matches('/'))).unwrap()
        );

        // An unchanged rebuild is byte-stable.
        let manifest = graph.build().unwrap();
        assert_eq!(fs::read(&config.manifest_path).unwrap(), manifest_bytes_v1);
        assert_eq!(manifest.assets["js/app.js"].output, app_v1);
        assert_eq!(manifest.assets["js/paste.js"].output, paste_v1);
        assert_eq!(manifest.assets["css/theme.css"].output, theme_v1);
        assert_eq!(manifest.assets["js/app.js"].version, 1);

        // Change each source once: v2 lands beside it, v1 retained.
        fs::write(
            dir.path().join("css/app.js"),
            "// @module app\nvar app = 2;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("js/theme.scss"),
            "// @module theme\n.theme { color: blue; }\n",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        let app = &manifest.assets["js/app.js"];
        let theme = &manifest.assets["css/theme.css"];
        assert_eq!(app.version, 2, "{app:?}");
        assert_eq!(theme.version, 2, "{theme:?}");
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history[0].output, app_v1);
        assert_eq!(theme.history[0].output, theme_v1);
        assert!(
            app.output.starts_with("/css/_paste_build/"),
            "{}",
            app.output
        );
        assert!(
            theme.output.starts_with("/js/_paste_build/"),
            "{}",
            theme.output
        );

        // Every recorded pointer — current and history — names a file on disk.
        for entry in manifest.assets.values() {
            for output in
                std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
            {
                assert!(
                    dir.path().join(output.trim_start_matches('/')).is_file(),
                    "{output}"
                );
            }
        }

        // The saved manifest reloads; the retained pins still serve v1.
        let loaded = Manifest::load(&config.manifest_path).unwrap();
        assert_eq!(loaded.assets["js/app.js"].version, 2);
        assert_eq!(body(&loaded, &app_pin), app_body_v1);
        assert_eq!(body(&loaded, &theme_pin), theme_body_v1);

        // A per-module render names the same current pointers.
        let opts = crate::jam::RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 0,
            per_module: true,
        };
        let rendered = crate::jam::render_all(
            &loaded,
            &graph.output_dirs,
            &opts,
            &[
                crate::jam::Event::Jam(crate::jam::Jam {
                    css: vec!["theme".to_string()],
                    js: vec!["app".to_string()],
                    ..Default::default()
                }),
                crate::jam::Event::Dump(crate::jam::DumpScope::All),
            ],
        )
        .unwrap();
        assert_eq!(
            rendered[0].urls,
            vec![theme.output.clone(), app.output.clone()],
            "{:?}",
            rendered[0].urls
        );
    }

    #[test]
    fn test_untrusted_recorded_source_roots_keep_their_sources() {
        // The cleanup guard must consult the same roots discovery walks —
        // the src/main/assets defaults when no source root is configured,
        // and every configured include path. A manifest recording either
        // kind of source root as a historical js output root, without a
        // generated_dirs record backing it, is untrusted provenance: the
        // source inside is reported, not deleted, and stays a module.
        use tempfile::tempdir;
        for (source_rel, include_paths) in [
            ("src/main/assets/js", vec![]),
            ("vendor/js", vec!["vendor/js"]),
        ] {
            let dir = tempdir().unwrap();
            let js_dir = dir.path().join(source_rel);
            fs::create_dir_all(&js_dir).unwrap();
            fs::create_dir_all(dir.path().join("src/main/assets/scss")).unwrap();
            fs::write(js_dir.join("assetgraph.js"), "var mine = 1;").unwrap();
            fs::write(
                dir.path().join("src/main/assets/scss/site.scss"),
                ".site { color: red; }",
            )
            .unwrap();
            // No source keys at all — the first case falls back to the
            // defaults, the second finds its root through include_paths.
            let include_json = include_paths
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(",");
            fs::write(
                dir.path().join("config.json"),
                format!(
                    r#"{{"source":{{}},"include_paths":[{include_json}],"output":{{"base":"dist","js":"dist/js","css":"dist/css","images":"dist/images"}},"manifest_path":"dist/manifest.json","storage":{{"backend":"dist","keep_versions":"all"}},"minify":false,"source_maps":false}}"#
                ),
            )
            .unwrap();
            let config = Config::load_with_base_dir(Path::new("config.json"), dir.path()).unwrap();
            let mut graph = AssetGraph::new(config.clone());
            let manifest = graph.build().unwrap();
            assert!(
                manifest.assets.contains_key("js/assetgraph.js"),
                "{source_rel} must yield a js/assetgraph.js module"
            );

            // Forge provenance: the source root recorded as a js output
            // root, with no generated_dirs entry for it.
            let mut manifest = manifest;
            manifest
                .output_roots
                .insert("js".to_string(), vec![source_rel.to_string()]);
            manifest.save(&config.manifest_path).unwrap();

            let manifest = graph.build().unwrap();
            assert_eq!(
                fs::read_to_string(js_dir.join("assetgraph.js")).unwrap(),
                "var mine = 1;",
                "a source file under an untrusted recorded root is never deleted: {source_rel}"
            );
            assert!(
                manifest.assets.contains_key("js/assetgraph.js"),
                "the source keeps its module: {source_rel}"
            );
        }
    }

    #[test]
    fn test_same_basename_in_another_generated_dir_does_not_establish_a_root() {
        // Nested modules a/dom and b/dom compile to identical basenames in
        // different recorded directories. A stray file carrying a real
        // emitted basename in another recorded dir (here the images root)
        // is not ownership evidence: only the configured root and roots
        // the manifest recorded as the type's root may lose an
        // assetgraph.{type} file — the consumer file at output.base and
        // the stray's parent must survive.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(js_dir.join("a")).unwrap();
        fs::create_dir_all(js_dir.join("b")).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a/dom.js"), "// @module a.dom\nvar a = 1;").unwrap();
        fs::write(js_dir.join("b/dom.js"), "// @module b.dom\nvar b = 1;").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.output.images = dir.path().join("dist/other");
        AssetGraph::new(config.clone()).build().unwrap();

        // A stray in a recorded generated dir named by a real emitted
        // basename (b/dom's compiled file), plus the consumer file the bad
        // inference would have deleted. Neither parent appears in
        // output_roots as a js root — generated_dirs records where files
        // were written, not which directory owned the rollup namespace —
        // so filename evidence alone cannot establish either.
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let emitted = manifest
            .assets
            .get("js/b/dom.js")
            .and_then(|entry| {
                Path::new(entry.output.trim_start_matches('/'))
                    .file_name()
                    .and_then(|name| name.to_str())
            })
            .expect("b/dom emitted basename")
            .to_string();
        fs::create_dir_all(dir.path().join("dist/other")).unwrap();
        fs::write(
            dir.path().join("dist/other").join(emitted),
            "var stray = 1;",
        )
        .unwrap();
        fs::write(dir.path().join("dist/assetgraph.js"), "consumer-owned").unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();

        AssetGraph::new(config).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("dist/assetgraph.js")).unwrap(),
            "consumer-owned",
            "output.base is not a type root"
        );
        assert!(
            !dir.path().join("dist/js/assetgraph.js").exists(),
            "the real js root's stale aggregate is still removed"
        );
    }

    #[test]
    fn test_identical_basename_nested_module_dir_is_not_a_type_root() {
        // a.js and sub/a.js with identical bytes compile to the same
        // filename at two depths: dist/js/a.<h>.v1.min.js and
        // dist/js/sub/a.<h>.v1.min.js. The nested module's parent is a
        // recorded generated directory but never a recorded type root, so
        // no amount of filename evidence can make it one — the
        // assetgraph.js sitting inside it survives.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(js_dir.join("sub")).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "var a = 1;").unwrap();
        fs::write(js_dir.join("sub/a.js"), "var a = 1;").unwrap();

        let config = dist_config(dir.path(), true);
        AssetGraph::new(config.clone()).build().unwrap();

        // A file the wrong inference would have deleted, and the stale
        // aggregate at the real type root that must still be removed.
        fs::write(
            dir.path().join("dist/js/sub/assetgraph.js"),
            "var mine = 1;",
        )
        .unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();

        AssetGraph::new(config).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("dist/js/sub/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "a module subdirectory is not a former type root"
        );
        assert!(
            !dir.path().join("dist/js/assetgraph.js").exists(),
            "the real js root's stale aggregate is still removed"
        );
    }

    #[test]
    fn test_copied_artifact_in_a_recorded_dir_does_not_establish_a_root() {
        // A byte-identical copy of a real compiled output under another
        // recorded generated dir is no more ownership evidence than a
        // stray name would be: the images root is generated territory but
        // never a type root, so its assetgraph.js survives — only the
        // real js root loses the stale name.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "var a = 1;").unwrap();

        let config = dist_config(dir.path(), true);
        AssetGraph::new(config.clone()).build().unwrap();

        // Copy the actual emitted file into the recorded images root.
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let emitted = manifest
            .assets
            .get("js/a.js")
            .and_then(|entry| {
                Path::new(entry.output.trim_start_matches('/'))
                    .file_name()
                    .and_then(|name| name.to_str())
            })
            .expect("a.js emitted basename")
            .to_string();
        fs::create_dir_all(dir.path().join("dist/images")).unwrap();
        fs::copy(
            dir.path().join("dist/js").join(&emitted),
            dir.path().join("dist/images").join(&emitted),
        )
        .unwrap();
        fs::write(
            dir.path().join("dist/images/assetgraph.js"),
            "var mine = 1;",
        )
        .unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();

        AssetGraph::new(config).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("dist/images/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "a copied artifact cannot make its directory a type root"
        );
        assert!(
            !dir.path().join("dist/js/assetgraph.js").exists(),
            "the real js root's stale aggregate is still removed"
        );
    }

    #[test]
    fn test_manifest_without_recorded_roots_reports_instead_of_deleting() {
        // A manifest written before output_roots existed cannot establish
        // the moved-from root: with the field absent, the stale name at
        // the old root is reported for explicit migration, not deleted.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(js_dir.join("a.js"), "var a = 1;").unwrap();

        let old_config = dist_config(dir.path(), true);
        AssetGraph::new(old_config.clone()).build().unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();

        // Simulate a manifest from before the field existed, then move
        // the js root so the old root is no longer the configured one.
        let manifest_json = fs::read_to_string(&old_config.manifest_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&manifest_json).unwrap();
        value.as_object_mut().unwrap().remove("output_roots");
        fs::write(
            &old_config.manifest_path,
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
        Manifest::remove_cache(&old_config.manifest_path).unwrap();

        let mut moved = dist_config(dir.path(), true);
        moved.output.js = dir.path().join("dist/js2");
        AssetGraph::new(moved).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("dist/js/assetgraph.js")).unwrap(),
            "var stale = 1;",
            "a root no manifest recorded is reported, not deleted"
        );
    }

    #[test]
    fn test_recorded_root_nested_under_a_source_root_is_cleaned() {
        // Generated output may live beneath a source root — discovery
        // excludes the generated directories. A manifest that recorded
        // assets/dist/js as the js root has full provenance for it:
        // moving the base to assets/public must still remove the stale
        // aggregate at the old root, while the same name in adjacent
        // generated-but-unowned directories and real sources survives.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/src")).unwrap();
        fs::write(dir.path().join("assets/src/app.js"), "var app = 1;").unwrap();
        fs::write(
            dir.path().join("assets/src/site.scss"),
            ".site { color: red; }",
        )
        .unwrap();

        let mut config = dirs_config(dir.path(), &["assets"], true);
        config.output.base = dir.path().join("assets/dist");
        config.output.js = dir.path().join("assets/dist/js");
        config.output.css = dir.path().join("assets/dist/css");
        config.output.images = dir.path().join("assets/dist/images");
        config.manifest_path = dir.path().join("assets/manifest.json");
        AssetGraph::new(config.clone()).build().unwrap();

        // Legacy aggregates at the recorded roots, plus lookalikes in a
        // recorded generated dir that was never a type root and in the
        // real source tree (a source file keeps its name even though it
        // is itself discovered as a module).
        fs::write(
            dir.path().join("assets/dist/js/assetgraph.js"),
            "var stale = 1;",
        )
        .unwrap();
        fs::write(
            dir.path().join("assets/dist/css/assetgraph.css"),
            "var stale = 1;",
        )
        .unwrap();
        fs::write(
            dir.path().join("assets/dist/assetgraph.js"),
            "var mine = 1;",
        )
        .unwrap();
        fs::write(dir.path().join("assets/src/assetgraph.js"), "var src = 1;").unwrap();

        let mut moved = config.clone();
        moved.output.base = dir.path().join("assets/public");
        moved.output.js = dir.path().join("assets/public/js");
        moved.output.css = dir.path().join("assets/public/css");
        moved.output.images = dir.path().join("assets/public/images");
        AssetGraph::new(moved).build().unwrap();

        assert!(
            !dir.path().join("assets/dist/js/assetgraph.js").exists(),
            "a recorded js root keeps deletion authority nested under sources"
        );
        assert!(
            !dir.path().join("assets/dist/css/assetgraph.css").exists(),
            "a recorded css root keeps deletion authority nested under sources"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("assets/dist/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "a recorded generated dir that was never a type root keeps its file"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("assets/src/assetgraph.js")).unwrap(),
            "var src = 1;",
            "a same-named source file is never a deletion target"
        );
    }

    #[test]
    fn test_flat_to_beside_source_migration_removes_only_the_old_root() {
        // The first flat build records dist/js in output_roots under the
        // js type; switching to beside-source moves outputs into
        // js/_paste_build/. The old flat root stays established by that
        // explicit record, while the source tree the beside-source labels
        // land in is never a deletion target.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("js")).unwrap();
        fs::write(dir.path().join("js/a.js"), "var a = 1;").unwrap();
        fs::write(dir.path().join("js/assetgraph.js"), "var mine = 1;").unwrap();

        let config = dirs_config(dir.path(), &["js"], true);
        AssetGraph::new(config.clone()).build().unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();

        let mut moved = dirs_config(dir.path(), &["js"], true);
        moved.output.layout = "beside-source".into();
        AssetGraph::new(moved).build().unwrap();

        assert!(
            !dir.path().join("dist/js/assetgraph.js").exists(),
            "the former flat root's stale aggregate is removed"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("js/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "the source tree is never a rollup location"
        );
        assert!(dir.path().join("js/_paste_build").is_dir());
    }

    #[test]
    fn test_beside_source_to_flat_migration_preserves_same_named_sources() {
        // The reverse move: labels like /js/_paste_build/a.min.js recorded
        // under beside-source must not retroactively make the source dir a
        // rollup root once the layout flips to flat.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("js")).unwrap();
        fs::write(dir.path().join("js/a.js"), "var a = 1;").unwrap();
        fs::write(dir.path().join("js/assetgraph.js"), "var mine = 1;").unwrap();

        AssetGraph::new(beside_source_config(dir.path(), &["js"], true))
            .build()
            .unwrap();
        assert!(dir.path().join("js/_paste_build").is_dir());

        let mut moved = dirs_config(dir.path(), &["js"], true);
        moved.output.layout = "flat".into();
        AssetGraph::new(moved).build().unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("js/assetgraph.js")).unwrap(),
            "var mine = 1;",
            "a same-named source file survives the layout change"
        );
        assert!(dir.path().join("dist/js").is_dir());
    }

    #[test]
    fn test_dist_manifest_bytes_are_stable_across_builds() {
        use tempfile::tempdir;

        // HashMap iteration order is randomized per process, so a manifest that
        // depends on it differs between builds of identical sources. Rebuilding
        // in-process cannot catch that; each build runs as its own process.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        for name in ["alpha", "beta", "gamma", "delta"] {
            fs::write(
                js_dir.join(format!("{name}.js")),
                format!("// @module {name}\n// @requires shared\nvar {name} = shared;"),
            )
            .unwrap();
        }
        fs::write(
            js_dir.join("shared.js"),
            "// @module shared\nvar shared = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n.main { color: red; }",
        )
        .unwrap();

        let manifest_path = dir.path().join("dist/manifest.json");
        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();
        let first = fs::read_to_string(&manifest_path).unwrap();

        // A fresh graph re-scans and re-orders everything from disk.
        let mut rebuilt = AssetGraph::new(dist_config(dir.path(), true));
        rebuilt.build().unwrap();
        let second = fs::read_to_string(&manifest_path).unwrap();

        assert_eq!(
            first, second,
            "a dist manifest must be byte-identical for identical sources"
        );
        assert!(
            !first.contains(dir.path().to_string_lossy().as_ref()),
            "a dist manifest must not record machine-specific paths: {first}"
        );
    }

    #[test]
    fn test_dist_output_carries_no_bincode_cache() {
        use tempfile::tempdir;

        // The cache is written from hash maps, so its bytes vary between runs.
        // Distributed output must not contain it, and a cache left by an earlier
        // build must be removed — load() trusts it only while it is no older
        // than the manifest.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();
        let cache = dir.path().join("dist/manifest.bin");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, b"stale cache").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();

        assert!(dir.path().join("dist/manifest.json").is_file());
        assert!(
            !cache.exists(),
            "a dist build must not leave a bincode cache behind"
        );
    }

    #[test]
    fn test_dot_dot_spelled_in_project_root_keeps_directory_labels() {
        // `src/../assets` is an in-project root: its labels keep their
        // directories, so two same-named files never share one label.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/a")).unwrap();
        fs::create_dir_all(dir.path().join("assets/b")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("assets/a/app.js"),
            "// @module a.app\nvar a = 1;",
        )
        .unwrap();
        fs::write(
            dir.path().join("assets/b/app.js"),
            "// @module b.app\nvar b = 1;",
        )
        .unwrap();
        let mut config = dirs_config(dir.path(), &[], false);
        config.source.dirs = vec![dir.path().join("src/../assets")];
        let manifest = AssetGraph::new(config).build().unwrap();
        let mut labels: Vec<_> = manifest.assets.values().map(|e| e.source.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["assets/a/app.js", "assets/b/app.js"]);
    }

    #[test]
    fn test_css_carry_never_steals_a_key_a_live_source_owns() {
        // a.scss (`@module theme`) moves to b.scss while a new a.scss lands
        // in the same build: css/a.css belongs to the new file, active.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("a.scss"),
            "// @module theme\n.t { color: red; }\n",
        )
        .unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        fs::rename(scss.join("a.scss"), scss.join("b.scss")).unwrap();
        fs::write(scss.join("a.scss"), ".fresh { color: blue; }\n").unwrap();
        let manifest = AssetGraph::new(config).build().unwrap();
        let a = &manifest.assets["css/a.css"];
        assert!(!a.retired, "the live a.scss owns css/a.css");
        assert!(a.history.iter().all(|h| h.version < a.version));
        assert!(manifest.modules.contains_key("theme"));
    }

    #[test]
    fn test_js_module_name_ending_in_js_is_refused() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(
            dir.path().join("src/js/chart.js"),
            "// @module chart.js\nvar c = 1;",
        )
        .unwrap();
        let err = AssetGraph::new(dist_config(dir.path(), false))
            .build()
            .expect_err("a JS module name ending in `.js` cannot load");
        assert!(format!("{err:#}").contains("script URL"), "{err:#}");
    }

    #[test]
    fn test_source_label_is_project_relative() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        let entry = manifest.assets.get("js/app.js").expect("app asset");
        assert_eq!(entry.source, "src/js/app.js");
        // The label resolves from the project root, which is what a debug
        // response or an inlined bootstrap reads.
        assert!(
            dir.path().join(&entry.source).is_file(),
            "source label should resolve: {}",
            entry.source
        );
    }

    #[test]
    fn test_source_label_for_an_include_path_names_the_include_directory() {
        use tempfile::tempdir;

        // A module pulled in from outside the project cannot be project-relative;
        // it is labelled relative to the include directory, which is stable
        // across machines, instead of by an absolute path.
        let project = tempdir().unwrap();
        let library = tempdir().unwrap();
        let js_dir = project.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(library.path().join("modules/widget")).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\n// @requires paste.ui.widget\nvar app = 1;",
        )
        .unwrap();
        fs::write(
            library.path().join("modules/widget/widget.js"),
            "// @module paste/ui/widget\nvar widget = 1;",
        )
        .unwrap();

        let mut config = dist_config(project.path(), true);
        config.include_paths = vec![library.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = manifest
            .assets
            .get("js/paste/ui/widget.js")
            .expect("include path asset");
        let library_name = library
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            entry.source,
            format!("{library_name}/modules/widget/widget.js")
        );
        assert!(
            !entry.source.starts_with('/'),
            "no absolute path: {}",
            entry.source
        );
    }

    #[test]
    fn test_include_path_build_output_is_not_scanned_as_source() {
        use tempfile::tempdir;

        // An include path points at another project, whose output directory this
        // build knows nothing about. Walking into it registers compiled copies as
        // modules, so the graph gains entries no source declares.
        let project = tempdir().unwrap();
        let library = tempdir().unwrap();
        let js_dir = project.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(library.path().join("modules/widget")).unwrap();
        fs::create_dir_all(library.path().join("target/example/js/paste")).unwrap();
        fs::create_dir_all(library.path().join("node_modules/vendor")).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 1;").unwrap();
        fs::write(
            library.path().join("modules/widget/widget.js"),
            "// @module paste/ui/widget\nvar widget = 1;",
        )
        .unwrap();
        fs::write(
            library.path().join("modules/widget/widget.scss"),
            "/* @module paste.ui.widget */\n.widget { color: red; }\n",
        )
        .unwrap();
        fs::write(
            library.path().join("target/example/js/paste/widget.min.js"),
            "var widget=1;",
        )
        .unwrap();
        fs::write(
            library
                .path()
                .join("target/example/js/paste/widget.min.scss"),
            ".widget { color: blue; }\n",
        )
        .unwrap();
        fs::write(
            library.path().join("node_modules/vendor/vendor.js"),
            "var vendor = 1;",
        )
        .unwrap();

        let mut config = dist_config(project.path(), true);
        config.include_paths = vec![library.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        assert!(
            manifest.assets.contains_key("js/paste/ui/widget.js"),
            "declared source module is scanned"
        );
        let built: Vec<&String> = manifest
            .assets
            .keys()
            .filter(|key| key.contains("target/") || key.contains("node_modules/"))
            .collect();
        assert!(
            built.is_empty(),
            "build output must not enter the graph: {built:?}"
        );
    }

    #[test]
    fn test_declared_modules_map_to_manifest_asset_keys() {
        use tempfile::tempdir;

        // A declared name that does not match the file path must still resolve:
        // consumers look the value up in `assets`.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("site.js"),
            "// @module paste.site\nvar site = 1;",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        let key = manifest
            .modules
            .get("paste.site")
            .expect("declared module in manifest");
        assert_eq!(key, "js/paste/site.js");
        assert!(
            manifest.assets.contains_key(key),
            "modules map must name an existing asset: {key}"
        );
    }

    #[test]
    fn test_one_name_declared_for_both_types_stays_resolvable() {
        use tempfile::tempdir;

        // The modules map has one entry per name. A script and a stylesheet may
        // declare the same name, and both must remain resolvable by type.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            js_dir.join("widget.js"),
            "// @module widget\nvar widget = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("widget.scss"),
            "// @module widget\n.widget { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        assert_eq!(
            manifest.asset_key_for_module("widget", "js").as_deref(),
            Some("js/widget.js")
        );
        assert_eq!(
            manifest.asset_key_for_module("widget", "css").as_deref(),
            Some("css/widget.css")
        );
    }

    #[test]
    fn test_declared_name_unrelated_to_its_path_keeps_its_mapping() {
        use tempfile::tempdir;

        // A stylesheet whose declared name does not match its path is the case
        // the modules map exists for: the convention key cannot find it, so the
        // mapping must survive a same-name script declaration.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            js_dir.join("theme.js"),
            "// @module site/theme\nvar theme = 1;",
        )
        .unwrap();
        fs::write(
            scss_dir.join("theme.scss"),
            "// @module site/theme\n.theme { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph.build().unwrap();

        // css/theme.css is not derivable from "site.theme", so it is the entry
        // the map must keep; the script resolves through the convention key.
        assert_eq!(
            manifest.modules.get("site.theme").map(String::as_str),
            Some("css/theme.css")
        );
        assert_eq!(
            manifest
                .asset_key_for_module("site.theme", "css")
                .as_deref(),
            Some("css/theme.css")
        );
        assert_eq!(
            manifest.asset_key_for_module("site.theme", "js").as_deref(),
            Some("js/site/theme.js")
        );
    }

    #[test]
    fn test_output_is_rewritten_when_the_transform_changes() {
        use tempfile::tempdir;

        // The same source printed by a different pipeline is different bytes, so
        // it must not reuse the previous output file name.
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\nvar app = function () { return 1; };",
        )
        .unwrap();

        let mut minifying = AssetGraph::new(dist_config(dir.path(), true));
        let minified = minifying.build().unwrap();
        let mut copying = AssetGraph::new(dist_config(dir.path(), false));
        let verbatim = copying.build().unwrap();

        let minified_output = minified.assets.get("js/app.js").unwrap().output.clone();
        let verbatim_output = verbatim.assets.get("js/app.js").unwrap().output.clone();
        assert_ne!(
            minified_output, verbatim_output,
            "different output bytes must not share an output file name"
        );
    }

    #[test]
    fn test_scss_without_module_not_pruned() {
        // Simulate the pruning logic from build()
        // CSS key is always tracked, even without @module
        let mut seen_asset_keys: HashSet<String> = HashSet::new();
        let mut assets: HashMap<String, String> = HashMap::new();

        // JS asset (has module, tracked in seen_asset_keys)
        let js_key = "js/paste/dom.js".to_string();
        seen_asset_keys.insert(js_key.clone());
        assets.insert(js_key, "js output".to_string());

        // CSS asset WITHOUT @module — key is always tracked
        let css_key = "css/main.css".to_string();
        assets.insert(css_key.clone(), "css output".to_string());
        // Always insert key regardless of @module
        seen_asset_keys.insert(css_key);

        // Prune (mirrors build() prune step)
        assets.retain(|key, _| seen_asset_keys.contains(key));

        // CSS asset should survive pruning
        assert!(
            assets.contains_key("css/main.css"),
            "CSS without @module should NOT be pruned from manifest"
        );
    }

    // ============================================================
    // Output-byte hashing: a rebuild changes the fingerprint only
    // when the emitted bytes change
    // ============================================================

    /// Build twice in `dir` with `minify`, mutating `js/app.js` between the
    /// builds; returns the two manifests' app entries.
    fn rebuild_with_js(
        dir: &Path,
        minify: bool,
        first: &str,
        second: &str,
    ) -> (Manifest, Manifest) {
        let js_dir = dir.join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), first).unwrap();
        let manifest1 = AssetGraph::new(dist_config(dir, minify)).build().unwrap();
        fs::write(js_dir.join("app.js"), second).unwrap();
        let manifest2 = AssetGraph::new(dist_config(dir, minify)).build().unwrap();
        (manifest1, manifest2)
    }

    #[test]
    fn test_js_literal_whitespace_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nwindow.label = 'a b';\n",
                "// @module app\nwindow.label = 'a  b';\n",
            );
            let e1 = &m1.assets["js/app.js"];
            let e2 = &m2.assets["js/app.js"];
            assert_ne!(e1.hash, e2.hash, "minify={minify}: hash must change");
            assert_eq!(e1.version + 1, e2.version, "minify={minify}");
            assert_ne!(e1.output, e2.output, "minify={minify}");
            let out2 = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(e2.output.trim_start_matches('/')),
            )
            .unwrap();
            assert!(
                out2.contains("a  b"),
                "minify={minify}: output must carry the change: {out2}"
            );
        }
    }

    #[test]
    fn test_js_asi_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nfunction f() { return\n1; } window.f = f;\n",
                "// @module app\nfunction f() { return 1; } window.f = f;\n",
            );
            assert_ne!(
                m1.assets["js/app.js"].hash, m2.assets["js/app.js"].hash,
                "minify={minify}: ASI changes the emitted bytes"
            );
        }
    }

    #[test]
    fn test_js_regex_and_nested_template_changes_roll_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let (m1, m2) = rebuild_with_js(
                dir.path(),
                minify,
                "// @module app\nvar re = /a b/; var t = `x${1}`;\n",
                "// @module app\nvar re = /a  b/; var t = `x${2}`;\n",
            );
            assert_ne!(
                m1.assets["js/app.js"].hash, m2.assets["js/app.js"].hash,
                "minify={minify}: regex/template contents are output bytes"
            );
        }
    }

    #[test]
    fn test_comment_only_change_hashes_by_emitted_bytes() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let (minified1, minified2) = rebuild_with_js(
            dir.path(),
            true,
            "// @module app\n// first comment\nvar app = 1;\n",
            "// @module app\n// second comment\nvar app = 1;\n",
        );
        assert_eq!(
            minified1.assets["js/app.js"].hash, minified2.assets["js/app.js"].hash,
            "comments do not reach the minified output, so the hash must not move"
        );
        assert_eq!(
            minified1.assets["js/app.js"].version,
            minified2.assets["js/app.js"].version
        );

        // In verbatim mode comments ARE the output: the same edit is a change.
        let dir2 = tempdir().unwrap();
        let (verbatim1, verbatim2) = rebuild_with_js(
            dir2.path(),
            false,
            "// @module app\n// first comment\nvar app = 1;\n",
            "// @module app\n// second comment\nvar app = 1;\n",
        );
        assert_ne!(
            verbatim1.assets["js/app.js"].hash, verbatim2.assets["js/app.js"].hash,
            "verbatim output carries comments, so the hash must move"
        );
    }

    #[test]
    fn test_css_literal_whitespace_change_rolls_the_version() {
        use tempfile::tempdir;
        for minify in [true, false] {
            let dir = tempdir().unwrap();
            let scss_dir = dir.path().join("src/scss");
            fs::create_dir_all(&scss_dir).unwrap();
            fs::write(
                scss_dir.join("app.scss"),
                "// @module app\n.x::before { content: 'a b'; }\n",
            )
            .unwrap();
            let m1 = AssetGraph::new(dist_config(dir.path(), minify))
                .build()
                .unwrap();
            fs::write(
                scss_dir.join("app.scss"),
                "// @module app\n.x::before { content: 'a  b'; }\n",
            )
            .unwrap();
            let m2 = AssetGraph::new(dist_config(dir.path(), minify))
                .build()
                .unwrap();
            let e1 = &m1.assets["css/app.css"];
            let e2 = &m2.assets["css/app.css"];
            assert_ne!(e1.hash, e2.hash, "minify={minify}: hash must change");
            assert_eq!(e1.version + 1, e2.version, "minify={minify}");
            let out2 = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(e2.output.trim_start_matches('/')),
            )
            .unwrap();
            assert!(
                out2.contains("a  b"),
                "minify={minify}: output must carry the change: {out2}"
            );
        }
    }

    // ============================================================
    // Module name validation
    // ============================================================

    #[test]
    fn test_build_rejects_an_invalid_js_module_name() {
        use tempfile::tempdir;
        for declared in ["/tmp", "a..b", ".name", ".."] {
            let dir = tempdir().unwrap();
            let js_dir = dir.path().join("src/js");
            fs::create_dir_all(&js_dir).unwrap();
            fs::write(
                js_dir.join("evil.js"),
                format!("// @module {declared}\nvar x = 1;\n"),
            )
            .unwrap();

            let mut graph = AssetGraph::new(dist_config(dir.path(), false));
            assert!(
                graph.build().is_err(),
                "@module {declared:?} must fail the build"
            );
            // Nothing may be written outside the output tree.
            assert!(
                !dir.path().join("tmp").exists(),
                "@module {declared:?} must not write outside dist"
            );
        }
    }

    #[test]
    fn test_build_rejects_an_invalid_js_requires_name() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\n// @requires ../escape\nvar app = 1;\n",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        assert!(graph.build().is_err());
    }

    #[test]
    fn test_build_rejects_an_invalid_scss_module_name() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("evil.scss"),
            "// @module ../escape\n.e { color: red; }\n",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        assert!(graph.build().is_err());
        assert!(!dir.path().join("escape").exists());
    }

    #[test]
    fn test_nested_source_paths_use_forward_slashes_in_keys() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js/nested");
        let scss_dir = dir.path().join("src/scss/nested");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(js_dir.join("deep.js"), "var deep = 1;").unwrap();
        fs::write(scss_dir.join("deep.scss"), ".deep { color: red; }").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        let manifest = graph.build().unwrap();

        assert!(manifest.assets.contains_key("js/nested/deep.js"));
        assert!(manifest.assets.contains_key("css/nested/deep.css"));
        for key in manifest.assets.keys() {
            assert!(!key.contains('\\'), "asset key has a backslash: {key}");
        }
        for name in manifest.modules.keys() {
            assert!(!name.contains('\\'), "module name has a backslash: {name}");
        }
    }

    /// A pid no live process on this host holds. Found by probing, not by
    /// forking a child: a fork in a parallel test briefly shares every open
    /// descriptor — another test's held build lock among them.
    fn dead_pid() -> u32 {
        (2..99_999u32)
            .rev()
            .find(|pid| !staging_writer_may_be_alive(*pid))
            .unwrap()
    }

    /// The versioned compiled files directly in `dir`, sorted.
    fn versioned_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| crate::storage::is_versioned_file(Path::new(name)))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn test_bounded_build_retries_a_prune_that_failed() {
        // keep_versions = 1: the third build retires v1, but its unlink
        // fails (a directory stands in for a file a server holds open).
        // The manifest that build publishes no longer names v1, so the
        // retry must come from the pending journal: once the obstacle
        // clears, the next build removes it and the journal goes away.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        let mut config = dist_config(dir.path(), false);
        config.storage.keep_versions = 1;
        let out = dir.path().join("dist/js");
        let journal = pending_journal_path(&config);
        let build = |body: &str| {
            fs::write(js.join("a.js"), body).unwrap();
            AssetGraph::new(config.clone()).build().unwrap()
        };
        build("var a = 1;");
        build("var a = 2;");
        let v1 = versioned_in(&out)
            .into_iter()
            .find(|name| name.contains(".v1."))
            .unwrap();
        fs::remove_file(out.join(&v1)).unwrap();
        fs::create_dir(out.join(&v1)).unwrap();
        fs::write(out.join(&v1).join("held"), "x").unwrap();
        build("var a = 3;");
        assert!(out.join(&v1).exists());
        assert!(
            fs::read_to_string(&journal).unwrap().contains(&v1),
            "the failed unlink is recorded for the next build"
        );

        fs::remove_dir_all(out.join(&v1)).unwrap();
        fs::write(out.join(&v1), "the unblocked file").unwrap();
        build("var a = 4;");
        assert!(
            !out.join(&v1).exists(),
            "the retired version was never retried"
        );
        assert!(!journal.exists(), "a settled journal is removed");
        assert_eq!(versioned_in(&out).len(), 2, "{:?}", versioned_in(&out));
    }

    #[test]
    fn test_next_build_removes_a_killed_builds_outputs() {
        // A build killed after writing a new version leaves the file and
        // its journal entry; nothing published names it. The next build
        // removes it — under a bound as under keep-all — while a versioned
        // file no journal or manifest lists stays where it is.
        use tempfile::tempdir;
        for keep in [1, usize::MAX] {
            let dir = tempdir().unwrap();
            let js = dir.path().join("src/js");
            fs::create_dir_all(&js).unwrap();
            fs::write(js.join("a.js"), "var a = 1;").unwrap();
            let mut config = dist_config(dir.path(), false);
            config.storage.keep_versions = keep;
            AssetGraph::new(config.clone()).build().unwrap();
            let out = dir.path().join("dist/js");
            let orphan = format!("a.{}.v2.min.js", "e".repeat(32));
            let stray = format!("a.{}.v9.min.js", "f".repeat(32));
            fs::write(out.join(&orphan), "half a build").unwrap();
            fs::write(out.join(&stray), "a copy from elsewhere").unwrap();
            let journal = pending_journal_path(&config);
            fs::write(&journal, format!("dist/js/{orphan}\n")).unwrap();

            AssetGraph::new(config).build().unwrap();
            assert!(!out.join(&orphan).exists(), "keep {keep}: orphan survived");
            assert!(out.join(&stray).exists(), "keep {keep}: stray removed");
            assert!(!journal.exists(), "keep {keep}: journal left behind");
        }
    }

    #[test]
    fn test_journal_never_outlives_a_publish_or_a_lost_manifest() {
        // A build killed after publishing (here: the build body without
        // its closing settle) must not leave the version it published in
        // the journal — or a later manifest loss, or an older committed
        // manifest restored over it, would make the next build delete the
        // newest version. And a journal entry met with no manifest at all
        // is kept, never removed.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        let config = dist_config(dir.path(), false);
        let out = dir.path().join("dist/js");
        let journal = pending_journal_path(&config);
        fs::write(js.join("a.js"), "var a = 1;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        let older = fs::read(&config.manifest_path).unwrap();

        fs::write(js.join("a.js"), "var a = 2;").unwrap();
        let mut graph = AssetGraph::new(config.clone());
        let published = graph.build_locked().unwrap();
        let v2 = Path::new(&published.assets["js/a.js"].output)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            !fs::read_to_string(&journal)
                .unwrap_or_default()
                .contains(&v2),
            "the published version is still journaled"
        );

        // An older committed manifest restored over the published one, and
        // the source moved on so nothing rebuilds v2.
        fs::write(&config.manifest_path, &older).unwrap();
        fs::write(js.join("a.js"), "var a = 9;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            out.join(&v2).exists(),
            "restoring an older manifest deleted v2"
        );

        // A journaled orphan met with the manifest gone entirely stays, and
        // is forgotten rather than judged by the next lineage.
        let orphan = format!("a.{}.v7.min.js", "e".repeat(32));
        fs::write(out.join(&orphan), "half a build").unwrap();
        fs::write(&journal, format!("dist/js/{orphan}\n")).unwrap();
        fs::remove_file(&config.manifest_path).unwrap();
        fs::write(js.join("a.js"), "var a = 3;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(out.join(&v2).exists(), "a manifest-less build deleted v2");
        assert!(
            out.join(&orphan).exists(),
            "a manifest-less build removed a journaled file"
        );
        AssetGraph::new(config).build().unwrap();
        assert!(
            out.join(&orphan).exists(),
            "the next lineage removed a journaled file"
        );
    }

    #[test]
    fn test_a_failed_build_without_a_manifest_removes_its_own_outputs() {
        // No manifest yet: the earlier journal's entries are forgotten, but
        // this build's own new files — never named, adopted by no one under
        // the lock — leave with the failure.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(dir.path().join("src/js/a.js"), "var a = 1;").unwrap();
        fs::write(dir.path().join("src/scss/s.scss"), ".x{color: $undefined}").unwrap();
        let config = dist_config(dir.path(), false);
        for _ in 0..3 {
            assert!(AssetGraph::new(config.clone()).build().is_err());
        }
        let out = dir.path().join("dist/js");
        let left = if out.exists() {
            versioned_in(&out)
        } else {
            Vec::new()
        };
        assert!(left.is_empty(), "failed manifest-less builds left {left:?}");
        assert!(!pending_journal_path(&config).exists());
    }

    #[test]
    fn test_a_failed_publish_removes_the_versions_it_wrote() {
        // The journal forgets the about-to-be-published versions before
        // the publish (so a kill after it never lists a published file);
        // when the publish itself fails, the closing settle still sees the
        // build's files and the manifest that never named them.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("a.js"), "var a = 1;").unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        let out = dir.path().join("dist/js");
        let before = versioned_in(&out);
        fs::write(js.join("a.js"), "var a = 2;").unwrap();
        crate::manifest::test_hooks::FAIL_PUBLISH.with(|f| f.set(true));
        let error = AssetGraph::new(config.clone()).build().unwrap_err();
        crate::manifest::test_hooks::FAIL_PUBLISH.with(|f| f.set(false));
        assert!(format!("{error:#}").contains("injected"), "{error:#}");
        assert_eq!(versioned_in(&out), before, "the unpublished v2 leaked");
        assert!(!pending_journal_path(&config).exists());
    }

    #[test]
    fn test_one_journal_for_every_spelling_of_a_project() {
        // A symlinked project root, an absolute or relative manifest: one
        // physical manifest, one journal — or one spelling's journal would
        // hide a pending file from the other's guard.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let project = dir.path().join("proj");
        fs::create_dir_all(project.join("dist")).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&project, &link).unwrap();
        #[cfg(not(unix))]
        return;
        let spelled = |base: &Path, manifest: PathBuf| {
            let mut config = dist_config(base, false);
            config.manifest_path = manifest;
            pending_journal_path(&config)
        };
        let journals = [
            spelled(&project, project.join("dist/manifest.json")),
            spelled(&link, link.join("dist/manifest.json")),
            spelled(&link, project.join("dist/manifest.json")),
            spelled(&project, link.join("dist/manifest.json")),
        ];
        let physical = |path: &Path| {
            (
                fs::canonicalize(path.parent().unwrap()).unwrap(),
                path.file_name().unwrap().to_os_string(),
            )
        };
        for journal in &journals[1..] {
            assert_eq!(physical(journal), physical(&journals[0]), "{journals:?}");
        }
    }

    #[test]
    fn test_a_moved_project_keeps_its_journal() {
        // The journal is named by the manifest relative to the project, so
        // moving or renaming the project as a whole finds the same one.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        fs::create_dir_all(before.join("dist")).unwrap();
        let name = |base: &Path| {
            pending_journal_path(&dist_config(base, false))
                .file_name()
                .unwrap()
                .to_os_string()
        };
        let original = name(&before);
        fs::rename(&before, &after).unwrap();
        assert_eq!(name(&after), original);
    }

    #[test]
    fn test_a_published_version_never_enters_the_journal_after_publication() {
        // Two routes once let the closing settle take a version the build
        // had just published: a transient failure to read the manifest
        // (the published files journaled, then deleted by a later older
        // manifest), and the manifest deleted before the settle (the
        // published files removed as this build's own).
        use tempfile::tempdir;
        for route in ["unreadable", "deleted"] {
            let dir = tempdir().unwrap();
            let js = dir.path().join("src/js");
            fs::create_dir_all(&js).unwrap();
            fs::write(js.join("a.js"), "var a = 1;").unwrap();
            let config = dist_config(dir.path(), false);
            AssetGraph::new(config.clone()).build().unwrap();
            let older = fs::read(&config.manifest_path).unwrap();
            fs::write(js.join("a.js"), "var a = 2;").unwrap();
            let mut graph = AssetGraph::new(config.clone());
            let published = graph.build_locked().unwrap();
            let v2 = dir
                .path()
                .join("dist")
                .join(published.assets["js/a.js"].output.trim_start_matches('/'));
            if route == "unreadable" {
                fs::write(&config.manifest_path, "{ not json").unwrap();
            } else {
                fs::remove_file(&config.manifest_path).unwrap();
            }
            let _ = fs::remove_file(config.manifest_path.with_extension("bin"));
            graph.settle_pending();
            assert!(v2.exists(), "{route}: the closing settle removed v2");
            let journal = fs::read_to_string(pending_journal_path(&config)).unwrap_or_default();
            assert!(journal.is_empty(), "{route}: v2 journaled: {journal}");
            // A later older manifest must find nothing of v2 to remove.
            fs::write(&config.manifest_path, &older).unwrap();
            fs::write(js.join("a.js"), "var a = 3;").unwrap();
            AssetGraph::new(config).build().unwrap();
            assert!(
                v2.exists(),
                "{route}: v2 deleted after the older manifest came back"
            );
        }
    }

    #[test]
    fn test_journal_is_rewritten_not_appended_and_drops_what_never_settles() {
        // An unreadable manifest settles nothing, but the journal must not
        // grow build after build; an entry outside the project and every
        // output root, or one that is no versioned name, can never settle
        // and leaves the journal while its file stays.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("a.js"), "var a = 1;").unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        let out = dir.path().join("dist/js");
        let journal = pending_journal_path(&config);
        let orphan = format!("a.{}.v2.min.js", "e".repeat(32));
        fs::write(out.join(&orphan), "x").unwrap();
        let victim = outside
            .path()
            .join(format!("lib.{}.v1.min.js", "c".repeat(32)));
        fs::write(&victim, "someone else's").unwrap();
        let climbing = dir
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap())
            .join(victim.file_name().unwrap());
        fs::write(
            &journal,
            format!(
                "dist/js/{orphan}\ndist/js/{orphan}\n{}\n{}\nnotes.txt\n",
                victim.display(),
                climbing.display()
            ),
        )
        .unwrap();

        fs::write(&config.manifest_path, "{ not json").unwrap();
        let _ = fs::remove_file(config.manifest_path.with_extension("bin"));
        let mut sizes = Vec::new();
        for _ in 0..4 {
            assert!(AssetGraph::new(config.clone()).build().is_err());
            sizes.push(fs::read_to_string(&journal).unwrap().lines().count());
        }
        assert_eq!(
            sizes,
            vec![1, 1, 1, 1],
            "the journal grew or kept unsettleable entries"
        );
        assert!(victim.exists() && out.join(&orphan).exists());
    }

    #[test]
    fn test_stale_staging_ignores_a_symlinked_output_root() {
        // An output root spelled beneath the project but linked elsewhere
        // is not the project's directory: residue there stays.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("a.js"), "var a = 1;").unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dist/css")).unwrap();
        #[cfg(not(unix))]
        return;
        let gone = dead_pid();
        let residue = outside
            .path()
            .join(format!(".paste-staged.file.{gone}.0.0123456789abcdef"));
        fs::write(&residue, "torn").unwrap();
        fs::File::options()
            .write(true)
            .open(&residue)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        let _ = AssetGraph::new(dist_config(dir.path(), false)).build();
        assert!(
            residue.exists(),
            "residue behind a symlinked root was removed"
        );
    }

    #[test]
    fn test_watch_backend_panic_requests_a_restart() {
        // The kqueue backend's thread can panic on a queued event for a
        // registration it already retired. Every watch in the process must
        // learn of it — its flag for the restart, and a message to wake a
        // blocked recv — and a finished watch is no longer told.
        let ordering = std::sync::atomic::Ordering::SeqCst;
        install_watch_backend_monitor();
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();
        let first = register_watch_wake(tx1);
        let second = register_watch_wake(tx2);
        let die = || {
            std::thread::Builder::new()
                .name("notify-rs kqueue loop".into())
                .spawn(|| panic!("event for an unwatched descriptor"))
                .unwrap()
                .join()
        };
        assert!(die().is_err());
        for (registration, rx) in [(&first, &rx1), (&second, &rx2)] {
            assert!(registration.died.swap(false, ordering));
            let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(message.is_err(), "the wake-up is an error event");
        }
        drop(first);
        assert!(die().is_err());
        assert!(second.died.swap(false, ordering));
        assert!(rx1.try_recv().is_err(), "a finished watch was still woken");
        // A panic on any other thread is not the watcher's.
        let _ = std::thread::Builder::new()
            .name("unrelated".into())
            .spawn(|| panic!("elsewhere"))
            .unwrap()
            .join();
        assert!(!second.died.load(ordering));
    }

    #[test]
    fn test_failed_build_removes_the_versions_it_wrote() {
        // Keep-all never prunes, so a failed build's freshly written
        // versions — which no published manifest names — would pile up in
        // committed output. The failed build removes what it created; the
        // published version and a file that predates the build stay.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&js).unwrap();
        fs::create_dir_all(&scss).unwrap();
        fs::write(js.join("a.js"), "var a = 0;").unwrap();
        fs::write(scss.join("s.scss"), ".x{color:red}").unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        let out = dir.path().join("dist/js");
        let published = versioned_in(&out);
        assert_eq!(published.len(), 1, "{published:?}");
        let foreign = format!("a.{}.v9.min.js", "f".repeat(32));
        fs::write(out.join(&foreign), "a copy from elsewhere").unwrap();

        fs::write(scss.join("s.scss"), ".x{color: $undefined}").unwrap();
        for i in 11..=13 {
            fs::write(js.join("a.js"), format!("var a = {i};")).unwrap();
            assert!(AssetGraph::new(config.clone()).build().is_err());
        }
        assert!(
            !pending_journal_path(&config).exists(),
            "a failed build that settled everything leaves no journal"
        );
        let mut expected = published.clone();
        expected.push(foreign.clone());
        expected.sort();
        assert_eq!(
            versioned_in(&out),
            expected,
            "a failed build left versions no manifest names"
        );

        fs::write(scss.join("s.scss"), ".x{color:blue}").unwrap();
        fs::write(js.join("a.js"), "var a = 14;").unwrap();
        let manifest = AssetGraph::new(config).build().unwrap();
        assert_eq!(manifest.assets["js/a.js"].version, 2);
        assert_eq!(versioned_in(&out).len(), 3, "{:?}", versioned_in(&out));
    }

    #[test]
    fn test_bounded_build_prunes_only_what_its_manifest_recorded() {
        // A numeric bound retires versions the loaded manifest recorded.
        // A versioned file it never named — a copy from elsewhere, or the
        // whole history once the manifest is lost — may back saved pages
        // and must survive the next build.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        let mut config = dist_config(dir.path(), false);
        config.storage.keep_versions = 1;
        let out = dir.path().join("dist/js");
        let build = |body: &str| {
            fs::write(js.join("a.js"), body).unwrap();
            AssetGraph::new(config.clone()).build().unwrap()
        };
        build("var a = 1;");
        build("var a = 2;");
        let foreign = format!("a.{}.v9.min.js", "f".repeat(32));
        fs::write(out.join(&foreign), "a copy from elsewhere").unwrap();
        build("var a = 3;");
        let kept = versioned_in(&out);
        assert_eq!(
            kept.len(),
            3,
            "the bound keeps current plus one, and the unrecorded copy: {kept:?}"
        );
        assert!(kept.contains(&foreign));

        // No manifest at all: nothing on disk is a prune candidate.
        fs::remove_file(&config.manifest_path).unwrap();
        let before = versioned_in(&out);
        let restarted = build("var a = 5;");
        let after = versioned_in(&out);
        for name in &before {
            assert!(after.contains(name), "{name} was pruned without a manifest");
        }

        // Ordinary retention still works from the manifest that build
        // published: two more versions retire the restarted v1.
        let first_of_lineage = Path::new(&restarted.assets["js/a.js"].output)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        build("var a = 6;");
        assert!(versioned_in(&out).contains(&first_of_lineage));
        build("var a = 7;");
        let survivors = versioned_in(&out);
        assert!(
            !survivors.contains(&first_of_lineage),
            "a version the loaded manifest retired beyond the bound is pruned: {survivors:?}"
        );
        for name in &before {
            assert!(survivors.contains(name), "{name} was pruned");
        }
    }

    #[test]
    fn test_build_removes_stale_staging_residue() {
        // A killed build leaves `.paste-staged.*` files beside its outputs
        // and its manifest. An output directory can be shared beyond this
        // project's lock, so the next build removes a staging file only
        // when it is old, its writer is gone, and it sits beneath the
        // project: a fresh one, a live writer's, one outside the project,
        // and any other name stay.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("a.js"), "var a = 1;").unwrap();
        let mut config = dist_config(dir.path(), false);
        config.output.css = outside.path().join("css");
        AssetGraph::new(config.clone()).build().unwrap();
        fs::create_dir_all(&config.output.css).unwrap();
        let gone = dead_pid();
        let live = std::process::id();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let plant = |path: PathBuf, stale: bool| {
            fs::write(&path, "torn").unwrap();
            if stale {
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
            path
        };
        let name = |kind: &str, pid: u32| format!(".paste-staged.{kind}.{pid}.0.0123456789abcdef");
        let stale = [
            plant(dir.path().join("dist/js").join(name("file", gone)), true),
            plant(dir.path().join("dist").join(name("json", gone)), true),
        ];
        let kept = [
            plant(dir.path().join("dist/js").join(name("bin", gone)), false),
            plant(dir.path().join("dist/js").join(name("file", live)), true),
            plant(config.output.css.join(name("file", gone)), true),
            plant(dir.path().join("dist/.paste-staged.notes"), true),
        ];
        AssetGraph::new(config).build().unwrap();
        for path in &stale {
            assert!(!path.exists(), "{} survived", path.display());
        }
        for path in &kept {
            assert!(path.exists(), "{} was removed", path.display());
        }
    }

    #[test]
    fn test_watch_drops_a_directory_event_from_publishing_the_manifest() {
        // kqueue reports a publish beside sources as a change to the
        // directory itself. With the directory's other entries untouched
        // since the build started, that event must not rebuild; any real
        // change to an entry — added, replaced, rewritten — still does.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let src = dir.path().join("src/js");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.js"), "var a = 1;").unwrap();
        let mut config = dist_config(dir.path(), false);
        config.manifest_path = src.join("manifest.json");
        let directory_event = || WatchBatch {
            rebuild: true,
            paths: vec![src.clone()],
            ..WatchBatch::default()
        };
        let listing = bookkeeping_free_listing(&config, &src);

        AssetGraph::new(config.clone()).build().unwrap();
        fs::write(src.join(".paste-staged.json.7.0.0123456789abcdef"), "{}").unwrap();
        fs::remove_file(src.join(".paste-staged.json.7.0.0123456789abcdef")).unwrap();
        fs::write(src.join(".paste-pending.0123456789abcdef"), "x").unwrap();
        let mut batch = directory_event();
        drop_bookkeeping_directory_event(&mut batch, &config, &src, &listing);
        assert!(
            !batch.rebuild,
            "publishing the manifest started another build"
        );

        for what in [
            "an added source",
            "an atomic replace",
            "an in-place rewrite",
        ] {
            let listing = bookkeeping_free_listing(&config, &src);
            match what {
                "an added source" => fs::write(src.join("b.js"), "var b;").unwrap(),
                "an atomic replace" => {
                    fs::write(src.join("a.tmp"), "var a = 1;").unwrap();
                    fs::rename(src.join("a.tmp"), src.join("a.js")).unwrap();
                }
                _ => fs::write(src.join("a.js"), "var a = 22;").unwrap(),
            }
            let mut batch = directory_event();
            drop_bookkeeping_directory_event(&mut batch, &config, &src, &listing);
            assert!(batch.rebuild, "{what} was dropped");
        }
        // A batch naming a source file is never dropped.
        let listing = bookkeeping_free_listing(&config, &src);
        let mut batch = WatchBatch {
            rebuild: true,
            paths: vec![src.clone(), src.join("a.js")],
            ..WatchBatch::default()
        };
        drop_bookkeeping_directory_event(&mut batch, &config, &src, &listing);
        assert!(batch.rebuild);
    }

    #[test]
    fn test_watch_ignores_build_bookkeeping_inside_a_root() {
        // A manifest kept inside a watched root: publishing it renames a
        // staged file over it and removes the dist `.bin` — neither is a
        // source change, or every build would trigger another.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        let mut config = dist_config(dir.path(), false);
        config.manifest_path = dir.path().join("src/js/manifest.json");
        let roots = config.source_dirs();
        let remove = notify::EventKind::Remove(notify::event::RemoveKind::File);
        let create = notify::EventKind::Create(notify::event::CreateKind::File);
        let rename = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Any,
        ));
        for (path, kind) in [
            ("src/js/manifest.bin", &remove),
            ("src/js/manifest.json", &rename),
            ("src/js/.paste-staged.json.7.0.0123456789abcdef", &create),
            ("src/js/.paste-staged.json.7.0.0123456789abcdef", &rename),
            ("manifest.lock", &create),
        ] {
            assert!(
                !is_source_change(&config, &roots, &dir.path().join(path), kind),
                "{path} counted as a source change"
            );
        }
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("src/js/app.js"),
            &create
        ));
    }

    #[test]
    fn test_is_source_change_filters_watch_events() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        // Source files count.
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("src/js/app.js"),
            &modify
        ));
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("src/scss/site.scss"),
            &modify
        ));

        // HTML is never a build input — template publication was removed,
        // so a markup file is not a source anywhere it lands.
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("templates/index.html"),
            &modify
        ));
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("src/js/readme.html"),
            &modify
        ));

        // Generated output never counts — including a tree nested under a
        // watched source directory.
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("dist/index.html"),
            &modify
        ));
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("dist/js/app.v1.js"),
            &modify
        ));
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("dist/css/site.css"),
            &modify
        ));

        let mut nested = config.clone();
        nested.output.base = dir.path().join("src/dist");
        assert!(!is_source_change(
            &nested,
            &roots,
            &dir.path().join("src/dist/out.js"),
            &modify
        ));
    }

    #[test]
    fn test_is_source_change_counts_directory_lifecycle() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let create = notify::EventKind::Create(notify::event::CreateKind::Folder);
        let remove = notify::EventKind::Remove(notify::event::RemoveKind::Folder);
        let rename = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        ));
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));

        // A directory that exists counts whatever the event — its own
        // creation, contents arriving, or a rename the notifier reported.
        let newdir = dir.path().join("src/js/widgets");
        std::fs::create_dir_all(&newdir).unwrap();
        assert!(is_source_change(&config, &roots, &newdir, &create));
        assert!(is_source_change(&config, &roots, &newdir, &modify));

        // A path that no longer exists counts on a lifecycle kind — the
        // directory and its sources left together — but not on a bare
        // data event an extensionless file could also produce.
        let gone = dir.path().join("src/js/vanished");
        assert!(is_source_change(&config, &roots, &gone, &remove));
        assert!(is_source_change(&config, &roots, &gone, &rename));
        assert!(!is_source_change(&config, &roots, &gone, &modify));

        // An unrelated extension on a real file still does not count.
        let note = dir.path().join("src/js/readme.txt");
        std::fs::write(&note, "x").unwrap();
        assert!(!is_source_change(&config, &roots, &note, &modify));
    }

    #[test]
    fn test_is_source_change_scopes_ancestor_watch_to_the_missing_root() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let mut config = dist_config(dir.path(), false);
        // One source root does not exist — the watcher sits on its nearest
        // existing ancestor and must not treat sibling subtrees as inputs.
        config.source.dirs = vec![dir.path().join("assets")];
        config.source.js = PathBuf::new();
        config.source.scss = PathBuf::new();
        let roots = config.source_dirs();
        let create = notify::EventKind::Create(notify::event::CreateKind::Folder);
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));

        // The missing root's own creation counts — it can carry sources in.
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("assets"),
            &create
        ));
        // An ancestor-step event counts — the chain leading to the root.
        assert!(is_source_change(
            &config,
            &roots,
            dir.path().join("assets").parent().unwrap(),
            &create
        ));
        // A sibling subtree under the watched ancestor does NOT count —
        // not even a source-extension file inside it.
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!is_source_change(
            &config,
            &roots,
            &sibling.join("unrelated.js"),
            &modify
        ));
        assert!(!is_source_change(
            &config,
            &roots,
            &dir.path().join("other/site.scss"),
            &create
        ));
    }

    #[test]
    fn test_watch_batch_accumulates_rescan_error_and_lifecycle() {
        // One debounce window holds several messages — a rescan flag, a
        // watcher error, directory lifecycle, and file changes. Nothing
        // in the batch may be dropped: each kind sets its own mark and
        // every in-scope path lands on the rebuild line.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        let create = notify::EventKind::Create(notify::event::CreateKind::Folder);

        let mut batch = WatchBatch::default();

        // A rescan event: paths may be empty — the flag alone rebuilds.
        let rescan_event = notify::Event::new(modify).set_flag(notify::event::Flag::Rescan);
        accumulate(&mut batch, &config, &roots, Ok(rescan_event));
        assert!(batch.rescan, "a rescan flag must be honored");
        assert!(batch.paths.is_empty() && !batch.rebuild);

        // A watcher error lands on the batch — it is surfaced, and it is
        // actionable: an error means events may have been lost, the same
        // contract the rescan flag carries, so it requests a rescan.
        accumulate(
            &mut batch,
            &config,
            &roots,
            Err(notify::Error::generic("backend overflow")),
        );
        assert_eq!(batch.errors.as_slice(), ["backend overflow".to_string()]);
        assert!(batch.rescan, "a watcher error must request a rescan");

        // A directory lifecycle event and a file change in the same batch
        // both count — neither is swallowed by the flag or the error.
        let newdir = dir.path().join("src/js/widgets");
        std::fs::create_dir_all(&newdir).unwrap();
        let file = dir.path().join("src/js/app.js");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "var a = 1;").unwrap();
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(create).add_path(newdir.clone())),
        );
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(file.clone())),
        );
        assert!(batch.rebuild, "in-scope paths trigger the rebuild");
        assert_eq!(
            batch.paths,
            vec![newdir, file],
            "every triggering path is carried to the log line"
        );

        // An out-of-scope event inside the same batch does not count.
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(dir.path().join("elsewhere/out.js"))),
        );
        assert_eq!(batch.paths.len(), 2, "out-of-scope paths are filtered");

        // A second, different watcher error in the same batch is kept
        // too — the batch reports every distinct error, not just the
        // first; an exact duplicate is reported once.
        accumulate(
            &mut batch,
            &config,
            &roots,
            Err(notify::Error::generic("queue full")),
        );
        accumulate(
            &mut batch,
            &config,
            &roots,
            Err(notify::Error::generic("backend overflow")),
        );
        assert_eq!(
            batch.errors.as_slice(),
            ["backend overflow".to_string(), "queue full".to_string()],
            "every distinct error survives the batch"
        );

        // The same path arriving twice counts once on the rebuild line.
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(dir.path().join("src/js/app.js"))),
        );
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(dir.path().join("src/js/app.js"))),
        );
        assert_eq!(
            batch.paths.iter().filter(|p| p.ends_with("app.js")).count(),
            1,
            "duplicate paths are deduplicated"
        );
    }

    #[test]
    fn test_watch_error_only_batch_requests_rescan() {
        // A batch holding nothing but watcher errors has no path list to
        // show — the events it was meant to carry may have been lost. The
        // batch must still rebuild: an error-only batch is a rescan
        // indication, not a log-only message, or changed sources stay
        // stale until the next lucky event.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut batch = WatchBatch::default();

        accumulate(
            &mut batch,
            &config,
            &roots,
            Err(notify::Error::generic("queue full")),
        );
        assert!(
            batch.rescan,
            "an error-only batch must request a rescan — possible event loss is actionable"
        );
        assert!(!batch.rebuild && batch.paths.is_empty());
        assert_eq!(batch.errors.as_slice(), ["queue full".to_string()]);
    }

    #[test]
    fn test_watch_error_beside_out_of_scope_event_still_rescans() {
        // An overflow error next to an unrelated event still forces the
        // rescan — the out-of-scope event earns nothing, and the error's
        // possible loss is what is actionable.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        let mut batch = WatchBatch::default();

        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(dir.path().join("elsewhere/out.js"))),
        );
        accumulate(
            &mut batch,
            &config,
            &roots,
            Err(notify::Error::generic("backend overflow")),
        );
        assert!(batch.rescan, "the error's rescan survives unrelated events");
        assert!(!batch.rebuild && batch.paths.is_empty());

        // Control: the same batch without the error stays inert.
        let mut quiet = WatchBatch::default();
        accumulate(
            &mut quiet,
            &config,
            &roots,
            Ok(notify::Event::new(modify).add_path(dir.path().join("elsewhere/out.js"))),
        );
        assert!(!quiet.rescan && !quiet.rebuild && quiet.errors.is_empty());
    }

    #[test]
    fn test_watch_reconcile_refresh_reregisters_every_desired_target() {
        // A rescan can mean a registration was lost; refresh the entire
        // desired set, releasing registrations that still exist first.
        // The backend's WatchNotFound result is handled by the caller.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let root = dir.path().join("src/js");
        std::fs::create_dir_all(&root).unwrap();
        let desired = desired_watches(std::slice::from_ref(&root), dir.path());
        // The tracked set claims the target is already registered.
        let physical = desired.clone();

        let (drop, add) = reconcile_watches(&desired, &physical, false);
        assert!(
            drop.is_empty() && add.is_empty(),
            "normal reconcile is a no-op"
        );

        let resolved = canonical_identity(&root);
        let parent = canonical_identity(&dir.path().join("src"));
        let (drop, add) = reconcile_watches(&desired, &physical, true);
        assert_eq!(
            drop,
            vec![parent.clone(), resolved.clone()],
            "refresh first releases each known registration"
        );
        assert_eq!(
            add,
            vec![
                (parent, RecursiveMode::NonRecursive),
                (resolved, RecursiveMode::Recursive),
            ],
            "refresh re-registers every desired target despite tracked entries"
        );
    }

    #[test]
    fn test_watch_registrations_track_roots_without_leaking() {
        // The physical watch set is derived from the logical roots each
        // batch: an existing root watches itself recursively; a missing
        // root watches its nearest existing ancestor NON-recursively;
        // targets several roots share deduplicate; and reconciliation
        // removes what no root still needs.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let root = dir.path().join("src/js");
        let other_missing = dir.path().join("src/css");
        // `src` exists so it is the shared nearest-ancestor for both
        // missing roots.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let shared = canonical_identity(&dir.path().join("src"));

        // Missing root → non-recursive ancestor registration.
        let desired = desired_watches(std::slice::from_ref(&root), dir.path());
        assert_eq!(desired.len(), 1);
        assert_eq!(
            desired.get(&shared),
            Some(&RecursiveMode::NonRecursive),
            "a missing root watches its ancestor non-recursively"
        );

        // Two missing roots under the same ancestor share the one
        // registration — not one watch each.
        let desired = desired_watches(&[root.clone(), other_missing.clone()], dir.path());
        assert_eq!(
            desired.len(),
            1,
            "missing roots under one ancestor deduplicate"
        );
        assert_eq!(desired[shared.as_path()], RecursiveMode::NonRecursive);

        // The root appearing moves its entry to a recursive self-watch;
        // the ancestor stays — the other missing root still needs it.
        std::fs::create_dir_all(&root).unwrap();
        let resolved_root = canonical_identity(&root);
        let desired = desired_watches(&[root.clone(), other_missing.clone()], dir.path());
        assert_eq!(
            desired[resolved_root.as_path()],
            RecursiveMode::Recursive,
            "an existing root watches itself recursively"
        );
        assert_eq!(
            desired[shared.as_path()],
            RecursiveMode::NonRecursive,
            "the shared ancestor remains while another root is missing"
        );

        // Reconcile: starting from the stale map, the ancestor's mode
        // change unregisters nothing (the target is still wanted — only
        // its mode stays NonRecursive) while the new root registers.
        let mut physical = BTreeMap::from([(shared.clone(), RecursiveMode::NonRecursive)]);
        let (drop, add) = reconcile_watches(&desired, &physical, false);
        assert!(drop.is_empty());
        assert_eq!(add, vec![(resolved_root.clone(), RecursiveMode::Recursive)]);
        for (t, m) in add {
            physical.insert(t, m);
        }

        // The root vanishing releases its self-watch and returns the
        // ancestor need; the cycle repeated does not multiply entries.
        std::fs::remove_dir(&root).unwrap();
        let desired = desired_watches(&[root.clone(), other_missing.clone()], dir.path());
        let (drop, add) = reconcile_watches(&desired, &physical, false);
        assert_eq!(
            drop,
            vec![resolved_root],
            "the vanished root's watch is released"
        );
        assert!(add.is_empty(), "the shared ancestor stays registered");
        for t in drop {
            physical.remove(&t);
        }
        assert_eq!(physical.len(), 1);

        // Both roots now existing collapses the ancestor need — but the
        // shared directory stays registered as the roots' PARENT: it is
        // the reporter that sees either name being replaced at the same
        // pathname. Only the second new root needs a registration.
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other_missing).unwrap();
        let desired = desired_watches(&[root.clone(), other_missing.clone()], dir.path());
        let (drop, add) = reconcile_watches(&desired, &physical, false);
        assert!(
            drop.is_empty(),
            "the shared ancestor stays as the roots' parent watcher"
        );
        assert_eq!(
            add,
            vec![
                (canonical_identity(&other_missing), RecursiveMode::Recursive),
                (canonical_identity(&root), RecursiveMode::Recursive),
            ],
            "both newly existing roots register their own recursive watch"
        );
    }

    #[test]
    fn test_watch_registration_survives_directory_replacement() {
        // Replacing a root at the same pathname leaves the canonical key
        // unchanged while the filesystem object is new — the registration
        // must move to the replacement or changes inside it never arrive.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(&root).unwrap();

        let (tx, rx) = channel();
        let watcher: RecommendedWatcher = Watcher::new(
            tx.clone(),
            notify::Config::default().with_poll_interval(Duration::from_secs(1)),
        )
        .unwrap();
        let mut watches = WatchSet {
            watcher,
            polled: BTreeMap::new(),
            event_tx: WatchSet::polls_directories().then_some(tx),
            physical: BTreeMap::new(),
            bound: BTreeMap::new(),
            restart_process: None,
            died: None,
        };
        let roots = vec![root.clone()];
        let no_extra = BTreeSet::new();
        update_watch_registrations(&mut watches, &roots, &no_extra, false, dir.path()).unwrap();

        let target = canonical_identity(&root);
        assert!(watches.physical.contains_key(&target));
        let first_bound = watches.bound.get(&target).copied();

        // The parent watch reports the name being replaced — without it
        // this batch would never arrive at all.
        assert!(
            desired_watches(&roots, dir.path()).contains_key(&canonical_identity(dir.path())),
            "the root's parent is watched for the name swap"
        );

        fs::rename(&root, dir.path().join("src.old")).unwrap();
        fs::create_dir_all(&root).unwrap();
        if let Some(first) = first_bound {
            assert_ne!(
                bound_identity(&target),
                Some(first),
                "a replacement at the same pathname is a different object"
            );
        }

        update_watch_registrations(&mut watches, &roots, &no_extra, false, dir.path()).unwrap();
        assert_eq!(
            watches.bound.get(&target).copied(),
            bound_identity(&target),
            "the registration re-pins the replacement's identity"
        );

        // A write inside the replacement must still produce an event on
        // the live channel — proving the watch bound the new object.
        fs::write(root.join("app.js"), "// after replacement").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(event)) if event.paths.iter().any(|p| p.ends_with("app.js")) => {
                    break;
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("no watch event inside the replacement directory")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("watch channel disconnected")
                }
            }
        }
    }

    /// A dangling root link materializes at its referent — creation
    /// lands in the referent's ancestor's events, nowhere near the
    /// spelled chain's ancestor. The registration follows the referent
    /// while the alias's own ancestor keeps watching the entry itself,
    /// and event scope admits the missing-target chain only.
    #[cfg(unix)]
    #[test]
    fn test_dangling_root_link_watches_the_referent_ancestor() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(outside.join("target"), project.join("src")).unwrap();
        let roots = vec![project.join("src")];

        let desired = desired_watches(&roots, &project);
        assert_eq!(
            desired.get(&canonical_identity(&outside)),
            Some(&RecursiveMode::NonRecursive),
            "a dangling root watches the referent's ancestor: {desired:?}"
        );
        assert_eq!(
            desired.get(&canonical_identity(&project)),
            Some(&RecursiveMode::NonRecursive),
            "the alias's own ancestor stays watched for retargets: {desired:?}"
        );

        // Events on the missing-target chain are in scope — the step
        // that creates the referent and the file appearing inside it —
        // while an unrelated sibling under that ancestor is not.
        let config = dirs_config(dir.path(), &[], false);
        let create = notify::EventKind::Create(notify::event::CreateKind::Folder);
        assert!(is_source_change(
            &config,
            &roots,
            &outside.join("target"),
            &create
        ));
        assert!(is_source_change(
            &config,
            &roots,
            &outside.join("target/app.js"),
            &create
        ));
        assert!(!is_source_change(
            &config,
            &roots,
            &outside.join("other/app.js"),
            &create
        ));

        // Once the referent exists the ordinary recursive registration
        // resolves through the link and the ancestor need disappears.
        fs::create_dir_all(outside.join("target")).unwrap();
        let desired = desired_watches(&roots, &project);
        assert_eq!(
            desired.get(&canonical_identity(&outside.join("target"))),
            Some(&RecursiveMode::Recursive),
            "the appeared referent takes the recursive watch: {desired:?}"
        );
    }

    /// A backend binds a registration to the target resolved at watch
    /// time, so a root behind a link must be tracked by resolved
    /// identity: retargeting the link changes the desired entry even
    /// though the configured path never changes. The link's parent is
    /// watched so the entry event that performs the retarget arrives.
    #[cfg(unix)]
    #[test]
    fn test_desired_watches_follow_root_link_identity() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let first = dir.path().join("real-scss-a");
        let second = dir.path().join("real-scss-b");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        let root = dir.path().join("assets/scss");
        symlink(&first, &root).unwrap();

        let parent = canonical_identity(root.parent().unwrap());
        let desired = desired_watches(std::slice::from_ref(&root), dir.path());
        assert_eq!(
            desired[&canonical_identity(&first)],
            RecursiveMode::Recursive,
            "the registration names the resolved target"
        );
        assert_eq!(
            desired[&parent],
            RecursiveMode::NonRecursive,
            "the link's entry is observed through its parent"
        );

        symlink(&second, dir.path().join("assets/scss-next")).unwrap();
        fs::rename(dir.path().join("assets/scss-next"), &root).unwrap();
        let next = desired_watches(std::slice::from_ref(&root), dir.path());
        let (drop, add) = reconcile_watches(&next, &desired, false);
        assert_eq!(
            drop,
            vec![canonical_identity(&first)],
            "the stale target is released after the retarget"
        );
        assert_eq!(
            add,
            vec![(canonical_identity(&second), RecursiveMode::Recursive)],
            "the new target is registered under the same logical root"
        );
    }

    #[test]
    fn test_watch_scope_and_lifecycle_cases() {
        // The scope rule: an ordinary event must land inside a configured
        // root to count — watching the ancestor of a root that does not
        // exist yet must not turn unrelated siblings into inputs — while
        // an event on the missing root's own ancestry chain stays
        // relevant because it can carry the root into existence.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let existing_root = dir.path().join("src/js");
        std::fs::create_dir_all(&existing_root).unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], false);
        // A second configured root that does not exist yet: the watcher
        // walks up and watches `src/` — making `src/*` siblings visible.
        config.source.dirs.push(dir.path().join("src/newroot"));
        let roots = config.source_dirs();

        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        let create_dir = notify::EventKind::Create(notify::event::CreateKind::Folder);
        let remove_dir = notify::EventKind::Remove(notify::event::RemoveKind::Folder);
        let rename = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        ));

        // An unrelated .js sibling under the watched ancestor is not an
        // input while the configured root is missing.
        assert!(
            !is_source_change(
                &config,
                &roots,
                &dir.path().join("src/unrelated.js"),
                &modify
            ),
            "a sibling under the ancestor watch must not rebuild"
        );
        // The chain that can create the missing root does count — a
        // mkdir on `src` itself or on `src/newroot` may carry it in.
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("src"),
            &create_dir
        ));
        assert!(is_source_change(
            &config,
            &roots,
            &dir.path().join("src/newroot"),
            &create_dir
        ));

        // Inside an existing root: file changes count…
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("app.js"),
            &modify
        ));
        // …a populated directory moved in counts (create)…
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("widgets"),
            &create_dir
        ));
        // …a populated directory moved out counts even though the path
        // no longer exists to probe…
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("widgets"),
            &remove_dir
        ));
        // …a source subtree rename counts…
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("old"),
            &rename
        ));
        // …and a directory with an extension-looking name — `theme.v2`
        // can hold `site.scss` modules — counts on removal and rename
        // even though `is_dir` is unanswerable once the path is gone.
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("theme.v2"),
            &remove_dir
        ));
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("theme.v2"),
            &rename
        ));
        // A still-present extension directory keeps answering directly.
        std::fs::create_dir_all(existing_root.join("theme.v2")).unwrap();
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root.join("theme.v2"),
            &create_dir
        ));
        std::fs::remove_dir(existing_root.join("theme.v2")).unwrap();
        // …and losing or recreating the root itself counts either way.
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root,
            &remove_dir
        ));
        assert!(is_source_change(
            &config,
            &roots,
            &existing_root,
            &create_dir
        ));

        // Generated output never counts — an output tree nested inside a
        // source root cannot loop the watcher, and neither can a
        // beside-source build directory.
        for generated in [
            config.output.base.join("js/app.min.js"),
            config.output.js.join("app.min.js"),
            existing_root.join("_paste_build").join("app.min.js"),
        ] {
            assert!(
                !is_source_change(&config, &roots, &generated, &modify),
                "{} is generated output, not a source",
                generated.display()
            );
        }

        // And through the batch path: a rescan flag arriving with no
        // paths still forces the one rebuild the debounce owes.
        let mut batch = WatchBatch::default();
        accumulate(
            &mut batch,
            &config,
            &roots,
            Ok(notify::Event::new(modify).set_flag(notify::event::Flag::Rescan)),
        );
        assert!(batch.rescan);
    }

    /// A config whose only source roots are `source.dirs` — the legacy
    /// js/scss roots are unset so `dirs` alone drives discovery.
    fn dirs_config(dir: &Path, roots: &[&str], minify: bool) -> Config {
        let mut config = dist_config(dir, minify);
        config.source.dirs = roots.iter().map(|root| dir.join(root)).collect();
        config.source.js = PathBuf::new();
        config.source.scss = PathBuf::new();
        config
    }

    /// `dirs_config` in beside-source layout; output.base stays inside
    /// base_dir as the layout requires.
    fn beside_source_config(dir: &Path, roots: &[&str], minify: bool) -> Config {
        let mut config = dirs_config(dir, roots, minify);
        config.output.layout = "beside-source".into();
        config
    }

    /// The directory a source-dir file compiles into under beside-source:
    /// `<file dir>/<build-dir-name>` beneath base_dir.
    fn build_dir_of(dir: &Path, rel: &str) -> PathBuf {
        let parent = Path::new(rel).parent().unwrap();
        dir.join(parent).join("_paste_build")
    }

    #[test]
    fn test_source_dirs_classifies_by_extension_not_layout() {
        // One root holds JS and styles together at arbitrary depth — the
        // folder layout means nothing to discovery, only the extension does.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let web = dir.path().join("web");
        fs::create_dir_all(web.join("deep/nested")).unwrap();
        fs::write(web.join("app.js"), "var app = 1;").unwrap();
        fs::write(web.join("deep/nested/extra.js"), "var extra = 1;").unwrap();
        fs::write(web.join("deep/site.scss"), ".site { color: red; }").unwrap();
        fs::write(web.join("plain.css"), "a  {  color :  red ;  }").unwrap();
        fs::write(web.join("deep/_reset.scss"), "* { margin: 0; }").unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["web"], true));
        let manifest = graph.build().unwrap();

        assert!(manifest.assets.contains_key("js/app.js"));
        assert!(manifest.assets.contains_key("js/deep/nested/extra.js"));
        assert!(manifest.assets.contains_key("css/deep/site.css"));
        assert!(manifest.assets.contains_key("css/plain.css"));
        // An _-prefixed scss partial is never an asset; a plain .css is.
        assert!(!manifest.assets.contains_key("css/deep/_reset.css"));

        // The plain .css ran the grass → lightningcss pipeline: the output
        // is minified, not a byte copy.
        let css_entry = &manifest.assets["css/plain.css"];
        let compiled = dir
            .path()
            .join("dist")
            .join(css_entry.output.trim_start_matches('/'));
        let body = fs::read_to_string(&compiled).unwrap();
        assert!(
            body.contains("color:red") && !body.contains("color :"),
            "plain css should be minified: {body}"
        );
    }

    #[test]
    fn test_disappearing_populated_root_fails_without_touching_the_manifest() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("app.js"), "var app = 1;").unwrap();

        // An optional root that was never populated may be absent from
        // the very first build.
        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js", "src/scss"], true));
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("js/app.js"));

        let manifest_path = dir.path().join("dist/manifest.json");
        let manifest_bytes = fs::read(&manifest_path).unwrap();
        let asset_file = dir
            .path()
            .join("dist")
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        let asset_bytes = fs::read(&asset_file).unwrap();

        // Losing the populated root is a root outage, not a deletion
        // sweep: the build fails and neither manifest nor file moves.
        fs::remove_dir_all(dir.path().join("src")).unwrap();
        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js", "src/scss"], true));
        let error = graph
            .build()
            .expect_err("a vanished populated root must fail");
        assert!(
            format!("{error:#}").contains("configured source root disappeared"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(&manifest_path).unwrap(),
            manifest_bytes,
            "the served manifest is preserved"
        );
        assert_eq!(
            fs::read(&asset_file).unwrap(),
            asset_bytes,
            "the retained asset bytes are preserved"
        );

        // The same rule holds on the local backend.
        let dir2 = tempdir().unwrap();
        let js2 = dir2.path().join("src/js");
        fs::create_dir_all(&js2).unwrap();
        fs::write(js2.join("app.js"), "var app = 1;").unwrap();
        let mut local = dirs_config(dir2.path(), &["src/js"], true);
        local.storage.backend = "local".into();
        AssetGraph::new(local.clone()).build().unwrap();
        fs::remove_dir_all(dir2.path().join("src")).unwrap();
        let error = AssetGraph::new(local)
            .build()
            .expect_err("local backend: a vanished populated root must fail");
        assert!(format!("{error:#}").contains("configured source root disappeared"));

        // Restoring the root rebuilds normally — the guard releases.
        let dir3 = tempdir().unwrap();
        let js3 = dir3.path().join("src/js");
        fs::create_dir_all(&js3).unwrap();
        fs::write(js3.join("app.js"), "var app = 1;").unwrap();
        let config3 = dirs_config(dir3.path(), &["src/js"], true);
        AssetGraph::new(config3.clone()).build().unwrap();
        fs::remove_dir_all(dir3.path().join("src")).unwrap();
        assert!(AssetGraph::new(config3.clone()).build().is_err());
        fs::create_dir_all(&js3).unwrap();
        fs::write(js3.join("app.js"), "var app = 2;").unwrap();
        let manifest3 = AssetGraph::new(config3).build().unwrap();
        assert!(manifest3.assets.contains_key("js/app.js"));
    }

    #[test]
    fn test_root_outage_vs_single_file_and_config_removal() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        for root in ["src/js", "src/scss"] {
            fs::create_dir_all(dir.path().join(root)).unwrap();
        }
        fs::write(dir.path().join("src/js/a.js"), "var a = 1;").unwrap();
        fs::write(dir.path().join("src/scss/s.scss"), ".s { color: red; }").unwrap();
        let config = dirs_config(dir.path(), &["src/js", "src/scss"], true);
        AssetGraph::new(config.clone()).build().unwrap();

        // Deleting one file inside a present root retires it — the
        // sibling root's outage is the only blocked case.
        fs::remove_file(dir.path().join("src/scss/s.scss")).unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            manifest.assets["css/s.css"].retired || !manifest.assets.contains_key("css/s.css"),
            "a deleted file in a present root retires normally"
        );
        fs::write(dir.path().join("src/scss/s.scss"), ".s { color: red; }").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();

        // The scss root disappearing fails; removing it from the
        // configuration is the explicit way to retire the whole root.
        fs::remove_dir_all(dir.path().join("src/scss")).unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a vanished populated root must fail");
        assert!(format!("{error:#}").contains("configured source root disappeared"));
        let narrowed = dirs_config(dir.path(), &["src/js"], true);
        let manifest = AssetGraph::new(narrowed).build().unwrap();
        assert!(manifest.assets.contains_key("js/a.js"));
        assert!(
            manifest.assets["css/s.css"].retired || !manifest.assets.contains_key("css/s.css"),
            "the deliberately removed root's assets retire on the next build"
        );
    }

    #[test]
    fn test_vanishing_parent_spelled_include_root_fails_closed() {
        // `../shared` — the sbt sibling pattern — keeps its `..` spelling
        // for watch, but its labels must still name the include root so a
        // vanished sibling is an outage, not a silent retirement.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let project = dir.path().join("proj");
        let shared = dir.path().join("shared");
        fs::create_dir_all(project.join("src/js")).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(
            project.join("src/js/app.js"),
            "// @module app\n// @requires shared.lib\nvar a = 1;\n",
        )
        .unwrap();
        fs::write(shared.join("lib.js"), "// @module shared.lib\nvar b = 2;\n").unwrap();
        let mut config = dist_config(&project, false);
        config.include_paths = vec![project.join("../shared")];
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let labels: Vec<_> = manifest.assets.values().map(|e| e.source.clone()).collect();
        assert!(labels.iter().all(|l| !l.contains("..")), "{labels:?}");
        let before = fs::read(project.join("dist/manifest.json")).unwrap();

        fs::remove_dir_all(&shared).unwrap();
        let err = AssetGraph::new(config)
            .build()
            .expect_err("a vanished sibling include root must refuse the build");
        assert!(format!("{err:#}").contains("disappeared"), "{err:#}");
        assert_eq!(
            fs::read(project.join("dist/manifest.json")).unwrap(),
            before
        );
    }

    #[test]
    fn test_disappearing_include_path_root_fails_the_same_way() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let vendor = dir.path().join("vendor-lib");
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(&vendor).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::write(vendor.join("widget.js"), "var widget = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![vendor.clone()];
        AssetGraph::new(config.clone()).build().unwrap();

        fs::remove_dir_all(&vendor).unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a vanished populated include path must fail");
        assert!(
            format!("{error:#}").contains("configured source root disappeared"),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_root_replaced_by_a_regular_file_fails_before_any_write() {
        // A populated root swapped for an ordinary file is not an
        // outage-shaped absence and not an empty tree: it is an invalid
        // root, named, on every backend — the manifest and every retained
        // byte stay exactly as the last good build left them.
        use tempfile::tempdir;
        for backend in ["dist", "local"] {
            let dir = tempdir().unwrap();
            let js = dir.path().join("src/js");
            fs::create_dir_all(&js).unwrap();
            fs::write(js.join("app.js"), "var app = 1;").unwrap();
            let mut config = dirs_config(dir.path(), &["src/js"], true);
            config.storage.backend = backend.into();
            let manifest = AssetGraph::new(config.clone()).build().unwrap();
            let manifest_path = config.manifest_path.clone();
            let manifest_bytes = fs::read(&manifest_path).unwrap();
            let asset_file = dir
                .path()
                .join("dist")
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
            let asset_bytes = fs::read(&asset_file).unwrap();

            fs::remove_dir_all(&js).unwrap();
            fs::write(&js, "not a directory").unwrap();
            let error = AssetGraph::new(config)
                .build()
                .expect_err("a non-directory root must fail the build");
            assert!(
                format!("{error:#}").contains("not a directory"),
                "{backend} got: {error:#}"
            );
            assert_eq!(
                fs::read(&manifest_path).unwrap(),
                manifest_bytes,
                "{backend}: the served manifest is preserved byte-for-byte"
            );
            assert_eq!(
                fs::read(&asset_file).unwrap(),
                asset_bytes,
                "{backend}: the retained asset is preserved"
            );
        }
    }

    #[test]
    fn test_include_root_replaced_by_a_regular_file_fails_the_same_way() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let vendor = dir.path().join("vendor-lib");
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(&vendor).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::write(vendor.join("widget.js"), "var widget = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![vendor.clone()];
        AssetGraph::new(config.clone()).build().unwrap();
        let manifest_bytes = fs::read(&config.manifest_path).unwrap();

        fs::remove_dir_all(&vendor).unwrap();
        fs::write(&vendor, "not a directory").unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a non-directory include root must fail");
        assert!(
            format!("{error:#}").contains("not a directory"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(dir.path().join("dist/manifest.json")).unwrap(),
            manifest_bytes,
            "the served manifest is preserved"
        );
    }

    #[test]
    fn test_include_root_inside_a_historical_output_dir_builds() {
        // The previous build's output lived at `dist`; output then moved
        // to `out`. The manifest still records `dist` as generated, but a
        // configured include root inside it is input now: the recorded
        // label must not prune it (that would silently drop its modules)
        // nor block the build forever (no reconfiguration could recover).
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src/js"], true);
        AssetGraph::new(config.clone()).build().unwrap();

        // Relocate the output tree; the manifest stays where it is.
        let out = dir.path().join("out");
        config.output.base = out.clone();
        config.output.js = out.join("js");
        config.output.css = out.join("css");
        config.output.images = out.join("images");
        config.include_paths = vec![dir.path().join("dist/vendor")];
        fs::create_dir_all(dir.path().join("dist/vendor")).unwrap();
        fs::write(
            dir.path().join("dist/vendor/widget.js"),
            "// @module widget\nvar widget = 1;",
        )
        .unwrap();

        let manifest = AssetGraph::new(config)
            .build()
            .expect("an include root inside a historical output builds");
        assert!(manifest.modules.contains_key("widget"));
        assert!(!manifest.generated_dirs.contains(&"dist".to_string()));
    }

    #[test]
    fn test_build_publish_failure_removes_only_the_staged_cache() {
        // The local backend stages its bincode cache pre-publish; an
        // injected publication fault fails the build, leaves the
        // previous manifest byte-identical, and removes exactly the
        // staged file the build created — never a lookalike it cannot
        // prove it wrote.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("app.js"), "var app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let manifest_bytes = fs::read(&config.manifest_path).unwrap();
        let unowned = config.manifest_path.with_extension("bin.tmp.notes");
        fs::write(&unowned, b"someone else's").unwrap();

        // A content change forces a real publication — an unchanged
        // rebuild never crosses the boundary at all.
        fs::write(js.join("app.js"), "var app = 2;").unwrap();
        let pid = std::process::id();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        crate::manifest::test_hooks::FAIL_PUBLISH.with(|f| f.set(true));
        let error = AssetGraph::new(config.clone())
            .build()
            .expect_err("a publication failure must fail the build");
        assert!(
            format!("{error:#}").contains("publication"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            manifest_bytes,
            "a failed publication replaced the authoritative manifest"
        );
        assert!(
            !crate::storage::staged_tmp_name(
                config.manifest_path.parent().unwrap(),
                &config.manifest_path,
                "bin",
                &format!("{pid}.9000000"),
            )
            .exists(),
            "a failed publication left its own staged cache behind"
        );
        assert_eq!(fs::read(&unowned).unwrap(), b"someone else's");
    }

    #[test]
    fn test_build_install_failure_warns_and_removes_only_the_staged_cache() {
        // Post-publish: the cache install failed, the new JSON is
        // already authoritative — the build reports success, readers
        // see the new manifest, and the staged file the build owned is
        // gone. The planted lookalike keeps its bytes.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        fs::create_dir_all(&js).unwrap();
        fs::write(js.join("app.js"), "var app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let unowned = config.manifest_path.with_extension("bin.tmp.notes");
        fs::write(&unowned, b"someone else's").unwrap();

        // A content change forces a real publication — an unchanged
        // rebuild neither publishes nor stages a cache.
        fs::write(js.join("app.js"), "var app = 2;").unwrap();
        let pid = std::process::id();
        crate::manifest::test_hooks::TMP_TAG_BASE.with(|c| c.set(Some(9_000_000)));
        crate::manifest::test_hooks::FAIL_CACHE_COMMIT.with(|f| f.set(true));
        AssetGraph::new(config.clone())
            .build()
            .expect("a post-publication cache fault is a warning, not a failed build");

        assert!(
            !crate::storage::staged_tmp_name(
                config.manifest_path.parent().unwrap(),
                &config.manifest_path,
                "bin",
                &format!("{pid}.9000000"),
            )
            .exists(),
            "a failed install left its own staged cache behind"
        );
        assert_eq!(fs::read(&unowned).unwrap(), b"someone else's");
        // And the next real publication re-creates a working cache.
        fs::write(js.join("app.js"), "var app = 3;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            config.manifest_path.with_extension("bin").exists(),
            "the retry installs the cache deterministically"
        );
    }

    #[test]
    fn test_an_unreadable_source_root_fails_rather_than_scanning_empty() {
        // chmod is POSIX-only; the Windows ACL equivalent is a different
        // mechanism, so this case is unix-scoped like the symlink tests.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            let js = dir.path().join("src/js");
            fs::create_dir_all(&js).unwrap();
            fs::write(js.join("app.js"), "var app = 1;").unwrap();
            let config = dirs_config(dir.path(), &["src/js"], true);
            AssetGraph::new(config.clone()).build().unwrap();
            let manifest_bytes = fs::read(&config.manifest_path).unwrap();

            fs::set_permissions(&js, fs::Permissions::from_mode(0o000)).unwrap();
            // Permission bits do not deny a privileged user (e.g. root in a
            // container); the walk error this test exercises is then
            // unreachable, so bail out rather than assert a path that cannot
            // run under this user.
            if fs::read_dir(&js).is_ok() {
                fs::set_permissions(&js, fs::Permissions::from_mode(0o755)).unwrap();
                return;
            }
            let result = AssetGraph::new(config.clone()).build();
            fs::set_permissions(&js, fs::Permissions::from_mode(0o755)).unwrap();
            let error = result.expect_err("an unreadable root must fail the build");
            assert!(
                !format!("{error:#}").contains("disappeared"),
                "an unreadable root is a walk error, not a disappearance: {error:#}"
            );
            assert_eq!(
                fs::read(&config.manifest_path).unwrap(),
                manifest_bytes,
                "the served manifest is preserved"
            );
        }
    }

    #[test]
    fn test_source_dirs_first_root_wins_a_duplicate_module() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("one")).unwrap();
        fs::create_dir_all(dir.path().join("two")).unwrap();
        fs::write(
            dir.path().join("one/shared.js"),
            "// @module shared\nvar shared = 'one';",
        )
        .unwrap();
        fs::write(
            dir.path().join("two/shared.js"),
            "// @module shared\nvar shared = 'two';",
        )
        .unwrap();
        fs::write(
            dir.path().join("two/only.js"),
            "// @module only\nvar only = 1;",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["one", "two"], false));
        let manifest = graph.build().unwrap();

        // Both roots were walked.
        assert!(manifest.assets.contains_key("js/shared.js"));
        assert!(manifest.assets.contains_key("js/only.js"));

        // The first root claimed the shared name: the second file is not
        // its source and never reaches the manifest.
        let entry = &manifest.assets["js/shared.js"];
        assert!(
            entry.source.contains("one"),
            "duplicate should keep the first root's file: {}",
            entry.source
        );
        let compiled = dir
            .path()
            .join("dist")
            .join(entry.output.trim_start_matches('/'));
        assert!(fs::read_to_string(&compiled).unwrap().contains("'one'"));
    }

    #[test]
    fn test_legacy_source_roots_hocon_and_json_match() {
        // A config with only the legacy source.js/source.scss keys — in
        // either syntax — produces the same module names and asset keys.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets/js")).unwrap();
        fs::create_dir_all(dir.path().join("assets/scss")).unwrap();
        fs::write(dir.path().join("assets/js/app.js"), "var app = 1;").unwrap();
        fs::write(
            dir.path().join("assets/scss/site.scss"),
            "// @module site\n.site { color: red; }",
        )
        .unwrap();

        fs::write(
            dir.path().join("paste.conf"),
            r#"source.js = "assets/js"
source.scss = "assets/scss"
output.base = "public"
output.js = "public/js"
output.css = "public/css"
output.images = "public/images"
manifest = "public/manifest.json"
storage.backend = "local"
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("paste.json"),
            r#"{
                "source": {"js": "assets/js", "scss": "assets/scss"},
                "output": {
                    "base": "public",
                    "js": "public/js",
                    "css": "public/css",
                    "images": "public/images"
                },
                "manifest_path": "public/manifest.json",
                "storage": {"backend": "local", "keep_versions": 10}
            }"#,
        )
        .unwrap();

        let hocon = Config::load_with_base_dir(Path::new("paste.conf"), dir.path()).unwrap();
        let json = Config::load_with_base_dir(Path::new("paste.json"), dir.path()).unwrap();

        // The legacy roots feed discovery in their old order: js, then scss.
        let base = dir.path().to_path_buf();
        assert_eq!(
            hocon.source_dirs(),
            vec![base.join("assets/js"), base.join("assets/scss")]
        );

        let manifest_h = AssetGraph::new(hocon).build().unwrap();
        let manifest_j = AssetGraph::new(json).build().unwrap();
        let hocon_keys: BTreeMap<_, _> = manifest_h.assets.keys().map(|k| (k, ())).collect();
        let json_keys: BTreeMap<_, _> = manifest_j.assets.keys().map(|k| (k, ())).collect();
        assert_eq!(
            hocon_keys, json_keys,
            "HOCON and JSON disagree on asset keys"
        );
        let hocon_modules: BTreeMap<_, _> = manifest_h.modules.keys().map(|k| (k, ())).collect();
        let json_modules: BTreeMap<_, _> = manifest_j.modules.keys().map(|k| (k, ())).collect();
        assert_eq!(hocon_modules, json_modules);
        assert!(manifest_h.assets.contains_key("js/app.js"));
        assert!(manifest_h.assets.contains_key("css/site.css"));
        assert!(manifest_h.modules.contains_key("app"));
        assert!(manifest_h.modules.contains_key("site"));
    }

    #[test]
    fn test_beside_source_dist_writes_next_to_the_source() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.storage.backend = "dist".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        // The file lands beside its source, in the build directory, and the
        // manifest records it relative to base_dir with forward slashes.
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.starts_with("/src/_paste_build/app.")
                && entry.output.ends_with(".v1.min.js"),
            "unexpected beside-source output: {}",
            entry.output
        );
        let compiled = dir.path().join(entry.output.trim_start_matches('/'));
        assert!(compiled.exists(), "missing {}", compiled.display());
        let body = fs::read_to_string(&compiled).unwrap();

        // The runtime's output root in beside-source mode is the project
        // root: the same resolver serves those exact bytes.
        let dirs = OutputDirs::under(dir.path());
        let r = crate::jam::combo(&manifest, &dirs, "/paste", "/paste/app.js", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, body);

        // An unchanged rebuild leaves the committed manifest byte-identical.
        let manifest_path = dir.path().join("dist/manifest.json");
        let before = fs::read(&manifest_path).unwrap();
        graph.build().unwrap();
        assert_eq!(before, fs::read(&manifest_path).unwrap());

        // A content change bumps the version; the dist backend keeps v1 in
        // the same build directory.
        fs::write(dir.path().join("src/app.js"), "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.ends_with(".v2.min.js"),
            "expected v2: {}",
            entry.output
        );
        assert!(compiled.exists(), "dist must retain the v1 file");
        assert!(dir
            .path()
            .join(entry.output.trim_start_matches('/'))
            .exists());
    }

    #[test]
    fn test_beside_source_local_keeps_only_the_current_file() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let first = dir
            .path()
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        assert!(first.exists());

        fs::write(dir.path().join("src/app.js"), "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let current = dir
            .path()
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        assert!(current.exists());
        assert!(!first.exists(), "local must remove the superseded file");

        let build_dir = build_dir_of(dir.path(), "src/app.js");
        let count = fs::read_dir(&build_dir).unwrap().count();
        assert_eq!(count, 1, "only the current file may remain");
    }

    #[test]
    fn test_beside_source_include_path_stays_under_output_base() {
        // An include-path file is another tree's source — it is never
        // written there. It keeps flat placement under output.base, and its
        // manifest output is still relative to base_dir.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
        fs::write(
            external.path().join("vendor.js"),
            "// @module vendor\nvar vendor = 1;",
        )
        .unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.include_paths = vec![external.path().to_path_buf()];
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = &manifest.assets["js/vendor.js"];
        assert!(
            entry.output.starts_with("/dist/js/vendor.") && entry.output.ends_with(".min.js"),
            "include-path output should stay flat under output.base: {}",
            entry.output
        );
        let compiled = dir.path().join(entry.output.trim_start_matches('/'));
        assert!(compiled.exists(), "missing {}", compiled.display());
        assert!(
            !external.path().join("_paste_build").exists(),
            "nothing may be written into the include path's tree"
        );

        let dirs = OutputDirs::under(dir.path());
        let r = crate::jam::combo(&manifest, &dirs, "/paste", "/paste/vendor.js", None);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, fs::read_to_string(&compiled).unwrap());
    }

    #[test]
    fn test_beside_source_module_rename_keeps_both_stem_siblings() {
        // A renamed @module is a new asset key on the same source file:
        // beside-source derives the output stem from the file, so the
        // retired key's output and the new key's output share one
        // (dir, stem, ext) prune group. Pruning each asset against only
        // its own retained set deleted the sibling's file — and the next
        // build then failed on the missing retained output.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module old\nvar a = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.storage.backend = "dist".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_output = dir
            .path()
            .join(manifest.assets["js/old.js"].output.trim_start_matches('/'));
        assert!(old_output.exists());

        fs::write(dir.path().join("src/app.js"), "// @module new\nvar a = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let new_output = dir
            .path()
            .join(manifest.assets["js/new.js"].output.trim_start_matches('/'));

        assert!(manifest.assets["js/old.js"].retired);
        assert!(
            old_output.exists(),
            "the retired key's retained output must survive the rename"
        );
        assert!(
            new_output.exists(),
            "the new key's output must not be pruned by its stem sibling"
        );
        let old_body = fs::read_to_string(&old_output).unwrap();
        let new_body = fs::read_to_string(&new_output).unwrap();
        assert!(
            old_body.contains("var a = 1;") && new_body.contains("var a = 2;"),
            "each output must hold its own build's bytes"
        );

        // The reported failure mode: the next unchanged build verified a
        // retained file pruning had already deleted.
        let manifest = graph.build().unwrap();
        assert!(old_output.exists() && new_output.exists());

        // File existence is not the contract — both explicit pins still
        // serve their recorded bytes after the third build.
        let dirs = OutputDirs::under(dir.path());
        let old = pin(&manifest, "js/old.js", "old", 1, "js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &old, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, old_body.as_str()),
            "the retired key's pin must keep serving its original bytes"
        );
        let new = pin(&manifest, "js/new.js", "new", 1, "js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &new, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, new_body.as_str()),
            "the new key's pin must serve the current build"
        );
    }

    #[test]
    fn test_saved_page_loader_url_keeps_serving_the_pinned_build() {
        // A page rendered before a rebuild keeps fetching
        // `{packagesUrl}{module}.js` under the fingerprint it was rendered
        // with. The manifest records each build's pins, so that URL
        // answers the original bytes — never a silent upgrade to the
        // current build's content.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.storage.backend = "dist".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let saved_segment = crate::jam::packages_segment(&manifest);
        let v1_body = fs::read_to_string(
            dir.path()
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/')),
        )
        .unwrap();

        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let current_segment = crate::jam::packages_segment(&manifest);
        assert_ne!(saved_segment, current_segment);

        let dirs = OutputDirs::under(dir.path());
        let url = format!("/paste/{saved_segment}/app.js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, v1_body.as_str()),
            "the saved fingerprint must serve the pinned build's bytes"
        );
        assert_eq!(r.cache_control, "public, max-age=31536000");

        // A fingerprint this manifest never recorded is a 404 — never a
        // substitution for current content.
        let r = crate::jam::combo(
            &manifest,
            &dirs,
            "/paste",
            "/paste/deadbeefdead/app.js",
            None,
        );
        assert_eq!(r.status, 404);

        // The current fingerprint still resolves unpinned to current.
        let url = format!("/paste/{current_segment}/app.js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 200);
        assert_ne!(r.body, v1_body);

        // Retire the module entirely — delete the source and build once
        // more. The saved page's URL still names the pinned build: the
        // snapshot record, not the active set, is what it resolves
        // through, and a retired entry's retained history still verifies.
        fs::remove_file(dir.path().join("src/app.js")).unwrap();
        let manifest = graph.build().unwrap();
        assert!(manifest.assets["js/app.js"].retired);
        let dirs = OutputDirs::under(dir.path());
        let url = format!("/paste/{saved_segment}/app.js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, v1_body.as_str()),
            "a retired module's saved loader URL must keep serving the pinned bytes"
        );

        // The current loader ignores retired assets: under the new
        // fingerprint the name is a 404 — nothing resolves it to a
        // retired entry.
        let retired_segment = crate::jam::packages_segment(&manifest);
        let url = format!("/paste/{retired_segment}/app.js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn test_local_readded_module_never_answers_an_old_loader_url() {
        // Local keeps no history: delete a source and recreate it and the
        // new bytes land at v1 under the same key. A saved page's loader
        // URL pinned the old build's hash — the pin must not resolve the
        // rebuilt slot: 404, never a substitution with different bytes.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        // A stable module keeps the pin map nonempty across the churn.
        fs::write(dir.path().join("src/paste.js"), "var rt = 1;").unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let saved_segment = crate::jam::packages_segment(&manifest);

        fs::remove_file(dir.path().join("src/app.js")).unwrap();
        graph.build().unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 2;").unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.assets["js/app.js"].version, 1,
            "local history restarts at v1 on re-add"
        );

        // The old fingerprint is still recorded — but only the pin that
        // still verifies: app's was pruned when its bytes changed, so the
        // saved URL 404s for it while the untouched member keeps serving.
        assert!(manifest.packages_segments.contains_key(&saved_segment));
        let dirs = OutputDirs::under(dir.path());
        let r = crate::jam::combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{saved_segment}/app.js"),
            None,
        );
        assert_eq!(
            (r.status, r.cacheable),
            (404, false),
            "a rebuilt v1 must never answer an old build's pin"
        );
        let r = crate::jam::combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{saved_segment}/paste.js"),
            None,
        );
        assert_eq!(
            r.status, 200,
            "the surviving pin under the same fingerprint still serves"
        );
    }

    #[test]
    fn test_first_upgrade_records_the_legacy_fingerprint() {
        // A manifest written before packages_segments existed has no
        // recorded fingerprints, but every page its builds rendered named
        // the legacy segment. The first build under this code records that
        // segment's pin map from the loaded state — and this build's new
        // fingerprint — so a saved loader URL keeps answering the bytes it
        // was minted against. Segments no build emitted stay unrecorded.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.storage.backend = "dist".into();
        let manifest_path = config.manifest_path.clone();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let legacy_segment = crate::jam::legacy_packages_segment(&manifest);
        let new_segment = crate::jam::packages_segment(&manifest);
        let v1_body = fs::read_to_string(
            dir.path()
                .join(manifest.assets["js/app.js"].output.trim_start_matches('/')),
        )
        .unwrap();

        // Roll the JSON back to the pre-segments shape — the manifest the
        // upgrade path actually loads.
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("packages_segments");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 2;").unwrap();
        let manifest = graph.build().unwrap();
        assert!(
            manifest.packages_segments.contains_key(&legacy_segment),
            "the first upgrade records the segment old pages saved"
        );
        assert!(manifest.packages_segments.contains_key(&new_segment));

        let dirs = OutputDirs::under(dir.path());
        let url = format!("/paste/{legacy_segment}/app.js");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, v1_body.as_str()),
            "the legacy fingerprint must serve the build it was minted at"
        );
    }

    #[test]
    fn test_corrupted_compiled_file_never_serves_under_its_url() {
        // The URL's hash qualifier names the recorded bytes. If the file
        // is replaced after the build, the request must answer 404 — a
        // year-cached wrong answer would be permanent — for the current
        // version and a retained historical version alike.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 1;").unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.storage.backend = "dist".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let v1 = manifest.assets["js/app.js"].clone();
        let v1_file = dir.path().join(v1.output.trim_start_matches('/'));

        fs::write(dir.path().join("src/app.js"), "// @module app\nvar a = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let dirs = OutputDirs::under(dir.path());

        // Corrupt the retained v1 file, then request its exact pin at its
        // own canonical stamp — the response must reach the read and
        // refuse the tampered bytes rather than redirecting elsewhere.
        fs::write(&v1_file, "tampered").unwrap();
        let stamp = crate::manifest::stamp_epoch(&v1.timestamp).unwrap();
        let url = format!("/paste/{stamp}/app+v{}.0-h{}.min.js", v1.version, v1.hash);
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404, "corrupted pinned bytes must not serve");

        // Corrupt the current file; its pin 404s too, and so does the
        // loader URL — verified bytes are the only servable ones.
        let entry = &manifest.assets["js/app.js"];
        fs::write(
            dir.path().join(entry.output.trim_start_matches('/')),
            "tampered",
        )
        .unwrap();
        let stamp = crate::manifest::stamp_epoch(&entry.timestamp).unwrap();
        let url = format!(
            "/paste/{stamp}/app+v{}.0-h{}.min.js",
            entry.version, entry.hash
        );
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404);
        let url = format!("/paste/{}/app.js", crate::jam::packages_segment(&manifest));
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &url, None);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn test_custom_build_dir_name_is_honored_and_never_scanned() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/compiled")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
        // A source inside a build-dir-name directory is generated output's
        // place — it is never a module even before the first build.
        fs::write(
            dir.path().join("src/compiled/ignored.js"),
            "// @module ignored\nvar ignored = 1;",
        )
        .unwrap();

        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.output.build_dir_name = "compiled".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();

        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.output.starts_with("/src/compiled/app."),
            "custom build dir not used: {}",
            entry.output
        );
        assert!(dir
            .path()
            .join(entry.output.trim_start_matches('/'))
            .exists());
        assert!(!manifest.modules.contains_key("ignored"));
        assert!(!manifest.assets.contains_key("js/compiled/ignored.js"));
    }

    #[test]
    fn test_config_validation_rejects_bad_layout_and_build_dir() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();

        let mut config = dist_config(dir.path(), true);
        config.output.layout = "sideways".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("output.layout"));

        for bad in ["a/b", "a\\b", "..", ""] {
            let mut config = dist_config(dir.path(), true);
            config.output.build_dir_name = bad.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("build-dir-name"),
                "{bad:?} should be rejected"
            );
        }

        let mut config = beside_source_config(dir.path(), &["src"], true);
        config.output.base = outside.path().join("out");
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("base_dir"));
    }

    #[test]
    fn test_pruning_a_stem_never_touches_its_longer_sibling() {
        // `site` and `site.theme` compile into the same output directory;
        // rebuilding `site` must not count or delete `site.theme`'s file.
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("src/site.js"), "var site = 1;").unwrap();
            fs::write(dir.path().join("src/site.theme.js"), "var theme = 1;").unwrap();

            let mut config = dirs_config(dir.path(), &["src"], false);
            config.storage.backend = backend.into();
            let mut graph = AssetGraph::new(config);
            let manifest = graph.build().unwrap();
            // Flat layout: the manifest output joins onto output.base.
            let theme = dir.path().join(
                manifest.assets["js/site/theme.js"]
                    .output
                    .trim_start_matches('/')
                    .replacen("js/", "dist/js/", 1),
            );
            assert!(theme.exists(), "{backend}: theme output missing");

            fs::write(dir.path().join("src/site.js"), "var site = 2;").unwrap();
            let manifest = graph.build().unwrap();
            let site = dir.path().join(
                manifest.assets["js/site.js"]
                    .output
                    .trim_start_matches('/')
                    .replacen("js/", "dist/js/", 1),
            );
            assert!(site.exists(), "{backend}: new site output missing");
            assert!(
                theme.exists(),
                "{backend}: rebuilding site deleted site.theme's file"
            );
        }
    }

    #[test]
    fn test_dist_prune_keeps_every_version_the_manifest_references() {
        // The manifest retains keep_versions history entries plus the
        // current file — every one of their pinned URLs must still serve.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 0;").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.backend = "dist".into();
        config.storage.keep_versions = 2;
        let mut graph = AssetGraph::new(config.clone());
        graph.build().unwrap();
        for i in 1..=4 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            graph.build().unwrap();
        }

        // Five versions built; the manifest references current plus the
        // two retained history entries — all must exist on disk.
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 5);
        assert_eq!(entry.history.len(), 2);
        let referenced: Vec<&str> = std::iter::once(entry.output.as_str())
            .chain(entry.history.iter().map(|h| h.output.as_str()))
            .collect();
        for output in &referenced {
            assert!(
                dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "referenced output missing: {output}"
            );
        }

        // And exactly those files remain — the pruned older versions are gone.
        let on_disk: HashSet<String> = fs::read_dir(dir.path().join("dist/js"))
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().to_string_lossy().to_string();
                name.starts_with("app.").then(|| format!("/js/{name}"))
            })
            .collect();
        let expected: HashSet<String> = referenced.iter().map(|o| o.to_string()).collect();
        assert_eq!(on_disk, expected);

        // The oldest retained version still serves its pinned URL.
        let oldest = entry.history.iter().min_by_key(|h| h.version).unwrap();
        let stamp = crate::manifest::stamp_epoch(&oldest.timestamp).unwrap();
        let dirs = config.output_dirs();
        let r = crate::jam::combo(
            &manifest,
            &dirs,
            "/paste",
            &format!("/paste/{stamp}/app+v{}.0.js", oldest.version),
            None,
        );
        assert_eq!(r.status, 200, "pinned v{} must serve", oldest.version);
        let oldest_file = dir
            .path()
            .join("dist")
            .join(oldest.output.trim_start_matches('/'));
        assert_eq!(r.body, fs::read_to_string(&oldest_file).unwrap());
    }

    #[test]
    fn test_dist_default_retention_prunes_nothing() {
        // Keep-all is the default a parsed config inherits: every versioned
        // URL a build ever minted — the legacy +vN.0 spelling and the
        // hash-qualified form — must keep serving its own bytes across
        // rebuilds, not merely still exist on disk.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        // A real parsed config — retention omitted — so the test exercises
        // the production default instead of a fixture-supplied value.
        fs::write(
            dir.path().join("paste.conf"),
            "source.dirs = [\"src\"]\n\
             output.base = \"dist\"\n\
             output.js = \"dist/js\"\n\
             output.css = \"dist/css\"\n\
             output.images = \"dist/images\"\n\
             manifest = \"dist/manifest.json\"\n\
             storage.backend = \"dist\"\n\
             jam.base-path = \"/paste\"\n\
             jam.hash-urls = false\n",
        )
        .unwrap();
        let config = Config::load_with_base_dir(Path::new("paste.conf"), dir.path()).unwrap();
        assert_eq!(
            config.storage.keep_versions,
            usize::MAX,
            "omitted retention must inherit keep-all"
        );
        let dirs = config.output_dirs();

        // (legacy pin, qualified pin, expected body) captured per version —
        // the URLs a page rendered at that build can carry: the legacy
        // spelling under the opted-out manifest, the hash-qualified form
        // under any manifest.
        let mut served: Vec<(String, String, String)> = Vec::new();
        for i in 0..15 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            let manifest = AssetGraph::new(config.clone()).build().unwrap();
            let entry = &manifest.assets["js/app.js"];
            let stamp = crate::manifest::stamp_epoch(&entry.timestamp).unwrap();
            let legacy = format!("/paste/{stamp}/app+v{}.0.min.js", entry.version);
            let qualified = format!(
                "/paste/{stamp}/app+v{}.0-h{}.min.js",
                entry.version, entry.hash
            );
            let body = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(entry.output.trim_start_matches('/')),
            )
            .unwrap();
            served.push((legacy, qualified, body));
        }

        // Serve from a fresh manifest load — the on-disk artifact is what a
        // runtime actually consults.
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 15);
        assert_eq!(entry.history.len(), 14, "keep-all retains every version");
        for (legacy, qualified, body) in &served {
            for url in [legacy, qualified] {
                let r = crate::jam::combo(&manifest, &dirs, "/paste", url, None);
                assert_eq!(r.status, 200, "{url} must still serve");
                assert_eq!(&r.body, body, "{url} must serve its own bytes");
            }
        }
    }

    #[test]
    fn test_unchanged_rebuild_scans_each_versioned_group_once() {
        // Long retained history is not a reason to rescan a directory: the
        // cleanup pass prunes each distinct (dir, stem, extension) group
        // once, shared across the current-location and recorded-location
        // passes, so an unchanged rebuild of one asset with fifteen
        // retained files performs a single scan — not two, and not one per
        // version. The count is thread-local, so the parallel test runner
        // cannot move this measurement; the scan count itself is asserted,
        // never wall time. A bound above the history length keeps every
        // file while still scanning — keep-all never scans at all.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();

        let mut config = dist_config(dir.path(), true);
        config.storage.keep_versions = 100;
        for i in 0..15 {
            fs::write(
                js_dir.join("a.js"),
                format!("// @module a\nvar version = {i};"),
            )
            .unwrap();
            AssetGraph::new(config.clone()).build().unwrap();
        }

        let before = crate::storage::PRUNE_VERSION_SCANS.with(|count| count.get());
        AssetGraph::new(config).build().unwrap();
        let scans = crate::storage::PRUNE_VERSION_SCANS.with(|count| count.get()) - before;
        assert_eq!(
            scans, 1,
            "an unchanged rebuild scans each versioned group once"
        );
    }

    #[test]
    fn test_reduced_keep_versions_applies_to_a_retired_asset() {
        // A retired entry answers to the same retention bound an active one
        // does — otherwise lowering the setting could never reclaim history
        // whose update path no longer runs. Its current version is not
        // history: the exact pin for it keeps serving at any bound.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        for i in 0..6 {
            fs::write(
                js_dir.join("a.js"),
                format!("// @module a\nvar version = {i};"),
            )
            .unwrap();
            AssetGraph::new(config.clone()).build().unwrap();
        }

        fs::remove_file(js_dir.join("a.js")).unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let entry = &manifest.assets["js/a.js"];
        assert!(entry.retired && entry.history.len() == 5);
        let dirs = config.output_dirs();
        let current_pin = pin(&manifest, "js/a.js", "a", 6, "js");
        // History pins carry the retained version's own stamp — index 0 is
        // the oldest retained entry (v1), index 3 is v4.
        let v4_pin = history_pin(&manifest, "js/a.js", "a", 3, "js");
        let v1_pin = history_pin(&manifest, "js/a.js", "a", 0, "js");
        let current_body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.output.trim_start_matches('/')),
        )
        .unwrap();
        let v4_body = fs::read_to_string(
            dir.path().join("dist").join(
                entry
                    .history
                    .iter()
                    .find(|h| h.version == 4)
                    .unwrap()
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        config.storage.keep_versions = 2;
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let entry = &manifest.assets["js/a.js"];
        assert!(entry.retired);
        assert_eq!(
            entry.history.len(),
            2,
            "the bound drains a retired entry's history"
        );
        assert_eq!(
            entry.history.iter().map(|h| h.version).collect::<Vec<_>>(),
            vec![4, 5],
            "the newest retained versions survive"
        );
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &current_pin, None);
        assert_eq!((r.status, &r.body), (200, &current_body));
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &v4_pin, None);
        assert_eq!((r.status, &r.body), (200, &v4_body));
        assert_eq!(
            crate::jam::combo(&manifest, &dirs, "/paste", &v1_pin, None).status,
            404,
            "an evicted version answers 404, never other bytes"
        );

        config.storage.keep_versions = 0;
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let entry = &manifest.assets["js/a.js"];
        assert!(entry.retired);
        assert!(entry.history.is_empty(), "a zero bound keeps current only");
        let r = crate::jam::combo(&manifest, &dirs, "/paste", &current_pin, None);
        assert_eq!((r.status, &r.body), (200, &current_body));
        assert_eq!(
            crate::jam::combo(&manifest, &dirs, "/paste", &v4_pin, None).status,
            404
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_cleanup_never_unlinks_through_symlinked_output_dirs() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(dir.path().join("src/scss/s.scss"), ".s { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        AssetGraph::new(config.clone()).build().unwrap();

        // Record dist/js as a former js root by relocating the type dir.
        config.output.js = dir.path().join("dist/scripts");
        AssetGraph::new(config.clone()).build().unwrap();

        let manifest_path = dir.path().join("dist/manifest.json");
        let manifest_before = fs::read(&manifest_path).unwrap();

        // Case 1 — a recorded (no longer configured) root swapped for a
        // symlink into an external tree: the external `assetgraph.js`
        // survives, the build reports the unsafe root, and it succeeds.
        let external = dir.path().join("external");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("assetgraph.js"), "var external = 1;").unwrap();
        fs::remove_dir_all(dir.path().join("dist/js")).unwrap();
        symlink(&external, dir.path().join("dist/js")).unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(
            fs::read_to_string(external.join("assetgraph.js")).unwrap(),
            "var external = 1;",
            "cleanup must not reach through the symlinked recorded root"
        );

        // Case 2 — the currently configured type root symlinked: the build
        // fails closed and the served manifest is untouched.
        fs::create_dir_all(dir.path().join("real-scripts")).unwrap();
        fs::rename(
            dir.path().join("dist/scripts"),
            dir.path().join("real-scripts/held"),
        )
        .unwrap();
        symlink(
            dir.path().join("real-scripts/held"),
            dir.path().join("dist/scripts"),
        )
        .unwrap();
        let error = AssetGraph::new(config.clone())
            .build()
            .expect_err("a symlinked configured root must fail the build");
        assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
        assert_eq!(
            fs::read(&manifest_path).unwrap(),
            manifest_before,
            "the last valid manifest survives a blocked cleanup"
        );

        // Case 3 — an intermediate ancestor (`dist` itself) swapped for a
        // link: every root beneath it is redirected, so the same refusal
        // fires through the ancestor check.
        fs::remove_file(dir.path().join("dist/scripts")).unwrap();
        fs::remove_file(dir.path().join("dist/js")).unwrap();
        fs::rename(dir.path().join("dist"), dir.path().join("dist-held")).unwrap();
        symlink(dir.path().join("dist-held"), dir.path().join("dist")).unwrap();
        let error = AssetGraph::new(config.clone())
            .build()
            .expect_err("a symlinked intermediate ancestor must fail");
        assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
        assert_eq!(
            fs::read(dir.path().join("dist-held/manifest.json")).unwrap(),
            manifest_before,
            "the last valid manifest survives a blocked cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_cleanup_guards_recorded_and_version_dirs_and_allows_external_roots() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        // A nested module gives outputs in a subdirectory of the type root.
        fs::write(js_dir.join("nested.js"), "// @module a.b\nvar ab = 1;").unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        fs::write(dir.path().join("src/scss/s.scss"), ".s { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        config.storage.keep_versions = 0;
        AssetGraph::new(config.clone()).build().unwrap();

        // The directory the module's outputs live in is the recorded
        // version dir the prune pass will scan. Move the real tree aside,
        // plant a version-named bait file in it, and swap the recorded
        // location for a symlink into it: reads resolve through the link
        // (the retained file is still found), but no unlink may follow.
        let external = dir.path().join("external");
        fs::rename(dir.path().join("dist/js/a"), &external).unwrap();
        let kept = external.join("b.0123456789abcdef0123456789abcdef.v9.min.js");
        fs::write(&kept, "external bytes").unwrap();
        symlink(&external, dir.path().join("dist/js/a")).unwrap();

        let manifest_path = dir.path().join("dist/manifest.json");
        let served_manifest = fs::read(&manifest_path).unwrap();
        fs::remove_file(js_dir.join("nested.js")).unwrap();
        let error = AssetGraph::new(config.clone())
            .build()
            .expect_err("a symlinked recorded dir must block the build");
        assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            "external bytes",
            "the external tree's file survives"
        );
        assert_eq!(
            fs::read(&manifest_path).unwrap(),
            served_manifest,
            "the served manifest is the last valid build's, untouched"
        );

        // A symlink *file* is safe to unlink — removing it deletes the
        // link, never the target. Stage a legacy aggregate name as a link
        // inside the established configured root of a fresh project.
        let dir2 = tempdir().unwrap();
        let js2 = dir2.path().join("src/js");
        fs::create_dir_all(&js2).unwrap();
        fs::write(js2.join("a.js"), "var a = 1;").unwrap();
        let mut config2 = dirs_config(dir2.path(), &["src/js"], true);
        config2.output.js = dir2.path().join("dist/js");
        let external2 = dir2.path().join("external");
        fs::create_dir_all(&external2).unwrap();
        let target2 = external2.join("assetgraph.js");
        fs::write(&target2, "external aggregate").unwrap();
        fs::create_dir_all(dir2.path().join("dist/js")).unwrap();
        symlink(&target2, dir2.path().join("dist/js/assetgraph.js")).unwrap();
        AssetGraph::new(config2).build().unwrap();
        assert!(
            !dir2.path().join("dist/js/assetgraph.js").exists(),
            "the link itself was removed"
        );
        assert_eq!(
            fs::read_to_string(&target2).unwrap(),
            "external aggregate",
            "the link's target survived"
        );

        // A legitimate output root outside base_dir keeps working.
        let dir3 = tempdir().unwrap();
        let js3 = dir3.path().join("src/js");
        fs::create_dir_all(&js3).unwrap();
        fs::write(js3.join("a.js"), "var a = 1;").unwrap();
        let out = tempdir().unwrap();
        let mut config3 = dirs_config(dir3.path(), &["src/js"], true);
        config3.output.base = out.path().to_path_buf();
        config3.output.js = out.path().join("js");
        config3.output.css = out.path().join("css");
        config3.output.images = out.path().join("images");
        config3.manifest_path = out.path().join("manifest.json");
        config3.storage.backend = "local".into();
        let manifest = AssetGraph::new(config3).build().unwrap();
        assert!(
            manifest.assets.keys().any(|k| k.starts_with("js/")),
            "an out-of-base_dir output root still builds"
        );
    }

    #[test]
    fn test_js_edit_between_declaration_scan_and_compile_aborts() {
        // The declaration scan and the compile pass read the file once —
        // an edit landing between that read and publication must abort
        // the build, never emit bytes for a source state the manifest
        // metadata no longer describes. The hook stands in for a file
        // watcher write racing the build.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("a.js");
        fs::write(&source, "// @module a\nvar a = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let edited = source.clone();
        graph.mid_build_edit = Some(std::sync::Arc::new(move |_| {
            fs::write(&edited, "// @module a\nvar a = 2;").unwrap();
        }));
        let error = graph
            .build()
            .expect_err("a mid-build source edit must abort");
        assert!(
            format!("{error:#}").contains("changed during the build"),
            "got: {error:#}"
        );
        assert!(
            !dir.path().join("dist/manifest.json").exists(),
            "the aborted build published no manifest"
        );

        // Once the edit settles, the next build compiles the real source.
        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        let entry = &manifest.assets["js/a.js"];
        let body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.output.trim_start_matches('/')),
        )
        .unwrap();
        assert!(body.contains("a=2"), "the settled bytes build: {body}");
    }

    #[test]
    fn test_scss_import_edit_between_scan_and_compile_aborts() {
        // The entry file's bytes are shared between declaration scan and
        // compile, but grass resolves @import itself — an edit landing in
        // an imported partial mid-build must still abort, because the
        // emitted CSS would describe an input state that never existed.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n@import 'palette';\n.main { color: $brand; }",
        )
        .unwrap();
        let partial = scss_dir.join("_palette.scss");
        fs::write(&partial, "$brand: red;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let edited = partial.clone();
        graph.mid_build_edit = Some(std::sync::Arc::new(move |_| {
            fs::write(&edited, "$brand: blue;").unwrap();
        }));
        let error = graph
            .build()
            .expect_err("a mid-build import edit must abort");
        assert!(
            format!("{error:#}").contains("changed during the build"),
            "got: {error:#}"
        );
        assert!(
            !dir.path().join("dist/manifest.json").exists(),
            "the aborted build published no manifest"
        );

        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        let entry = &manifest.assets["css/main.css"];
        let body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.output.trim_start_matches('/')),
        )
        .unwrap();
        assert!(body.contains("#00f"), "the settled import builds: {body}");
    }

    #[cfg(unix)]
    #[test]
    fn test_dangling_directory_symlink_watches_missing_sass_referent() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let links = dir.path().join("shared");
        let variants = dir.path().join("variants");
        fs::create_dir_all(&links).unwrap();
        fs::create_dir_all(&variants).unwrap();
        let referent_dir = variants.join("missing");
        let alias_dir = links.join("themes");
        symlink(&referent_dir, &alias_dir).unwrap();
        let candidate = alias_dir.join("_theme.scss");
        let expected = referent_dir.join("_theme.scss");
        let snapshot = SnapshotFs::new(&candidate, None);
        assert!(!grass::Fs::is_file(&snapshot, &candidate));
        let missing = snapshot.missing();
        assert!(missing.contains(&observed_input_path(&expected)));
        let desired = desired_watches_with_inputs(&[], &missing, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&variants)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert!(!snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));

        fs::create_dir_all(&referent_dir).unwrap();
        fs::write(&expected, "$accent: red;").unwrap();
        assert!(snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));
    }

    #[cfg(unix)]
    #[test]
    fn test_dangling_sass_symlink_watches_missing_referent() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let links = dir.path().join("shared");
        let variants = dir.path().join("variants");
        fs::create_dir_all(&links).unwrap();
        fs::create_dir_all(&variants).unwrap();
        let referent = variants.join("_theme.scss");
        let alias = links.join("_theme.scss");
        symlink(&referent, &alias).unwrap();

        let snapshot = SnapshotFs::new(&alias, None);
        assert!(!grass::Fs::is_file(&snapshot, &alias));
        let missing = snapshot.missing();
        assert!(missing.contains(&observed_input_path(&alias)));
        assert!(missing.contains(&observed_input_path(&referent)));
        let watched = desired_watches_with_inputs(&[], &missing, dir.path());
        assert_eq!(
            watched.get(&observed_input_path(&links)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert_eq!(
            watched.get(&observed_input_path(&variants)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert!(!snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));

        fs::write(&referent, "$accent: red;").unwrap();
        assert!(snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_sass_path_preserves_symlink_parent_resolution() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        let real_package = dir.path().join("external/package");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&real_package).unwrap();
        let linked = project.join("scss");
        symlink(&real_package, &linked).unwrap();

        let candidate = linked.join("../_theme.scss");
        let real_candidate = dir.path().join("external/_theme.scss");
        assert!(!candidate.exists());
        assert_eq!(
            observed_input_path(&candidate),
            observed_input_path(&real_candidate)
        );
        assert_eq!(
            supplemental_watch(&observed_input_path(&candidate))
                .unwrap()
                .0,
            observed_input_path(&dir.path().join("external"))
        );

        fs::write(&real_candidate, "$accent: blue;").unwrap();
        assert_eq!(
            observed_input_path(&candidate),
            fs::canonicalize(&candidate).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_import_symlink_entry_is_an_input_even_after_canonicalization() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared");
        let targets = dir.path().join("targets");
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&targets).unwrap();
        let target = targets.join("one.scss");
        fs::write(&target, "$accent: red;").unwrap();
        let alias = shared.join("_theme.scss");
        symlink(&target, &alias).unwrap();
        let snapshot = SnapshotFs::new(&alias, None);
        let canonical = grass::Fs::canonicalize(&snapshot, &alias).unwrap();
        grass::Fs::read(&snapshot, &canonical).unwrap();
        let captured = snapshot.reads();
        assert!(captured.contains(&(canonical.clone(), b"$accent: red;".to_vec())));
        assert!(captured.contains(&(observed_entry_path(&alias), b"$accent: red;".to_vec())));
        assert_eq!(
            observed_entry_path(&alias),
            fs::canonicalize(&shared).unwrap().join("_theme.scss")
        );
        assert_ne!(
            observed_entry_path(&alias),
            fs::canonicalize(&alias).unwrap()
        );

        let next = targets.join("two.scss");
        fs::write(&next, "$accent: blue;").unwrap();
        let replacement = shared.join("replacement");
        symlink(&next, &replacement).unwrap();
        fs::rename(&replacement, &alias).unwrap();
        assert_ne!(fs::read(&alias).unwrap(), b"$accent: red;".to_vec());
        assert_eq!(fs::read(&canonical).unwrap(), b"$accent: red;".to_vec());
    }

    #[cfg(unix)]
    #[test]
    fn test_external_sass_directory_symlink_retarget_updates_file_watches() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let first = dir.path().join("variants/first");
        let second = dir.path().join("variants/second");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let old_import = first.join("_theme.scss");
        let new_import = second.join("_theme.scss");
        fs::write(&old_import, "$accent: red;").unwrap();
        fs::write(&new_import, "$accent: red;").unwrap();
        let alias = dir.path().join("src/linked");
        symlink(&first, &alias).unwrap();
        fs::write(
            scss.join("site.scss"),
            "@use '../linked/theme' as theme; .site { color: theme.$accent; }",
        )
        .unwrap();

        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let original = graph.build().unwrap();
        let inputs = external_build_inputs(&graph, &config, &roots);
        assert!(
            inputs.contains(&observed_entry_path(&alias)),
            "observe the directory link entry"
        );
        assert!(
            inputs.contains(&observed_input_path(&old_import)),
            "observe the resolved imported file"
        );
        let desired = desired_watches_with_inputs(&roots, &inputs, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&dir.path().join("src"))),
            Some(&RecursiveMode::NonRecursive)
        );

        let replacement = dir.path().join("src/new-linked");
        symlink(&second, &replacement).unwrap();
        fs::rename(&replacement, &alias).unwrap();
        assert!(
            external_inputs_changed_since_build(&graph, &inputs),
            "equal-content directory link retarget must refresh dependencies"
        );
        let unchanged = graph.build().unwrap();
        assert_eq!(
            original.assets["css/site.css"].hash,
            unchanged.assets["css/site.css"].hash
        );
        let next = external_build_inputs(&graph, &config, &roots);
        assert!(next.contains(&observed_input_path(&new_import)));
        assert!(!next.contains(&observed_input_path(&old_import)));
        assert!(!external_inputs_changed_since_build(&graph, &next));
        fs::write(&new_import, "$accent: blue;").unwrap();
        assert!(external_inputs_changed_since_build(&graph, &next));
        let updated = graph.build().unwrap();
        assert_ne!(
            original.assets["css/site.css"].hash,
            updated.assets["css/site.css"].hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_external_sass_import_symlink_retarget_is_watched() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        let targets = dir.path().join("variants");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&targets).unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        let first_target = targets.join("one.scss");
        let second_target = targets.join("two.scss");
        fs::write(&first_target, "$accent: red;").unwrap();
        // Same bytes, different target: a content-only observation would
        // leave the watcher attached to one.scss after the retarget.
        fs::write(&second_target, "$accent: red;").unwrap();
        let alias = shared.join("_theme.scss");
        symlink(&first_target, &alias).unwrap();
        fs::write(
            scss.join("site.scss"),
            "@use '../shared/theme' as theme; .site { color: theme.$accent; }",
        )
        .unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let first = graph.build().unwrap();
        let captured = external_build_inputs(&graph, &config, &roots);
        assert!(captured.contains(&observed_input_path(&first_target)));
        assert!(
            captured.contains(&observed_entry_path(&alias)),
            "track the link entry, not only its old target"
        );
        let desired = desired_watches_with_inputs(&roots, &captured, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&shared)),
            Some(&RecursiveMode::NonRecursive)
        );

        let replacement = shared.join("replacement");
        symlink(&second_target, &replacement).unwrap();
        fs::rename(&replacement, &alias).unwrap();
        assert!(
            external_inputs_changed_since_build(&graph, &captured),
            "same-content retargeting must trigger a registration refresh"
        );
        let event = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        ));
        assert!(is_source_change_with_inputs(
            &config, &roots, &captured, &alias, &event
        ));
        let same_bytes = graph.build().unwrap();
        assert_eq!(
            first.assets["css/site.css"].hash,
            same_bytes.assets["css/site.css"].hash
        );
        let new_inputs = external_build_inputs(&graph, &config, &roots);
        assert!(new_inputs.contains(&observed_input_path(&second_target)));
        assert!(!new_inputs.contains(&observed_input_path(&first_target)));
        assert!(!external_inputs_changed_since_build(&graph, &new_inputs));

        fs::write(&second_target, "$accent: blue;").unwrap();
        assert!(external_inputs_changed_since_build(&graph, &new_inputs));
        let second = graph.build().unwrap();
        assert_ne!(
            first.assets["css/site.css"].hash,
            second.assets["css/site.css"].hash
        );
    }

    #[test]
    fn test_watch_tracks_shared_external_sass_inputs_without_sibling_noise() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let partial = shared.join("_theme.scss");
        fs::write(&partial, "$accent: red;").unwrap();
        let body = "@use \"../shared/theme\" as theme; .app { color: theme.$accent; }";
        fs::write(scss.join("one.scss"), body).unwrap();
        fs::write(scss.join("two.scss"), body).unwrap();

        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let first = graph.build().unwrap();
        let external = external_build_inputs(&graph, &config, &roots);
        assert_eq!(external.len(), 1, "both users of a partial share one input");
        assert!(external.contains(&fs::canonicalize(&partial).unwrap()));
        let desired = desired_watches_with_inputs(&roots, &external, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&shared)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert_eq!(
            desired.get(&observed_input_path(&partial)),
            Some(&RecursiveMode::NonRecursive),
            "file writes on kqueue require an explicit file registration"
        );
        assert!(!external_inputs_changed_since_build(&graph, &external));
        let modify = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        assert!(is_source_change_with_inputs(
            &config, &roots, &external, &partial, &modify
        ));
        assert!(!is_source_change_with_inputs(
            &config,
            &roots,
            &external,
            &shared.join("unrelated.scss"),
            &modify
        ));
        // Both compiled assets actually depend on this outside-root file.
        fs::write(&partial, "$accent: blue;").unwrap();
        assert!(
            external_inputs_changed_since_build(&graph, &external),
            "an edit between publication and supplemental registration owes an immediate rescan"
        );
        let second = graph.build().unwrap();
        let second_inputs = external_build_inputs(&graph, &config, &roots);
        assert!(!external_inputs_changed_since_build(&graph, &second_inputs));
        for key in ["css/one.css", "css/two.css"] {
            assert_ne!(first.assets[key].hash, second.assets[key].hash);
            assert_eq!(first.assets[key].version + 1, second.assets[key].version);
        }
        // Successful removal of the imports also removes the supplemental
        // watch. Intermediate failed edits must not remove the last good
        // registration (that invariant is exercised by the next test).
        fs::write(scss.join("one.scss"), ".app { color: red; }").unwrap();
        fs::write(scss.join("two.scss"), ".app { color: blue; }").unwrap();
        graph.build().unwrap();
        assert!(external_build_inputs(&graph, &config, &roots).is_empty());
    }

    #[test]
    fn test_ambiguous_directory_entry_checks_external_sass_before_rebuilding() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(
            scss.join("site.scss"),
            "@use '../shared/theme'; .site {color: theme.$accent;}",
        )
        .unwrap();
        let partial = shared.join("_theme.scss");
        fs::write(&partial, "$accent: red;").unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        graph.build().unwrap();
        let external = external_build_inputs(&graph, &config, &roots);
        let sibling = shared.join("unrelated.scss");
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(sibling);
        let mut quiet = WatchBatch::default();
        accumulate_with_inputs(&mut quiet, &config, &roots, &external, Ok(event.clone()));
        assert!(quiet.ambiguous_external_entry);
        confirm_ambiguous_external_entry(&mut quiet, &graph, &config, &roots, &external, true);
        assert!(
            !quiet.rebuild && !quiet.rescan,
            "an unrelated sibling does not rebuild"
        );
        // A backend can report this exact same sibling path for a
        // creation/replacement of the actual imported file.
        fs::write(&partial, "$accent: blue;").unwrap();
        let mut changed = WatchBatch::default();
        accumulate_with_inputs(&mut changed, &config, &roots, &external, Ok(event));
        confirm_ambiguous_external_entry(&mut changed, &graph, &config, &roots, &external, true);
        assert!(changed.rebuild, "changed imported bytes trigger recovery");
        graph.build().unwrap();
        assert!(!external_inputs_changed_since_build(&graph, &external));
        // A later failed rebuild can clear captured_inputs before it
        // visits the prior import. Unrelated sibling notifications must
        // not turn that missing capture into an endless retry trigger.
        graph.captured_inputs.borrow_mut().clear();
        graph.failed_sass_snapshot.borrow_mut().reads.clear();
        graph.attempted_sass_inputs.borrow_mut().clear();
        let mut after_failure = WatchBatch {
            ambiguous_external_entry: true,
            ..Default::default()
        };
        confirm_ambiguous_external_entry(
            &mut after_failure,
            &graph,
            &config,
            &roots,
            &external,
            false,
        );
        assert!(
            !after_failure.rebuild,
            "a failed build does not invent sibling changes"
        );
    }

    #[test]
    fn test_new_missing_sass_import_exposes_recoverable_file_candidate() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let source = scss.join("site.scss");
        fs::write(&source, ".site { color: red; }").unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let baseline = graph.build().unwrap();
        let published = fs::read(&config.manifest_path).unwrap();
        let stable = external_build_inputs(&graph, &config, &roots);
        fs::write(
            &source,
            "@use \"../shared/new-palette\" as colors; .site { color: colors.$accent; }",
        )
        .unwrap();
        let failure = graph.build().unwrap_err();
        assert!(failure.downcast_ref::<RecoverableWatchInput>().is_some());
        assert_eq!(fs::read(&config.manifest_path).unwrap(), published);
        let candidates = external_attempted_inputs(&graph, &config, &roots);
        assert!(
            !failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "unchanged failed probes must not start an immediate retry loop"
        );
        let missing = shared.join("_new-palette.scss");
        let inputs = watched_external_inputs(&stable, &candidates);
        assert!(
            is_source_change_with_inputs(
                &config,
                &roots,
                &inputs,
                &missing,
                &notify::EventKind::Create(notify::event::CreateKind::File),
            ),
            "a newly created partial must itself trigger recovery: {inputs:?}"
        );
        let desired = desired_watches_with_inputs(&roots, &inputs, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&shared)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert!(is_recoverable_watch_failure(
            &failure, &roots, &inputs, &desired
        ));
        fs::write(&missing, "$accent: blue;").unwrap();
        assert!(
            failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "a repair before provisional watch registration must trigger a rescan"
        );
        let recovered = graph.build().unwrap();
        assert!(
            !failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "a successful build clears failed import snapshots"
        );
        assert_ne!(
            baseline.assets["css/site.css"].hash,
            recovered.assets["css/site.css"].hash
        );
        assert!(
            external_build_inputs(&graph, &config, &roots).contains(&observed_input_path(&missing))
        );
    }

    #[test]
    fn test_failed_sass_external_read_changed_before_watch_registration() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let source = scss.join("site.scss");
        let partial = shared.join("_theme.scss");
        fs::write(
            &source,
            "@use '../shared/theme'; .site { color: theme.$accent; }",
        )
        .unwrap();
        fs::write(&partial, "$accent: (").unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let failure = graph.build().unwrap_err();
        assert!(failure.downcast_ref::<RecoverableWatchInput>().is_some());
        assert!(!failed_external_attempts_changed_since_build(
            &graph, &config, &roots
        ));
        assert!(external_attempted_inputs(&graph, &config, &roots)
            .contains(&observed_input_path(&partial)));
        fs::write(&partial, "$accent: blue;").unwrap();
        assert!(
            failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "successful reads made by a failed compilation must also be rechecked"
        );
        let published = graph.build().unwrap();
        assert!(published.assets.contains_key("css/site.css"));
    }

    #[test]
    fn test_failed_sass_coherence_check_requests_rescan_after_external_edit() {
        // Grass compiles valid Sass and reads an external partial before
        // the per-asset coherence check discovers a concurrent edit.
        // The post-compile error must retain that partial's old read so
        // watch can recover an edit made before the external watch exists.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let source = scss.join("site.scss");
        let partial = shared.join("_theme.scss");
        fs::write(
            &source,
            "@use '../shared/theme'; .site { color: theme.$accent; }",
        )
        .unwrap();
        fs::write(&partial, "$accent: red;").unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let mut graph = AssetGraph::new(config.clone());
        let baseline = graph.build().unwrap();
        let published = fs::read(&config.manifest_path).unwrap();
        let source_for_hook = source.clone();
        let partial_for_hook = partial.clone();
        let fired = std::sync::atomic::AtomicBool::new(false);
        graph.mid_build_edit = Some(std::sync::Arc::new(move |path| {
            if path == source_for_hook && !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                fs::write(&partial_for_hook, "$accent: blue;").unwrap();
            }
        }));
        let failure = graph
            .build()
            .expect_err("an external input changed mid-build");
        assert!(failure.downcast_ref::<RecoverableWatchInput>().is_some());
        assert_eq!(fs::read(&config.manifest_path).unwrap(), published);
        let observed = observed_input_path(&partial);
        assert!(external_attempted_inputs(&graph, &config, &roots).contains(&observed));
        assert!(graph
            .failed_sass_snapshot
            .borrow()
            .reads
            .contains_key(&observed));
        assert!(
            failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "an edit after successful compilation but before a new watch registers requires a rescan"
        );
        graph.mid_build_edit = None;
        let rebuilt = graph.build().unwrap();
        assert_ne!(
            baseline.assets["css/site.css"].hash,
            rebuilt.assets["css/site.css"].hash
        );
        assert!(!failed_external_attempts_changed_since_build(
            &graph, &config, &roots
        ));
    }

    #[test]
    fn test_conflicting_sass_captures_do_not_suppress_watch_gap_recovery() {
        // Two entrypoints can consume different bytes for one external
        // import. If the latest failed read matches disk, it must not
        // mask the earlier captured bytes that invalidated that build.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        let shared = dir.path().join("src/shared");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let partial = shared.join("_theme.scss");
        let path = observed_input_path(&partial);
        let old = b"$accent: red;".to_vec();
        let new = b"$accent: blue;".to_vec();
        fs::write(&partial, &new).unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let graph = AssetGraph::new(config.clone());
        graph
            .attempted_sass_inputs
            .borrow_mut()
            .insert(path.clone());
        graph.capture_input(path.clone(), old.clone()).unwrap();
        assert!(graph.capture_input(path.clone(), new.clone()).is_err());
        assert_eq!(
            graph.failed_sass_snapshot.borrow().reads.get(&path),
            Some(&new)
        );
        assert!(
            failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "matching the last failed read cannot dismiss the earlier captured generation"
        );
        fs::write(&partial, &old).unwrap();
        assert!(
            failed_external_attempts_changed_since_build(&graph, &config, &roots),
            "matching the earlier capture cannot dismiss the conflicting failed generation"
        );
    }

    #[test]
    fn test_sass_negative_probe_checks_operation_not_just_path_existence() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file = dir.path().join("_palette.scss");
        fs::write(&file, "$accent: red;").unwrap();
        let snapshot = SnapshotFs::new(&file, None);
        assert!(!grass::Fs::is_dir(&snapshot, &file));
        assert!(!snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(snapshot
            .negative_probes()
            .iter()
            .any(SassNegativeProbe::now_succeeds));
    }

    #[test]
    fn test_missing_nested_sass_candidates_follow_ancestor_lifecycle() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let source = dir.path().join("src/scss/site.scss");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let config = dist_config(dir.path(), false);
        let roots = config.source_dirs();
        let fs_view = SnapshotFs::new(&source, None);
        let missing = dir.path().join("src/shared/nested/_theme.scss");
        assert!(!grass::Fs::is_file(&fs_view, &missing));
        let inputs = fs_view.missing();
        assert!(inputs.contains(&observed_input_path(&missing)));
        let desired = desired_watches_with_inputs(&roots, &inputs, dir.path());
        assert_eq!(
            desired.get(&observed_input_path(&dir.path().join("src"))),
            Some(&RecursiveMode::NonRecursive)
        );
        let lifecycle = notify::EventKind::Create(notify::event::CreateKind::Folder);
        let nested = dir.path().join("src/shared");
        assert!(is_source_change_with_inputs(
            &config, &roots, &inputs, &nested, &lifecycle
        ));
        // Kqueue can announce a directory entry change as an ordinary
        // parent-directory Modify rather than Create/Remove.
        let directory_modified = notify::EventKind::Modify(notify::event::ModifyKind::Any);
        assert!(is_source_change_with_inputs(
            &config,
            &roots,
            &inputs,
            &dir.path().join("src"),
            &directory_modified
        ));
        assert!(!is_source_change_with_inputs(
            &config,
            &roots,
            &inputs,
            &dir.path().join("src/unrelated"),
            &lifecycle
        ));
        fs::create_dir_all(&nested).unwrap();
        let new_desired = desired_watches_with_inputs(&roots, &inputs, dir.path());
        assert_eq!(
            new_desired.get(&observed_input_path(&nested)),
            Some(&RecursiveMode::NonRecursive)
        );
        assert!(is_source_change_with_inputs(
            &config,
            &roots,
            &inputs,
            &nested.join("nested"),
            &lifecycle
        ));
        // Directory deletion, not just leaf-file updates, invalidates a
        // consumed dependency as well.
        let removed = notify::EventKind::Remove(notify::event::RemoveKind::Folder);
        assert!(is_source_change_with_inputs(
            &config, &roots, &inputs, &nested, &removed
        ));
        let output = dir.path().join("dist/css/never.scss");
        assert!(!is_source_change_with_inputs(
            &config, &roots, &inputs, &output, &lifecycle
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_watch_recovers_a_canonical_error_under_a_symlinked_root() {
        // Captured inputs are tagged with canonical paths; a root spelled
        // through a symlink must still own them.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(real.join("src/scss")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let spelled_root = link.join("src/scss");
        let roots = vec![spelled_root.clone()];
        let desired = desired_watches(&roots, &link);
        let canonical = fs::canonicalize(&real)
            .unwrap()
            .join("src/scss/_shared.scss");
        let error =
            anyhow::anyhow!("changed during the build").context(RecoverableWatchInput(canonical));
        assert!(is_recoverable_watch_failure(
            &error,
            &roots,
            &BTreeSet::new(),
            &desired
        ));
    }

    #[test]
    fn test_watch_structural_errors_fail_and_disconnected_channel_is_an_error() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let root = dir.path().join("src/scss");
        fs::create_dir_all(&root).unwrap();
        let input = root.join("site.scss");
        let roots = vec![root.clone()];
        let desired = desired_watches(&roots, dir.path());
        let empty = BTreeSet::new();
        let corrupt_manifest = anyhow::anyhow!("corrupt existing manifest");
        assert!(!is_recoverable_watch_failure(
            &corrupt_manifest,
            &roots,
            &empty,
            &desired
        ));
        let watched = anyhow::anyhow!("invalid Sass").context(RecoverableWatchInput(input.clone()));
        assert!(is_recoverable_watch_failure(
            &watched, &roots, &empty, &desired
        ));
        assert!(!is_recoverable_watch_failure(
            &watched,
            &roots,
            &empty,
            &BTreeMap::new()
        ));
        let (tx, rx) = channel::<std::result::Result<notify::Event, notify::Error>>();
        drop(tx);
        let error = next_watch_message(&rx).unwrap_err();
        assert!(format!("{error:#}").contains("disconnected"));
    }

    #[test]
    fn test_snapshot_fs_serves_the_first_read_for_every_spelling() {
        // A file requested twice through different spellings of the same
        // canonical path must get the first read's bytes both times, even
        // when the file changed between requests — the compiler can only
        // ever observe one coherent input, never whatever the second
        // spelling happens to re-read.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let partial = dir.path().join("_palette.scss");
        fs::write(&partial, "$brand: red;").unwrap();

        let fs_view = SnapshotFs::new(&dir.path().join("main.scss"), None);
        let first = grass::Fs::read(&fs_view, &partial).unwrap();
        fs::write(&partial, "$brand: blue;").unwrap();
        let spelled = dir.path().join("./_palette.scss");
        let second = grass::Fs::read(&fs_view, &spelled).unwrap();
        assert_eq!(
            first, second,
            "a later spelling of the same file must see the captured bytes"
        );
        assert_eq!(first, b"$brand: red;");
    }

    #[test]
    fn test_late_source_edit_after_its_own_check_still_aborts() {
        // An edit landing after its asset's per-compile verification —
        // a JS source touched while a later SCSS asset compiles — escapes
        // the early check but must still abort at the pre-publish sweep,
        // with the previous manifest left alone.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(dir.path().join("src/scss")).unwrap();
        let js_source = dir.path().join("src/js/a.js");
        fs::write(&js_source, "// @module a\nvar a = 1;").unwrap();
        fs::write(
            dir.path().join("src/scss/site.scss"),
            ".site { color: red; }",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest_before = graph.build().unwrap();
        let manifest_bytes = fs::read(dir.path().join("dist/manifest.json")).unwrap();

        let edited = js_source.clone();
        graph.mid_build_edit = Some(std::sync::Arc::new(move |path| {
            // Fire only while a later asset compiles — the JS source's own
            // verification has already passed, so only the pre-publish
            // sweep can catch this.
            if path.extension().is_some_and(|e| e == "scss") {
                fs::write(&edited, "// @module a\nvar a = 2;").unwrap();
            }
        }));
        let error = graph
            .build()
            .expect_err("a late source edit must abort at publication");
        assert!(
            format!("{error:#}").contains("changed during the build"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(dir.path().join("dist/manifest.json")).unwrap(),
            manifest_bytes,
            "the previous manifest survives an aborted publication"
        );
        assert_eq!(manifest_before.assets["js/a.js"].version, 1);
    }

    #[test]
    fn test_source_root_vanishing_after_preflight_still_fails() {
        // The preflight guard passed while the root still existed; it
        // disappeared before the walk reached it — discovery's own
        // classification must refuse the outage the same way, never
        // scanning the absent root into an empty source set.
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            let js = dir.path().join("src/js");
            fs::create_dir_all(&js).unwrap();
            fs::write(js.join("app.js"), "var app = 1;").unwrap();
            let mut config = dirs_config(dir.path(), &["src/js"], true);
            config.storage.backend = backend.into();
            let built = AssetGraph::new(config.clone()).build().unwrap();
            let manifest_bytes = fs::read(&config.manifest_path).unwrap();
            let served = dir
                .path()
                .join("dist")
                .join(built.assets["js/app.js"].output.trim_start_matches('/'));
            let served_bytes = fs::read(&served).unwrap();

            let mut graph = AssetGraph::new(config.clone());
            let victim = js.clone();
            let fired = std::sync::atomic::AtomicBool::new(false);
            graph.pre_discovery_edit = Some(std::sync::Arc::new(move || {
                // The post-guard/pre-discovery seam — the root is removed
                // once, after the guard has already passed it.
                if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    fs::remove_dir_all(&victim).unwrap();
                }
            }));
            let error = graph
                .build()
                .expect_err(&format!("a root vanishing mid-build must fail ({backend})"));
            assert!(
                format!("{error:#}").contains("disappeared"),
                "{backend}: got {error:#}"
            );
            assert_eq!(
                fs::read(&config.manifest_path).unwrap(),
                manifest_bytes,
                "{backend}: the served manifest is preserved"
            );
            assert_eq!(
                fs::read(&served).unwrap(),
                served_bytes,
                "{backend}: the pinned output still resolves to its bytes"
            );
        }
    }

    #[test]
    fn test_include_root_vanishing_after_preflight_still_fails() {
        // Same inter-phase outage for an include path: the walk's own
        // classification carries the provenance check, not the
        // already-passed preflight.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        let vendor = dir.path().join("vendor");
        fs::create_dir_all(&js).unwrap();
        fs::create_dir_all(&vendor).unwrap();
        fs::write(js.join("app.js"), "var app = 1;").unwrap();
        fs::write(vendor.join("lib.js"), "// @module lib\nvar lib = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![vendor.clone()];
        AssetGraph::new(config.clone()).build().unwrap();
        let manifest_bytes = fs::read(&config.manifest_path).unwrap();

        let mut graph = AssetGraph::new(config.clone());
        let fired = std::sync::atomic::AtomicBool::new(false);
        graph.pre_discovery_edit = Some(std::sync::Arc::new(move || {
            if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                fs::remove_dir_all(&vendor).unwrap();
            }
        }));
        let error = graph
            .build()
            .expect_err("an include root vanishing mid-build must fail");
        assert!(
            format!("{error:#}").contains("disappeared"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            manifest_bytes,
            "the served manifest is preserved"
        );
    }

    #[test]
    fn test_vanishing_nested_include_root_fails_like_a_source_root() {
        // An include root nested under the project records a
        // base_dir-relative label — `vendor/packages/widgets/widget.js` —
        // so a vanish must be attributed to it by the same path check a
        // source root gets; a basename check never matches that label.
        use tempfile::tempdir;
        for backend in ["dist", "local"] {
            let dir = tempdir().unwrap();
            let widgets = dir.path().join("vendor/packages/widgets");
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            fs::create_dir_all(&widgets).unwrap();
            fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
            fs::write(widgets.join("widget.js"), "var widget = 1;").unwrap();
            let mut config = dirs_config(dir.path(), &["src/js"], true);
            config.storage.backend = backend.into();
            config.include_paths = vec![widgets.clone()];
            let manifest_path = config.manifest_path.clone();
            let manifest = AssetGraph::new(config.clone())
                .build()
                .unwrap_or_else(|e| panic!("{backend} healthy build failed: {e:#}"));
            let entry = manifest
                .assets
                .get("js/widget.js")
                .expect("the nested include contributed its module");
            assert_eq!(entry.source, "vendor/packages/widgets/widget.js");
            let asset_path = config
                .output
                .base
                .join(entry.output.trim_start_matches('/'));
            let asset_bytes = fs::read(&asset_path).unwrap();
            let manifest_bytes = fs::read(&manifest_path).unwrap();

            fs::remove_dir_all(&widgets).unwrap();
            let error = AssetGraph::new(config.clone())
                .build()
                .expect_err("a vanished populated nested include root must fail");
            assert!(
                format!("{error:#}").contains("configured source root disappeared"),
                "{backend} got: {error:#}"
            );
            assert_eq!(
                fs::read(&manifest_path).unwrap(),
                manifest_bytes,
                "{backend}: the served manifest is preserved"
            );
            assert_eq!(
                fs::read(&asset_path).unwrap(),
                asset_bytes,
                "{backend}: the served file is preserved"
            );
        }
    }

    #[test]
    fn test_vanishing_nested_include_root_mid_walk_fails() {
        // The nested include disappears between preflight and discovery —
        // the walk's own classification repeats the provenance check.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let widgets = dir.path().join("vendor/packages/widgets");
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(&widgets).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::write(widgets.join("widget.js"), "var widget = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![widgets.clone()];
        AssetGraph::new(config.clone()).build().unwrap();

        let mut graph = AssetGraph::new(config.clone());
        let fired = std::sync::atomic::AtomicBool::new(false);
        graph.pre_discovery_edit = Some(std::sync::Arc::new(move || {
            if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                fs::remove_dir_all(&widgets).unwrap();
            }
        }));
        let error = graph
            .build()
            .expect_err("a nested include vanishing mid-build must fail");
        assert!(
            format!("{error:#}").contains("disappeared"),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_absent_nested_include_that_never_contributed_is_optional() {
        // A nested include root that never existed never contributed —
        // its absence stays optional, and a lone file delete inside a
        // still-existing root stays an intentional retirement.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let widgets = dir.path().join("vendor/packages/widgets");
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![widgets.clone()];
        AssetGraph::new(config.clone())
            .build()
            .expect("a never-contributed absent include root stays optional");

        fs::create_dir_all(&widgets).unwrap();
        fs::write(widgets.join("widget.js"), "var widget = 1;").unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        assert!(manifest.assets.contains_key("js/widget.js"));

        fs::remove_file(widgets.join("widget.js")).unwrap();
        let manifest = AssetGraph::new(config)
            .build()
            .expect("deleting a file inside a surviving root is an intentional retirement");
        assert!(
            manifest.assets["js/widget.js"].retired,
            "the entry survives retired so its historical pins still serve"
        );
    }

    #[test]
    fn test_vanishing_same_basename_include_root_fails_closed() {
        // Two external include roots both named `vendor` produce the same
        // `vendor/…` label space. Removing one while the other still
        // serves that space cannot be attributed — the build refuses with
        // an attribution diagnostic rather than blaming the empty root
        // for the other's entries or silently dropping either.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let external_a = tempdir().unwrap();
        let external_b = tempdir().unwrap();
        let vendor_a = external_a.path().join("vendor");
        let vendor_b = external_b.path().join("vendor");
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::create_dir_all(&vendor_a).unwrap();
        fs::create_dir_all(&vendor_b).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::write(vendor_b.join("widget.js"), "var widget = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.include_paths = vec![vendor_a.clone(), vendor_b.clone()];
        let manifest_path = config.manifest_path.clone();
        AssetGraph::new(config.clone())
            .build()
            .expect("both roots present is unambiguous");
        let manifest_bytes = fs::read(&manifest_path).unwrap();

        // The EMPTY root is the one that disappears — nothing it
        // contributed can be named, but the surviving root's labels are
        // indistinguishable from it, so the build refuses honestly.
        fs::remove_dir_all(&vendor_a).unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("an ambiguous vanished root must refuse the build");
        let message = format!("{error:#}");
        assert!(
            message.contains("attribut") && message.contains(&vendor_a.display().to_string()),
            "got: {message}"
        );
        assert_eq!(fs::read(&manifest_path).unwrap(), manifest_bytes);
    }

    #[cfg(unix)]
    fn symlink(link: &std::path::Path, target: &std::path::Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(not(unix))]
    fn symlink(link: &std::path::Path, target: &std::path::Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    #[test]
    fn test_manifest_symlink_is_refused_without_touching_its_target() {
        // A configured manifest path that is a symlink reads its target
        // but publication replaces the link itself — the deployment's
        // shared manifest keeps stale bytes while the configured path
        // becomes an independent file. Both backends refuse before any
        // write; the link and its target survive untouched.
        use tempfile::tempdir;
        for backend in ["dist", "local"] {
            let dir = tempdir().unwrap();
            let external = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
            let mut config = dirs_config(dir.path(), &["src/js"], true);
            config.storage.backend = backend.into();
            AssetGraph::new(config.clone())
                .build()
                .unwrap_or_else(|e| panic!("{backend} healthy build failed: {e:#}"));

            // The arrangement a shared-manifest deployment makes: the
            // authoritative file lives elsewhere and the configured path
            // links to it.
            let shared = external.path().join("authoritative.json");
            fs::copy(&config.manifest_path, &shared).unwrap();
            fs::remove_file(&config.manifest_path).unwrap();
            if !symlink(&config.manifest_path, &shared) {
                eprintln!("symlink creation refused by this environment — skipping");
                return;
            }
            let shared_bytes = fs::read(&shared).unwrap();

            let error = AssetGraph::new(config.clone())
                .build()
                .expect_err("a symlinked manifest path must refuse the build");
            assert!(
                format!("{error:#}").contains("symlink"),
                "{backend} got: {error:#}"
            );
            assert!(
                fs::symlink_metadata(&config.manifest_path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{backend}: the configured path is still a link, not a replaced file"
            );
            assert_eq!(
                fs::read(&shared).unwrap(),
                shared_bytes,
                "{backend}: the shared manifest bytes are untouched"
            );
            assert!(
                fs::read_dir(dir.path().join("dist/js"))
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false),
                "{backend}: the built output is untouched"
            );
        }
    }

    #[test]
    fn test_manifest_dangling_symlink_is_not_a_first_build() {
        // A symlink to a missing target reads as absent — but it is not a
        // first build: the link itself is a configured destination the
        // rename would replace. Refuse before treating it as empty.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        let missing_target = dir.path().join("elsewhere").join("manifest.json");
        if !symlink(&config.manifest_path, &missing_target) {
            eprintln!("symlink creation refused by this environment — skipping");
            return;
        }
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a dangling manifest link must refuse the build");
        assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
        assert!(fs::symlink_metadata(dir.path().join("dist/manifest.json"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn test_manifest_directory_destination_is_refused() {
        // A non-regular, non-link entry at the manifest path is not a
        // missing manifest either — refuse rather than rename over it.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.manifest_path = dir.path().join("manifest.json");
        fs::create_dir(&config.manifest_path).unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("a directory manifest destination must refuse");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_local_unchanged_rebuild_keeps_the_page_generation() {
        // Local rebuilds stamp `generated` fresh each run — that alone
        // must not mint a new manifest generation, or a page holding the
        // token dies mid-render. An identical rebuild keeps the exact
        // JSON bytes and the page's token stays valid.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/features/paste.js"),
            dir.path().join("src/js/paste.js"),
        )
        .unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();

        let settings = crate::api::Settings {
            manifest: config.manifest_path.clone(),
            output_base: config.output.base.clone(),
            output_js: Some(config.output.js.clone()),
            output_css: Some(config.output.css.clone()),
            cdn_base: String::new(),
            base_path: "/paste".into(),
            inline_threshold: 0,
            per_module: false,
        };
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;
        let page = crate::api::render_page(&settings, events, None).unwrap();

        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            bytes,
            "an unchanged local rebuild never rewrites the authoritative JSON"
        );
        // The same page continues — its token still matches the bytes.
        crate::api::render_page(&settings, events, Some(&page.generation)).unwrap();
    }

    #[test]
    fn test_local_content_change_still_retires_the_page() {
        // A real change still mints a new generation: the old page's
        // token is rejected, the fresh page's is not.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/features/paste.js"),
            dir.path().join("src/js/paste.js"),
        )
        .unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let settings = crate::api::Settings {
            manifest: config.manifest_path.clone(),
            output_base: config.output.base.clone(),
            output_js: None,
            output_css: None,
            cdn_base: String::new(),
            base_path: "/paste".into(),
            inline_threshold: 0,
            per_module: false,
        };
        let events = r#"{"events":[{"jam":{"js":["app"]}},{"dump":"js"}]}"#;
        let page = crate::api::render_page(&settings, events, None).unwrap();

        fs::write(dir.path().join("src/js/app.js"), "var app = 2;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        let error = crate::api::render_page(&settings, events, Some(&page.generation))
            .expect_err("a changed manifest rejects the old token");
        assert!(
            error
                .downcast_ref::<crate::api::GenerationMismatch>()
                .is_some(),
            "got: {error:#}"
        );
        crate::api::render_page(&settings, events, None)
            .expect("a fresh page renders against the new generation");

        // A build-level policy change is also effective state — same
        // compiled bytes, different manifest fields.
        config.jam.hash_urls = !config.jam.hash_urls;
        let page = crate::api::render_page(&settings, events, None).unwrap();
        AssetGraph::new(config).build().unwrap();
        let error = crate::api::render_page(&settings, events, Some(&page.generation))
            .expect_err("a policy change rejects the old token");
        assert!(
            error
                .downcast_ref::<crate::api::GenerationMismatch>()
                .is_some(),
            "got: {error:#}"
        );
    }

    #[test]
    fn test_local_output_repair_keeps_the_generation() {
        // A corrupted versioned file is repaired to its recorded bytes
        // without republishing the manifest — the page's token survives.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();
        let output = config
            .output
            .base
            .join(manifest.assets["js/app.js"].output.trim_start_matches('/'));
        let good = fs::read(&output).unwrap();
        fs::write(&output, "corrupted").unwrap();

        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(fs::read(&output).unwrap(), good, "the file is repaired");
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            bytes,
            "the manifest generation is untouched"
        );
    }

    #[test]
    fn test_local_unchanged_rebuild_repairs_a_missing_or_stale_cache() {
        // The `.bin` is disposable but worth having: skipping its repair
        // on a no-op build would leave every later `load` re-parsing the
        // JSON (or re-rejecting a stale cache) forever. Repair is
        // maintenance — it never moves the authoritative bytes or the
        // page-generation token.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();
        let cache = config.manifest_path.with_extension("bin");
        assert!(Manifest::cache_is_current(&config.manifest_path, &bytes));

        // Missing: the rebuild restores a valid cache.
        fs::remove_file(&cache).unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
        assert!(Manifest::cache_is_current(&config.manifest_path, &bytes));

        // Corrupt: `load` already dropped the undecodable entry; the
        // rebuild installs a fresh one rather than leaving the gap.
        fs::write(&cache, b"not bincode").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
        assert!(Manifest::cache_is_current(&config.manifest_path, &bytes));

        // Stale hash: a decodable cache bound to different JSON bytes
        // would be bypassed on every load until replaced.
        let other = tempdir().unwrap();
        fs::create_dir_all(other.path().join("src/js")).unwrap();
        fs::write(other.path().join("src/js/app.js"), "var app = 2;").unwrap();
        let mut other_config = dirs_config(other.path(), &["src/js"], true);
        other_config.storage.backend = "local".into();
        AssetGraph::new(other_config.clone()).build().unwrap();
        fs::copy(other_config.manifest_path.with_extension("bin"), &cache).unwrap();
        assert!(!Manifest::cache_is_current(&config.manifest_path, &bytes));
        AssetGraph::new(config.clone()).build().unwrap();
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
        assert!(Manifest::cache_is_current(&config.manifest_path, &bytes));

        // A current cache is never rewritten — the commit path renames,
        // so a needless rewrite would mint a new file identity.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let inode = fs::metadata(&cache).unwrap().ino();
            AssetGraph::new(config.clone()).build().unwrap();
            assert_eq!(
                fs::metadata(&cache).unwrap().ino(),
                inode,
                "a valid cache was rewritten on a no-op build"
            );
        }
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
    }

    #[test]
    fn test_dist_unchanged_rebuild_removes_a_stray_cache() {
        // dist ships the reproducible JSON only — a `.bin` parked beside
        // it must not survive even when nothing else changed.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "dist".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();
        let cache = config.manifest_path.with_extension("bin");
        assert!(fs::symlink_metadata(&cache).is_err());

        fs::write(&cache, b"stray").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            fs::symlink_metadata(&cache).is_err(),
            "the stray cache entry survived an unchanged dist build"
        );
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
    }

    #[test]
    fn test_dist_unchanged_rebuild_removes_a_dangling_cache_link() {
        // A `.bin` link to a missing target reads as absent to
        // `exists()` — but its directory entry is still a stray that
        // must not ship. Only the entry goes; the target path is never
        // touched.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "dist".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();
        let cache = config.manifest_path.with_extension("bin");

        if !symlink(&cache, &dir.path().join("missing-target.bin")) {
            eprintln!("symlink creation refused by this environment — skipping");
            return;
        }
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            fs::symlink_metadata(&cache).is_err(),
            "the dangling link survived an unchanged dist build"
        );
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);

        // A link to a populated file loses only the entry — the
        // target's bytes are never touched.
        let external = tempdir().unwrap();
        let sentinel = external.path().join("outside.bin");
        fs::write(&sentinel, b"outside").unwrap();
        if !symlink(&cache, &sentinel) {
            eprintln!("symlink creation refused by this environment — skipping");
            return;
        }
        AssetGraph::new(config.clone()).build().unwrap();
        assert!(fs::symlink_metadata(&cache).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
    }

    #[test]
    fn test_local_cache_repair_failure_warns_without_failing_the_build() {
        // On a no-op build the cache is pure maintenance: a staging or
        // install fault warns and leaves the authoritative JSON alone —
        // never a failed build over a file nobody published.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.storage.backend = "local".into();
        AssetGraph::new(config.clone()).build().unwrap();
        let bytes = fs::read(&config.manifest_path).unwrap();
        let cache = config.manifest_path.with_extension("bin");

        fs::remove_file(&cache).unwrap();
        crate::manifest::test_hooks::FAIL_CACHE_STAGE.with(|f| f.set(true));
        AssetGraph::new(config.clone())
            .build()
            .expect("a cache-staging failure on a no-op build is a warning");
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
        assert!(fs::symlink_metadata(&cache).is_err());

        crate::manifest::test_hooks::FAIL_CACHE_COMMIT.with(|f| f.set(true));
        AssetGraph::new(config.clone())
            .build()
            .expect("a cache-install failure on a no-op build is a warning");
        assert_eq!(fs::read(&config.manifest_path).unwrap(), bytes);
        assert!(fs::symlink_metadata(&cache).is_err());
        // The failed install removed its own staged file — nothing is
        // stranded beside the manifest.
        let residue = fs::read_dir(config.manifest_path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".paste-staged.bin")
            });
        assert!(!residue, "a staged cache file was stranded");
    }

    #[test]
    fn test_vanishing_external_source_root_with_a_bare_label_peer_fails_closed() {
        // Two external source roots both record bare file names —
        // neither is attributable once one disappears.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let external_a = tempdir().unwrap();
        let external_b = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        fs::write(external_b.path().join("lib.js"), "var lib = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src/js"], true);
        config.source.dirs = vec![
            dir.path().join("src/js"),
            external_a.path().to_path_buf(),
            external_b.path().to_path_buf(),
        ];
        AssetGraph::new(config.clone())
            .build()
            .expect("both roots present is unambiguous");

        fs::remove_dir_all(external_a.path()).unwrap();
        let error = AssetGraph::new(config)
            .build()
            .expect_err("an ambiguous vanished source root must refuse");
        assert!(format!("{error:#}").contains("attribut"), "got: {error:#}");
    }

    #[test]
    fn test_shared_import_changed_between_two_stylesheets_aborts() {
        // a.scss and b.scss import the same partial through different
        // spellings of one canonical path; an edit landing after a's
        // compile captured it but before b's compile reads it must abort
        // — a published manifest may never carry two generations of one
        // input.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        let a_scss = scss.join("a.scss");
        fs::write(&a_scss, "@import 'palette';\n.a { color: $brand; }").unwrap();
        fs::write(
            scss.join("b.scss"),
            "@import './palette';\n.b { color: $brand; }",
        )
        .unwrap();
        let partial = scss.join("_palette.scss");
        fs::write(&partial, "$brand: red;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        graph.build().unwrap();
        let manifest_bytes = fs::read(dir.path().join("dist/manifest.json")).unwrap();

        let edited = partial.clone();
        let a_path = a_scss.clone();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        graph.mid_build_edit = Some(std::sync::Arc::new(move |path| {
            // The hook fires for a.scss twice: once inside its compile —
            // where its own verification would catch the edit — and once
            // after its inputs merged, the window before b.scss reads the
            // shared import. The edit lands on the second firing.
            if path == a_path && calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                fs::write(&edited, "$brand: blue;").unwrap();
            }
        }));
        let error = graph
            .build()
            .expect_err("a shared import edited between compiles must abort");
        assert!(
            format!("{error:#}").contains("changed during the build"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read(dir.path().join("dist/manifest.json")).unwrap(),
            manifest_bytes,
            "the served manifest is preserved"
        );

        // Once the import settles, both styles compile from it.
        let rebuilt = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        for key in ["css/a.css", "css/b.css"] {
            let body = fs::read_to_string(
                dir.path()
                    .join("dist")
                    .join(rebuilt.assets[key].output.trim_start_matches('/')),
            )
            .unwrap();
            assert!(
                body.contains("#00f"),
                "{key} did not build the settled import: {body}"
            );
        }
    }

    #[test]
    fn test_unparseable_js_passes_through_verbatim_and_still_builds() {
        // The documented contract for a file the minifier cannot handle —
        // genuinely malformed input and syntax newer than the parser alike:
        // warn and emit the author's exact bytes. The output is never worse
        // than minify=false — it is the same source the file held — so the
        // optional optimization must never gate publication.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = "// legacy helper\nvar broken = function( { ;\n";
        fs::write(js_dir.join("legacy.js"), source).unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), true));
        let manifest = graph
            .build()
            .expect("an unminifiable file must not fail the build");
        let entry = &manifest.assets["js/legacy.js"];
        let body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.output.trim_start_matches('/')),
        )
        .unwrap();
        assert_eq!(body, source, "unparseable input passes through byte-exact");
    }

    #[test]
    fn test_unparseable_js_with_declarations_fails_naming_the_file() {
        // An unparseable file yields no comment spans, so its declarations
        // cannot be read; a path-derived name with no dependencies would
        // publish the wrong module silently.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("main.js"),
            "// @module app.main\n// @requires app.util\nvar broken = function( { ;\n",
        )
        .unwrap();
        let err = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .expect_err("declarations that cannot be read must fail the build");
        assert!(format!("{err:#}").contains("main.js"), "{err:#}");
        assert!(!dir.path().join("dist/manifest.json").exists());
    }

    #[test]
    fn test_declarations_accept_a_trailing_semicolon_and_ignore_lookalikes() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("autogrow.scss"),
            "/*\n@module \"paste.ui.autogrow\";\n*/\n.a { color: red; }\n",
        )
        .unwrap();
        fs::write(
            scss_dir.join("line.scss"),
            "// @module \"paste.ui.line\";\n.b { color: red; }\n",
        )
        .unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "// @module app\n// Loaded by the @module-federation/enhanced host.\n/* uses @module.exports and @requires: none */\nvar a = 1;\n",
        )
        .unwrap();
        let manifest = AssetGraph::new(dist_config(dir.path(), false))
            .build()
            .unwrap();
        assert!(manifest.modules.contains_key("paste.ui.autogrow"));
        assert!(manifest.modules.contains_key("paste.ui.line"));
        assert_eq!(
            manifest.assets["js/app.js"].dependencies,
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_build_fails_fast_while_another_writer_holds_the_lock() {
        // The writer contract is fail-fast: a second build against a
        // manifest whose lock is held refuses rather than interleaving.
        // Holding the lock from the test makes the contention
        // deterministic — no racing two real builds.
        use fs2::FileExt;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("a.js"), "var a = 1;").unwrap();

        let config = dist_config(dir.path(), true);
        let lock_path = dir.path().join("manifest.lock");
        let held = std::fs::File::create(&lock_path).unwrap();
        held.try_lock_exclusive().unwrap();

        let error = AssetGraph::new(config)
            .build()
            .expect_err("a held write lock must fail the build fast");
        assert!(
            format!("{error:#}").contains("write lock"),
            "got: {error:#}"
        );

        // Releasing the lock frees the manifest — the next build writes,
        // and whatever bytes were already in the lock file are left
        // alone: the lock is never truncated or rewritten.
        held.unlock().unwrap();
        drop(held);
        fs::write(&lock_path, "previous lock's state").unwrap();
        let manifest = AssetGraph::new(dist_config(dir.path(), true))
            .build()
            .unwrap();
        assert!(manifest.assets.contains_key("js/a.js"));
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            "previous lock's state",
            "an existing lock file is preserved, not truncated"
        );

        // A symlink at the lock path is refused without ever following
        // it — the target's bytes are untouchable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir2 = tempdir().unwrap();
            let js2 = dir2.path().join("src/js");
            fs::create_dir_all(&js2).unwrap();
            fs::write(js2.join("a.js"), "var a = 1;").unwrap();
            let sentinel = dir2.path().join("sentinel.txt");
            fs::write(&sentinel, "not the lock").unwrap();
            symlink(&sentinel, dir2.path().join("manifest.lock")).unwrap();
            let error = AssetGraph::new(dist_config(dir2.path(), true))
                .build()
                .expect_err("a symlinked lock path must refuse the build");
            assert!(format!("{error:#}").contains("symlink"), "got: {error:#}");
            assert_eq!(
                fs::read_to_string(&sentinel).unwrap(),
                "not the lock",
                "the symlink's target was never opened"
            );
        }

        // A FIFO at the lock path cannot stall the build waiting on a
        // writer: the nonblocking open returns it, the handle check
        // refuses it, and the error says what it found.
        #[cfg(unix)]
        {
            let dir3 = tempdir().unwrap();
            {
                use std::os::unix::ffi::OsStrExt;
                let fifo = dir3.path().join("manifest.lock");
                let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
            }
            let error = AssetGraph::new(dist_config(dir3.path(), true))
                .build()
                .expect_err("a special file at the lock path must refuse the build");
            assert!(
                format!("{error:#}").contains("not a regular file"),
                "got: {error:#}"
            );
        }
    }

    #[test]
    fn test_save_never_removes_unowned_files() {
        // A save only writes and renames its own uniquely-tagged tmps —
        // files beside the manifest belong to whoever put them there,
        // even ones matching the `<name>.tmp.<pid>.<n>` producer shape.
        // Crash residue is inert: publication uses unique tags and an
        // atomic rename, and load takes the renamed manifest only.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        let planted = [
            (".paste-staged.json.1.1.deadbeefdeadbeef", "torn json"),
            (".paste-staged.bin.1.1.deadbeefdeadbeef", "torn bin"),
            ("manifest.json.tmp.notes", "a prefix-sharing neighbour"),
            ("manifest.json.bak", "keep me"),
            ("other.json.tmp.1.1", "keep me too"),
        ];
        for (name, body) in planted {
            fs::write(dir.path().join(name), body).unwrap();
        }

        let manifest = Manifest::new();
        manifest.save(&manifest_path).unwrap();

        assert!(manifest_path.exists());
        assert!(manifest_path.with_extension("bin").exists());
        for (name, body) in planted {
            assert_eq!(
                fs::read(dir.path().join(name)).unwrap(),
                body.as_bytes(),
                "{name} was unlinked by a save that never wrote it"
            );
        }
        // The abandoned producer-named residue never confuses the loader:
        // the renamed JSON and its digest-checked cache are authoritative.
        let loaded = Manifest::load(&manifest_path).unwrap();
        assert_eq!(loaded.version, manifest.version);
    }

    #[test]
    fn test_moved_output_root_removes_the_stale_rollup_at_its_old_location() {
        // The manifest's output_roots records which directories served as
        // each type's output root, so relocating the type outputs does not
        // strand a legacy aggregate under the old root — while every
        // recorded version moves to the new layout and keeps serving.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&js_dir).unwrap();
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 1;").unwrap();
        // A css asset too: only a directory the manifest recorded as that
        // type's root is a former type root, so dist/css must appear in
        // output_roots for its stale rollup to be owned.
        fs::write(
            scss_dir.join("main.scss"),
            "// @module main\n.main { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        AssetGraph::new(config.clone()).build().unwrap();
        // A second version gives the module a retained history entry — the
        // relocation must move and keep serving both, not just the current.
        fs::write(js_dir.join("a.js"), "// @module a\nvar a = 2;").unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        let entry = &manifest.assets["js/a.js"];
        assert_eq!((entry.version, entry.history.len()), (2, 1));
        let v2_body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.output.trim_start_matches('/')),
        )
        .unwrap();
        let v1_pin = history_pin(&manifest, "js/a.js", "a", 0, "js");
        let v2_pin = pin(&manifest, "js/a.js", "a", 2, "js");
        let v1_body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(entry.history[0].output.trim_start_matches('/')),
        )
        .unwrap();

        // The tree an aggregate-emitting build left behind.
        fs::create_dir_all(dir.path().join("dist/css")).unwrap();
        fs::write(dir.path().join("dist/js/assetgraph.js"), "var stale = 1;").unwrap();
        fs::write(dir.path().join("dist/css/assetgraph.css"), ".stale{}").unwrap();

        config.output.js = dir.path().join("dist/scripts");
        config.output.css = dir.path().join("dist/styles");
        let mut moved = AssetGraph::new(config.clone());
        let manifest = moved.build().unwrap();

        assert!(!dir.path().join("dist/js/assetgraph.js").exists());
        assert!(!dir.path().join("dist/css/assetgraph.css").exists());
        assert!(!dir.path().join("dist/scripts/assetgraph.js").exists());
        assert!(!dir.path().join("dist/styles/assetgraph.css").exists());

        let entry = &manifest.assets["js/a.js"];
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let moved_file = moved
                .output_dirs
                .compiled_file(output, AssetType::Js)
                .unwrap();
            assert!(
                moved_file.starts_with(dir.path().join("dist/scripts")) && moved_file.exists(),
                "every retained version relocates to the new root: {output}"
            );
        }
        let r = crate::jam::combo(&manifest, &moved.output_dirs, "/paste", &v2_pin, None);
        assert_eq!((r.status, &r.body), (200, &v2_body));
        let r = crate::jam::combo(&manifest, &moved.output_dirs, "/paste", &v1_pin, None);
        assert_eq!((r.status, &r.body), (200, &v1_body));
    }

    // ============================================================
    // Output repair: an existing file is not proof of correct bytes
    // ============================================================

    #[test]
    fn test_rebuild_repairs_a_corrupted_output_without_bumping_the_version() {
        // A tampered or truncated output used to survive every rebuild: the
        // write was skipped whenever the file existed. The build must now
        // restore the bytes an unchanged source produces, without rolling
        // the version its URLs are pinned to.
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src")).unwrap();
            fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
            fs::write(dir.path().join("src/site.scss"), ".site { color: red; }").unwrap();

            let mut config = dirs_config(dir.path(), &["src"], false);
            config.storage.backend = backend.into();
            let mut graph = AssetGraph::new(config);
            let manifest = graph.build().unwrap();

            for (key, prefix) in [("js/app.js", "dist"), ("css/site.css", "dist")] {
                let entry = &manifest.assets[key];
                let file = dir
                    .path()
                    .join(prefix)
                    .join(entry.output.trim_start_matches('/'));
                let expected = fs::read(&file).unwrap();
                fs::write(&file, b"corrupted").unwrap();

                let rebuilt = graph.build().unwrap();
                let entry = &rebuilt.assets[key];
                assert_eq!(
                    entry.version, 1,
                    "{backend}: repairing {key} must not roll the version"
                );
                assert_eq!(
                    fs::read(&file).unwrap(),
                    expected,
                    "{backend}: corrupted {key} output must be restored"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_output_file_is_refused_never_written_through() {
        // A versioned output path that is a symlink is not this build's
        // file: the write refuses rather than open the link's target —
        // whether the target happens to hold the expected bytes or not —
        // and the external file is left exactly as it was.
        use std::os::unix::fs::symlink;
        for backend in ["local", "dist"] {
            for matching in [true, false] {
                use tempfile::tempdir;
                let dir = tempdir().unwrap();
                fs::create_dir_all(dir.path().join("src")).unwrap();
                fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

                let mut config = dirs_config(dir.path(), &["src"], false);
                config.storage.backend = backend.into();
                let mut graph = AssetGraph::new(config);
                let manifest = graph.build().unwrap();
                let entry = &manifest.assets["js/app.js"];
                let file = dir
                    .path()
                    .join("dist")
                    .join(entry.output.trim_start_matches('/'));
                let expected = fs::read(&file).unwrap();

                let outside = tempdir().unwrap();
                let target = outside.path().join("external.js");
                let external_bytes = if matching {
                    expected.clone()
                } else {
                    b"external bytes".to_vec()
                };
                fs::write(&target, &external_bytes).unwrap();
                fs::remove_file(&file).unwrap();
                symlink(&target, &file).unwrap();

                let err = graph.build().unwrap_err();
                assert!(
                    format!("{err:#}").contains("symlink"),
                    "{backend}/matching={matching}: the refusal must name the symlink: {err:#}"
                );
                assert_eq!(
                    fs::read(&target).unwrap(),
                    external_bytes,
                    "{backend}/matching={matching}: the external target must be untouched"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_intermediate_output_dir_is_refused() {
        // A nested module's output lands in a generated subdirectory. If
        // that subdirectory is a symlink into an external tree the write
        // must refuse before creating or touching anything beneath it.
        use std::os::unix::fs::symlink;
        for backend in ["local", "dist"] {
            use tempfile::tempdir;
            let dir = tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/a")).unwrap();
            fs::write(
                dir.path().join("src/a/dom.js"),
                "// @module a.dom\nvar a = 1;",
            )
            .unwrap();

            let mut config = dirs_config(dir.path(), &["src"], false);
            config.storage.backend = backend.into();
            let mut graph = AssetGraph::new(config);
            let manifest = graph.build().unwrap();
            let entry = &manifest.assets["js/a/dom.js"];
            let file = dir
                .path()
                .join("dist")
                .join(entry.output.trim_start_matches('/'));
            let subdir = file.parent().unwrap().to_path_buf();
            assert_ne!(
                subdir,
                dir.path().join("dist/js"),
                "the test needs a genuinely nested output directory"
            );

            let outside = tempdir().unwrap();
            let external_dir = outside.path().join("external");
            fs::create_dir_all(&external_dir).unwrap();
            fs::write(external_dir.join("kept.txt"), "external").unwrap();
            fs::remove_dir_all(&subdir).unwrap();
            symlink(&external_dir, &subdir).unwrap();

            let err = graph.build().unwrap_err();
            assert!(
                format!("{err:#}").contains("symlink"),
                "{backend}: the refusal must name the symlinked component: {err:#}"
            );
            assert!(
                !external_dir.join(file.file_name().unwrap()).exists(),
                "{backend}: nothing may be written into the external tree"
            );
        }
    }

    #[test]
    fn test_post_publish_cache_failure_warns_but_keeps_the_build() {
        // Once the manifest JSON is swapped in, remaining work is
        // maintenance: a bincode cache that cannot be installed — here a
        // directory parked at its path — degrades to a warning. The build
        // still reports success and the authoritative manifest is live.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.backend = "local".into();
        fs::create_dir_all(dir.path().join("dist/manifest.bin")).unwrap();

        let mut graph = AssetGraph::new(config);
        let manifest = graph
            .build()
            .expect("a cache-commit failure must not fail the build");
        assert!(manifest.assets.contains_key("js/app.js"));
        assert!(dir.path().join("dist/manifest.json").exists());
        assert!(
            dir.path().join("dist/manifest.bin").is_dir(),
            "the blocking directory is left alone — loads bypass it for the JSON"
        );
        // The same manifest loads cleanly: the unusable cache path is
        // detected and the authoritative JSON serves instead.
        let loaded = Manifest::load(&dir.path().join("dist/manifest.json")).unwrap();
        assert_eq!(loaded.assets.len(), manifest.assets.len());
    }

    #[test]
    fn test_build_fails_on_a_corrupt_manifest_and_preserves_it() {
        // A manifest that cannot be loaded used to be silently replaced by a
        // fresh one, discarding every version the deployable URLs name. The
        // build must fail and leave the file untouched.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = dirs_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config.clone());
        graph.build().unwrap();

        let corrupt = b"{ not json";
        fs::write(&config.manifest_path, corrupt).unwrap();
        let before = fs::read(&config.manifest_path).unwrap();

        assert!(
            graph.build().is_err(),
            "a corrupt manifest must fail the build"
        );
        assert_eq!(
            fs::read(&config.manifest_path).unwrap(),
            before,
            "the corrupt manifest must be preserved, not reset"
        );
    }

    #[test]
    fn test_layout_move_into_a_source_root_keeps_building() {
        // `source.dirs = ["js"]`: a flat → beside-source move materializes
        // the retained flat v1 at `js/…`, inside the source root. That root
        // must stay a source, not become recorded generated output.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("js")).unwrap();
        fs::write(dir.path().join("js/app.js"), "var app = 1;").unwrap();
        let flat = AssetGraph::new(dirs_config(dir.path(), &["js"], false))
            .build()
            .unwrap();
        assert_eq!(flat.assets["js/app.js"].version, 1);

        let config = beside_source_config(dir.path(), &["js"], false);
        let moved = AssetGraph::new(config.clone()).build().unwrap();
        assert!(
            !moved.generated_dirs.contains(&"js".to_string()),
            "{:?}",
            moved.generated_dirs
        );
        let mut graph = AssetGraph::new(config);
        let third = graph.build().expect("a later build must still succeed");
        assert_eq!(
            serve(&third, &graph, &pin(&third, "js/app.js", "app", 1, "js")),
            200
        );
    }

    #[test]
    fn test_old_output_dir_may_later_hold_a_source_root() {
        // generated_dirs outlive their config; once a configured source
        // root sits under a recorded label, the label stops blocking it.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();
        let mut config = dirs_config(dir.path(), &["src"], false);
        let web = dir.path().join("web");
        config.output.base = web.clone();
        config.output.js = web.join("js");
        config.output.css = web.join("css");
        config.output.images = web.join("images");
        config.manifest_path = dir.path().join("dist/manifest.json");
        AssetGraph::new(config).build().unwrap();

        fs::create_dir_all(dir.path().join("web/src")).unwrap();
        fs::write(
            dir.path().join("web/src/extra.js"),
            "// @module extra\nvar e = 1;",
        )
        .unwrap();
        let config = dirs_config(dir.path(), &["src", "web/src"], false);
        let manifest = AssetGraph::new(config)
            .build()
            .expect("a source root under an old output dir must build");
        assert!(manifest.modules.contains_key("extra"));
    }

    #[test]
    fn test_case_only_rename_keeps_every_referenced_file() {
        // On a case-insensitive filesystem `src/JS` and `src/js` are one
        // directory; pruning must not treat the other spelling's files as
        // unreferenced. Skipped where the filesystem is case-sensitive.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("probe"), "").unwrap();
        if !dir.path().join("PROBE").exists() {
            return;
        }
        fs::create_dir_all(dir.path().join("src/JS")).unwrap();
        fs::write(dir.path().join("src/JS/app.js"), "var app = 1;").unwrap();
        let config = beside_source_config(dir.path(), &["src"], false);
        AssetGraph::new(config.clone()).build().unwrap();

        fs::rename(dir.path().join("src/JS"), dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 2;").unwrap();
        let manifest = AssetGraph::new(config.clone()).build().unwrap();
        for entry in manifest.assets.values() {
            for output in
                std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
            {
                assert!(
                    dir.path().join(output.trim_start_matches('/')).is_file(),
                    "referenced output {output} was deleted"
                );
            }
        }
        AssetGraph::new(config)
            .build()
            .expect("the next build still finds every retained file");
    }

    #[test]
    fn test_deleting_a_file_that_claimed_a_conventional_name_keeps_building() {
        // legacy.scss declared `site.theme`, which is also site/theme.scss's
        // path-derived name; once legacy.scss is gone the conventional alias
        // must not collide with the pin legacy issued.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(scss.join("site")).unwrap();
        fs::write(
            scss.join("legacy.scss"),
            "// @module site.theme\n.a { color: red; }\n",
        )
        .unwrap();
        fs::write(
            scss.join("site/theme.scss"),
            "// @module theme\n.b { color: blue; }\n",
        )
        .unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        fs::remove_file(scss.join("legacy.scss")).unwrap();
        AssetGraph::new(config.clone())
            .build()
            .expect("the rebuild after the deletion");
        AssetGraph::new(config)
            .build()
            .expect("and every build after it");
    }

    #[test]
    fn test_seeding_legacy_name_pins_is_deterministic() {
        // Two CSS keys derive the conventional name `a.b`; seeding a
        // manifest written before historical_names existed must not let
        // HashMap order decide which one claims the slot.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js = dir.path().join("src/js");
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(js.join("a")).unwrap();
        fs::create_dir_all(scss.join("a")).unwrap();
        fs::write(js.join("a/b.js"), "var ab = 1;").unwrap();
        fs::write(scss.join("a/b.scss"), ".x { color: red; }").unwrap();
        fs::write(scss.join("a.b.scss"), ".y { color: blue; }").unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&config.manifest_path).unwrap()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("historical_names");
        object.remove("ambiguous_name_pins");
        let legacy = serde_json::to_vec(&legacy).unwrap();
        let mut outcomes = BTreeSet::new();
        for _ in 0..10 {
            fs::write(&config.manifest_path, &legacy).unwrap();
            AssetGraph::new(config.clone()).build().unwrap();
            outcomes.insert(fs::read(&config.manifest_path).unwrap());
        }
        assert_eq!(
            outcomes.len(),
            1,
            "seeding produced {} different manifests",
            outcomes.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_manifest_through_a_symlinked_directory_is_refused() {
        // `meta -> ../elsewhere`: the manifest's directory must obey the
        // same no-follow rule as output directories.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let project = dir.path().join("proj");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(project.join("src/js")).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(project.join("src/js/app.js"), "var app = 1;").unwrap();
        std::os::unix::fs::symlink(&elsewhere, project.join("meta")).unwrap();
        let mut config = dist_config(&project, false);
        config.manifest_path = project.join("meta/sub/manifest.json");
        let err = AssetGraph::new(config)
            .build()
            .expect_err("a symlinked manifest directory must be refused");
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(
            !elsewhere.join("sub").exists(),
            "nothing was written through the link"
        );
    }

    #[test]
    fn test_retained_hashes_verify_in_any_hex_case() {
        // Combo accepts a recorded hash in either case; the build's
        // retention pass must agree instead of calling the file corrupted.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/js")).unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 1;").unwrap();
        let config = dist_config(dir.path(), false);
        AssetGraph::new(config.clone()).build().unwrap();
        fs::write(dir.path().join("src/js/app.js"), "var app = 2;").unwrap();
        AssetGraph::new(config.clone()).build().unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&config.manifest_path).unwrap()).unwrap();
        for history in json["assets"]["js/app.js"]["history"]
            .as_array_mut()
            .unwrap()
        {
            let upper = history["hash"].as_str().unwrap().to_ascii_uppercase();
            history["hash"] = serde_json::Value::String(upper);
        }
        fs::write(&config.manifest_path, serde_json::to_vec(&json).unwrap()).unwrap();
        AssetGraph::new(config)
            .build()
            .expect("an uppercase recorded hash still verifies");
    }

    #[test]
    fn test_layout_change_issues_a_new_version_and_the_old_url_still_serves() {
        // A retained version's public path never moves in place. A layout
        // change that relocates the output issues a NEW version at the new
        // path; the old version retires into history verbatim — output,
        // hash, version — and its file keeps serving the bytes a saved
        // page fetched, until retention (and only retention) lets it go.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = dirs_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        let v1_output = entry.output.clone();
        let v1_hash = entry.hash.clone();
        assert_eq!(entry.version, 1);
        assert!(
            v1_output.starts_with("/js/app.") && v1_output.ends_with(".v1.min.js"),
            "flat output: {v1_output}"
        );
        // The bytes the direct URL served before the move.
        let v1_file = dir
            .path()
            .join("dist")
            .join(v1_output.trim_start_matches('/'));
        let v1_body = fs::read_to_string(&v1_file).unwrap();

        // flat -> beside-source: a new version at the new public path.
        let config = beside_source_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(
            entry.version, 2,
            "a path-moving change issues a new version"
        );
        let v2_output = entry.output.clone();
        assert_ne!(v1_output, v2_output);
        assert!(
            v2_output.starts_with("/src/_paste_build/app.") && v2_output.ends_with(".v2.min.js"),
            "beside-source output: {v2_output}"
        );

        // v1 is history verbatim — same version, hash, and public path.
        let h1 = entry
            .history
            .iter()
            .find(|h| h.version == 1)
            .expect("v1 retained in history");
        assert_eq!(h1.output, v1_output, "the old version keeps its path");
        assert_eq!(h1.hash, v1_hash);

        // The pinned URL resolves to the same recorded path, and the file
        // the new layout's resolver maps it to still holds the bytes it
        // served — no rewrite of the record, no substitution of content.
        let pinned = manifest
            .resolve_version("app", "js", Some(1))
            .expect("the recorded pin resolves");
        assert_eq!(pinned.output, v1_output);
        assert_eq!(pinned.hash, v1_hash);
        let v1_served = graph
            .output_dirs
            .compiled_file(&v1_output, AssetType::Js)
            .expect("the recorded output resolves under the new layout");
        assert_eq!(
            fs::read_to_string(&v1_served).unwrap(),
            v1_body,
            "the old direct URL still serves its original bytes"
        );
        // The new URL exists at its own path and serves the current build.
        let v2_file = dir.path().join(v2_output.trim_start_matches('/'));
        assert!(
            v2_file.exists(),
            "the current output exists at the new path"
        );
        assert_eq!(
            fs::read_to_string(&v2_file).unwrap(),
            v1_body,
            "identical content, relocated — not rewritten in place"
        );

        // Retention, not relocation, is what eventually releases the old
        // location: another content change under keep_versions=1 evicts v1,
        // and its file goes with the record.
        let mut config = config;
        config.storage.keep_versions = 1;
        fs::write(dir.path().join("src/app.js"), "var app = 2;").unwrap();
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 3);
        assert!(
            entry.history.iter().all(|h| h.version != 1),
            "v1 is outside the retained bound"
        );
        assert!(
            manifest.resolve_version("app", "js", Some(1)).is_none(),
            "an evicted version no longer resolves"
        );
        assert!(
            !graph
                .output_dirs
                .compiled_file(&v1_output, AssetType::Js)
                .is_some_and(|path| path.exists()),
            "the evicted version's file is pruned with its record"
        );
    }

    #[test]
    fn test_layout_change_same_path_keeps_the_version() {
        // A change that leaves the recorded public path identical is not a
        // move: identical content at the same output prefix keeps its
        // version. This pins the boundary — only a changed public path
        // bumps.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.js"), "var app = 1;").unwrap();

        let config = dirs_config(dir.path(), &["src"], false);
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let v1 = manifest.assets["js/app.js"].output.clone();

        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 1, "same bytes, same path — no version bump");
        assert_eq!(entry.output, v1);
        assert!(entry.history.is_empty());
    }

    #[test]
    fn test_reducing_keep_versions_trims_history_on_an_unchanged_rebuild() {
        // Retention lives in the manifest: lowering keep_versions must drain
        // recorded history on the next build — even one whose content did
        // not change — and pruning must remove exactly the versions no
        // longer referenced while every retained file survives.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.keep_versions = 3;
        let mut graph = AssetGraph::new(config.clone());
        for i in 0..4 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            graph.build().unwrap();
        }
        let manifest = Manifest::load(&config.manifest_path).unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 4);
        assert_eq!(entry.history.len(), 3);
        let evicted: Vec<String> = entry
            .history
            .iter()
            .take(2)
            .map(|h| h.output.clone())
            .collect();

        // An unchanged rebuild under a smaller bound trims the recorded
        // history and the files it no longer names.
        config.storage.keep_versions = 1;
        let mut graph = AssetGraph::new(config.clone());
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 4, "unchanged content keeps the version");
        assert_eq!(entry.history.len(), 1, "history honors the new bound");
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let file = dir.path().join("dist").join(output.trim_start_matches('/'));
            assert!(
                file.exists(),
                "a referenced version must exist: {}",
                file.display()
            );
        }
        for output in &evicted {
            assert!(
                !dir.path()
                    .join("dist")
                    .join(output.trim_start_matches('/'))
                    .exists(),
                "an evicted version must be pruned: {output}"
            );
        }
    }

    #[test]
    fn test_local_manifest_records_no_version_history() {
        // Local keeps only the current file on disk, so its manifest keeps
        // no retained history either — an entry it could never resolve to a
        // file would be a dangling pin.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        for i in 0..3 {
            fs::write(dir.path().join("src/app.js"), format!("var app = {i};")).unwrap();
            let manifest = graph.build().unwrap();
            let entry = &manifest.assets["js/app.js"];
            assert_eq!(entry.version, i + 1);
            assert!(
                entry.history.is_empty(),
                "local records no prunable history"
            );
        }
    }

    /// Answer a combo request against a built graph — tests read `status`.
    fn serve(manifest: &Manifest, graph: &AssetGraph, path: &str) -> u16 {
        crate::jam::combo(manifest, &graph.output_dirs, "/paste", path, None).status
    }

    /// The stamp segment for a pinned URL naming `entry`'s own stamp.
    fn pin(manifest: &Manifest, key: &str, module: &str, version: u32, ext: &str) -> String {
        let entry = &manifest.assets[key];
        let stamp = crate::manifest::stamp_epoch(&entry.timestamp).unwrap();
        format!("/paste/{stamp}/{module}+v{version}.0.min.{ext}")
    }

    #[test]
    fn test_declared_module_source_rename_retains_the_old_direct_urls() {
        // A declared module whose source moved files must not delete the
        // recorded outputs a page may hold: the old key's own direct file
        // URL stays materialized and the version history keeps its
        // recorded stems — renamed after the new source they would be
        // files nobody's URL ever named, while the recorded ones deleted.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.theme */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.modules["site.theme"], "css/theme.css");
        let old_output = manifest.assets["css/theme.css"].output.clone();
        let old_file = dir
            .path()
            .join("dist")
            .join(old_output.trim_start_matches('/'));
        assert!(old_file.exists());
        let old_body = fs::read_to_string(&old_file).unwrap();

        // Same @module, new file: the entry carries its sequence to the
        // new key — changed bytes, so v2 — and the old key stays behind,
        // retired, holding the records the old URLs name.
        fs::remove_file(scss.join("theme.scss")).unwrap();
        fs::write(
            scss.join("main.scss"),
            "/* @module site.theme */\n.site { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.modules["site.theme"], "css/main.css");
        assert_eq!(manifest.assets["css/main.css"].version, 2);
        assert!(!manifest.assets["css/main.css"].retired);
        assert!(
            manifest.assets["css/theme.css"].retired,
            "the old key remains, retired — it is the old URL's record"
        );

        // The direct file URL a page saved still resolves to its bytes.
        assert!(
            old_file.exists(),
            "the old key's versioned file must survive the rename: {}",
            old_file.display()
        );
        assert_eq!(fs::read_to_string(&old_file).unwrap(), old_body);

        // The pin names the module's version — the carried entry's
        // history serves the recorded output under its own stem.
        let pinned = pin(&manifest, "css/theme.css", "site.theme", 1, "css");
        assert_eq!(
            serve(&manifest, &graph, &pinned),
            200,
            "the v1 pin must keep serving the recorded bytes"
        );
        let resolved = manifest
            .resolve_version("site.theme", "css", Some(1))
            .unwrap();
        assert_eq!(
            resolved.output, old_output,
            "the history record keeps its recorded output — the URL"
        );
        let history_file = dir
            .path()
            .join("dist")
            .join(resolved.output.trim_start_matches('/'));
        assert!(history_file.exists(), "the v1 file must exist");

        // The retired key's own path-derived name still pins too.
        let key_pin = pin(&manifest, "css/theme.css", "theme", 1, "css");
        assert_eq!(serve(&manifest, &graph, &key_pin), 200);
    }

    #[test]
    fn test_module_rename_retires_the_old_name_but_its_pins_still_serve() {
        // Renaming a @module on the same file moves the name's CURRENT
        // mapping — the old spelling is no longer a live name and an
        // unpinned lookup resolves nothing. What the old name keeps is
        // its recorded identity: `site.theme+v1` still serves the bytes
        // it meant when issued.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.theme */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "site.theme", 1, "css");
        let old_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        // The module renames in place — same file, new declared name.
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.main */\n.site { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.modules["site.main"], "css/theme.css");

        // The old name's pin still serves the recorded bytes — the
        // mapping moved, the name's recorded identity did not vanish.
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &old_pin, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, old_body.as_str()),
            "a pin under the renamed module's old spelling must serve"
        );
        // An unpinned lookup under the old spelling resolves nothing —
        // the name is history, not a live alias into the renamed asset.
        assert_eq!(
            manifest.asset_key_for_module("site.theme", "css"),
            None,
            "a renamed module's old name is no longer current"
        );
    }

    #[test]
    fn test_reusing_a_historical_name_cannot_alias_an_old_pin() {
        // The name a module once carried is not reusable identity: when a
        // different file later claims it, the name's current binding moves
        // to the new claimant at a version past the recorded history, and
        // every earlier `name+vN` pin keeps serving ITS bytes — never the
        // new file's.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.theme */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "site.theme", 1, "css");
        let old_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        // theme.scss is deleted and site/theme.scss — a different asset
        // whose PATH derives the same conventional name — claims
        // `site.theme`. The name moves to the claimant, never backwards
        // onto the retired asset.
        fs::remove_file(scss.join("theme.scss")).unwrap();
        fs::create_dir_all(scss.join("site")).unwrap();
        fs::write(scss.join("site/theme.scss"), ".nav { color: navy; }").unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest
                .asset_key_for_module("site.theme", "css")
                .as_deref(),
            Some("css/site/theme.css"),
            "the reused name binds the new claimant"
        );
        let claimant = &manifest.assets["css/site/theme.css"];
        assert_eq!(
            claimant.version, 2,
            "the name's recorded v1 forces the claimant past the recorded history"
        );

        // The old pin still serves the old asset's bytes — the name's
        // recorded identity, not whatever now carries the spelling.
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &old_pin, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, old_body.as_str()),
            "name+v1 keeps serving the version that name was issued on"
        );
        // And it is THAT identity: resolving the pin names the old key and
        // old recorded hash, never the claimant's.
        let resolved = manifest
            .resolve_version("site.theme", "css", Some(1))
            .unwrap();
        assert_eq!(
            resolved.hash, manifest.assets["css/theme.css"].hash,
            "the pin verifies against the recorded bytes"
        );

        // The claimant's own pin serves ITS bytes under the same name at
        // its own version — `name+v2` and `name+v1` are different files.
        let new_pin = pin(&manifest, "css/site/theme.css", "site.theme", 2, "css");
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &new_pin, None);
        let new_body = fs::read_to_string(
            dir.path()
                .join("dist")
                .join(claimant.output.trim_start_matches('/')),
        )
        .unwrap();
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, new_body.as_str()),
            "the claimant serves under its own version, distinct identity"
        );
    }

    #[test]
    fn test_rename_only_same_content_retires_the_old_name() {
        // A rename with no content change still moves only the name's
        // CURRENT binding: the old spelling stops resolving unpinned, the
        // recorded `old+vN` pin keeps serving, and the new name binds the
        // same versioned bytes.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        let body = "/* @module site.theme */\n.site { color: red; }";
        fs::write(scss.join("theme.scss"), body).unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "site.theme", 1, "css");

        fs::write(
            scss.join("theme.scss"),
            "/* @module site.main */\n.site { color: red; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.modules["site.main"], "css/theme.css");
        assert_eq!(
            manifest.asset_key_for_module("site.theme", "css"),
            None,
            "the renamed name is history, not a live alias"
        );
        assert_eq!(
            serve(&manifest, &graph, &old_pin),
            200,
            "the recorded pin still serves"
        );
    }

    #[test]
    fn test_repeated_rename_serves_every_recorded_pin() {
        // A -> B -> C: pins minted under A and B keep serving their own
        // recorded bytes; only C answers an unpinned lookup.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.a */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let pin_a = pin(&manifest, "css/theme.css", "site.a", 1, "css");
        let body_a = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        fs::write(
            scss.join("theme.scss"),
            "/* @module site.b */\n.site { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        let pin_b = pin(&manifest, "css/theme.css", "site.b", 2, "css");
        let body_b = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        fs::write(
            scss.join("theme.scss"),
            "/* @module site.c */\n.site { color: green; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();

        // Each retired name's pin serves the bytes it was issued against.
        for (p, expected) in [(pin_a, body_a), (pin_b, body_b)] {
            let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &p, None);
            assert_eq!((r.status, r.body.as_str()), (200, expected.as_str()));
        }
        for old in ["site.a", "site.b"] {
            assert_eq!(
                manifest.asset_key_for_module(old, "css"),
                None,
                "{old} is history — never a live alias"
            );
        }
        assert_eq!(
            manifest.asset_key_for_module("site.c", "css").as_deref(),
            Some("css/theme.css"),
            "only the current name resolves unpinned"
        );
    }

    #[test]
    fn test_rename_plus_dependency_change_keeps_old_pin() {
        // A rename that also rewrites the dependency contract still keeps
        // the old name's pin serving the old bytes — dependency metadata
        // is part of the version's identity, not a reason to orphan URLs.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(scss.join("base.scss"), ".base { margin: 0; }").unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.theme */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "site.theme", 1, "css");
        let old_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        fs::write(
            scss.join("theme.scss"),
            "/* @module site.main\n   @requires base */\n.site { color: red; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("site.theme", "css"),
            None,
            "renamed + re-dependenced: the old name is still retired"
        );
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &old_pin, None);
        assert_eq!((r.status, r.body.as_str()), (200, old_body.as_str()));
    }

    #[test]
    fn test_source_filename_swap_serves_each_recorded_identity() {
        // Swapping two files' contents swaps which key holds which bytes —
        // each name's recorded pin must keep serving the bytes THAT name
        // was issued against, never follow the content to the other key.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(scss.join("first.scss"), ".first { color: red; }").unwrap();
        fs::write(scss.join("second.scss"), ".second { color: blue; }").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let first_pin = pin(&manifest, "css/first.css", "first", 1, "css");
        let second_pin = pin(&manifest, "css/second.css", "second", 1, "css");
        let first_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/first.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();
        let second_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/second.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        // The swap: first.scss now carries second's source and vice versa.
        fs::write(scss.join("first.scss"), ".second { color: blue; }").unwrap();
        fs::write(scss.join("second.scss"), ".first { color: red; }").unwrap();
        let manifest = graph.build().unwrap();

        // Current names resolve to v2 with the swapped bytes…
        for (name, key) in [("first", "css/first.css"), ("second", "css/second.css")] {
            assert_eq!(
                manifest.asset_key_for_module(name, "css").as_deref(),
                Some(key)
            );
            assert_eq!(manifest.assets[key].version, 2);
        }
        // …and each v1 pin still serves the bytes it was issued with.
        for (p, expected) in [(first_pin, first_body), (second_pin, second_body)] {
            let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &p, None);
            assert_eq!((r.status, r.body.as_str()), (200, expected.as_str()));
        }
    }

    #[test]
    fn test_source_path_reuse_bumps_version_and_keeps_the_old_pin() {
        // Deleting a source and later recreating the same path with new
        // content reuses the asset key: the recreated file continues the
        // version line and every earlier pin keeps serving its own bytes.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(scss.join("theme.scss"), ".a { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "theme", 1, "css");
        let old_body = fs::read_to_string(
            dir.path().join("dist").join(
                manifest.assets["css/theme.css"]
                    .output
                    .trim_start_matches('/'),
            ),
        )
        .unwrap();

        fs::remove_file(scss.join("theme.scss")).unwrap();
        let manifest = graph.build().unwrap();
        assert!(manifest.assets["css/theme.css"].retired);

        fs::write(scss.join("theme.scss"), ".a { color: blue; }").unwrap();
        let manifest = graph.build().unwrap();
        assert!(
            !manifest.assets["css/theme.css"].retired,
            "the recreated source un-retires the entry"
        );
        assert_eq!(manifest.assets["css/theme.css"].version, 2);
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &old_pin, None);
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, old_body.as_str()),
            "the path's v1 pin still serves the deleted generation's bytes"
        );
    }

    #[test]
    fn test_name_pins_evict_with_their_retained_version() {
        // A name pin lives only as long as the version it names: bounded
        // retention that evicts the version evicts the pin, and the old
        // URL then 404s — retention, not resolution, owns that lifecycle.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(scss.join("theme.scss"), ".a { color: red; }").unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        config.storage.keep_versions = 0;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let old_pin = pin(&manifest, "css/theme.css", "theme", 1, "css");

        fs::write(scss.join("theme.scss"), ".a { color: blue; }").unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.assets["css/theme.css"].version, 2);
        assert!(
            manifest.assets["css/theme.css"].history.is_empty(),
            "keep-versions 0 retains nothing"
        );
        assert!(
            manifest.historical_names.is_empty()
                || manifest
                    .historical_names
                    .values()
                    .all(|pins| !pins.contains_key(&1)),
            "the evicted version's name pin is pruned"
        );
        assert_eq!(
            serve(&manifest, &graph, &old_pin),
            404,
            "an evicted version's pin serves nothing"
        );
    }

    #[test]
    fn test_malformed_declarations_fail_naming_file_line_and_text() {
        // A comment carrying `@module`/`@requires` but no usable argument
        // is an attempted declaration — it fails the build naming the
        // file, the line, the keyword, and the full comment text, never
        // quietly becoming "no declaration" under a path-derived name.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();

        for (source, kind, line, segment) in [
            ("// @module\nvar x = 1;", "@module", 1usize, "@module"),
            ("// @module \"\"\nvar x = 1;", "@module", 1, "@module \"\""),
            ("var x = 1;\n// @module\n", "@module", 2, "@module"),
            ("/* @module */\nvar x = 1;", "@module", 1, "@module"),
            ("// @requires\nvar x = 1;", "@requires", 1, "@requires"),
            (
                "// @requires \"\"\nvar x = 1;",
                "@requires",
                1,
                "@requires \"\"",
            ),
        ] {
            fs::write(js_dir.join("bad.js"), source).unwrap();
            let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
            let error = graph.build().unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("bad.js") && message.contains(kind),
                "the error names file and keyword: {message}"
            );
            assert!(
                message.contains(&format!(":{line}:")),
                "the error names the declaration's line ({line}): {message}"
            );
            assert!(
                message.contains(&format!("{segment:?}")),
                "the error quotes the complete offending annotation text: {message}"
            );
        }

        // Invalid trailing characters ride the token into validation —
        // each fails as an invalid name, not a truncated one.
        for token in [
            "mod$bad", "mod:bad", "mod@bad", "mod\\bad", "mod,bad", "mod!bad",
        ] {
            fs::write(
                js_dir.join("bad.js"),
                format!("// @module {token}\nvar x = 1;"),
            )
            .unwrap();
            let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
            let error = graph.build().unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("invalid @module name") && message.contains("bad.js"),
                "the whole offending token fails as an invalid name: {message}"
            );
        }

        // The same contract holds in stylesheets — a `/* @module */`
        // block with no name and a `// @requires` line with none both
        // fail rather than derive from the path.
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::remove_dir_all(&js_dir).unwrap();
        for source in [
            "/* @module */\n.x { color: red; }",
            "// @requires\n.x { color: red; }",
        ] {
            fs::write(scss_dir.join("bad.scss"), source).unwrap();
            let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/scss"], true));
            let error = graph.build().unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("bad.scss") && message.contains(":1:"),
                "style declarations fail with file and line too: {message}"
            );
        }
    }

    #[test]
    fn test_declarations_reject_trailing_material_and_partial_forms() {
        // The grammar is exact: `@module name` / `@requires name`, nothing
        // after the name but the line's end or the next annotation. Every
        // occurrence is validated — a malformed LATER one still fails even
        // when an earlier one was valid.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();

        for (source, segment) in [
            // Material after the argument is not dropped — it fails.
            ("// @module app extra\nvar x = 1;", "@module app extra"),
            (
                "// @module \"app\" trailing\nvar x = 1;",
                "@module \"app\" trailing",
            ),
            (
                "// @requires dep trailing\nvar x = 1;",
                "@requires dep trailing",
            ),
            // A quote opened must close; a stray quote is material.
            ("// @module \"app\nvar x = 1;", "@module \"app"),
            ("// @module app\"\nvar x = 1;", "@module app\""),
            // A second occurrence is parsed too — the argument-less tail
            // fails even though `@requires a` was valid.
            ("// @requires a @requires\nvar x = 1;", "@requires"),
            ("// @module a @module\nvar x = 1;", "@module"),
        ] {
            fs::write(js_dir.join("bad.js"), source).unwrap();
            let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
            let error = graph.build().unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains("bad.js") && message.contains(&format!("{segment:?}")),
                "the complete offending annotation is reported: {message}"
            );
        }

        // Two DIFFERENT @module names in one file conflict — the first
        // may not silently win.
        fs::write(js_dir.join("bad.js"), "// @module a @module b\nvar x = 1;").unwrap();
        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        let error = graph.build().unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("conflicting @module"),
            "differing @module declarations conflict: {message}"
        );

        // The same-name repeat is tolerated; a differing name in a LATER
        // comment is still a conflict.
        fs::write(
            js_dir.join("bad.js"),
            "// @module a\n// @module b\nvar x = 1;",
        )
        .unwrap();
        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        assert!(
            format!("{:#}", graph.build().unwrap_err()).contains("conflicting @module"),
            "a conflicting @module in a second comment fails"
        );
        fs::write(
            js_dir.join("ok.js"),
            "// @module a\n// @module a\nvar x = 1;",
        )
        .unwrap();
        fs::remove_file(js_dir.join("bad.js")).unwrap();
        let manifest = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true))
            .build()
            .unwrap();
        assert_eq!(
            manifest.asset_key_for_module("a", "js").as_deref(),
            Some("js/a.js"),
            "a repeated identical @module is not a conflict"
        );
    }

    #[test]
    fn test_requires_order_preserved_and_block_comment_lines_are_exact() {
        // Two valid @requires keep their order, and a malformed annotation
        // inside a block comment reports the annotation's own line — not
        // the comment's opening line.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();

        fs::write(
            js_dir.join("ordered.js"),
            "/*\n * @requires dep/one\n * @requires dep/two\n * @module ordered\n */\nvar x = 1;",
        )
        .unwrap();
        let manifest = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true))
            .build()
            .unwrap();
        let deps = &manifest.assets["js/ordered.js"].dependencies;
        assert_eq!(
            deps,
            &["dep.one".to_string(), "dep.two".to_string()],
            "@requires order is preserved"
        );

        fs::write(
            js_dir.join("ordered.js"),
            "/*\n * description\n * description\n * @module invalid!\n */\nvar x = 1;",
        )
        .unwrap();
        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        let message = format!("{:#}", graph.build().unwrap_err());
        assert!(
            message.contains(":4:"),
            "the error names the annotation's line, not the comment's: {message}"
        );
        assert!(
            message.contains("\"@module invalid!\""),
            "the offending segment is quoted: {message}"
        );
    }

    #[test]
    fn test_style_comment_scanner_does_not_panic_on_utf8_boundaries() {
        // `u` followed by a multibyte character: `i + 3` lands inside the
        // euro sign's bytes — a &str slice there panics. The scanner is
        // byte-oriented and must not slice at unchecked offsets.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        for source in [
            "u€ { color: red; }",
            "u日本語 { color: red; }",
            ".x { content: \"u€\"; }",
            // The url() forms still skip their unquoted argument — neither
            // `//` nor `/*` inside one can manufacture a comment.
            ".x { background: url(//cdn.example/x) }",
            ".x { background: URL(//cdn.example/x) }",
            ".x { background: Url(data:image/png;base64,/*) }",
            ".x { background: url(\"//cdn.example/x\") }",
        ] {
            fs::write(scss_dir.join("site.scss"), source).unwrap();
            let manifest = AssetGraph::new(dirs_config(dir.path(), &["src/scss"], true))
                .build()
                .unwrap();
            assert!(
                manifest.assets.contains_key("css/site.css"),
                "valid input must build without panic: {source:?}"
            );
        }
        // Unicode inside a real comment still scans normally.
        fs::write(
            scss_dir.join("site.scss"),
            "/* @module ünïcode */\n.x { color: red; }",
        )
        .unwrap();
        let manifest = AssetGraph::new(dirs_config(dir.path(), &["src/scss"], true))
            .build()
            .unwrap();
        assert_eq!(
            manifest.asset_key_for_module("ünïcode", "css").as_deref(),
            Some("css/site.css"),
            "a real Unicode declaration is still honored"
        );
    }

    #[test]
    fn test_hash_qualified_pin_verifies_against_the_recorded_identity() {
        // A `-h{hash}` qualifier on a renamed name's pin verifies against
        // the RECORDED identity's hash — a correct qualifier serves, a
        // qualifier for different bytes 404s, and the current claimant's
        // hash can never satisfy the old pin.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss = dir.path().join("src/scss");
        fs::create_dir_all(&scss).unwrap();
        fs::write(
            scss.join("theme.scss"),
            "/* @module site.theme */\n.site { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), false);
        config.jam.enabled = None;
        config.jam.hash_urls = true;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["css/theme.css"];
        let stamp = crate::manifest::stamp_epoch(&entry.timestamp).unwrap();
        let v1_hash = entry.hash.clone();

        fs::write(
            scss.join("theme.scss"),
            "/* @module site.main */\n.site { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();

        // The renamed name's recorded pin serves only under its own hash.
        let good = format!("/paste/{stamp}/site.theme+v1.0-h{v1_hash}.min.css");
        let wrong = format!("/paste/{stamp}/site.theme+v1.0-h{}.min.css", "0".repeat(32));
        assert_eq!(serve(&manifest, &graph, &good), 200);
        assert_eq!(
            serve(&manifest, &graph, &wrong),
            404,
            "a hash qualifier naming other bytes is a 404, never a serve"
        );
        // The claimant's own hash cannot satisfy the old pin either.
        let claimant_hash = &manifest.assets["css/theme.css"].hash;
        let claimant = format!("/paste/{stamp}/site.theme+v1.0-h{claimant_hash}.min.css");
        assert_eq!(
            serve(&manifest, &graph, &claimant),
            404,
            "the current version's hash does not vouch for the old pin"
        );
        // The legacy unqualified form canonicalizes onto the qualified
        // spelling — same identity, same bytes — never to other content.
        let old_pin = pin(&manifest, "css/theme.css", "site.theme", 1, "css");
        let r = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &old_pin, None);
        assert_eq!(r.status, 302);
        assert_eq!(
            r.location.as_deref(),
            Some(good.as_str()),
            "the legacy pin redirects only onto its own qualified spelling"
        );
    }

    #[test]
    fn test_declarations_inside_strings_do_not_register() {
        // A `@module` inside a string literal is string content — only a
        // declaration inside a comment may claim a module.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("app.js"),
            "var doc = \"@module phantom\";\nvar app = 1;",
        )
        .unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        let manifest = graph.build().unwrap();
        // No phantom module — the file's path-derived name stands.
        assert!(manifest.assets.contains_key("js/app.js"));
        assert!(
            !manifest.assets.contains_key("js/phantom.js"),
            "a string literal must never declare a module"
        );

        // The comment forms still declare as they always did.
        fs::write(
            js_dir.join("app.js"),
            "// @module app\nvar doc = \"@module phantom\";",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("js/app.js"));
        assert!(!manifest.assets.contains_key("js/phantom.js"));
    }

    #[test]
    fn test_declaration_tokens_are_validated_whole() {
        // The named token is checked end to end — `@module foo!` is not
        // a declaration of `foo` with a stray tail; it is a malformed
        // declaration and the build says so.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("bad.js"), "// @module foo!\nvar bad = 1;").unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        let error = graph.build().unwrap_err();
        assert!(
            format!("{error:#}").contains("foo!"),
            "the whole invalid token is reported: {error:#}"
        );

        // @requires gets the same check — `a,b` is not `a` plus noise.
        fs::write(
            js_dir.join("bad.js"),
            "// @module bad\n// @requires dep,other\nvar bad = 1;",
        )
        .unwrap();
        let error = graph.build().unwrap_err();
        assert!(
            format!("{error:#}").contains("dep,other"),
            "the whole invalid dependency token is reported: {error:#}"
        );
    }

    #[test]
    fn test_regex_and_template_comment_lookalikes_never_declare() {
        // A lexer that cannot tell a regex literal from a comment opener
        // invents declarations: `//` inside a character class, after a
        // division, or inside a template expression is code, not comment
        // text. Declarations come from the parser's own comment trivia —
        // only real comments may register a module.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(
            js_dir.join("re.js"),
            concat!(
                // A `//` pair inside a regex character class.
                "var cls = /[\\/\\/ @module phantom]/;\n",
                // A `//` pair inside a regex alternative.
                "var re = /a\\/\\/ @module phantom2/;\n",
                // A `//` pair inside template text (not an interpolation).
                "var tpl = `x // @module phantom3 y`;\n",
                // Division is arithmetic, never a comment opener.
                "var two = 8 / 2 / 1;\n",
            ),
        )
        .unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/js"], true));
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("js/re.js"));
        for phantom in ["phantom", "phantom2", "phantom3"] {
            assert!(
                manifest.asset_key_for_module(phantom, "js").is_none(),
                "{phantom} was never a comment — no module may register"
            );
            assert!(
                !manifest.assets.keys().any(|key| key.contains(phantom)),
                "{phantom} must not become an asset"
            );
        }
        // The real comment still declares when present.
        fs::write(
            js_dir.join("re.js"),
            concat!(
                "// @module real\n",
                "var cls = /[\\/\\/ @module phantom]/;\n",
            ),
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("real", "js").as_deref(),
            Some("js/real.js"),
            "a genuine comment still declares"
        );
        assert!(
            manifest.asset_key_for_module("phantom", "js").is_none(),
            "the regex body still declares nothing"
        );
    }

    #[test]
    fn test_style_url_and_string_comment_lookalikes_never_declare() {
        // `url(//cdn/x)` carries an unquoted protocol-relative URL — the
        // `//` is address data, and a `@module` inside it must not
        // register. Strings get the same treatment.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("icons.scss"),
            concat!(
                ".a { background: url(//cdn.example/x // @module phantom\n); }\n",
                ".b { content: '/* @module phantom2 */'; }\n",
            ),
        )
        .unwrap();

        let mut graph = AssetGraph::new(dirs_config(dir.path(), &["src/scss"], true));
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("css/icons.css"));
        for phantom in ["phantom", "phantom2"] {
            assert!(
                manifest.asset_key_for_module(phantom, "css").is_none(),
                "{phantom} was never a comment — no module may register"
            );
        }
        fs::write(
            scss_dir.join("icons.scss"),
            "/* @module icons */\n.a { background: url(//cdn.example/x); }\n",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("icons", "css").as_deref(),
            Some("css/icons.css")
        );
    }

    /// The stamp segment for a history pin — the retained version's own stamp.
    fn history_pin(
        manifest: &Manifest,
        key: &str,
        module: &str,
        history_index: usize,
        ext: &str,
    ) -> String {
        let history = &manifest.assets[key].history[history_index];
        let stamp = crate::manifest::stamp_epoch(&history.timestamp).unwrap();
        format!("/paste/{stamp}/{module}+v{}.0.min.{ext}", history.version)
    }

    #[test]
    fn test_dist_deleted_source_retires_but_serves_the_exact_pin() {
        // A distribution build keeps history on source deletion — the old
        // pinned URL still serves bytes while fresh lookups see only the
        // active set.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("app.js"), "// @module app\nvar app = 'A';").unwrap();
        fs::write(js_dir.join("keep.js"), "// @module keep\nvar keep = 'K';").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("app", "js").as_deref(),
            Some("js/app.js")
        );
        let pinned = pin(&manifest, "js/app.js", "app", 1, "js");
        assert_eq!(serve(&manifest, &graph, &pinned), 200);
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 200);

        fs::remove_file(js_dir.join("app.js")).unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(
            entry.retired,
            "a deleted source retires, it does not vanish"
        );
        assert!(
            manifest.asset_key_for_module("app", "js").is_none(),
            "the active-only lookup filters a retired name"
        );
        assert_eq!(
            manifest.modules.get("app").map(String::as_str),
            None,
            "the modules map holds current names only — the pin record lives in historical_names"
        );

        // Fresh requests fail; the exact pin still serves the same bytes.
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 404);
        let response = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &pinned, None);
        assert_eq!(response.status, 200);
        assert!(
            response.body.contains("A"),
            "the pinned URL serves the retired bytes: {}",
            response.body
        );

        // Render expansion refuses the retired module.
        let opts = crate::jam::RenderOptions {
            cdn_base: String::new(),
            base_path: "/paste".to_string(),
            inline_threshold: 1024,
            per_module: false,
        };
        assert!(
            crate::jam::render_all(
                &manifest,
                &graph.output_dirs,
                &opts,
                &[
                    crate::jam::Event::Jam(crate::jam::Jam {
                        js: vec!["app".to_string()],
                        ..Default::default()
                    }),
                    crate::jam::Event::Dump(crate::jam::DumpScope::Js),
                ],
            )
            .is_err(),
            "a retired module cannot enter a new page"
        );

        // The packages fingerprint omits retired entries.
        let mut unretired = manifest.clone();
        unretired.assets.get_mut("js/app.js").unwrap().retired = false;
        assert_ne!(
            crate::jam::packages_segment(&manifest),
            crate::jam::packages_segment(&unretired),
            "a retired entry must not change the loader fingerprint"
        );
    }

    #[test]
    fn test_dist_readded_source_continues_the_version_sequence() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "// @module app\nvar app = 'A';").unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let v1_pin = pin(&manifest, "js/app.js", "app", 1, "js");
        let v1_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body;

        fs::remove_file(&source).unwrap();
        let manifest = graph.build().unwrap();
        assert!(manifest.assets["js/app.js"].retired);

        // Re-added at the same key: the flag clears and the sequence
        // continues rather than restarting at v1.
        fs::write(&source, "// @module app\nvar app = 'B';").unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert!(!entry.retired);
        assert_eq!(entry.version, 2);
        assert_eq!(entry.history.len(), 1);
        assert_eq!(entry.history[0].version, 1);
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 200);
        // The original pin still resolves the original bytes.
        let response = crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, v1_body);
    }

    #[test]
    fn test_local_deleted_source_no_longer_resolves() {
        // Local has no history to protect — a removed source is gone
        // entirely, pin or no pin.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = dir.path().join("src/app.js");
        fs::write(&source, "// @module app\nvar app = 1;").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], true);
        config.storage.backend = "local".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let pinned = pin(&manifest, "js/app.js", "app", 1, "js");
        assert_eq!(serve(&manifest, &graph, &pinned), 200);

        fs::remove_file(&source).unwrap();
        let manifest = graph.build().unwrap();
        assert!(!manifest.assets.contains_key("js/app.js"));
        assert!(!manifest.modules.contains_key("app"));
        assert_eq!(serve(&manifest, &graph, "/paste/app.min.js"), 404);
        assert_eq!(serve(&manifest, &graph, &pinned), 404);
    }

    #[test]
    fn test_js_dependency_only_edit_versions_identical_output() {
        // A requires-line compiles to the same bytes, but the dependency
        // metadata is semantic: the version rolls and both pins resolve.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        fs::write(js_dir.join("a.js"), "// @module lib.a\nvar libA = 1;").unwrap();
        fs::write(js_dir.join("c.js"), "// @module lib.c\nvar libC = 1;").unwrap();
        let b_src = js_dir.join("b.js");
        fs::write(
            &b_src,
            "// @module lib.b\n// @requires lib.a\nvar libB = libA;",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        // JS asset keys follow the declared module name, not the file path.
        let entry = &manifest.assets["js/lib/b.js"];
        assert_eq!(entry.version, 1);
        assert_eq!(entry.dependencies, vec!["lib.a".to_string()]);
        let same_hash = entry.hash.clone();
        let v1_pin = pin(&manifest, "js/lib/b.js", "lib.b", 1, "js");

        // Swap the declared dependency — the minified bytes are unchanged.
        fs::write(
            &b_src,
            "// @module lib.b\n// @requires lib.c\nvar libB = libA;",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/lib/b.js"];
        assert_eq!(entry.hash, same_hash, "the compiled bytes are identical");
        assert_eq!(entry.version, 2, "a dependency change is a new version");
        assert_eq!(entry.dependencies, vec!["lib.c".to_string()]);
        assert_eq!(entry.history.len(), 1);
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        let v2_pin = pin(&manifest, "js/lib/b.js", "lib.b", 2, "js");
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);

        // A comment that compiles away entirely bumps nothing.
        fs::write(
            js_dir.join("a.js"),
            "// @module lib.a\n// a discarded comment\nvar libA = 1;",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.assets["js/lib/a.js"].version, 1,
            "an edit that compiles to the same bytes keeps the version"
        );
    }

    #[test]
    fn test_css_deps_expand_like_js_and_survive_renames() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let scss_dir = dir.path().join("src/scss");
        fs::create_dir_all(&scss_dir).unwrap();
        fs::write(
            scss_dir.join("c.scss"),
            "// @module chain.c\n.c { color: green; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("b.scss"),
            "// @module chain.b\n// @requires chain.c\n.b { color: blue; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("a.scss"),
            "// @module chain.a\n// @requires chain.b\n.a { color: red; }",
        )
        .unwrap();

        let mut config = dist_config(dir.path(), true);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        // Stored dependencies are the transitive backfilled set, as for JS.
        assert_eq!(
            manifest.assets["css/a.css"].dependencies,
            vec!["chain.b".to_string(), "chain.c".to_string()],
            "expanded transitive deps are stored"
        );
        assert_eq!(manifest.assets["css/a.css"].version, 1);

        // A dependency-metadata change on b rolls b — and a, whose expanded
        // list now includes chain.d — while c keeps its version.
        fs::write(
            scss_dir.join("d.scss"),
            "// @module chain.d\n.d { color: black; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("b.scss"),
            "// @module chain.b\n// @requires chain.c\n// @requires chain.d\n.b { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(manifest.assets["css/b.css"].version, 2);
        assert_eq!(
            manifest.assets["css/a.css"].version, 2,
            "the dependent's expanded dep list changed too"
        );
        assert_eq!(manifest.assets["css/a.css"].history.len(), 1);
        assert_eq!(
            manifest.assets["css/a.css"].dependencies,
            vec![
                "chain.b".to_string(),
                "chain.c".to_string(),
                "chain.d".to_string()
            ]
        );
        assert_eq!(manifest.assets["css/c.css"].version, 1);
        assert_eq!(manifest.assets["css/d.css"].version, 1);
        let a_v1_pin = history_pin(&manifest, "css/a.css", "chain.a", 0, "css");
        let a_stamp_v2 = pin(&manifest, "css/a.css", "chain.a", 2, "css");

        // A declared module moved to another source path carries its entry —
        // version, history, and every retained pin included.
        fs::create_dir_all(scss_dir.join("nested")).unwrap();
        fs::write(
            scss_dir.join("nested/moved.scss"),
            "// @module chain.a\n// @requires chain.b\n.a { color: red; }",
        )
        .unwrap();
        fs::remove_file(scss_dir.join("a.scss")).unwrap();
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["css/nested/moved.css"];
        // The vacated key is not deleted: it stays as a retired shell whose
        // recorded outputs keep the direct URLs a saved page may hold.
        assert!(
            manifest.assets["css/a.css"].retired,
            "the vacated key survives as a retired entry"
        );
        // The moved asset's public path changes — `/css/a.*` to
        // `/css/nested/moved.*` — so it issues a new version at the new
        // path rather than rewriting v2's recorded location.
        assert_eq!(
            entry.version, 3,
            "a path-moving rename issues a new version"
        );
        assert_eq!(entry.history.len(), 2);
        assert_eq!(
            manifest.modules.get("chain.a").map(String::as_str),
            Some("css/nested/moved.css")
        );
        assert_eq!(serve(&manifest, &graph, &a_v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &a_stamp_v2), 200);

        // Duplicate declarations keep the first file in scan order; the
        // later one is not compiled or recorded — a path-keyed duplicate
        // asset would still serve under its own key, so the first
        // declaration would not hold.
        fs::write(
            scss_dir.join("dup_a.scss"),
            "// @module theme.dup\n.dupa { color: red; }",
        )
        .unwrap();
        fs::write(
            scss_dir.join("dup_b.scss"),
            "// @module theme.dup\n.dupb { color: blue; }",
        )
        .unwrap();
        let manifest = graph.build().unwrap();
        assert_eq!(
            manifest.asset_key_for_module("theme.dup", "css").as_deref(),
            Some("css/dup_a.css"),
            "the first declaration in scan order wins"
        );
        assert!(
            manifest
                .assets
                .get("css/dup_b.css")
                .is_none_or(|entry| entry.retired),
            "a duplicate declaration never becomes an active asset"
        );
        let fresh = crate::jam::combo(
            &manifest,
            &graph.output_dirs,
            "/paste",
            "/paste/theme.dup.min.css",
            None,
        );
        assert_eq!(fresh.status, 200);
        assert!(
            fresh.body.contains(".dupa"),
            "the unpinned combo serves the first"
        );
        assert!(
            !fresh.body.contains(".dupb"),
            "the unpinned combo never serves the duplicate: {}",
            fresh.body
        );
    }

    #[test]
    fn test_aliased_output_dir_is_never_rescanned() {
        // `other/../assets/dist` is the same directory as `assets/dist`:
        // loading normalizes the alias onto the absolute form the walk
        // compares against, so generated output nested under a source root
        // is pruned rather than discovered as source on the next build.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let src = dir.path().join("assets");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("app.js"), "// @module app\nvar app = 1;").unwrap();

        fs::write(
            dir.path().join("paste.conf"),
            r#"source.dirs = ["assets"]
output.base = "other/../assets/dist"
manifest = "manifest.json"
storage.backend = "dist"
"#,
        )
        .unwrap();
        let config = Config::load_with_base_dir(Path::new("paste.conf"), dir.path()).unwrap();
        assert_eq!(
            config.output.base,
            dir.path().join("assets/dist"),
            "the alias must normalize to the real output directory"
        );

        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        assert!(manifest.assets.contains_key("js/app.js"));

        // A generated file inside the output tree — which sits under a
        // source root here — must never be discovered as a source, and a
        // stale all-modules aggregate left by an older build is removed.
        let stale = dir.path().join("assets/dist/js/assetgraph.js");
        fs::write(&stale, "var stale = 1;").unwrap();
        let manifest = graph.build().unwrap();
        assert!(
            !stale.exists(),
            "a stale aggregate under the output root is removed"
        );
        assert!(
            !manifest.assets.keys().any(|key| key.contains("assetgraph")),
            "generated files must never re-enter discovery"
        );
        assert_eq!(
            manifest.assets["js/app.js"].version, 1,
            "the second build is a no-op"
        );
    }

    #[test]
    fn test_layout_migration_preserves_every_retained_pin() {
        // flat -> beside-source -> renamed build dir -> flat: every retained
        // version is located by manifest-owned metadata, verified against its
        // recorded hash, and materialized where the new layout's resolver
        // maps its recorded output. The old pins keep serving throughout —
        // and every path-moving build issues a new version, never a rewrite.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = dir.path().join("src/app.js");
        fs::write(&source, "// @module app\nvar app = 'A';").unwrap();

        let mut config = dirs_config(dir.path(), &["src"], false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let v1_pin = pin(&manifest, "js/app.js", "app", 1, "js");
        let v1_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body;
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);

        fs::write(&source, "// @module app\nvar app = 'B';").unwrap();
        let manifest = graph.build().unwrap();
        let v2_pin = pin(&manifest, "js/app.js", "app", 2, "js");
        let v2_body =
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v2_pin, None).body;
        assert_eq!(manifest.assets["js/app.js"].history.len(), 1);
        let retained_stamp = manifest.assets["js/app.js"].history[0].timestamp.clone();
        let retained_hash = manifest.assets["js/app.js"].history[0].hash.clone();
        let retained_output = manifest.assets["js/app.js"].history[0].output.clone();

        // -> beside-source: the moved output is a NEW version; v1 and v2
        // stay in history at their recorded paths.
        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(
            entry.version, 3,
            "a layout move issues a new version at the new path"
        );
        assert_eq!(entry.history.len(), 2, "retained history survives");
        assert!(entry.output.starts_with("/src/_paste_build/app."));
        assert_eq!(
            entry.history[0].version, 1,
            "the retained version number survives"
        );
        assert_eq!(
            entry.history[0].timestamp, retained_stamp,
            "the retained stamp survives"
        );
        assert_eq!(
            entry.history[0].hash, retained_hash,
            "the retained hash survives"
        );
        // A recorded history output is the path a saved pin resolves to —
        // it is never rewritten. The new layout's resolver maps it onto
        // the new root, and the migration materializes the same verified
        // bytes at that resolved location.
        assert_eq!(
            entry.history[0].output, retained_output,
            "the recorded history output is never rewritten"
        );
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let resolved = graph
                .output_dirs
                .compiled_file(output, AssetType::Js)
                .expect("a recorded output must resolve");
            assert!(
                resolved.exists(),
                "the retained version exists at its resolved location: {output} -> {}",
                resolved.display()
            );
        }
        // The old flat versioned files are pruned.
        let flat_js = dir.path().join("dist/js");
        let leftovers: Vec<_> = fs::read_dir(&flat_js)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.iter().all(|name| !name.starts_with("app.")),
            "the old flat app outputs are pruned: {leftovers:?}"
        );
        assert_eq!(
            serve(&manifest, &graph, &v1_pin),
            200,
            "the v1 pin survives a layout move"
        );
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
        assert_eq!(
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v1_pin, None).body,
            v1_body,
            "the v1 pin still serves the original bytes"
        );

        // -> renamed build dir: another moved output, another new version.
        let mut config = beside_source_config(dir.path(), &["src"], false);
        config.jam.enabled = None;
        config.output.build_dir_name = "gen".into();
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(entry.version, 4);
        assert!(entry.output.starts_with("/src/gen/app."));
        // `_paste_build` keeps exactly the retained outputs recorded
        // under it — v3's public path lives there permanently; anything
        // else there is stale.
        let old_build = dir.path().join("src/_paste_build");
        if old_build.exists() {
            for leftover in fs::read_dir(&old_build).unwrap().flatten() {
                let name = leftover.file_name().to_string_lossy().into_owned();
                assert!(
                    entry.history.iter().any(|h| h.output.ends_with(&name)),
                    "{name} is not a retained recorded output — stale file"
                );
            }
        }
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let resolved = graph
                .output_dirs
                .compiled_file(output, AssetType::Js)
                .expect("a recorded output must resolve");
            assert!(
                resolved.exists(),
                "the retained version exists at its resolved location: {output} -> {}",
                resolved.display()
            );
        }
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
        assert_eq!(
            crate::jam::combo(&manifest, &graph.output_dirs, "/paste", &v2_pin, None).body,
            v2_body
        );

        // -> flat again
        let mut config = dirs_config(dir.path(), &["src"], false);
        config.jam.enabled = None;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        let entry = &manifest.assets["js/app.js"];
        assert_eq!(
            manifest.assets.len(),
            1,
            "generated files never re-enter discovery"
        );
        assert_eq!(entry.version, 5);
        assert!(entry.output.starts_with("/js/app."));
        for output in std::iter::once(&entry.output).chain(entry.history.iter().map(|h| &h.output))
        {
            let resolved = graph
                .output_dirs
                .compiled_file(output, AssetType::Js)
                .expect("a recorded output must resolve");
            assert!(
                resolved.exists(),
                "the retained version exists at its resolved location: {output} -> {}",
                resolved.display()
            );
        }
        assert_eq!(serve(&manifest, &graph, &v1_pin), 200);
        assert_eq!(serve(&manifest, &graph, &v2_pin), 200);
    }

    #[test]
    fn test_tampered_or_missing_retained_output_fails_the_build() {
        // A retained file that no longer matches its recorded hash is never
        // silently replaced by current bytes — the build stops and names it.
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "var app = 1;").unwrap();

        let mut graph = AssetGraph::new(dist_config(dir.path(), false));
        graph.build().unwrap();
        fs::write(&source, "var app = 2;").unwrap();
        let manifest = graph.build().unwrap();
        let retained = &manifest.assets["js/app.js"].history[0];
        let retained_file = dir
            .path()
            .join("dist")
            .join(retained.output.trim_start_matches('/'));
        assert!(retained_file.exists());

        fs::write(&retained_file, "tampered").unwrap();
        let err = graph.build().unwrap_err();
        assert!(
            err.to_string().contains("corrupted retained output"),
            "tampering must be detected: {err}"
        );

        fs::remove_file(&retained_file).unwrap();
        let err = graph.build().unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("missing") && message.contains("js/app.js"),
            "a missing retained file must be named: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_source_fails_the_build() {
        // A discovery or read error propagates — a partial manifest is never
        // published in place of a failed scan.
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("app.js");
        fs::write(&source, "var app = 1;").unwrap();

        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&source).is_ok() {
            // Permission bits do not stop a superuser or a sandbox that
            // bypasses them, so this environment cannot express "unreadable".
            fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let result = AssetGraph::new(dist_config(dir.path(), false)).build();
        // Restore before asserting so cleanup cannot fail on some platforms.
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(result.is_err(), "an unreadable source must fail the build");
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_source_subdir_fails_discovery() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        let blocked = js_dir.join("private");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("hidden.js"), "var h = 1;").unwrap();
        fs::write(js_dir.join("app.js"), "var app = 1;").unwrap();

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // Same probe as above: skip when the runner can read through
            // permission bits (root containers, some sandboxes).
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let result = AssetGraph::new(dist_config(dir.path(), false)).build();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "a walk error must fail the build");
    }

    // ============================================================
    // jam.hash-urls policy in real builds
    // ============================================================

    /// One single-module project built at a forced source mtime.
    fn build_at_stamp(
        body: &str,
        requires: &str,
        hash_urls: bool,
    ) -> (tempfile::TempDir, Manifest, AssetGraph) {
        let dir = tempfile::tempdir().unwrap();
        let js_dir = dir.path().join("src/js");
        fs::create_dir_all(&js_dir).unwrap();
        let source = js_dir.join("a.js");
        fs::write(&source, format!("// @module a\n{requires}{body}")).unwrap();
        // Both builds see the same source mtime — the stamp cannot be what
        // distinguishes their URLs.
        filetime::set_file_mtime(
            &source,
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .unwrap();
        let mut config = dist_config(dir.path(), true);
        config.jam.hash_urls = hash_urls;
        let mut graph = AssetGraph::new(config);
        let manifest = graph.build().unwrap();
        (dir, manifest, graph)
    }

    #[test]
    fn test_hash_urls_distinguish_same_stamp_same_version_builds() {
        // Two independent builds — identical source mtimes, different
        // compiled bytes, no prior manifest — mint the same version and the
        // same stamp, but different qualified URLs: the hash, not the stamp,
        // names the content.
        let (_dir_a, manifest_a, _graph_a) = build_at_stamp("var a = 1;", "", true);
        let (_dir_b, manifest_b, graph_b) = build_at_stamp("var a = 2;", "", true);
        let entry_a = &manifest_a.assets["js/a.js"];
        let entry_b = &manifest_b.assets["js/a.js"];

        assert!(manifest_a.hash_urls);
        assert_eq!(entry_a.version, 1);
        assert_eq!(entry_b.version, 1);
        assert_eq!(entry_a.timestamp, entry_b.timestamp);
        assert_ne!(entry_a.hash, entry_b.hash);

        let url_a = manifest_a.jam_url(&["a"], "js", "/paste", false).unwrap();
        let url_b = manifest_b.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert_ne!(url_a, url_b);
        assert!(url_a.contains(&format!("-h{}", entry_a.hash)), "{url_a}");
        assert!(url_b.contains(&format!("-h{}", entry_b.hash)), "{url_b}");

        // Build A's qualified URL asked of build B names bytes B never
        // wrote — a 404, never a redirect or different content.
        let r = crate::jam::combo(&manifest_b, &graph_b.output_dirs, "/paste", &url_a, None);
        assert_eq!(r.status, 404, "{url_a}");
        assert_eq!(r.cache_control, "no-cache");
    }

    #[test]
    fn test_rebuild_unchanged_content_keeps_the_qualified_url() {
        let (_dir, manifest, mut graph) = build_at_stamp("var a = 1;", "", true);
        let url = manifest.jam_url(&["a"], "js", "/paste", false).unwrap();

        let manifest2 = graph.build().unwrap();
        let url2 = manifest2.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert_eq!(url, url2, "an unchanged rebuild must mint the same URL");
        assert_eq!(manifest2.assets["js/a.js"].version, 1);

        // Flipping the real build config — not just the in-memory field —
        // rewrites only the policy: every asset record (version, hash,
        // stamp, history) is carried forward unchanged while the persisted
        // manifest and its emitted URLs switch to the legacy shape.
        graph.config.jam.hash_urls = false;
        let legacy = graph.build().unwrap();
        assert!(!legacy.hash_urls);
        assert_eq!(legacy.assets, manifest2.assets);
        let legacy_url = legacy.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert!(legacy_url.ends_with("/a+v1.0.min.js"), "{legacy_url}");
        assert!(!legacy_url.contains("-h"), "{legacy_url}");

        graph.config.jam.hash_urls = true;
        let reenabled = graph.build().unwrap();
        assert!(reenabled.hash_urls);
        assert_eq!(reenabled.assets, manifest2.assets);
        assert_eq!(
            reenabled.jam_url(&["a"], "js", "/paste", false).unwrap(),
            url,
            "re-enabling must restore the original qualified URL"
        );
    }

    #[test]
    fn test_hash_url_optout_build_emits_legacy_urls() {
        let (_dir, manifest, _graph) = build_at_stamp("var a = 1;", "", false);
        assert!(!manifest.hash_urls);
        let url = manifest.jam_url(&["a"], "js", "/paste", false).unwrap();
        assert!(url.ends_with("/a+v1.0.min.js"), "{url}");
        assert!(!url.contains("-h"), "{url}");
    }

    /// Reference-parity checks: the original implementation's observable
    /// build and serve semantics, asserted end to end against real builds
    /// and the URLs they mint. The historical rules being proven:
    /// `Module.coalesce`/`bump_version` (version bumps only on a changed
    /// compiled checksum, records the old version into history, keeps
    /// its file), `Primer.set_primed_content` (a same-source rebuild
    /// inherits everything), `Manifest.build`/`_clean_unprimed_modules`
    /// (a vanished source retires its record but the manifest keeps it),
    /// and `Jammer` resolution (a `+vN` pin serves that version's own
    /// recorded bytes, and the group stamp is the max of the resolved
    /// versions' recorded stamps).
    mod historical_parity {
        use super::*;

        fn build(dir: &Path) -> (AssetGraph, Manifest) {
            let mut graph = AssetGraph::new(dist_config(dir, true));
            let manifest = graph.build().unwrap();
            (graph, manifest)
        }

        fn write_js(dir: &Path, name: &str, body: &str) {
            fs::write(dir.join("src/js").join(name), body).unwrap();
        }

        fn serve(manifest: &Manifest, graph: &AssetGraph, url: &str) -> crate::jam::ComboResponse {
            // A noncanonical URL redirects; follow it so every assertion
            // reads the 200 body's bytes.
            let r = crate::jam::combo(manifest, &graph.output_dirs, "/paste", url, None);
            match r.status {
                302 => crate::jam::combo(
                    manifest,
                    &graph.output_dirs,
                    "/paste",
                    r.location.as_deref().unwrap(),
                    None,
                ),
                _ => r,
            }
        }

        #[test]
        fn an_unchanged_rebuild_keeps_every_record() {
            // `can_prime`/`coalesce`: a same-source rebuild inherits
            // version, history, and stamp — the manifest record does not
            // move and the issued file is not rewritten.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            fs::create_dir_all(dir.path().join("src/scss")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            let (mut graph, m1) = build(dir.path());
            let out1 = m1.assets["js/a.js"].output.clone();
            let bytes1 =
                fs::read(dir.path().join("dist").join(out1.trim_start_matches('/'))).unwrap();

            let m2 = graph.build().unwrap();
            assert_eq!(
                m2.assets, m1.assets,
                "an unchanged rebuild rewrites no record"
            );
            assert_eq!(m2.sorted_deps, m1.sorted_deps);
            assert_eq!(m2.modules, m1.modules);
            let bytes2 =
                fs::read(dir.path().join("dist").join(out1.trim_start_matches('/'))).unwrap();
            assert_eq!(bytes1, bytes2);
            assert!(m2.assets["js/a.js"].history.is_empty());
            assert_eq!(m2.assets["js/a.js"].version, 1);
        }

        #[test]
        fn a_content_edit_bumps_the_version_and_retains_the_old_file() {
            // `bump_version`: a changed compiled checksum bumps the
            // version, pushes the old record into history, and leaves the
            // old file servable — a `+vN` pin names its own bytes, never
            // the newer version's.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            let (mut graph, m1) = build(dir.path());
            let e1 = m1.assets["js/a.js"].clone();
            let bytes_v1 = fs::read(
                dir.path()
                    .join("dist")
                    .join(e1.output.trim_start_matches('/')),
            )
            .unwrap();

            write_js(dir.path(), "a.js", "// @module a\nvar a = 2;");
            let m2 = graph.build().unwrap();
            let e2 = &m2.assets["js/a.js"];
            assert_eq!(e2.version, e1.version + 1);
            assert_ne!(e2.hash, e1.hash);
            assert_ne!(e2.output, e1.output);
            assert_eq!(e2.history.len(), 1);
            assert_eq!(e2.history[0].version, e1.version);
            assert_eq!(e2.history[0].hash, e1.hash);
            assert_eq!(e2.history[0].output, e1.output);
            assert_eq!(e2.history[0].timestamp, e1.timestamp);

            // The v1 file stays on disk and its pin serves v1's bytes.
            assert!(dir
                .path()
                .join("dist")
                .join(e1.output.trim_start_matches('/'))
                .exists());
            let r = serve(&m2, &graph, &format!("/paste/0/a+v{}.0.min.js", e1.version));
            assert_eq!(r.status, 200);
            assert_eq!(r.body.as_bytes(), bytes_v1);
        }

        #[test]
        fn a_dep_only_edit_versions_the_asset() {
            // The original bumped on the primed checksum alone and kept
            // the version when a source edit left the compiled bytes
            // unchanged — each serialized version carried its own
            // dependency list, so a `+vN` pin resolved the edges recorded
            // with it. This manifest keeps dependencies on the entry, so
            // a dep-only edit takes a NEW version at the same content
            // hash: the group a module expands into is part of what its
            // version means, and an unbumped edge would alias a saved
            // pin onto different bytes.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "paste.js", "// @module paste\nvar paste = {};");
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            write_js(dir.path(), "b.js", "// @module b\nvar b = 1;");
            let (mut graph, m1) = build(dir.path());
            let e1 = m1.assets["js/a.js"].clone();
            assert!(e1.dependencies.is_empty());

            // The @requires line is stripped from the emitted bytes —
            // the only change is the edge.
            write_js(
                dir.path(),
                "a.js",
                "// @module a\n// @requires b\nvar a = 1;",
            );
            let m2 = graph.build().unwrap();
            let e2 = &m2.assets["js/a.js"];
            assert_eq!(
                e2.version,
                e1.version + 1,
                "an edge change is a version change"
            );
            assert_eq!(e2.hash, e1.hash, "the emitted bytes did not change");
            assert_ne!(e2.output, e1.output, "the version component did");
            assert_eq!(e2.history.len(), 1);
            assert_eq!(e2.history[0].version, e1.version);
            assert_eq!(e2.dependencies, vec!["b".to_string()]);

            // The edge lands in the sorted layering: b is emitted ahead
            // of a now, and a's rendered group pulls it in.
            let js_deps = &m2.sorted_deps["js"];
            let pos_b = js_deps.iter().position(|(n, _, _)| n == "b").unwrap();
            let pos_a = js_deps.iter().position(|(n, _, _)| n == "a").unwrap();
            assert!(pos_b < pos_a, "{js_deps:?}");

            // And the edge is live at render: a jam of "a" expands to
            // {b, a} in layering order.
            let rendered = crate::jam::render_all(
                &m2,
                &graph.output_dirs,
                &crate::jam::RenderOptions {
                    cdn_base: String::new(),
                    base_path: "/paste".into(),
                    inline_threshold: 0,
                    per_module: false,
                },
                &[
                    crate::jam::Event::Jam(crate::jam::Jam {
                        js: vec!["a".into()],
                        ..Default::default()
                    }),
                    crate::jam::Event::Dump(crate::jam::DumpScope::All),
                    crate::jam::Event::Finish,
                ],
            )
            .unwrap();
            let url = &rendered[0].urls[0];
            assert!(url.contains("b+v") && url.contains("a+v"), "{url}");
        }

        #[test]
        fn a_deleted_source_retires_but_its_pin_still_serves() {
            // `Manifest._clean_unprimed_modules` + `Module.remove`:
            // versioning keeps the record and the file so historical pins
            // keep resolving. One deliberate difference from the
            // original, which left a removed module answering bare-name
            // requests too: a deleted module here retires from unpinned
            // resolution entirely — `version_removed` means the module is
            // gone for new pages, while its retained versions still
            // serve their pins.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            write_js(dir.path(), "b.js", "// @module b\nvar b = 1;");
            let (mut graph, m1) = build(dir.path());
            let e1 = m1.assets["js/a.js"].clone();
            let bytes_v1 = fs::read(
                dir.path()
                    .join("dist")
                    .join(e1.output.trim_start_matches('/')),
            )
            .unwrap();

            fs::remove_file(dir.path().join("src/js/a.js")).unwrap();
            let m2 = graph.build().unwrap();
            let e2 = &m2.assets["js/a.js"];
            assert!(e2.retired, "a vanished source retires its record");
            assert_eq!(e2.version, e1.version);
            assert!(dir
                .path()
                .join("dist")
                .join(e1.output.trim_start_matches('/'))
                .exists());

            // The retained version answers its pin with its own bytes…
            let r = serve(&m2, &graph, &format!("/paste/0/a+v{}.0.min.js", e1.version));
            assert_eq!(r.status, 200);
            assert_eq!(r.body.as_bytes(), bytes_v1);
            // …while a bare name no longer resolves it, and the name is
            // absent from freshly minted groups.
            let r = crate::jam::combo(&m2, &graph.output_dirs, "/paste", "/paste/0/a.min.js", None);
            assert_eq!(r.status, 404);
            let url = m2.jam_url(&["b"], "js", "/paste", false).unwrap();
            assert!(!url.contains("a+v"), "{url}");
        }

        #[test]
        fn a_restored_source_revives_its_record() {
            // `Primer.set_primed_content`: a reappearing source whose
            // bytes match the recorded checksum inherits the record
            // whole. The original had a wart here — `coalesce` copied
            // `version_removed` onto the revived module, so an identical
            // restore stayed marked removed — where this build clears
            // retirement on any matching revival and bumps a changed
            // restore the same as any other edit.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            let (mut graph, m1) = build(dir.path());
            let e1 = m1.assets["js/a.js"].clone();

            fs::remove_file(dir.path().join("src/js/a.js")).unwrap();
            let m2 = graph.build().unwrap();
            assert!(m2.assets["js/a.js"].retired);

            // Same bytes back: active again at the same version — the
            // pin space never moved.
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            let m3 = graph.build().unwrap();
            let e3 = &m3.assets["js/a.js"];
            assert!(!e3.retired);
            assert_eq!(e3.version, e1.version);
            assert_eq!(e3.output, e1.output);

            // Changed bytes back: an ordinary bump — also active.
            fs::remove_file(dir.path().join("src/js/a.js")).unwrap();
            let m4 = graph.build().unwrap();
            assert!(m4.assets["js/a.js"].retired);
            write_js(dir.path(), "a.js", "// @module a\nvar a = 3;");
            let m5 = graph.build().unwrap();
            let e5 = &m5.assets["js/a.js"];
            assert!(!e5.retired);
            assert_eq!(e5.version, e1.version + 1);
        }

        #[test]
        fn pruning_drops_the_file_and_its_pin_404s() {
            // Retention bound zero: a bumped version retires the old file
            // outright, and the dead pin fails closed — never a redirect
            // onto the newer bytes it no longer names.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            let mut config = dist_config(dir.path(), true);
            config.storage.keep_versions = 0;
            let mut graph = AssetGraph::new(config);
            let m1 = graph.build().unwrap();
            let e1 = m1.assets["js/a.js"].clone();

            write_js(dir.path(), "a.js", "// @module a\nvar a = 2;");
            let m2 = graph.build().unwrap();
            let e2 = &m2.assets["js/a.js"];
            assert_eq!(e2.version, 2);
            assert!(e2.history.is_empty());
            assert!(!dir
                .path()
                .join("dist")
                .join(e1.output.trim_start_matches('/'))
                .exists());

            let r = crate::jam::combo(
                &m2,
                &graph.output_dirs,
                "/paste",
                &format!("/paste/0/a+v{}.0.min.js", e1.version),
                None,
            );
            assert_eq!(r.status, 404);
            assert_eq!(r.cache_control, "no-cache");
        }

        #[test]
        fn mixed_old_and_new_pins_serve_each_recorded_version() {
            // `Jammer._initialize`: every `+vN` pin resolves that
            // version's own serialized record — a group can hold a
            // current version and a historical one side by side, and the
            // group stamp is the max of the resolved versions' own
            // stamps (Jammer.last_modified).
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            write_js(dir.path(), "b.js", "// @module b\nvar b = 1;");
            let (mut graph, m1) = build(dir.path());
            let a_v1 = m1.assets["js/a.js"].clone();
            let b_v1 = m1.assets["js/b.js"].clone();
            let bytes = |output: &str| {
                fs::read(dir.path().join("dist").join(output.trim_start_matches('/'))).unwrap()
            };
            let a_bytes = bytes(&a_v1.output);
            let b_v1_bytes = bytes(&b_v1.output);

            write_js(dir.path(), "b.js", "// @module b\nvar b = 2;");
            let m2 = graph.build().unwrap();
            assert_eq!(m2.assets["js/b.js"].version, 2);

            // a at current + b pinned at its retained v1: the response
            // is exactly those two recorded files, in dependency order.
            let r = serve(
                &m2,
                &graph,
                &format!(
                    "/paste/0/a+v{}.0~b+v{}.0.min.js",
                    a_v1.version, b_v1.version
                ),
            );
            assert_eq!(r.status, 200);
            // The group's body is the resolved files concatenated with
            // the JS join separator — one group, individual file reads.
            let mut expected = a_bytes.clone();
            expected.extend_from_slice(b"\n;\n");
            expected.extend_from_slice(&b_v1_bytes);
            assert_eq!(
                r.body.as_bytes(),
                expected,
                "a pin must serve its own bytes"
            );

            // The group stamp is the resolved versions' max — b's
            // historical pin carries its own stamp, not v2's.
            let pin_stamp = crate::manifest::stamp_epoch(&b_v1.timestamp).unwrap();
            let a_stamp = crate::manifest::stamp_epoch(&a_v1.timestamp).unwrap();
            let want_stamp = pin_stamp.max(a_stamp);
            let url = format!(
                "/paste/{want_stamp}/a+v{}.0~b+v{}.0.min.js",
                a_v1.version, b_v1.version
            );
            let r = crate::jam::combo(&m2, &graph.output_dirs, "/paste", &url, None);
            assert_eq!(r.status, 200, "the resolved max stamp is canonical: {url}");
        }

        #[test]
        fn legacy_comma_and_major_pins_resolve_to_canonical() {
            // The original spelled group URLs `a+v1.0,b+v2.0.min.js` —
            // comma-separated, `+v` with a float version. Both forms
            // still parse: a comma request redirects onto the canonical
            // `~` spelling rather than 404ing a saved URL.
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("src/js")).unwrap();
            write_js(dir.path(), "a.js", "// @module a\nvar a = 1;");
            write_js(dir.path(), "b.js", "// @module b\nvar b = 1;");
            let (graph, m1) = build(dir.path());

            let r = crate::jam::combo(
                &m1,
                &graph.output_dirs,
                "/paste",
                "/paste/0/a+v1.0,b+v1.0.min.js",
                None,
            );
            assert_eq!(r.status, 302);
            let location = r.location.unwrap();
            assert!(location.contains('~'), "{location}");
            let r = crate::jam::combo(&m1, &graph.output_dirs, "/paste", &location, None);
            assert_eq!(r.status, 200);

            // A bare `+v1` major pin names version 1 as `+v1.0` did.
            let r = serve(&m1, &graph, "/paste/0/a+v1.min.js");
            assert_eq!(r.status, 200);
        }
    }
}
