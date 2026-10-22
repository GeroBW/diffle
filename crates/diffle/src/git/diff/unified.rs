//! Patches in `git diff`'s format: its headers, path quoting, hunk grouping and the `@@` function
//! context line, so what the viewer parses and shows is what git would print.
use gix::ObjectId;
use gix::diff::blob::{Algorithm, Diff, InternedInput};

use super::rename::MAX_SCORE;
use super::sides::{GITLINK, kind};

/// Git's `FIRST_FEW_BYTES`: a NUL this early makes a file binary.
const SNIFF: usize = 8000;
/// Git keeps this much of a hunk's function line.
const FUNC_LINE: usize = 80;

pub fn is_binary(buf: &[u8]) -> bool {
    buf[..buf.len().min(SNIFF)].contains(&0)
}

/// The line diff git shows: Myers with its slider and indent heuristics.
fn line_diff<'a>(old: &'a [u8], new: &'a [u8]) -> (InternedInput<&'a [u8]>, Diff) {
    let input = InternedInput::new(old, new);
    let diff = gix::diff::blob::diff_with_slider_heuristics(Algorithm::Myers, &input);
    (input, diff)
}

/// Lines added and removed, as `--numstat` counts them.
pub fn line_counts(old: &[u8], new: &[u8]) -> (i64, i64) {
    let (_, diff) = line_diff(old, new);
    (i64::from(diff.count_additions()), i64::from(diff.count_removals()))
}

/// One side of a file pair as the header needs it.
pub struct Side<'a> {
    pub path: &'a [u8],
    pub mode: u32,
    pub id: ObjectId,
    pub short: String,
}

/// The pair's content: text to diff, or binary.
pub enum Body<'a> {
    Text(&'a [u8], &'a [u8]),
    Binary,
}

/// Appends one file pair's patch. A typechange is two patches, a deletion then an addition, as git splits it.
pub fn write(
    out: &mut Vec<u8>,
    (old, new): (Option<&Side>, Option<&Side>),
    rename: Option<u64>,
    body: Body,
    context: u32,
    abbrev: usize,
) {
    let (a, b) = (old.or(new).expect("a side"), new.or(old).expect("a side"));
    out.extend_from_slice(b"diff --git ");
    out.extend_from_slice(&quote_two(b"a/", a.path));
    out.push(b' ');
    out.extend_from_slice(&quote_two(b"b/", b.path));
    out.push(b'\n');
    match (old, new) {
        (None, Some(new)) => out.extend_from_slice(format!("new file mode {:06o}\n", new.mode).as_bytes()),
        (Some(old), None) => out.extend_from_slice(format!("deleted file mode {:06o}\n", old.mode).as_bytes()),
        (Some(old), Some(new)) => {
            if let Some(score) = rename {
                out.extend_from_slice(
                    format!("similarity index {}%\nrename from ", score * 100 / MAX_SCORE).as_bytes(),
                );
                out.extend_from_slice(&quote(old.path));
                out.extend_from_slice(b"\nrename to ");
                out.extend_from_slice(&quote(new.path));
                out.push(b'\n');
            }
            if old.mode != new.mode {
                out.extend_from_slice(format!("old mode {:06o}\nnew mode {:06o}\n", old.mode, new.mode).as_bytes());
            }
        }
        (None, None) => unreachable!("a side"),
    }
    let (old_id, new_id) = (old.map(|s| s.id), new.map(|s| s.id));
    if old_id != new_id {
        let short = |side: Option<&Side>| side.map_or_else(|| "0".repeat(abbrev), |s| s.short.clone());
        out.extend_from_slice(format!("index {}..{}", short(old), short(new)).as_bytes());
        if let (Some(old), Some(new)) = (old, new)
            && old.mode == new.mode
        {
            out.extend_from_slice(format!(" {:06o}", old.mode).as_bytes());
        }
        out.push(b'\n');
    }
    let label =
        |prefix: &[u8], side: Option<&Side>| side.map_or_else(|| b"/dev/null".to_vec(), |s| quote_two(prefix, s.path));
    let (old_label, new_label) = (label(b"a/", old), label(b"b/", new));
    match body {
        Body::Binary if old_id != new_id => {
            out.extend_from_slice(b"Binary files ");
            out.extend_from_slice(&old_label);
            out.extend_from_slice(b" and ");
            out.extend_from_slice(&new_label);
            out.extend_from_slice(b" differ\n");
        }
        Body::Binary => {}
        Body::Text(before, after) => {
            let hunks = hunks(before, after, context);
            if hunks.is_empty() {
                return;
            }
            // Git tabs a name with a space, so the line's end is unambiguous to `patch`.
            let tab = |side: Option<&Side>| if side.is_some_and(|s| s.path.contains(&b' ')) { &b"\t"[..] } else { b"" };
            out.extend_from_slice(b"--- ");
            out.extend_from_slice(&old_label);
            out.extend_from_slice(tab(old));
            out.extend_from_slice(b"\n+++ ");
            out.extend_from_slice(&new_label);
            out.extend_from_slice(tab(new));
            out.push(b'\n');
            out.extend_from_slice(&hunks);
        }
    }
}

