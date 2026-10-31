//! Core graph types: observations (nodes), tool scripts (nodes), and the edges
//! that connect them. Everything here is plain data shared by the store, the
//! controller, the HTTP API, and (as JSON) the Chrome extension.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Canonical signature of one form field on a page. The extension computes
/// these in the DOM; the backend treats them as opaque identity for matching.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldSig {
    pub selector: String,
    pub tag: String,
    #[serde(default)]
    pub input_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub autocomplete: String,
}

impl FieldSig {
    /// Identity used for fingerprinting and similarity. Excludes the selector,
    /// which can drift (nth-of-type paths) while the field is semantically the same.
    pub fn key(&self) -> String {
        fn norm(s: &str) -> String {
            s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
        }
        format!(
            "{}|{}|{}|{}|{}|{}",
            norm(&self.tag),
            norm(&self.input_type),
            norm(&self.name),
            norm(&self.id),
            norm(&self.label),
            norm(&self.autocomplete)
        )
    }
}

/// A node: one observed page shape (host + normalized URL + set of fields).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Observation {
    pub id: String,
    pub host: String,
    pub url_pattern: String,
    pub fingerprint: String,
    pub title: String,
    pub fields: Vec<FieldSig>,
    pub seen_count: i64,
    pub created_at: DateTime<Utc>,
}

/// Where a value comes from when a step runs. Profile references keep user
/// data out of scripts (and out of the LLM prompt).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum ValueRef {
    Profile { profile: String },
    Literal { literal: String },
}

/// One declarative action the extension knows how to execute.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Step {
    Fill { selector: String, value: ValueRef },
    Select { selector: String, value: ValueRef },
    /// Set a checkbox/radio. The value resolves to a boolean: literal "true"/"false",
    /// or a profile key whose value is truthy ("true", "yes", "1", "on").
    Check { selector: String, value: ValueRef },
    Click { selector: String },
    WaitFor {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Submit { selector: String },
    Navigate { url: String },
}

/// A tool script is either a list of steps or a raw JS snippet run in the page.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", content = "body", rename_all = "lowercase")]
pub enum ScriptBody {
    Steps(Vec<Step>),
    Js(String),
}

impl ScriptBody {
    pub fn kind(&self) -> &'static str {
        match self {
            ScriptBody::Steps(_) => "steps",
            ScriptBody::Js(_) => "js",
        }
    }
    /// Serialized body for storage (steps as JSON array, js as the raw string).
    pub fn body_string(&self) -> String {
        match self {
            ScriptBody::Steps(steps) => serde_json::to_string(steps).unwrap_or_else(|_| "[]".into()),
            ScriptBody::Js(code) => code.clone(),
        }
    }
    pub fn from_parts(kind: &str, body: &str) -> anyhow::Result<Self> {
        match kind {
            "steps" => {
                let mut raw: serde_json::Value = serde_json::from_str(body)?;
                upgrade_legacy_steps(&mut raw);
                Ok(ScriptBody::Steps(serde_json::from_value(raw)?))
            }
            "js" => Ok(ScriptBody::Js(body.to_string())),
            other => anyhow::bail!("unknown script kind {other:?}"),
        }
    }
}

