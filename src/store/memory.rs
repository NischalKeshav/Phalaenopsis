//! In-process graph store. Not persistent; used by tests and `PHALAENOPSIS_STORE=memory`.

use super::GraphStore;
use crate::model::*;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

#[derive(Default)]
struct Inner {
    observations: HashMap<String, Observation>,
    scripts: HashMap<String, ToolScript>,
    applies: HashMap<(String, String), AppliesEdge>,
    transitions: HashMap<(String, String, Option<String>), Transition>,
    profile: BTreeMap<String, String>,
    context: HashMap<String, ContextDoc>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: RwLock<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GraphStore for MemoryStore {
    fn kind(&self) -> &'static str {
        "memory"
    }

    async fn insert_observation(&self, obs: &Observation) -> Result<()> {
        self.inner.write().unwrap().observations.insert(obs.id.clone(), obs.clone());
        Ok(())
    }

    async fn update_observation(&self, obs: &Observation) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        let existing = g.observations.get_mut(&obs.id).ok_or_else(|| anyhow!("observation {} not found", obs.id))?;
        existing.title = obs.title.clone();
        existing.fields = obs.fields.clone();
        existing.seen_count = obs.seen_count;
        Ok(())
    }

    async fn get_observation(&self, id: &str) -> Result<Option<Observation>> {
        Ok(self.inner.read().unwrap().observations.get(id).cloned())
    }

    async fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Observation>> {
        Ok(self.inner.read().unwrap().observations.values().find(|o| o.fingerprint == fingerprint).cloned())
    }

    async fn observations_for_host(&self, host: &str) -> Result<Vec<Observation>> {
        Ok(self.inner.read().unwrap().observations.values().filter(|o| o.host == host).cloned().collect())
    }

    async fn all_observations(&self) -> Result<Vec<Observation>> {
        let mut v: Vec<_> = self.inner.read().unwrap().observations.values().cloned().collect();
        v.sort_by_key(|a| a.created_at);
        Ok(v)
    }

    async fn insert_script(&self, script: &ToolScript) -> Result<()> {
        self.inner.write().unwrap().scripts.insert(script.id.clone(), script.clone());
        Ok(())
    }

    async fn get_script(&self, id: &str) -> Result<Option<ToolScript>> {
        Ok(self.inner.read().unwrap().scripts.get(id).cloned())
    }

    async fn delete_script(&self, id: &str) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        g.scripts.remove(id);
        g.applies.retain(|(_, s), _| s != id);
        Ok(())
    }

    async fn all_scripts(&self) -> Result<Vec<ToolScript>> {
        let mut v: Vec<_> = self.inner.read().unwrap().scripts.values().cloned().collect();
        v.sort_by_key(|a| a.created_at);
        Ok(v)
    }

    async fn link_applies(&self, observation_id: &str, script_id: &str) -> Result<AppliesEdge> {
        let mut g = self.inner.write().unwrap();
        if !g.observations.contains_key(observation_id) {
            return Err(anyhow!("observation {observation_id} not found"));
        }
        if !g.scripts.contains_key(script_id) {
            return Err(anyhow!("script {script_id} not found"));
        }
        let key = (observation_id.to_string(), script_id.to_string());
        let edge = g.applies.entry(key).or_insert_with(|| AppliesEdge {
            observation_id: observation_id.into(),
            script_id: script_id.into(),
            successes: 0,
            failures: 0,
            last_run: Utc::now(),
        });
        Ok(edge.clone())
    }

    async fn applies_for(&self, observation_id: &str) -> Result<Vec<(ToolScript, AppliesEdge)>> {
        let g = self.inner.read().unwrap();
        Ok(g
            .applies
            .values()
            .filter(|e| e.observation_id == observation_id)
            .filter_map(|e| g.scripts.get(&e.script_id).map(|s| (s.clone(), e.clone())))
            .collect())
    }

    async fn all_applies(&self) -> Result<Vec<AppliesEdge>> {
        Ok(self.inner.read().unwrap().applies.values().cloned().collect())
    }

    async fn record_run(&self, observation_id: &str, script_id: &str, ok: bool) -> Result<AppliesEdge> {
        let mut g = self.inner.write().unwrap();
        let key = (observation_id.to_string(), script_id.to_string());
        let edge = g.applies.get_mut(&key).ok_or_else(|| anyhow!("no applies edge {observation_id} -> {script_id}"))?;
        if ok {
            edge.successes += 1;
        } else {
            edge.failures += 1;
        }
        edge.last_run = Utc::now();
        Ok(edge.clone())
    }

    async fn add_transition(&self, from: &str, to: &str, via: Option<&str>) -> Result<Transition> {
        let mut g = self.inner.write().unwrap();
        let key = (from.to_string(), to.to_string(), via.map(str::to_string));
        let t = g.transitions.entry(key).or_insert_with(|| Transition {
            from: from.into(),
            to: to.into(),
            via: via.map(str::to_string),
            hits: 0,
        });
        t.hits += 1;
        Ok(t.clone())
    }

    async fn transitions_from(&self, observation_id: &str) -> Result<Vec<Transition>> {
        Ok(self.inner.read().unwrap().transitions.values().filter(|t| t.from == observation_id).cloned().collect())
    }

    async fn all_transitions(&self) -> Result<Vec<Transition>> {
        Ok(self.inner.read().unwrap().transitions.values().cloned().collect())
    }

    async fn insert_context(&self, doc: &ContextDoc) -> Result<()> {
        self.inner.write().unwrap().context.insert(doc.id.clone(), doc.clone());
        Ok(())
    }

    async fn get_context(&self, id: &str) -> Result<Option<ContextDoc>> {
        Ok(self.inner.read().unwrap().context.get(id).cloned())
    }

    async fn context_for(&self, host: &str) -> Result<Vec<ContextDoc>> {
        let mut v: Vec<_> = self.inner.read().unwrap().context.values().filter(|d| d.scope == "*" || d.scope == host).cloned().collect();
        v.sort_by_key(|d| d.created_at);
        Ok(v)
    }

    async fn all_context(&self) -> Result<Vec<ContextDoc>> {
        let mut v: Vec<_> = self.inner.read().unwrap().context.values().cloned().collect();
        v.sort_by_key(|d| d.created_at);
        Ok(v)
    }

    async fn delete_context(&self, id: &str) -> Result<()> {
        self.inner.write().unwrap().context.remove(id);
        Ok(())
    }

    async fn profile_all(&self) -> Result<Profile> {
        Ok(self.inner.read().unwrap().profile.clone())
    }

    async fn profile_set(&self, key: &str, value: &str) -> Result<()> {
        self.inner.write().unwrap().profile.insert(key.into(), value.into());
        Ok(())
    }

    async fn profile_delete(&self, key: &str) -> Result<()> {
        self.inner.write().unwrap().profile.remove(key);
        Ok(())
    }
}
