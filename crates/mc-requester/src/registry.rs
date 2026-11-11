use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;

use crate::{BuildError, Capabilities, Requester, RequesterFactory, SharedResources};

/// The requesters a binary was built with, keyed by id.
#[derive(Default)]
pub struct RequesterRegistry {
    factories: BTreeMap<&'static str, Box<dyn RequesterFactory>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RequesterInfo {
    pub id: &'static str,
    pub capabilities: Capabilities,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("a requester with id `{0}` is already registered")]
    Duplicate(&'static str),
    #[error("unknown requester `{id}` (available: {available})")]
    Unknown { id: String, available: String },
    #[error(transparent)]
    Build(#[from] BuildError),
}

impl RequesterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, factory: impl RequesterFactory) -> Result<(), RegistryError> {
        let id = factory.id();
        if self.factories.contains_key(id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.factories.insert(id, Box::new(factory));
        Ok(())
    }

    /// Registered requesters, sorted by id.
    pub fn list(&self) -> Vec<RequesterInfo> {
        self.factories
            .values()
            .map(|factory| RequesterInfo {
                id: factory.id(),
                capabilities: factory.capabilities(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }

    pub fn build(
        &self,
        id: &str,
        config: &serde_json::Value,
        shared: &SharedResources,
    ) -> Result<Arc<dyn Requester>, RegistryError> {
        let factory = self
            .factories
            .get(id)
            .ok_or_else(|| RegistryError::Unknown {
                id: id.to_owned(),
                available: self.available(),
            })?;
        Ok(factory.build(config, shared)?)
    }

    fn available(&self) -> String {
        if self.factories.is_empty() {
            return "none".to_owned();
        }
        self.factories
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    }
}
