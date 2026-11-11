//! Looking at a site without crawling it: what the rules would do with a
//! URL, and the page tree of a finished copy.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mc_core::{
    Decision, EntryState, LinkKind, Policy, Rule, RuleAction, Scope, StateStore, UrlEntry,
    normalize,
};
use mc_requester::Url;
use mc_store::SqliteStore;

/// Builds the skip/include rules from the form. Empty fields add nothing.
pub fn rules(exclude: &str, include: &str) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for (pattern, action) in [
        (exclude, RuleAction::Exclude),
        (include, RuleAction::Include),
    ] {
        let pattern = pattern.trim();
        if !pattern.is_empty() {
            rules.push(
                Rule::new(pattern, action)
                    .map_err(|e| format!("Invalid pattern “{pattern}”: {e}"))?,
            );
        }
    }
    Ok(rules)
}

/// What a crawl starting at `seed` would do with a link to `url`, in words.
pub fn explain(seed: &Url, rules: Vec<Rule>, url: &str) -> String {
    let Some(url) = Url::parse(url.trim()).ok().and_then(|u| normalize(&u)) else {
        return "Enter a full http(s) address to test.".to_owned();
    };
    let policy = Policy {
        scope: Scope::new(seed.host_str().map(str::to_owned)),
        rules,
        include_external_assets: false,
        max_depth: None,
    };
    match policy.decide(&url, LinkKind::Page, 1) {
        Decision::Fetch { follow_links: true } => "Copied, and its links are followed.".to_owned(),
        Decision::Fetch {
            follow_links: false,
        } => "Saved, but its links are not followed.".to_owned(),
        Decision::Skip(reason) => format!("Skipped: {reason}."),
    }
}

/// One line of the link map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapLine {
    pub url: String,
    pub depth: usize,
    /// Empty when the page was saved.
    pub note: String,
}

/// The pages of a copy as a tree, each under the page where it was first
/// found, depth first. At most `limit` lines.
pub fn link_map(output: &Path, limit: usize) -> Result<Vec<MapLine>, String> {
    if !SqliteStore::exists_in(output) {
        return Err("No copy in this folder yet.".to_owned());
    }
    let mut store = SqliteStore::open(output).map_err(|e| e.to_string())?;
    let entries = store.load().map_err(|e| e.to_string())?.entries;
    Ok(tree(&entries, limit))
}

fn tree(entries: &[UrlEntry], limit: usize) -> Vec<MapLine> {
    let pages: Vec<&UrlEntry> = entries
        .iter()
        .filter(|e| e.kind == LinkKind::Page)
        .collect();
    let mut children: HashMap<&Url, Vec<&UrlEntry>> = HashMap::new();
    let mut roots = Vec::new();
    for page in &pages {
        match &page.referrer {
            Some(parent) => children.entry(parent).or_default().push(page),
            None => roots.push(*page),
        }
    }

    let mut lines = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(&UrlEntry, usize)> = roots.into_iter().rev().map(|r| (r, 0)).collect();
    while let Some((entry, depth)) = stack.pop() {
        if lines.len() == limit || !seen.insert(&entry.url) {
            continue;
        }
        lines.push(MapLine {
            url: entry.url.to_string(),
            depth,
            note: note(&entry.state),
        });
        if let Some(kids) = children.get(&entry.url) {
            stack.extend(kids.iter().rev().map(|kid| (*kid, depth + 1)));
        }
    }
    lines
}

