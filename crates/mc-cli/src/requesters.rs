//! Which requesters this build has, and building the one a copy asks for.
//! Shared by the command-line tool and the desktop app.

use std::sync::Arc;

use mc_requester::{AUTO_ID, HybridRequester, Requester, RequesterRegistry, SharedResources};
use mc_requester_wreq::WreqFactory;

use crate::options::CrawlOptions;

/// Every requester this build includes. Playwright needs the `browser` feature.
pub fn registry() -> RequesterRegistry {
    let mut registry = RequesterRegistry::new();
    registry
        .register(WreqFactory)
        .expect("requester ids are unique");
    #[cfg(feature = "browser")]
    registry
        .register(mc_requester_playwright::PlaywrightFactory)
        .expect("requester ids are unique");
    registry
}

/// The requester named by `--requester`. `auto` combines wreq with the
/// browser, using the browser only for pages that need it.
pub fn build_requester(
    registry: &RequesterRegistry,
    options: &CrawlOptions,
    login: Option<serde_json::Value>,
    shared: &SharedResources,
) -> Result<Arc<dyn Requester>, String> {
    let uses_browser = options.requester == AUTO_ID || options.requester == "playwright";
    if login.is_some() && !uses_browser {
        return Err("--login-script needs --requester playwright or auto".to_owned());
    }
    let build = |id: &str| {
        let mut config = options.requester_config(id);
        if id == "playwright"
            && let (Some(steps), Some(object)) = (&login, config.as_object_mut())
        {
            object.insert("login".to_owned(), steps.clone());
        }
        registry
            .build(id, &config, shared)
            .map_err(|error| error.to_string())
    };
    if options.requester != AUTO_ID {
        return build(&options.requester);
    }
    let fast = build("wreq")?;
    let browser = build("playwright").map_err(|error| {
        format!("--requester auto needs a build with browser support ({error})")
    })?;
    Ok(Arc::new(HybridRequester::new(fast, browser)))
}
