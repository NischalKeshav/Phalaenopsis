//! Storage abstraction over the graph. TypeDB is the real backend; the memory
//! store exists for tests and for running without Docker.

pub mod memory;
pub mod typedb;

use crate::model::*;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait GraphStore: Send + Sync {
    fn kind(&self) -> &'static str;

    // Observations
    async fn insert_observation(&self, obs: &Observation) -> Result<()>;
    /// Overwrite title, fields, and seen_count of an existing observation.
    async fn update_observation(&self, obs: &Observation) -> Result<()>;
    async fn get_observation(&self, id: &str) -> Result<Option<Observation>>;
    async fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Observation>>;
    async fn observations_for_host(&self, host: &str) -> Result<Vec<Observation>>;
    async fn all_observations(&self) -> Result<Vec<Observation>>;

    // Scripts
    async fn insert_script(&self, script: &ToolScript) -> Result<()>;
    async fn get_script(&self, id: &str) -> Result<Option<ToolScript>>;
    async fn delete_script(&self, id: &str) -> Result<()>;
    async fn all_scripts(&self) -> Result<Vec<ToolScript>>;

    // Edges
    async fn link_applies(&self, observation_id: &str, script_id: &str) -> Result<AppliesEdge>;
    async fn applies_for(&self, observation_id: &str) -> Result<Vec<(ToolScript, AppliesEdge)>>;
    async fn all_applies(&self) -> Result<Vec<AppliesEdge>>;
    async fn record_run(&self, observation_id: &str, script_id: &str, ok: bool) -> Result<AppliesEdge>;
    async fn add_transition(&self, from: &str, to: &str, via: Option<&str>) -> Result<Transition>;
    async fn transitions_from(&self, observation_id: &str) -> Result<Vec<Transition>>;
    async fn all_transitions(&self) -> Result<Vec<Transition>>;

    // Context documents
    async fn insert_context(&self, doc: &ContextDoc) -> Result<()>;
    async fn get_context(&self, id: &str) -> Result<Option<ContextDoc>>;
    /// Docs scoped to `host` plus global ("*") docs, oldest first.
    async fn context_for(&self, host: &str) -> Result<Vec<ContextDoc>>;
    async fn all_context(&self) -> Result<Vec<ContextDoc>>;
    async fn delete_context(&self, id: &str) -> Result<()>;

    // Profile
    async fn profile_all(&self) -> Result<Profile>;
    async fn profile_set(&self, key: &str, value: &str) -> Result<()>;
    async fn profile_delete(&self, key: &str) -> Result<()>;

    async fn export(&self) -> Result<GraphExport> {
        Ok(GraphExport {
            observations: self.all_observations().await?,
            scripts: self.all_scripts().await?,
            applies: self.all_applies().await?,
            transitions: self.all_transitions().await?,
            profile_keys: self.profile_all().await?.keys().cloned().collect(),
            context_docs: self.all_context().await?.iter().map(ContextDoc::summary).collect(),
        })
    }
}