/// Whether two modes hold different kinds of thing (file, link, submodule), which git patches as two.
pub fn splits(old: u32, new: u32) -> bool {
    kind(old) != kind(new) && !(kind(old) == GITLINK && kind(new) == GITLINK)
}

/// The hunks of a text diff with `context` lines around each change; changes at most twice the
/// context apart share a hunk.
fn hunks(old: &[u8], new: &[u8], context: u32) -> Vec<u8> {
    let (input, diff) = line_diff(old, new);
    let (before, after) = (&input.before, &input.after);
    let line = |token| input.interner[token];
    let changes: Vec<_> = diff.hunks().collect();
    let mut out = Vec::new();
    let mut func = FuncLine::default();
    let mut i = 0;
    while i < changes.len() {
        let mut j = i;
        while j + 1 < changes.len() && changes[j + 1].before.start - changes[j].before.end <= 2 * context {
            j += 1;
        }
        let (first, last) = (&changes[i], &changes[j]);
        let s1 = first.before.start.saturating_sub(context);
        let s2 = first.after.start.saturating_sub(context);
        let e1 = (last.before.end + context).min(before.len() as u32);
        let e2 = (last.after.end + context).min(after.len() as u32);
        let header = |start: u32, count: u32| {
            let start = if count == 0 { start } else { start + 1 };
            if count == 1 { format!("{start}") } else { format!("{start},{count}") }
        };
        out.extend_from_slice(format!("@@ -{} +{} @@", header(s1, e1 - s1), header(s2, e2 - s2)).as_bytes());
        let text = func.find(|l| line(before[l as usize]), i64::from(s1) - 1);
        if !text.is_empty() {
            out.push(b' ');
            out.extend_from_slice(text);
        }
        out.push(b'\n');
        let (mut o, mut n) = (s1, s2);
        for change in &changes[i..=j] {
            while o < change.before.start {
                emit(&mut out, b' ', line(before[o as usize]));
                (o, n) = (o + 1, n + 1);
            }
            for k in change.before.clone() {
                emit(&mut out, b'-', line(before[k as usize]));
            }
            for k in change.after.clone() {
                emit(&mut out, b'+', line(after[k as usize]));
            }
            (o, n) = (change.before.end, change.after.end);
        }
        while o < e1 {
            emit(&mut out, b' ', line(before[o as usize]));
            (o, n) = (o + 1, n + 1);
        }
        let _ = n;
        i = j + 1;
    }
    out
}

fn emit(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
    out.push(prefix);
    out.extend_from_slice(line);
    if line.last() != Some(&b'\n') {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

/// The `@@` header's function line: the nearest old-side line above the hunk that starts with a
/// letter, `_` or `$`, searched back only to the previous hunk's start and otherwise carried over.
#[derive(Default)]
struct FuncLine {
    text: Vec<u8>,
    searched_to: Option<i64>,
}

impl FuncLine {
    fn find<'a>(&'a mut self, line: impl Fn(u32) -> &'a [u8], start: i64) -> &'a [u8] {
        let limit = self.searched_to.unwrap_or(-1);
        self.searched_to = Some(start);
        let mut l = start;
        while l > limit && l >= 0 {
            let rec = line(l as u32);
            if rec.first().is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b'$') {
                let mut len = rec.len().min(FUNC_LINE);
                while len > 0 && is_space(rec[len - 1]) {
                    len -= 1;
                }
                // Whitespace goes first: a space before the split character stays.
                if len < rec.len() {
                    len = whole_chars(&rec[..len]);
                }
                self.text = rec[..len].to_vec();
                break;
            }
            l -= 1;
        }
        &self.text
    }
}

/// How much of `cut` ends on a whole UTF-8 character: git drops a sequence the cut split.
fn whole_chars(cut: &[u8]) -> usize {
    let start = cut.len().saturating_sub(4);
    let Some(lead) = (start..cut.len()).rev().find(|&i| cut[i] & 0xc0 != 0x80) else { return cut.len() };
    let width = match cut[lead] {
        b if b >= 0xf0 => 4,
        b if b >= 0xe0 => 3,
        b if b >= 0xc0 => 2,
        _ => 1,
    };
    if lead + width > cut.len() { lead } else { cut.len() }
}

/// C's `isspace`, which git trims the function line with.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `prefix` and `path` as git prints them together: in quotes, escaped, when the path needs it.
fn quote_two(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    let mut joined = prefix.to_vec();
    joined.extend_from_slice(path);
    quote(&joined)
}

/// Git's C-style quoting with `core.quotePath=false`: control characters, `"` and `\` are escaped
/// and the whole name quoted; other bytes, UTF-8 included, pass through.
fn quote(name: &[u8]) -> Vec<u8> {
    if !name.iter().any(|&c| c < 0x20 || c == b'"' || c == b'\\' || c == 0x7f) {
        return name.to_vec();
    }
    let mut out = vec![b'"'];
    for &c in name {
        match c {
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            c if c < 0x20 || c == 0x7f => out.extend_from_slice(format!("\\{c:03o}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
    out
}
