//! Autocomplete for the script editor: the hooks a script can define, its
//! `log` function, and the fields of the objects the hooks receive.

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, Rope};
use gpui_kit::{App, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, Documentation,
};

/// What a script can name: (label, inserted text, kind, description).
type Suggestion = (&'static str, &'static str, CompletionItemKind, &'static str);

const HOOKS: &[Suggestion] = &[
    (
        "onStart",
        "function onStart(info) {\n  \n}",
        CompletionItemKind::FUNCTION,
        "Called once when the copy starts. info.url, info.output",
    ),
    (
        "shouldFetch",
        "function shouldFetch(url, kind) {\n  return true;\n}",
        CompletionItemKind::FUNCTION,
        "Called before each download; return false to skip. kind: \"page\" or \"asset\"",
    ),
    (
        "onFile",
        "function onFile(file) {\n  \n}",
        CompletionItemKind::FUNCTION,
        "Called after each file is saved. file.url, .path, .bytes, .mime, .type",
    ),
    (
        "onError",
        "function onError(url, reason) {\n  \n}",
        CompletionItemKind::FUNCTION,
        "Called when a download fails",
    ),
    (
        "onFinish",
        "function onFinish(summary) {\n  \n}",
        CompletionItemKind::FUNCTION,
        "Called when the copy ends. summary.saved, .failed, .skipped, .bytes, .seconds, .stopped",
    ),
    (
        "log",
        "log()",
        CompletionItemKind::FUNCTION,
        "log(text): writes a line to Activity and the log panel",
    ),
];

const FIELDS: &[(&str, &[&str])] = &[
    ("info", &["url", "output"]),
    ("file", &["url", "path", "bytes", "mime", "type"]),
    (
        "summary",
        &["saved", "failed", "skipped", "bytes", "seconds", "stopped"],
    ),
];

const KEYWORDS: &[&str] = &[
    "function",
    "return",
    "const",
    "let",
    "if",
    "else",
    "for",
    "while",
    "true",
    "false",
    "null",
    "includes",
    "startsWith",
    "endsWith",
    "length",
];

/// Completes the word before the cursor.
pub struct ScriptCompletion;

impl ScriptCompletion {
    /// Suggestions for `text` with the cursor at byte `offset`.
    pub fn suggest(text: &str, offset: usize) -> Vec<CompletionItem> {
        let before = &text[..offset.min(text.len())];
        let start = before
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
            .map_or(0, |i| i + 1);
        let word = &before[start..];
        // `file.by` → fields of `file` starting with `by`.
        if let Some((object, prefix)) = word.rsplit_once('.') {
            let object = object.rsplit('.').next().unwrap_or(object);
            return FIELDS
                .iter()
                .filter(|(name, _)| *name == object)
                .flat_map(|(_, fields)| fields.iter())
                .filter(|field| field.starts_with(prefix))
                .map(|field| CompletionItem {
                    label: (*field).to_owned(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(format!("{object}.{field}")),
                    insert_text: Some((*field).to_owned()),
                    ..Default::default()
                })
                .collect();
        }
        // Nothing typed (Ctrl+Space on a blank spot): everything.
        let lower = word.to_lowercase();
        let hooks = HOOKS
            .iter()
            .filter(|(label, ..)| label.to_lowercase().starts_with(&lower))
            .map(|(label, insert, kind, doc)| CompletionItem {
                label: (*label).to_owned(),
                kind: Some(*kind),
                detail: Some((*doc).to_owned()),
                documentation: Some(Documentation::String((*doc).to_owned())),
                insert_text: Some((*insert).to_owned()),
                ..Default::default()
            });
        let keywords = KEYWORDS
            .iter()
            .filter(|keyword| keyword.starts_with(word) && (word.is_empty() || **keyword != word))
            .map(|keyword| CompletionItem {
                label: (*keyword).to_owned(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some((*keyword).to_owned()),
                ..Default::default()
            });
        hooks.chain(keywords).collect()
    }
}

impl CompletionProvider for ScriptCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let items = Self::suggest(&text.to_string(), offset);
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text == "."
            || new_text
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(text: &str) -> Vec<String> {
        ScriptCompletion::suggest(text, text.len())
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn completes_hooks_fields_and_keywords() {
        assert_eq!(labels("on"), ["onStart", "onFile", "onError", "onFinish"]);
        let all = labels("  ");
        assert!(all.len() > 20 && all[0] == "onStart", "{all:?}");
        assert!(all.contains(&"log".to_owned()) && all.contains(&"return".to_owned()));
        assert_eq!(labels("function should"), ["shouldFetch"]);
        assert_eq!(labels("  if (file.by"), ["bytes"]);
        assert_eq!(
            labels("summary."),
            ["saved", "failed", "skipped", "bytes", "seconds", "stopped"]
        );
        assert_eq!(labels("ret"), ["return"]);
        assert_eq!(labels("x = ").len(), all.len(), "nothing typed: everything");
        let on_file = ScriptCompletion::suggest("onF", 3)
            .into_iter()
            .find(|item| item.label == "onFile")
            .unwrap();
        assert!(
            on_file
                .insert_text
                .unwrap()
                .starts_with("function onFile(file) {")
        );
    }
}