/// Older scripts stored `{"op":"check","checked":bool}`; rewrite to the value form.
fn upgrade_legacy_steps(raw: &mut serde_json::Value) {
    if let Some(steps) = raw.as_array_mut() {
        for step in steps {
            let Some(obj) = step.as_object_mut() else { continue };
            if obj.get("op").and_then(|v| v.as_str()) == Some("check") && !obj.contains_key("value") {
                let checked = obj.remove("checked").and_then(|v| v.as_bool()).unwrap_or(true);
                obj.insert("value".into(), serde_json::json!({ "literal": checked.to_string() }));
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Llm,
    Recorded,
    Manual,
    /// Trace of an interactive agent session driven by a user prompt.
    Agent,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Llm => "llm",
            Origin::Recorded => "recorded",
            Origin::Manual => "manual",
            Origin::Agent => "agent",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "llm" => Origin::Llm,
            "recorded" => Origin::Recorded,
            "agent" => Origin::Agent,
            _ => Origin::Manual,
        }
    }
}

/// A document the user uploaded as context (transcript, resume, notes).
/// `scope` is a host name or "*" for every site. `text` is the extracted text.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ContextDoc {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub scope: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

impl ContextDoc {
    /// Listing form: full text replaced by a short preview.
    pub fn summary(&self) -> serde_json::Value {
        let preview: String = self.text.chars().take(200).collect();
        serde_json::json!({
            "id": self.id, "name": self.name, "mime": self.mime, "scope": self.scope,
            "chars": self.text.chars().count(), "preview": preview, "created_at": self.created_at,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct SelectOption {
    pub value: String,
    pub text: String,
}

/// A field as the agent sees it: signature plus current value and options.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentField {
    #[serde(flatten)]
    pub sig: FieldSig,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub options: Vec<SelectOption>,
    #[serde(default)]
    pub required: bool,
    /// The page marks this field as having an error.
    #[serde(default)]
    pub invalid: bool,
    /// A select still showing the option the page preselected; nobody chose it.
    #[serde(default)]
    pub page_default: bool,
}

/// A clickable element (button, link) the agent may use.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ActionSig {
    pub selector: String,
    pub tag: String,
    #[serde(default)]
    pub text: String,
}

/// Richer page view used only by the interactive agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentPage {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub fields: Vec<AgentField>,
    #[serde(default)]
    pub actions: Vec<ActionSig>,
    #[serde(default)]
    pub text: String,
    /// Lines of visible text that appeared since the previous snapshot
    /// (validation popups, "saved" banners).
    #[serde(default)]
    pub new_text: Vec<String>,
    /// Fields the person changed themselves since the previous snapshot.
    #[serde(default)]
    pub user_edits: Vec<UserEdit>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserEdit {
    pub selector: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: String,
}

impl AgentPage {
    pub fn snapshot(&self) -> PageSnapshot {
        PageSnapshot { url: self.url.clone(), title: self.title.clone(), fields: self.fields.iter().map(|f| f.sig.clone()).collect() }
    }
}

/// A node: something the agent can do on a page.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ToolScript {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub body: ScriptBody,
    pub origin: Origin,
    pub created_at: DateTime<Utc>,
}

/// Edge Observation -> ToolScript with outcome counters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppliesEdge {
    pub observation_id: String,
    pub script_id: String,
    pub successes: i64,
    pub failures: i64,
    pub last_run: DateTime<Utc>,
}

impl AppliesEdge {
    /// Laplace-smoothed success rate: an untried script starts at 0.5.
    pub fn confidence(&self) -> f64 {
        (self.successes as f64 + 1.0) / (self.successes as f64 + self.failures as f64 + 2.0)
    }
}

/// Edge Observation -> Observation: running `via` on `from` led to `to`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub via: Option<String>,
    pub hits: i64,
}

/// One DOM event captured by the extension in record mode.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RecordedAction {
    pub op: String,
    pub selector: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub ts: f64,
}

/// Snapshot of a page as the extension sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PageSnapshot {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub fields: Vec<FieldSig>,
}

/// Whole-graph dump for debugging and visualization.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GraphExport {
    pub observations: Vec<Observation>,
    pub scripts: Vec<ToolScript>,
    pub applies: Vec<AppliesEdge>,
    pub transitions: Vec<Transition>,
    pub profile_keys: Vec<String>,
    #[serde(default)]
    pub context_docs: Vec<serde_json::Value>,
}

pub type Profile = BTreeMap<String, String>;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_check_steps_upgrade() {
        let body = ScriptBody::from_parts("steps", r##"[{"op":"check","selector":"#a","checked":false},{"op":"click","selector":"#b"}]"##).unwrap();
        assert_eq!(
            body,
            ScriptBody::Steps(vec![
                Step::Check { selector: "#a".into(), value: ValueRef::Literal { literal: "false".into() } },
                Step::Click { selector: "#b".into() },
            ])
        );
    }

    #[test]
    fn unknown_kind_is_an_error() {
        assert!(ScriptBody::from_parts("python", "print(1)").is_err());
    }
}
