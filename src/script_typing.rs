//! What the script editor does as you type (Studio Settings > Script
//! Editor): closing brackets and quotes, the `end` after a block opener,
//! and keeping the indentation on Enter.
//!
//! Each function takes the text and the selection (a byte range; empty is
//! a caret) and returns the edit to make instead of the key's own, or None
//! to let the text box handle the key as usual.

use std::ops::Range;

use crate::script_editor::{tokenize, Tok};

#[derive(Debug, PartialEq)]
pub struct Edit {
    /// The bytes replaced.
    pub range: Range<usize>,
    pub insert: String,
    /// Where the caret goes, as a byte offset into the new text.
    pub caret: usize,
}

fn closer(c: char) -> Option<char> {
    Some(match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '"' | '\'' | '`' => c,
        _ => return None,
    })
}

fn word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// A typed character: an opener gets its closer (around the selection if
/// there is one), a closer right before the same closer steps over it.
pub fn typed(text: &str, sel: Range<usize>, c: char) -> Option<Edit> {
    let next = text[sel.end..].chars().next();
    let prev = text[..sel.start].chars().next_back();
    let quote = matches!(c, '"' | '\'' | '`');
    // Step over the closer the editor added (or that's already there).
    if sel.is_empty() && next == Some(c) && (quote || matches!(c, ')' | ']' | '}')) {
        return Some(Edit { range: sel.clone(), insert: String::new(), caret: sel.start + c.len_utf8() });
    }
    let close = closer(c)?;
    let inner = &text[sel.clone()];
    if inner.is_empty() {
        // Not right before a word (`(|foo`), and a quote not inside one (`don't`).
        if word_char(next) || (quote && word_char(prev)) {
            return None;
        }
    }
    Some(Edit { insert: format!("{c}{inner}{close}"), caret: sel.start + c.len_utf8() + inner.len(), range: sel })
}

/// Backspace between an empty pair (`(|)`, `"|"`) deletes both.
pub fn backspace(text: &str, sel: Range<usize>) -> Option<Edit> {
    if !sel.is_empty() {
        return None;
    }
    let prev = text[..sel.start].chars().next_back()?;
    let next = text[sel.end..].chars().next()?;
    if closer(prev) != Some(next) {
        return None;
    }
    let start = sel.start - prev.len_utf8();
    Some(Edit { range: start..sel.end + next.len_utf8(), insert: String::new(), caret: start })
}

/// The significant tokens of `src` (no whitespace or comments) as (text, kind).
fn tokens(src: &str) -> impl Iterator<Item = (&str, Tok)> {
    tokenize(src)
        .into_iter()
        .map(move |(r, t)| (&src[r], t))
        .filter(|(s, t)| *t != Tok::Comment && !s.trim().is_empty())
}

/// Blocks opened and not yet closed: `function`, `if` and `do` (which also
/// covers `while … do` and `for … do`) less `end`.
fn unclosed_blocks(src: &str) -> i32 {
    tokens(src)
        .filter(|(_, t)| *t == Tok::Keyword)
        .map(|(s, _)| match s {
            "function" | "if" | "do" => 1,
            "end" => -1,
            _ => 0,
        })
        .sum()
}

/// Whether a line (up to the caret) opens a block: it ends in `then`, `do`,
/// `else` or `repeat`, or in the `)` of a function header that this line
/// doesn't also close.
fn opens_block(line: &str) -> bool {
    let toks: Vec<(&str, Tok)> = tokens(line).collect();
    match toks.last() {
        Some(("then" | "do" | "else" | "repeat", Tok::Keyword)) => true,
        Some((")", Tok::Op)) => {
            let opened = toks.iter().filter(|t| *t == &("function", Tok::Keyword)).count();
            let closed = toks.iter().filter(|t| *t == &("end", Tok::Keyword)).count();
            opened > closed
        }
        _ => false,
    }
}