fn note(state: &EntryState) -> String {
    match state {
        EntryState::Saved(_) => String::new(),
        EntryState::Queued => "not fetched yet".to_owned(),
        EntryState::Alias(to) => format!("redirects to {to}"),
        EntryState::Remote(to) => format!("left the site for {to}"),
        EntryState::Failed(reason) => format!("failed: {reason}"),
        EntryState::Skipped(reason) => format!("skipped: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_core::{LocalPath, SavedFile, SkipReason};
    use mc_requester::Validators;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn explains_rule_decisions() {
        let seed = url("https://site.test/");
        let rules = rules("/admin", "^https://docs\\.other\\.test/").unwrap();
        let explain = |u: &str| explain(&seed, rules.clone(), u);
        assert_eq!(
            explain("https://site.test/blog"),
            "Copied, and its links are followed."
        );
        assert_eq!(
            explain("https://site.test/admin/users"),
            "Skipped: excluded by a rule."
        );
        assert_eq!(
            explain("https://other.test/"),
            "Skipped: outside the crawl scope."
        );
        assert_eq!(
            explain("https://docs.other.test/a"),
            "Saved, but its links are not followed."
        );
        assert_eq!(
            explain("https://site.test/logout"),
            "Skipped: looks like a logout link."
        );
        assert!(explain("not a url").contains("full http"));
        assert!(
            super::rules("(", "")
                .unwrap_err()
                .contains("Invalid pattern")
        );
        assert!(super::rules(" ", "").unwrap().is_empty());
    }

    #[test]
    fn builds_the_page_tree() {
        let saved = EntryState::Saved(SavedFile {
            path: LocalPath::parse("s.test/x.html").unwrap(),
            mime: None,
            document: None,
            bytes: 1,
            validators: Validators::default(),
        });
        let entry = |u: &str, referrer: Option<&str>, kind, state: EntryState| UrlEntry {
            url: url(u),
            depth: 0,
            kind,
            referrer: referrer.map(url),
            follow_links: true,
            state,
        };
        let entries = vec![
            entry("https://s.test/", None, LinkKind::Page, saved.clone()),
            entry(
                "https://s.test/a",
                Some("https://s.test/"),
                LinkKind::Page,
                saved.clone(),
            ),
            entry(
                "https://s.test/style.css",
                Some("https://s.test/"),
                LinkKind::Asset,
                saved.clone(),
            ),
            entry(
                "https://s.test/a/1",
                Some("https://s.test/a"),
                LinkKind::Page,
                EntryState::Failed("HTTP 404".into()),
            ),
            entry(
                "https://s.test/b",
                Some("https://s.test/"),
                LinkKind::Page,
                EntryState::Skipped(SkipReason::TooDeep),
            ),
        ];
        let lines = tree(&entries, 100);
        let shown: Vec<_> = lines.iter().map(|l| (l.url.as_str(), l.depth)).collect();
        assert_eq!(
            shown,
            [
                ("https://s.test/", 0),
                ("https://s.test/a", 1),
                ("https://s.test/a/1", 2),
                ("https://s.test/b", 1)
            ]
        );
        assert_eq!(lines[2].note, "failed: HTTP 404");
        assert_eq!(tree(&entries, 2).len(), 2);
    }
}

/// Row height and indent of the drawn link map, in pixels.
const ROW: usize = 22;
const INDENT: usize = 22;

/// The link map drawn as a tree diagram, as a standalone SVG.
pub fn link_map_svg(lines: &[MapLine]) -> String {
    let widest = lines
        .iter()
        .map(|line| {
            40 + line.depth * INDENT + (line.url.chars().count() + line.note.chars().count()) * 7
        })
        .max()
        .unwrap_or(200);
    let height = lines.len() * ROW + 40;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{widest}\" height=\"{height}\" \
         viewBox=\"0 0 {widest} {height}\" font-family=\"system-ui, sans-serif\" font-size=\"12\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n"
    );
    // The nearest earlier line one level up is each line's parent.
    let mut parents: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        parents.truncate(line.depth);
        let (x, y) = (20 + line.depth * INDENT, 30 + index * ROW);
        if let Some(&parent) = parents.last() {
            let (px, py) = (20 + (line.depth - 1) * INDENT, 30 + parent * ROW);
            svg.push_str(&format!(
                "<path d=\"M{px} {py} V{y} H{x}\" fill=\"none\" stroke=\"#9198a1\"/>\n"
            ));
        }
        let color = if line.note.is_empty() {
            "#2563eb"
        } else {
            "#cf222e"
        };
        svg.push_str(&format!(
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"4\" fill=\"{color}\"/>\n"
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" fill=\"#1f2328\">{}",
            x + 10,
            y + 4,
            xml_escape(&line.url)
        ));
        if !line.note.is_empty() {
            svg.push_str(&format!(
                " <tspan fill=\"#cf222e\">({})</tspan>",
                xml_escape(&line.note)
            ));
        }
        svg.push_str("</text>\n");
        parents.push(index);
    }
    svg.push_str("</svg>\n");
    svg
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod svg_tests {
    use super::*;

    #[test]
    fn draws_edges_to_parents() {
        let line = |url: &str, depth, note: &str| MapLine {
            url: url.to_owned(),
            depth,
            note: note.to_owned(),
        };
        let svg = link_map_svg(&[
            line("https://s.test/", 0, ""),
            line("https://s.test/a?x=1&y=<2>", 1, ""),
            line("https://s.test/a/b", 2, "failed: HTTP 404"),
            line("https://s.test/c", 1, ""),
        ]);
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert_eq!(svg.matches("<circle").count(), 4);
        // Root → a, a → b, root → c.
        assert_eq!(svg.matches("<path").count(), 3);
        assert!(svg.contains("M20 30 V96 H42"), "{svg}");
        assert!(svg.contains("x=1&amp;y=&lt;2&gt;"));
        assert!(svg.contains("(failed: HTTP 404)"));
    }
}
