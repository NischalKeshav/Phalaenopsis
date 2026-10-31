//! The memory controller decides what becomes a node, which node a page
//! snapshot belongs to, which edges to follow, and when to learn new ones.

use crate::fingerprint::{fingerprint, normalize_url, similarity};
use crate::llm::LlmClient;
use crate::model::*;
use crate::store::GraphStore;
use anyhow::{Result, anyhow};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub struct MemoryController {
    pub store: Arc<dyn GraphStore>,
    pub llm: Option<LlmClient>,
    pub similarity_threshold: f64,
    pub sessions: std::sync::Mutex<std::collections::HashMap<String, crate::agent::AgentSession>>,
    /// Background-model state per agent session (plan, notes, running job),
    /// shared with spawned tasks so results land while the page loop continues.
    pub background: crate::agent::BackgroundMap,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PreviousStep {
    pub observation_id: String,
    #[serde(default)]
    pub script_id: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ObserveRequest {
    #[serde(flatten)]
    pub page: PageSnapshot,
    /// Where the browser came from, so the controller can record a transition edge.
    #[serde(default)]
    pub previous: Option<PreviousStep>,
    /// Ask the model for a script if the node has none. Off by default so idle
    /// browsing never spends tokens.
    #[serde(default)]
    pub generate: bool,
    /// When false, only match existing nodes (used for pages with no form
    /// fields yet, so ordinary browsing does not grow the graph).
    #[serde(default = "yes")]
    pub create: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Debug, Clone)]
pub struct RankedScript {
    #[serde(flatten)]
    pub script: ToolScript,
    pub observation_id: String,
    pub confidence: f64,
    pub successes: i64,
    pub failures: i64,
}

impl RankedScript {
    pub fn from_edge(script: ToolScript, edge: &AppliesEdge) -> Self {
        Self { script, observation_id: edge.observation_id.clone(), confidence: edge.confidence(), successes: edge.successes, failures: edge.failures }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct ObserveResponse {
    pub observation: Observation,
    /// True when the page matched an existing node (exactly or by similarity).
    pub matched: bool,
    /// True when the match was by similarity rather than exact fingerprint.
    pub merged: bool,
    pub scripts: Vec<RankedScript>,
    pub next: Vec<Transition>,
    pub generated: bool,
    pub llm_available: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RunReport {
    pub observation_id: String,
    pub script_id: String,
    pub ok: bool,
    #[serde(default)]
    pub failed_step: Option<usize>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub resulting_page: Option<PageSnapshot>,
}

#[derive(Serialize, Debug, Clone)]
pub struct RunResponse {
    pub edge: AppliesEdge,
    pub confidence: f64,
    pub transition: Option<Transition>,
}

impl MemoryController {
    pub fn new(store: Arc<dyn GraphStore>, llm: Option<LlmClient>) -> Self {
        Self { store, llm, similarity_threshold: 0.8, sessions: Default::default(), background: Default::default() }
    }

    /// Resolve a page snapshot to a node, creating or merging as needed.
    /// Returns (observation, matched, merged).
    pub async fn resolve(&self, page: &PageSnapshot) -> Result<(Observation, bool, bool)> {
        self.resolve_with(page, true).await?.ok_or_else(|| anyhow!("unreachable: create=true always yields a node"))
    }

    pub async fn resolve_with(&self, page: &PageSnapshot, create: bool) -> Result<Option<(Observation, bool, bool)>> {
        let (host, url_pattern) = normalize_url(&page.url);
        let fp = fingerprint(&host, &url_pattern, &page.fields);

        if let Some(mut obs) = self.store.find_by_fingerprint(&fp).await? {
            obs.seen_count += 1;
            if !page.title.is_empty() {
                obs.title = page.title.clone();
            }
            obs.fields = page.fields.clone();
            self.store.update_observation(&obs).await?;
            return Ok(Some((obs, true, false)));
        }

        let candidates = self.store.observations_for_host(&host).await?;
        let best = candidates
            .into_iter()
            // Two empty field sets are trivially "identical"; pages without fields
            // only ever match by exact fingerprint (same URL pattern).
            .filter(|c| !c.fields.is_empty() && !page.fields.is_empty())
            .map(|c| {
                let mut score = similarity(&c.fields, &page.fields);
                if c.url_pattern != url_pattern {
                    score *= 0.9;
                }
                (score, c)
            })
            .filter(|(score, _)| *score >= self.similarity_threshold)
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((_, mut obs)) = best {
            obs.seen_count += 1;
            obs.fields = page.fields.clone();
            if !page.title.is_empty() {
                obs.title = page.title.clone();
            }
            self.store.update_observation(&obs).await?;
            return Ok(Some((obs, true, true)));
        }
        if !create {
            return Ok(None);
        }

        let obs = Observation {
            id: new_id(),
            host,
            url_pattern,
            fingerprint: fp,
            title: page.title.clone(),
            fields: page.fields.clone(),
            seen_count: 1,
            created_at: Utc::now(),
        };
        self.store.insert_observation(&obs).await?;
        Ok(Some((obs, false, false)))
    }

    /// Returns None only when `create` is false and nothing matched.
    pub async fn observe(&self, req: ObserveRequest) -> Result<Option<ObserveResponse>> {
        let Some((obs, matched, merged)) = self.resolve_with(&req.page, req.create).await? else {
            return Ok(None);
        };

        if let Some(prev) = &req.previous
            && prev.observation_id != obs.id
                && let Err(e) = self.store.add_transition(&prev.observation_id, &obs.id, prev.script_id.as_deref()).await {
                    tracing::warn!("transition not recorded: {e}");
                }

        let mut scripts = self.ranked_scripts(&obs.id).await?;
        let mut generated = false;
        if scripts.is_empty() && req.generate && self.llm.is_some() && !obs.fields.is_empty() {
            match self.generate_script(&obs).await {
                Ok(s) => {
                    scripts.push(s);
                    generated = true;
                }
                Err(e) => tracing::warn!("script generation failed: {e:#}"),
            }
        }
        let next = self.store.transitions_from(&obs.id).await?;
        Ok(Some(ObserveResponse { observation: obs, matched, merged, scripts, next, generated, llm_available: self.llm.is_some() }))
    }

    pub async fn ranked_scripts(&self, observation_id: &str) -> Result<Vec<RankedScript>> {
        let mut scripts: Vec<RankedScript> = self
            .store
            .applies_for(observation_id)
            .await?
            .into_iter()
            .map(|(s, e)| RankedScript::from_edge(s, &e))
            .collect();
        scripts.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.script.created_at.cmp(&a.script.created_at))
        });
        Ok(scripts)
    }

    /// Ask the model for a script for this node, store it, and link it.
    pub async fn generate_script(&self, obs: &Observation) -> Result<RankedScript> {
        let llm = self.llm.as_ref().ok_or_else(|| anyhow!("no OPENROUTER_KEY or OPENAI_KEY configured"))?;
        let keys: Vec<String> = self.store.profile_all().await?.keys().cloned().collect();
        let steps = llm.generate_steps(obs, &keys).await?;
        if steps.is_empty() {
            return Err(anyhow!("model produced no usable steps"));
        }
        let script = ToolScript {
            id: new_id(),
            name: format!("{} ({})", short_title(obs), llm.model),
            body: ScriptBody::Steps(steps),
            origin: Origin::Llm,
            created_at: Utc::now(),
        };
        self.add_script(&obs.id, script).await
    }

    pub async fn add_script(&self, observation_id: &str, script: ToolScript) -> Result<RankedScript> {
        self.store.insert_script(&script).await?;
        let edge = self.store.link_applies(observation_id, &script.id).await?;
        Ok(RankedScript::from_edge(script, &edge))
    }

    /// Turn a recorded DOM event log into a steps script and link it.
    pub async fn learn(&self, observation_id: &str, actions: &[RecordedAction]) -> Result<RankedScript> {
        let obs = self.store.get_observation(observation_id).await?.ok_or_else(|| anyhow!("observation {observation_id} not found"))?;
        let profile = self.store.profile_all().await?;
        let steps = actions_to_steps(&obs, actions, &profile);
        if steps.is_empty() {
            return Err(anyhow!("no actions worth learning"));
        }
        let script = ToolScript {
            id: new_id(),
            name: format!("Recorded on {}", short_title(&obs)),
            body: ScriptBody::Steps(steps),
            origin: Origin::Recorded,
            created_at: Utc::now(),
        };
        self.add_script(&obs.id, script).await
    }

    /// Save a "Learn from user" demonstration: the moves a person made across one
    /// or more pages, split by the page node they were on. Each page with
    /// learnable moves gets a recorded script; consecutive pages get a transition
    /// edge tagged with the script that led from one to the next.
    pub async fn learn_session(&self, segments: &[LearnSegment]) -> Result<LearnSessionResponse> {
        // Moves on the same node in a row are one visit, even if the extension split them.
        let mut visits: Vec<(String, Vec<RecordedAction>)> = Vec::new();
        for seg in segments {
            if seg.observation_id.trim().is_empty() {
                continue;
            }
            match visits.last_mut() {
                Some((id, actions)) if *id == seg.observation_id => actions.extend(seg.actions.iter().cloned()),
                _ => visits.push((seg.observation_id.clone(), seg.actions.clone())),
            }
        }
        let profile = self.store.profile_all().await?;
        let mut out = LearnSessionResponse::default();
        // (observation, script learned there) for each visit that resolved to a node.
        let mut path: Vec<(String, Option<String>)> = Vec::new();
        for (observation_id, actions) in &visits {
            let Some(obs) = self.store.get_observation(observation_id).await? else {
                out.skipped.push(format!("page {observation_id} is not in the graph"));
                continue;
            };
            let steps = actions_to_steps(&obs, actions, &profile);
            let script_id = if steps.is_empty() {
                None
            } else {
                let script = ToolScript {
                    id: new_id(),
                    name: format!("Learned from you on {}", short_title(&obs)),
                    body: ScriptBody::Steps(steps),
                    origin: Origin::Recorded,
                    created_at: Utc::now(),
                };
                let ranked = self.add_script(&obs.id, script).await?;
                let id = ranked.script.id.clone();
                out.scripts.push(ranked);
                Some(id)
            };
            path.push((obs.id.clone(), script_id));
        }
        for pair in path.windows(2) {
            let ((from, via), (to, _)) = (&pair[0], &pair[1]);
            if from != to {
                out.transitions.push(self.store.add_transition(from, to, via.as_deref()).await?);
            }
        }
        out.pages = path.len();
        if out.scripts.is_empty() && out.transitions.is_empty() {
            return Err(anyhow!("no actions worth learning"));
        }
        Ok(out)
    }

    pub async fn report_run(&self, report: RunReport) -> Result<RunResponse> {
        if !report.ok {
            tracing::info!(
                "script {} failed on {} at step {:?}: {}",
                report.script_id, report.observation_id, report.failed_step, report.error.as_deref().unwrap_or("")
            );
        }
        let edge = self.store.record_run(&report.observation_id, &report.script_id, report.ok).await?;
        let mut transition = None;
        if let Some(page) = &report.resulting_page {
            let (after, _, _) = self.resolve(page).await?;
            if after.id != report.observation_id {
                transition = Some(self.store.add_transition(&report.observation_id, &after.id, Some(&report.script_id)).await?);
            }
        }
        Ok(RunResponse { confidence: edge.confidence(), edge, transition })
    }
}

/// Moves recorded while the browser was on one page node.
#[derive(Deserialize, Debug, Clone)]
pub struct LearnSegment {
    pub observation_id: String,
    #[serde(default)]
    pub actions: Vec<RecordedAction>,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct LearnSessionResponse {
    /// One recorded script per page where the person did something learnable.
    pub scripts: Vec<RankedScript>,
    /// Page-to-page edges the demonstration walked.
    pub transitions: Vec<Transition>,
    /// Distinct page visits that made it into the graph.
    pub pages: usize,
    /// Why some recorded moves were dropped.
    pub skipped: Vec<String>,
}

fn short_title(obs: &Observation) -> String {
    if obs.title.trim().is_empty() {
        format!("{}{}", obs.host, obs.url_pattern)
    } else {
        obs.title.trim().chars().take(60).collect()
    }
}

/// Collapse a raw event log into steps. Later events on the same selector win,
/// clicks on fields that then get filled are dropped, and literal values equal
/// to a profile value are replaced with a profile reference.
pub fn actions_to_steps(obs: &Observation, actions: &[RecordedAction], profile: &Profile) -> Vec<Step> {
    let field_of = |selector: &str| obs.fields.iter().find(|f| f.selector == selector);
    let mut steps: Vec<Step> = Vec::new();
    for a in actions {
        let selector = a.selector.trim().to_string();
        if selector.is_empty() {
            continue;
        }
        let field = field_of(&selector);
        let tag = field.map(|f| f.tag.as_str()).unwrap_or("");
        let input_type = field.map(|f| f.input_type.as_str()).unwrap_or("");
        let step = match a.op.as_str() {
            "input" | "change" => {
                let value = a.value.clone().unwrap_or_default();
                if tag == "select" {
                    Step::Select { selector: selector.clone(), value: generalize(&value, profile) }
                } else if matches!(input_type, "checkbox" | "radio") {
                    Step::Check { selector: selector.clone(), value: generalize(&value, profile) }
                } else {
                    Step::Fill { selector: selector.clone(), value: generalize(&value, profile) }
                }
            }
            "click" => {
                if field.is_some() && !matches!(input_type, "checkbox" | "radio" | "submit" | "button") {
                    continue; // focusing a text field, not an action
                }
                if matches!(input_type, "checkbox" | "radio") {
                    continue; // the change event carries the state
                }
                Step::Click { selector: selector.clone() }
            }
            _ => continue,
        };
        steps.retain(|s| step_selector(s) != Some(selector.as_str()) || matches!(s, Step::Click { .. }) != matches!(step, Step::Click { .. }));
        steps.push(step);
    }
    steps
}

pub fn step_selector(step: &Step) -> Option<&str> {
    match step {
        Step::Fill { selector, .. }
        | Step::Select { selector, .. }
        | Step::Check { selector, .. }
        | Step::Click { selector }
        | Step::WaitFor { selector, .. }
        | Step::Submit { selector } => Some(selector),
        Step::Navigate { .. } => None,
    }
}

pub fn generalize(value: &str, profile: &Profile) -> ValueRef {
    let needle = value.trim().to_lowercase();
    if !needle.is_empty()
        && let Some((key, _)) = profile.iter().find(|(_, v)| v.trim().to_lowercase() == needle) {
            return ValueRef::Profile { profile: key.clone() };
        }
    ValueRef::Literal { literal: value.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;

    fn field(selector: &str, tag: &str, input_type: &str, name: &str) -> FieldSig {
        FieldSig {
            selector: selector.into(),
            tag: tag.into(),
            input_type: input_type.into(),
            name: name.into(),
            id: String::new(),
            label: name.into(),
            placeholder: String::new(),
            autocomplete: String::new(),
        }
    }

    fn page(url: &str, fields: Vec<FieldSig>) -> PageSnapshot {
        PageSnapshot { url: url.into(), title: "Test".into(), fields }
    }

    fn controller() -> MemoryController {
        MemoryController::new(Arc::new(MemoryStore::new()), None)
    }

    fn act(op: &str, selector: &str, value: Option<&str>) -> RecordedAction {
        RecordedAction { op: op.into(), selector: selector.into(), value: value.map(str::to_string), ts: 0.0 }
    }

    #[tokio::test]
    async fn exact_revisit_matches_same_node() {
        let c = controller();
        let p = page("https://a.com/form?x=1", vec![field("#f", "input", "text", "first")]);
        let r1 = c.observe(ObserveRequest { page: p.clone(), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let r2 = c.observe(ObserveRequest { page: p.clone(), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        assert!(!r1.matched);
        assert!(r2.matched && !r2.merged);
        assert_eq!(r1.observation.id, r2.observation.id);
        assert_eq!(r2.observation.seen_count, 2);
    }

    #[tokio::test]
    async fn similar_page_merges_but_different_page_does_not() {
        let c = controller();
        let base: Vec<FieldSig> = (0..10).map(|i| field(&format!("#f{i}"), "input", "text", &format!("n{i}"))).collect();
        let r1 = c.observe(ObserveRequest { page: page("https://a.com/form", base.clone()), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let mut one_extra = base.clone();
        one_extra.push(field("#extra", "input", "text", "extra"));
        let r2 = c.observe(ObserveRequest { page: page("https://a.com/form", one_extra), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        assert!(r2.matched && r2.merged);
        assert_eq!(r1.observation.id, r2.observation.id);
        let other = vec![field("#q", "input", "text", "q")];
        let r3 = c.observe(ObserveRequest { page: page("https://a.com/form", other), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        assert!(!r3.matched);
        assert_ne!(r3.observation.id, r1.observation.id);
    }

    #[tokio::test]
    async fn fieldless_pages_on_different_urls_stay_separate() {
        let c = controller();
        let a = c.observe(ObserveRequest { page: page("https://a.com/coursework", vec![]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let b = c.observe(ObserveRequest { page: page("https://a.com/coursework_mit", vec![]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        assert!(!b.matched);
        assert_ne!(a.observation.id, b.observation.id);
    }

    #[tokio::test]
    async fn lookup_only_never_creates() {
        let c = controller();
        let empty = page("https://a.com/coursework", vec![]);
        let none = c.observe(ObserveRequest { page: empty.clone(), previous: None, generate: false, create: false }).await.unwrap();
        assert!(none.is_none());
        assert!(c.store.all_observations().await.unwrap().is_empty());
        let made = c.observe(ObserveRequest { page: empty.clone(), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let found = c.observe(ObserveRequest { page: empty, previous: None, generate: false, create: false }).await.unwrap().unwrap();
        assert_eq!(found.observation.id, made.observation.id);
    }

    #[tokio::test]
    async fn previous_step_records_transition() {
        let c = controller();
        let r1 = c.observe(ObserveRequest { page: page("https://a.com/1", vec![field("#a", "input", "text", "a")]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let r2 = c
            .observe(ObserveRequest {
                page: page("https://a.com/2", vec![field("#b", "input", "text", "b")]),
                previous: Some(PreviousStep { observation_id: r1.observation.id.clone(), script_id: None }),
                generate: false,
                create: true,
            })
            .await
            .unwrap().unwrap();
        let next = c.store.transitions_from(&r1.observation.id).await.unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].to, r2.observation.id);
        assert_eq!(next[0].hits, 1);
    }

    #[tokio::test]
    async fn runs_update_confidence_and_ranking() {
        let c = controller();
        let r = c.observe(ObserveRequest { page: page("https://a.com/1", vec![field("#a", "input", "text", "a")]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let mk = |name: &str| ToolScript { id: new_id(), name: name.into(), body: ScriptBody::Js("1".into()), origin: Origin::Manual, created_at: Utc::now() };
        let good = c.add_script(&r.observation.id, mk("good")).await.unwrap();
        let bad = c.add_script(&r.observation.id, mk("bad")).await.unwrap();
        assert_eq!(good.confidence, 0.5);
        for _ in 0..3 {
            c.report_run(RunReport { observation_id: r.observation.id.clone(), script_id: good.script.id.clone(), ok: true, failed_step: None, error: None, resulting_page: None }).await.unwrap();
        }
        let res = c.report_run(RunReport { observation_id: r.observation.id.clone(), script_id: bad.script.id.clone(), ok: false, failed_step: Some(0), error: None, resulting_page: None }).await.unwrap();
        assert!(res.confidence < 0.5);
        let ranked = c.ranked_scripts(&r.observation.id).await.unwrap();
        assert_eq!(ranked[0].script.id, good.script.id);
        assert!((ranked[0].confidence - 0.8).abs() < 1e-9);
    }

    #[tokio::test]
    async fn learn_generalizes_profile_values_and_collapses_events() {
        let c = controller();
        c.store.profile_set("firstName", "Ada").await.unwrap();
        let fields = vec![
            field("#first", "input", "text", "first"),
            field("#country", "select", "", "country"),
            field("#agree", "input", "checkbox", "agree"),
        ];
        let r = c.observe(ObserveRequest { page: page("https://a.com/1", fields), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let actions = vec![
            act("click", "#first", None),
            act("input", "#first", Some("A")),
            act("input", "#first", Some("Ad")),
            act("input", "#first", Some("ada")),
            act("change", "#country", Some("US")),
            act("click", "#agree", None),
            act("change", "#agree", Some("true")),
            act("click", "#next", None),
        ];
        let learned = c.learn(&r.observation.id, &actions).await.unwrap();
        assert_eq!(learned.script.origin, Origin::Recorded);
        let ScriptBody::Steps(steps) = &learned.script.body else { panic!("expected steps") };
        assert_eq!(
            steps,
            &vec![
                Step::Fill { selector: "#first".into(), value: ValueRef::Profile { profile: "firstName".into() } },
                Step::Select { selector: "#country".into(), value: ValueRef::Literal { literal: "US".into() } },
                Step::Check { selector: "#agree".into(), value: ValueRef::Literal { literal: "true".into() } },
                Step::Click { selector: "#next".into() },
            ]
        );
    }

    #[tokio::test]
    async fn learn_session_saves_a_script_per_page_and_links_them() {
        let c = controller();
        let list = c.observe(ObserveRequest { page: page("https://a.com/list", vec![]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let form = c
            .observe(ObserveRequest { page: page("https://a.com/form", vec![field("#name", "input", "text", "name")]), previous: None, generate: false, create: true })
            .await
            .unwrap()
            .unwrap();
        let (list_id, form_id) = (list.observation.id.clone(), form.observation.id.clone());
        let seg = |id: &str, actions: Vec<RecordedAction>| LearnSegment { observation_id: id.into(), actions };
        let out = c
            .learn_session(&[
                seg(&list_id, vec![act("click", "#add", None)]),
                // Split by the extension mid-visit; must merge into one script.
                seg(&form_id, vec![act("input", "#name", Some("Calc"))]),
                seg(&form_id, vec![act("click", "#save", None)]),
                seg("", vec![act("click", "#lost", None)]),
            ])
            .await
            .unwrap();
        assert_eq!(out.pages, 2);
        assert_eq!(out.scripts.len(), 2);
        assert!(out.scripts.iter().all(|s| s.script.origin == Origin::Recorded));
        let ScriptBody::Steps(form_steps) = &out.scripts[1].script.body else { panic!("expected steps") };
        assert_eq!(form_steps.len(), 2);
        assert_eq!(out.transitions.len(), 1);
        let t = &out.transitions[0];
        assert_eq!((t.from.as_str(), t.to.as_str()), (list_id.as_str(), form_id.as_str()));
        assert_eq!(t.via.as_deref(), Some(out.scripts[0].script.id.as_str()));
        assert_eq!(c.ranked_scripts(&form_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn learn_session_reports_unknown_pages_and_rejects_empty() {
        let c = controller();
        let err = c.learn_session(&[LearnSegment { observation_id: "nope".into(), actions: vec![act("click", "#x", None)] }]).await.unwrap_err();
        assert!(err.to_string().contains("no actions worth learning"));
        let r = c.observe(ObserveRequest { page: page("https://a.com/1", vec![field("#a", "input", "text", "a")]), previous: None, generate: false, create: true }).await.unwrap().unwrap();
        let out = c
            .learn_session(&[
                LearnSegment { observation_id: "nope".into(), actions: vec![act("click", "#x", None)] },
                LearnSegment { observation_id: r.observation.id.clone(), actions: vec![act("input", "#a", Some("hi"))] },
            ])
            .await
            .unwrap();
        assert_eq!(out.scripts.len(), 1);
        assert_eq!(out.skipped.len(), 1);
    }
}
