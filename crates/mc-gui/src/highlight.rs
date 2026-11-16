//! Syntax colors for the script editor: a small JavaScript tokenizer that
//! marks keywords, strings, comments, numbers, function names and the hook
//! names, plus fold ranges for `{ … }` blocks. Scripts are short, so each
//! change simply re-reads the whole text.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use gpui_kit::{Context, HighlightStyle, SharedString, Window};

const KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "of",
    "return",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "yield",
];
const CONSTANTS: &[&str] = &["true", "false", "null", "undefined", "NaN", "Infinity"];
/// The hooks and the functions the copy provides.
const HOOKS: &[&str] = &[
    "onStart",
    "shouldFetch",
    "onFile",
    "onError",
    "onFinish",
    "log",
];

/// Highlights `javascript` (and `js`) in code editors.
pub fn factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        matches!(language, "javascript" | "js")
            .then(|| Box::new(JsHighlighter::default()) as Box<dyn InputHighlighter>)
    })
}

#[derive(Default)]
pub struct JsHighlighter {
    /// Ordered, non-overlapping byte ranges and their style names.
    tokens: Vec<(Range<usize>, &'static str)>,
    folds: Vec<FoldRange>,
}

impl InputHighlighter for JsHighlighter {
    fn language(&self) -> SharedString {
        "javascript".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        (self.tokens, self.folds) = tokenize(&text.to_string());
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let mut at = range.start;
        let first = self.tokens.partition_point(|(r, _)| r.end <= range.start);
        for (token, name) in &self.tokens[first..] {
            if token.start >= range.end {
                break;
            }
            let start = token.start.max(at);
            let end = token.end.min(range.end);
            if start >= end {
                continue;
            }
            if at < start {
                runs.push((at..start, HighlightStyle::default()));
            }
            runs.push((start..end, resolver.style(name).unwrap_or_default()));
            at = end;
        }
        if at < range.end {
            runs.push((at..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.folds.clone()
    }
}

/// Splits `text` into styled tokens and finds the blocks that span lines.
pub fn tokenize(text: &str) -> (Vec<(Range<usize>, &'static str)>, Vec<FoldRange>) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut folds = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut line = 0;
    let mut i = 0;
    // The last token that wasn't a space or comment, to tell `a.b` and
    // division from regular expressions.
    let mut previous = b'\n';
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        match c {
            b'\n' => {
                line += 1;
                i += 1;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                tokens.push((start..i, "comment"));
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    line += usize::from(bytes[i] == b'\n');
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                tokens.push((start..i, "comment"));
                continue;
            }
            b'/' if !(previous.is_ascii_alphanumeric() || b")]_$".contains(&previous)) => {
                // A regular expression literal.
                i += 1;
                let mut class = false;
                while i < bytes.len() && bytes[i] != b'\n' {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'[' => class = true,
                        b']' => class = false,
                        b'/' if !class => break,
                        _ => {}
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                tokens.push((start..i, "string.regex"));
            }
            b'"' | b'\'' | b'`' => {
                i += 1;
                while i < bytes.len() && bytes[i] != c {
                    if bytes[i] == b'\n' {
                        if c != b'`' {
                            break;
                        }
                        line += 1;
                    }
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                tokens.push((start..i, "string"));
            }
            b'0'..=b'9' => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
                {
                    i += 1;
                }
                tokens.push((start..i, "number"));
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                let word = &text[start..i];
                let rest = text[i..].trim_start_matches([' ', '\t']);
                let name = if previous == b'.' {
                    if rest.starts_with('(') {
                        "function"
                    } else {
                        "property"
                    }
                } else if KEYWORDS.contains(&word) {
                    "keyword"
                } else if CONSTANTS.contains(&word) {
                    "boolean"
                } else if HOOKS.contains(&word) || rest.starts_with('(') {
                    "function"
                } else if word.starts_with(|c: char| c.is_ascii_uppercase()) {
                    "type"
                } else {
                    "variable"
                };
                tokens.push((start..i, name));
            }
            b'{' => {
                open.push(line);
                i += 1;
                tokens.push((start..i, "punctuation.bracket"));
            }
            b'}' => {
                if let Some(from) = open.pop()
                    && line > from
                {
                    folds.push(FoldRange::new(from, line));
                }
                i += 1;
                tokens.push((start..i, "punctuation.bracket"));
            }
            b'(' | b')' | b'[' | b']' => {
                i += 1;
                tokens.push((start..i, "punctuation.bracket"));
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            _ if c.is_ascii_punctuation() => {
                i += 1;
                tokens.push((start..i, "operator"));
            }
            _ => {
                // Other text (non-ASCII letters) keeps the plain color.
                i += text[i..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
        }
        previous = bytes[i - 1];
    }
    folds.sort_by_key(|f| f.start_line);
    (tokens, folds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<(&str, &'static str)> {
        tokenize(text)
            .0
            .into_iter()
            .map(|(r, name)| (&text[r], name))
            .collect()
    }

    #[test]
    fn colors_javascript() {
        let text = "function onFile(file) { // big\n  if (file.bytes > 1e7) log(`x ${1}`) }";
        let named = names(text);
        assert!(named.contains(&("function", "keyword")));
        assert!(named.contains(&("onFile", "function")));
        assert!(named.contains(&("// big", "comment")));
        assert!(named.contains(&("bytes", "property")));
        assert!(named.contains(&("1e7", "number")));
        assert!(named.contains(&("log", "function")));
        assert!(named.contains(&("`x ${1}`", "string")));
        assert_eq!(tokenize(text).1, [FoldRange::new(0, 1)]);
    }

    #[test]
    fn tells_regular_expressions_from_division() {
        let named = names("let a = b / 2; let r = /a\\/b[/]/g.test(u)");
        assert!(named.contains(&("/", "operator")));
        assert!(named.contains(&("/a\\/b[/]/g", "string.regex")));
        assert!(named.contains(&("test", "function")));
        assert!(names("\"un/closed").contains(&("\"un/closed", "string")));
    }

    #[test]
    fn style_runs_cover_the_range() {
        let mut highlighter = JsHighlighter::default();
        let text = "let x = 'a'; // c";
        highlighter.tokens = tokenize(text).0;
        struct Any;
        impl HighlightStyleResolver for Any {
            fn style(&self, _: &str) -> Option<HighlightStyle> {
                Some(HighlightStyle::default())
            }
        }
        for range in [0..text.len(), 2..9, 5..6, 13..17] {
            let runs = highlighter.styles(&range, &Any);
            assert_eq!(runs.first().unwrap().0.start, range.start);
            assert_eq!(runs.last().unwrap().0.end, range.end);
            assert!(
                runs.windows(2).all(|w| w[0].0.end == w[1].0.start),
                "{range:?}"
            );
        }
    }
}
