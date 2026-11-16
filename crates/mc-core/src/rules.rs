use mc_extract::LinkKind;
use regex::Regex;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Include,
    Exclude,
    /// Save the resource but don't follow the page links in it.
    DownloadOnly,
}

/// A regex matched against the full normalized URL. Rules are checked in
/// order and the first match wins.
#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: Regex,
    pub action: RuleAction,
}

impl Rule {
    pub fn new(pattern: &str, action: RuleAction) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: Regex::new(pattern)?,
            action,
        })
    }
}

/// The hosts a crawl may wander through: the start URLs' hosts plus aliases.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    hosts: Vec<String>,
}

impl Scope {
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        let mut hosts: Vec<String> = hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect();
        hosts.sort();
        hosts.dedup();
        Self { hosts }
    }

    pub fn contains(&self, url: &Url) -> bool {
        url.host_str()
            .is_some_and(|host| self.hosts.iter().any(|h| h == host))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Fetch { follow_links: bool },
    Skip(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Excluded,
    OutOfScope,
    TooDeep,
    UrlLimit,
    /// Disallowed by the site's robots.txt.
    Robots,
    /// Looks like a logout link, which would end the session.
    Logout,
    /// A file type the rules leave out.
    FileType,
    /// Larger than the size limit for its file type.
    TooBig,
    /// A script hook said not to fetch it.
    Script,
}

impl SkipReason {
    pub const ALL: [SkipReason; 9] = [
        Self::Excluded,
        Self::OutOfScope,
        Self::TooDeep,
        Self::UrlLimit,
        Self::Robots,
        Self::Logout,
        Self::FileType,
        Self::TooBig,
        Self::Script,
    ];

    /// A stable name for storage and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Excluded => "excluded",
            Self::OutOfScope => "out-of-scope",
            Self::TooDeep => "too-deep",
            Self::UrlLimit => "url-limit",
            Self::Robots => "robots",
            Self::Logout => "logout",
            Self::FileType => "file-type",
            Self::TooBig => "too-big",
            Self::Script => "script",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == name)
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Excluded => "excluded by a rule",
            Self::OutOfScope => "outside the crawl scope",
            Self::TooDeep => "deeper than the depth limit",
            Self::UrlLimit => "URL limit reached",
            Self::Robots => "disallowed by robots.txt",
            Self::Logout => "looks like a logout link",
            Self::FileType => "a file type left out by the rules",
            Self::TooBig => "larger than the limit for its type",
            Self::Script => "skipped by the script",
        })
    }
}

/// Decides, for each discovered link, whether it is fetched.
#[derive(Debug, Clone)]
pub struct Policy {
    pub scope: Scope,
    pub rules: Vec<Rule>,
    /// Fetch images, stylesheets and scripts from other hosts.
    pub include_external_assets: bool,
    /// Page links deeper than this are not followed. Assets don't count.
    pub max_depth: Option<u32>,
}

