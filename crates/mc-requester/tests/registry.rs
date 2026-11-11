#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mc_requester::mock::MockRequester;
use mc_requester::{
    BuildError, Capabilities, RegistryError, Requester, RequesterFactory, RequesterRegistry,
    SharedResources,
};
use serde_json::json;

struct FakeFactory {
    id: &'static str,
}

impl RequesterFactory for FakeFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            http2: true,
            ..Capabilities::default()
        }
    }

    fn build(
        &self,
        config: &serde_json::Value,
        _shared: &SharedResources,
    ) -> Result<Arc<dyn Requester>, BuildError> {
        if config.get("bogus").is_some() {
            return Err(BuildError {
                requester: self.id,
                message: "unknown key `bogus`".to_owned(),
            });
        }
        Ok(Arc::new(MockRequester::new()))
    }
}

#[test]
fn lists_registered_requesters_sorted_by_id() {
    let mut registry = RequesterRegistry::new();
    assert!(registry.is_empty());
    registry.register(FakeFactory { id: "wreq" }).unwrap();
    registry.register(FakeFactory { id: "playwright" }).unwrap();

    let ids: Vec<_> = registry.list().iter().map(|info| info.id).collect();
    assert_eq!(ids, ["playwright", "wreq"]);
    assert!(registry.list()[0].capabilities.http2);
}

#[test]
fn rejects_duplicate_ids() {
    let mut registry = RequesterRegistry::new();
    registry.register(FakeFactory { id: "wreq" }).unwrap();
    let err = registry.register(FakeFactory { id: "wreq" }).unwrap_err();
    assert!(matches!(err, RegistryError::Duplicate("wreq")));
}

#[test]
fn builds_by_id() {
    let mut registry = RequesterRegistry::new();
    registry.register(FakeFactory { id: "wreq" }).unwrap();
    let requester = registry
        .build("wreq", &json!({}), &SharedResources::default())
        .unwrap();
    assert_eq!(requester.id(), "mock");
}

#[test]
fn unknown_id_names_the_available_requesters() {
    let mut registry = RequesterRegistry::new();
    registry.register(FakeFactory { id: "wreq" }).unwrap();
    registry.register(FakeFactory { id: "playwright" }).unwrap();
    let err = registry
        .build("curl", &json!({}), &SharedResources::default())
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "unknown requester `curl` (available: playwright, wreq)"
    );
}

#[test]
fn unknown_id_on_empty_registry() {
    let err = RequesterRegistry::new()
        .build("wreq", &json!({}), &SharedResources::default())
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "unknown requester `wreq` (available: none)"
    );
}

#[test]
fn factory_config_errors_pass_through() {
    let mut registry = RequesterRegistry::new();
    registry.register(FakeFactory { id: "wreq" }).unwrap();
    let err = registry
        .build("wreq", &json!({ "bogus": 1 }), &SharedResources::default())
        .err()
        .unwrap();
    assert!(matches!(err, RegistryError::Build(_)));
    assert_eq!(
        err.to_string(),
        "invalid configuration for requester `wreq`: unknown key `bogus`"
    );
}
