//! The command palette's list: which commands exist right now, and which
//! match what was typed. The window decides how to run them.

use crate::app::Tab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewCopy,
    Start,
    Update,
    Stop,
    Show(Tab),
    ExportMap,
    OpenReport,
    /// Open the saved copy at this index of the sidebar.
    Select(usize),
}

/// What the palette needs to know about the window.
pub struct Context<'a> {
    pub running: bool,
    pub can_update: bool,
    pub has_report: bool,
    pub projects: &'a [String],
}

pub fn commands(cx: &Context<'_>) -> Vec<(String, Command)> {
    let mut list = vec![("New copy".to_owned(), Command::NewCopy)];
    if cx.running {
        list.push(("Stop copying".to_owned(), Command::Stop));
    } else {
        list.push(("Copy now".to_owned(), Command::Start));
        if cx.can_update {
            list.push(("Update this copy".to_owned(), Command::Update));
        }
    }
    list.push(("Show options".to_owned(), Command::Show(Tab::Copy)));
    list.push(("Show limits".to_owned(), Command::Show(Tab::Limits)));
    list.push(("Show rules".to_owned(), Command::Show(Tab::Rules)));
    list.push(("Show authentication".to_owned(), Command::Show(Tab::Auth)));
    list.push(("Show output".to_owned(), Command::Show(Tab::Output)));
    list.push(("Show link map".to_owned(), Command::Show(Tab::LinkMap)));
    list.push(("Export link map as SVG".to_owned(), Command::ExportMap));
    if cx.has_report {
        list.push(("Open report".to_owned(), Command::OpenReport));
    }
    if !cx.running {
        list.extend(
            cx.projects
                .iter()
                .enumerate()
                .map(|(index, name)| (format!("Open copy: {name}"), Command::Select(index))),
        );
    }
    list
}

/// Commands whose label contains every word of `query`, in list order.
pub fn matching(commands: Vec<(String, Command)>, query: &str) -> Vec<(String, Command)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    commands
        .into_iter()
        .filter(|(label, _)| {
            let label = label.to_lowercase();
            words.iter().all(|word| label.contains(word.as_str()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(list: &[(String, Command)]) -> Vec<&str> {
        list.iter().map(|(label, _)| label.as_str()).collect()
    }

    #[test]
    fn offers_what_fits_the_moment() {
        let projects = ["a.test".to_owned()];
        let idle = commands(&Context {
            running: false,
            can_update: true,
            has_report: true,
            projects: &projects,
        });
        let idle = labels(&idle);
        assert!(idle.contains(&"Update this copy") && idle.contains(&"Open copy: a.test"));
        assert!(!idle.contains(&"Stop copying"));

        let busy = commands(&Context {
            running: true,
            can_update: true,
            has_report: false,
            projects: &projects,
        });
        let busy = labels(&busy);
        assert!(busy.contains(&"Stop copying"));
        assert!(!busy.contains(&"Copy now") && !busy.contains(&"Open copy: a.test"));
    }

    #[test]
    fn filters_by_every_word() {
        let all = commands(&Context {
            running: false,
            can_update: false,
            has_report: false,
            projects: &[],
        });
        assert_eq!(
            labels(&matching(all.clone(), "link")),
            ["Show link map", "Export link map as SVG"]
        );
        assert_eq!(
            labels(&matching(all.clone(), "MAP svg")),
            ["Export link map as SVG"]
        );
        assert_eq!(matching(all.clone(), "").len(), all.len());
        assert!(matching(all, "nothing like this").is_empty());
    }
}