impl Policy {
    pub fn decide(&self, url: &Url, kind: LinkKind, depth: u32) -> Decision {
        let rule = self
            .rules
            .iter()
            .find(|rule| rule.pattern.is_match(url.as_str()));
        if rule.is_some_and(|r| r.action == RuleAction::Exclude) {
            return Decision::Skip(SkipReason::Excluded);
        }
        // Following a logout link ends the session the crawl depends on.
        if rule.is_none() && kind == LinkKind::Page && looks_like_logout(url) {
            return Decision::Skip(SkipReason::Logout);
        }
        if kind == LinkKind::Page && self.max_depth.is_some_and(|max| depth > max) {
            return Decision::Skip(SkipReason::TooDeep);
        }

        let in_scope = self.scope.contains(url);
        match (rule.map(|r| r.action), kind) {
            (Some(RuleAction::DownloadOnly), _) => Decision::Fetch {
                follow_links: false,
            },
            // An explicit include fetches anything, but only crawls onward
            // from pages inside the scope, so one rule can't release the
            // crawler onto the whole web.
            (Some(_), _) => Decision::Fetch {
                follow_links: in_scope && kind == LinkKind::Page,
            },
            (None, LinkKind::Page) if in_scope => Decision::Fetch { follow_links: true },
            (None, LinkKind::Asset) if in_scope || self.include_external_assets => {
                Decision::Fetch {
                    follow_links: false,
                }
            }
            (None, _) => Decision::Skip(SkipReason::OutOfScope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn policy(rules: Vec<Rule>) -> Policy {
        Policy {
            scope: Scope::new(["site.test".to_owned(), "www.site.test".to_owned()]),
            rules,
            include_external_assets: false,
            max_depth: Some(2),
        }
    }

    #[test]
    fn default_scope_rules() {
        let p = policy(vec![]);
        let follow = Decision::Fetch { follow_links: true };
        let save = Decision::Fetch {
            follow_links: false,
        };
        assert_eq!(
            p.decide(&url("https://site.test/a"), LinkKind::Page, 1),
            follow
        );
        assert_eq!(
            p.decide(&url("https://www.site.test/a"), LinkKind::Page, 1),
            follow
        );
        assert_eq!(
            p.decide(&url("https://site.test/a.png"), LinkKind::Asset, 9),
            save
        );
        assert_eq!(
            p.decide(&url("https://other.test/"), LinkKind::Page, 1),
            Decision::Skip(SkipReason::OutOfScope)
        );
        assert_eq!(
            p.decide(&url("https://cdn.test/a.css"), LinkKind::Asset, 1),
            Decision::Skip(SkipReason::OutOfScope)
        );
        assert_eq!(
            p.decide(&url("https://site.test/deep"), LinkKind::Page, 3),
            Decision::Skip(SkipReason::TooDeep)
        );
    }

    #[test]
    fn external_assets_option() {
        let mut p = policy(vec![]);
        p.include_external_assets = true;
        assert_eq!(
            p.decide(&url("https://cdn.test/a.css"), LinkKind::Asset, 1),
            Decision::Fetch {
                follow_links: false
            }
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let p = policy(vec![
            Rule::new("/admin/public", RuleAction::Include).unwrap(),
            Rule::new("/admin", RuleAction::Exclude).unwrap(),
            Rule::new(r"\.pdf$", RuleAction::DownloadOnly).unwrap(),
            Rule::new("^https://docs\\.other\\.test/", RuleAction::Include).unwrap(),
        ]);
        assert_eq!(
            p.decide(&url("https://site.test/admin/public/x"), LinkKind::Page, 1),
            Decision::Fetch { follow_links: true }
        );
        assert_eq!(
            p.decide(&url("https://site.test/admin/users"), LinkKind::Page, 1),
            Decision::Skip(SkipReason::Excluded)
        );
        assert_eq!(
            p.decide(&url("https://site.test/doc.pdf"), LinkKind::Page, 1),
            Decision::Fetch {
                follow_links: false
            }
        );
        // Included but out of scope: saved, not crawled onward.
        assert_eq!(
            p.decide(&url("https://docs.other.test/page"), LinkKind::Page, 1),
            Decision::Fetch {
                follow_links: false
            }
        );
    }
}

/// Path segments that log the user out when visited.
const LOGOUT_WORDS: &[&str] = &[
    "logout", "log-out", "log_out", "signout", "sign-out", "sign_out", "logoff",
];

fn looks_like_logout(url: &Url) -> bool {
    let in_path = url.path_segments().is_some_and(|mut segments| {
        segments.any(|segment| {
            let stem = segment
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            LOGOUT_WORDS.contains(&stem.as_str())
        })
    });
    let in_query = url
        .query_pairs()
        .any(|(_, value)| LOGOUT_WORDS.contains(&value.to_ascii_lowercase().as_str()));
    in_path || in_query
}

#[cfg(test)]
mod logout_tests {
    use super::*;

    #[test]
    fn spots_logout_links() {
        let policy = Policy {
            scope: Scope::new(["site.test".to_owned()]),
            rules: vec![Rule::new("/keep/logout", RuleAction::Include).unwrap()],
            include_external_assets: false,
            max_depth: None,
        };
        let decide = |s: &str| policy.decide(&Url::parse(s).unwrap(), LinkKind::Page, 1);
        let skipped = Decision::Skip(SkipReason::Logout);
        for url in [
            "https://site.test/logout",
            "https://site.test/account/Sign-Out.php",
            "https://site.test/index.php?action=logout",
        ] {
            assert_eq!(decide(url), skipped, "{url}");
        }
        assert_ne!(
            decide("https://site.test/blog/logout-tips-for-admins"),
            skipped
        );
        // An explicit include wins.
        assert_ne!(decide("https://site.test/keep/logout"), skipped);
    }
}