/// Enter: after a block opener with no `end` for it in the script, adds
/// the `end` (rest of the line goes after it) and puts the caret in the
/// indented line between; else keeps the line's indentation, one `unit`
/// more after an opener.
pub fn enter(text: &str, sel: Range<usize>, auto_end: bool, auto_indent: bool, unit: &str) -> Option<Edit> {
    let line_start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..sel.start];
    let indent: String = before.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let opener = opens_block(before);
    let line_end = text[sel.end..].find('\n').map_or(text.len(), |i| sel.end + i);
    // Only closers may follow the caret (`task.spawn(function()|)`).
    let rest_ok = text[sel.end..line_end].chars().all(|c| c.is_whitespace() || ")]},;".contains(c));
    let ends_with_repeat = tokens(before).last() == Some(("repeat", Tok::Keyword));
    if auto_end && opener && rest_ok && !ends_with_repeat {
        let mut after = text.to_string();
        after.replace_range(sel.clone(), "");
        if unclosed_blocks(&after) > 0 {
            let inner = format!("\n{indent}{unit}");
            return Some(Edit { caret: sel.start + inner.len(), insert: format!("{inner}\n{indent}end"), range: sel });
        }
    }
    if auto_indent && (opener || !indent.is_empty()) {
        let insert = format!("\n{indent}{}", if opener { unit } else { "" });
        return Some(Edit { caret: sel.start + insert.len(), insert, range: sel });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies an edit; `|` marks the caret in the result.
    fn run(text: &str, f: impl Fn(&str, Range<usize>) -> Option<Edit>) -> Option<String> {
        let at = text.find('|').unwrap();
        let src = text.replace('|', "");
        let e = f(&src, at..at)?;
        let mut out = src.clone();
        out.replace_range(e.range, &e.insert);
        out.insert(e.caret, '|');
        Some(out)
    }

    #[test]
    fn brackets_and_quotes_close() {
        let t = |s: &str, c: char| run(s, |x, r| typed(x, r, c));
        assert_eq!(t("print|", '(').as_deref(), Some("print(|)"));
        assert_eq!(t("x = |", '{').as_deref(), Some("x = {|}"));
        assert_eq!(t("print(|)", '"').as_deref(), Some("print(\"|\")"));
        // Stepping over the added closer.
        assert_eq!(t("print(\"hi|\")", '"').as_deref(), Some("print(\"hi\"|)"));
        assert_eq!(t("print(1|)", ')').as_deref(), Some("print(1)|"));
        // Not before a word, nor a quote inside one.
        assert_eq!(t("|foo", '('), None);
        assert_eq!(t("-- don|", '\''), None);
        // Around a selection.
        let e = typed("a + b", 0..5, '(').unwrap();
        assert_eq!((e.insert.as_str(), e.caret), ("(a + b)", 6));
    }

    #[test]
    fn backspace_deletes_an_empty_pair() {
        assert_eq!(run("f(|)", backspace).as_deref(), Some("f|"));
        assert_eq!(run("'|'", backspace).as_deref(), Some("|"));
        assert_eq!(run("f(x|)", backspace), None);
    }

    #[test]
    fn enter_adds_end_after_block_openers() {
        let e = |s: &str| run(s, |x, r| enter(x, r, true, true, "\t"));
        assert_eq!(e("if x then|").as_deref(), Some("if x then\n\t|\nend"));
        assert_eq!(e("\tfor i = 1, 3 do|").as_deref(), Some("\tfor i = 1, 3 do\n\t\t|\n\tend"));
        assert_eq!(e("local function f(a)|").as_deref(), Some("local function f(a)\n\t|\nend"));
        assert_eq!(e("task.spawn(function()|)").as_deref(), Some("task.spawn(function()\n\t|\nend)"));
        // Already closed: just indents.
        assert_eq!(e("while true do|\nend").as_deref(), Some("while true do\n\t|\nend"));
        assert_eq!(e("if a then\nelseif b then|\nend").as_deref(), Some("if a then\nelseif b then\n\t|\nend"));
        // Not openers: a closed one-liner, a call, a comment.
        assert_eq!(e("if a then b() end|"), None);
        assert_eq!(e("print(x)|"), None);
        assert_eq!(e("x() -- then|"), None);
        // Indentation is kept.
        assert_eq!(e("\t\tprint(1)|").as_deref(), Some("\t\tprint(1)\n\t\t|"));
        // Text after the caret that isn't a closer: no end.
        assert_eq!(e("if x then| y()").as_deref(), Some("if x then\n\t| y()"));
        // repeat … until: indented, no end.
        assert_eq!(e("repeat|").as_deref(), Some("repeat\n\t|"));
        // Spaces for indentation.
        assert_eq!(
            run("do|", |x, r| enter(x, r, true, true, "    ")).as_deref(),
            Some("do\n    |\nend")
        );
        // Off: nothing.
        assert_eq!(run("if x then|", |x, r| enter(x, r, false, false, "\t")), None);
    }
}
