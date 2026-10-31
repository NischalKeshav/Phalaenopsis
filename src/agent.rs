//! Interactive agent, structured after "interaction models" (Thinking Machines,
//! 2026) with the *website* as the other party instead of a human:
//!
//! * Interaction model (fast, small context) stays present on the page in
//!   micro-turns: act, perceive what the page did, react.
//! * The page can interject: the executor perceives after every action and
//!   interrupts the batch on surprises (error text, flagged fields, navigation),
//!   returning timestamped environment events.
//! * Background model (stronger, slower) runs asynchronously: it reads the
//!   documents once and produces a plan of items, and answers delegated
//!   questions. Its results are merged into the loop when they arrive.
//!
//! Successful actions are saved into the graph as replayable scripts per page,
//! so the next visit needs no model at all.

use crate::controller::{MemoryController, RankedScript, generalize};
use crate::fingerprint::{normalize_url, site_of};
use crate::llm::Usage;
use crate::model::*;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};

pub const MAX_TURNS: usize = 80;
/// Same actions on an unchanged page this many turns in a row = stuck.
const REPEAT_WARN: usize = 2;
const REPEAT_STOP: usize = 4;
const MAX_CONTEXT_CHARS: usize = 60_000;
const MAX_PAGE_TEXT: usize = 5_000;
const MAX_OPTIONS: usize = 60;
const HISTORY_TURNS: usize = 4;
/// How long a micro-turn waits for the background model before replying anyway.
const BACKGROUND_WAIT_SECS: u64 = 45;
/// At most one delegated question per this many micro-turns (unless stuck).
const DELEGATE_EVERY: usize = 4;
/// Consecutive "working" micro-turns with no actions before giving up and asking the user.
const MAX_IDLE_TURNS: usize = 3;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StepResult {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Turn {
    pub url_key: String,
    pub observation_id: String,
    pub message: String,
    pub actions: Vec<Step>,
    pub results: Vec<StepResult>,
    pub rejected: Vec<String>,
    /// What the page did while this turn's actions ran (filled in next turn).
    #[serde(default)]
    pub events: Vec<EnvEvent>,
    #[serde(default)]
    pub interrupted: Option<String>,
    /// Hash of the page the agent saw this turn (fields, values, text).
    #[serde(skip)]
    pub page_sig: u64,
}

fn sig_of<T: std::hash::Hash>(t: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

fn page_sig(page: &AgentPage) -> u64 {
    let fields: Vec<_> = page.fields.iter().map(|f| (&f.sig.selector, &f.value)).collect();
    sig_of(&(fields, &page.text))
}

fn actions_sig(actions: &[Step]) -> u64 {
    sig_of(&serde_json::to_string(actions).unwrap_or_default())
}

/// How many of the latest turns ran the same actions on the same page as now.
fn repeat_count(turns: &[Turn], current_page: u64) -> usize {
    let Some(last) = turns.last() else { return 0 };
    if last.actions.is_empty() || last.page_sig != current_page {
        return 0;
    }
    let sig = actions_sig(&last.actions);
    turns.iter().rev().take_while(|t| actions_sig(&t.actions) == sig && t.page_sig == current_page).count()
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentSession {
    pub id: String,
    pub prompt: String,
    pub host: String,
    pub turns: Vec<Turn>,
    /// url_key (host + url pattern) -> observation id, so one page keeps one node
    /// while rows are added and fields appear.
    pub pages: BTreeMap<String, String>,
    pub finished: bool,
    pub saved: Vec<RankedScript>,
    /// Turns before this index were already written to the graph.
    pub saved_turns: usize,
    pub created_at: DateTime<Utc>,
    pub last_turn_at: DateTime<Utc>,
    /// Background notes already shown to the interaction model.
    pub notes_seen: usize,
    /// Every selector seen on any page of this session. Lets one batch cross a
    /// dialog boundary (Save, Add New, fill the next row): the executor waits
    /// for elements to appear and the page interrupts on errors.
    #[serde(skip)]
    pub known_selectors: HashSet<String>,
    /// Micro-turn of the last delegated question (throttles background consults).
    pub last_delegate_turn: Option<usize>,
    /// Consecutive micro-turns where the model said "working" but did nothing.
    pub idle_turns: usize,
    /// Fields the person edited during this session (selector -> latest edit).
    pub user_edits: BTreeMap<String, UserEdit>,
    /// Micro-turn when a question for the user was last checked against the documents.
    pub last_user_question_check: Option<usize>,
}

/// Something the page did while actions ran, as perceived by the executor.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct EnvEvent {
    /// Milliseconds since the batch started.
    #[serde(default)]
    pub t_ms: f64,
    /// text | dialog_opened | dialog_closed | field_error | navigated | step_failed
    pub kind: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Deserialize, Debug)]
pub struct AgentStepRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    pub page: AgentPage,
    /// Outcome of each action returned by the previous turn, in order.
    #[serde(default)]
    pub results: Vec<StepResult>,
    /// Environment events perceived while the previous actions ran.
    #[serde(default)]
    pub events: Vec<EnvEvent>,
    /// Set when the page interrupted the previous batch (why).
    #[serde(default)]
    pub interrupted: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct AgentStepResponse {
    pub session_id: String,
    pub observation_id: String,
    pub turn: usize,
    pub message: String,
    pub actions: Vec<Step>,
    pub done: bool,
    pub saved: Vec<RankedScript>,
    /// Current plan from the background model, with item statuses.
    pub plan: Option<Plan>,
    /// What the background model is doing right now, if anything.
    pub background: Option<String>,
    /// Tokens and cost so far in this session, per role.
    pub usage: UsageSplit,
}

// ---------- background model state ----------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Kv {
    pub field: String,
    pub value: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    pub values: Vec<Kv>,
    /// pending | doing | done | skipped
    pub status: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Plan {
    pub summary: String,
    pub items: Vec<PlanItem>,
    /// Required facts the documents do not contain.
    pub questions: Vec<String>,
}

impl Plan {
    fn apply(&mut self, updates: &[ItemUpdate]) {
        for u in updates {
            if let Some(item) = self.items.iter_mut().find(|i| i.id == u.item) {
                if matches!(u.status.as_str(), "pending" | "doing" | "done" | "skipped") {
                    item.status = u.status.clone();
                }
                if !u.note.trim().is_empty() {
                    item.note = u.note.trim().to_string();
                }
            }
        }
    }

    fn add(&mut self, raw: Vec<RawPlanItem>) {
        for r in raw {
            let id = format!("i{}", self.items.len() + 1);
            self.items.push(PlanItem { id, title: r.title, values: r.values, status: "pending".into(), note: String::new() });
        }
    }

    /// One line per item: cheap for the interaction model to read every micro-turn.
    fn compact(&self) -> serde_json::Value {
        let items: Vec<String> = self
            .items
            .iter()
            .map(|i| {
                let vals = i.values.iter().map(|kv| format!("{}={}", kv.field, kv.value)).collect::<Vec<_>>().join("; ");
                let note = if i.note.is_empty() { String::new() } else { format!(" ({})", i.note) };
                format!("{} [{}] {}: {}{}", i.id, i.status, i.title, vals, note)
            })
            .collect();
        json!({ "summary": self.summary, "items": items, "open_questions": self.questions })
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct BgNote {
    pub question: String,
    pub answer: String,
    pub at: DateTime<Utc>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Background {
    pub plan: Option<Plan>,
    pub notes: Vec<BgNote>,
    /// Description of the job in flight, if any.
    pub running: Option<String>,
    pub planning_failed: bool,
    /// Tokens and cost spent by each role in this session.
    pub usage: UsageSplit,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct UsageSplit {
    pub interaction: Usage,
    pub background: Usage,
}

impl UsageSplit {
    pub fn total(&self) -> Usage {
        let mut t = self.interaction.clone();
        t.add(&self.background);
        t
    }
}

pub type BackgroundMap = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Background>>>;

// ---------- model I/O ----------

#[derive(Deserialize, Debug)]
struct RawAction {
    op: String,
    selector: String,
    value: String,
}

#[derive(Deserialize, Debug, Clone)]
struct ItemUpdate {
    item: String,
    status: String,
    note: String,
}

#[derive(Deserialize, Debug)]
struct RawReply {
    message: String,
    actions: Vec<RawAction>,
    item_updates: Vec<ItemUpdate>,
    delegate: String,
    /// working | finished | need_user
    state: String,
}

#[derive(Deserialize, Debug)]
struct RawPlanItem {
    title: String,
    values: Vec<Kv>,
}

#[derive(Deserialize, Debug)]
struct RawPlan {
    summary: String,
    items: Vec<RawPlanItem>,
    questions: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct RawConsult {
    answer: String,
    add_items: Vec<RawPlanItem>,
}

const SYSTEM: &str = "You are the interaction layer of a form-filling agent, operating a live web page for the user in micro-turns: \
act a little, see what the page did, react. A slower background model works alongside you: it has read the user's documents \
and produced a plan of items (e.g. one per class to enter), and it answers questions you delegate. \
Each micro-turn you get: the request, the plan (item lines like 'i3 [pending] Title: field=value; ...'), new background notes, \
the page (fields with current values/options/required/error flags, clickable actions, visible text, OPEN DIALOG text first), \
new_text (text that appeared since your last look), environment_events (what the page did while your last actions ran, with ms timestamps), \
interrupted (set when the page reacted unexpectedly and your remaining actions were NOT run), recent history, and timing. \
Reply with: message (one short sentence for the user), actions, item_updates, delegate. \
Actions: fill (text/textarea/date/number; value = exact text), select (native <select> or a radio-group; value = the option's visible text), \
check (checkbox; value \"true\"/\"false\"), click (buttons/links; value \"\"), waitFor (value \"\"). Use only selectors listed in the page. \
A select with page_default=true still shows the page's own preset (e.g. a state preselected by the site): it is not an answer. \
If the plan has a value for that field, set it. \
radio-group fields are sets of radio buttons: select the option text (e.g. \"10th\", \"Yes\"). Map plan values onto the options shown (e.g. 'Grade 10' -> '10th'). \
Work through plan items in order. Before entering an item, check the page text: if it is already listed (e.g. a table row with that class), \
mark it done and move on; never enter an item twice. Mark an item doing when you start it, done once it appears on the page, \
skipped with a note if it cannot be entered. \
Batch optimistically: the page interrupts you if anything goes wrong (errors, flagged fields), and the executor waits for elements to appear. \
So one batch can fill every field of an item, click Save, click Add New, and fill and save the next item, using selectors you have already seen \
in this session (e.g. the dialog's fields from a previous row). Aim for one to three items per batch. The first time, when you have not seen the \
dialog yet, end the batch with the click that opens it. \
React to the page first: if interrupted, environment_events, new_text, or fields marked missing/error show a problem, fix exactly that before anything else. \
Never click Save again without changing what the error complains about; never repeat an identical batch on an unchanged page. \
Required fields must be filled before saving. The plan is authoritative: use its values directly and never ask to confirm them. \
Never answer a question by guessing, by picking a default, or by following the form's own hint (e.g. 'please select no'): \
if the plan and notes do not answer it (or say 'unknown'), delegate it; if the background model says the documents do not say, \
stop and ask the user, leaving that item unsaved. \
delegate: a short question for the background model, or \"\" (the usual case). Delegate only when a required value is absent from the plan and notes \
(e.g. whether an exam was taken), when no option fits a plan value, or when you are stuck; keep acting on other items meanwhile. \
Ask the user only if the background model says the documents do not contain the answer. \
If you notice a value you entered earlier is wrong (it does not match the plan or documents), fix it; never leave your own mistake in place. \
person_edited lists fields the person changed themselves while you worked. Their edits win: never overwrite them unless the request says to, \
and mention them in your final message (e.g. 'You changed Username to ...; I left it'). \
Never invent data. Do not click the final submit of an application, pay, sign, or delete anything; saving a row or dialog is fine. \
state: working while anything in the request remains to do (even if this turn only waits for the background model or needs data from it); \
finished only when every plan item for the latest request is done or skipped and the page shows it; need_user when only the user can unblock you. \
If you need data you do not have (e.g. the class list from the documents), say what in delegate and keep state working. \
When finished or need_user, return no actions and say what was done, what was skipped, and what you need. \
Your final message must match what the page shows.";

const PLANNER: &str = "You are the background planner of a form-filling agent. The user asked for something to be filled on a web form. \
Read the request, the user's documents (transcripts, resumes, notes), the profile, and the current page. \
Produce the list of entries to make: one item per repeated entry the request implies (e.g. one per class on a coursework page), \
or a single item for a one-off form. For each item give field/value pairs named after what the form asks (use the page's labels and \
options when visible; otherwise natural names such as subject category, year taken, level, class name, grade received). \
Normalize values to how forms usually phrase them and include the source phrasing when helpful (e.g. year taken = '9th' for 'Grade 9'). \
Record every fact the documents give for each item, even ones the current page does not ask for yet, because later dialogs often \
reveal more questions (e.g. 'AP exam score' -> '5', 'AP exam taken' -> 'Yes' when a score is listed, original grade text '5 / A'). \
Copy names exactly as the documents spell them (e.g. class name 'English 9 Honors', never shortened to 'English 9'). \
Keep the original wording alongside normalized values when they differ. If an AP/IB class has no exam score listed, record 'AP exam taken' -> 'unknown'. \
If existing_plan is present, the user has made a new or follow-up request: output only the NEW items that request needs \
(not items already in existing_plan); earlier_requests is background. \
Never invent facts. questions lists only required facts that are absent from the documents. summary is one sentence.";

const CONSULT: &str = "You are the background model of a form-filling agent. The fast interaction layer working the live page has a question. \
You get the request, documents, profile, plan, the current page, and recent history. Answer concisely and concretely (exact values to enter, \
which option to choose, what the error means and how to fix it). If the documents do not contain the answer, say so plainly so the agent \
can ask the user. add_items lists plan items that are missing from the plan (usually empty).";

fn reply_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "message": {"type": "string"},
            "actions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": {"type": "string", "enum": ["fill", "select", "check", "click", "waitFor"]},
                        "selector": {"type": "string"},
                        "value": {"type": "string"}
                    },
                    "required": ["op", "selector", "value"],
                    "additionalProperties": false
                }
            },
            "item_updates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "item": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "doing", "done", "skipped"]},
                        "note": {"type": "string"}
                    },
                    "required": ["item", "status", "note"],
                    "additionalProperties": false
                }
            },
            "delegate": {"type": "string"},
            "state": {"type": "string", "enum": ["working", "finished", "need_user"]}
        },
        "required": ["message", "actions", "item_updates", "delegate", "state"],
        "additionalProperties": false
    })
}

fn item_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "values": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"field": {"type": "string"}, "value": {"type": "string"}},
                    "required": ["field", "value"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["title", "values"],
        "additionalProperties": false
    })
}

fn plan_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "items": {"type": "array", "items": item_schema()},
            "questions": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["summary", "items", "questions"],
        "additionalProperties": false
    })
}

fn consult_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "add_items": {"type": "array", "items": item_schema()}
        },
        "required": ["answer", "add_items"],
        "additionalProperties": false
    })
}

fn to_step(a: RawAction) -> Option<Step> {
    let selector = a.selector;
    let value = ValueRef::Literal { literal: a.value };
    match a.op.as_str() {
        "fill" => Some(Step::Fill { selector, value }),
        "select" => Some(Step::Select { selector, value }),
        "check" => Some(Step::Check { selector, value }),
        "click" => Some(Step::Click { selector }),
        "waitFor" => Some(Step::WaitFor { selector, timeout_ms: Some(5000) }),
        _ => None,
    }
}

fn page_view(page: &AgentPage) -> serde_json::Value {
    let fields: Vec<_> = page
        .fields
        .iter()
        .map(|f| {
            let mut v = json!({
                "selector": f.sig.selector, "tag": f.sig.tag, "type": f.sig.input_type,
                "label": f.sig.label, "name": f.sig.name, "value": f.value,
            });
            if !f.sig.placeholder.is_empty() {
                v["placeholder"] = json!(f.sig.placeholder);
            }
            if f.required {
                v["required"] = json!(true);
            }
            if f.invalid {
                v["error"] = json!(true);
            }
            if f.required && f.value.trim().is_empty() {
                v["missing"] = json!(true);
            }
            if f.page_default {
                v["page_default"] = json!(true);
            }
            if !f.options.is_empty() {
                // Options are matched by visible text, so send text only unless the value differs meaningfully.
                let opts: Vec<_> = f
                    .options
                    .iter()
                    .filter(|o| !o.text.trim().is_empty())
                    .take(MAX_OPTIONS)
                    .map(|o| o.text.trim().to_string())
                    .collect();
                if f.options.len() > MAX_OPTIONS {
                    v["options_truncated"] = json!(f.options.len());
                }
                v["options"] = json!(opts);
            }
            // A dropdown with only a placeholder usually fills after an earlier choice.
            if f.sig.tag == "select" && f.options.iter().filter(|o| !o.value.is_empty()).count() == 0 {
                v["options_not_loaded"] = json!(true);
            }
            v
        })
        .collect();
    let text: String = page.text.chars().take(MAX_PAGE_TEXT).collect();
    let mut v = json!({"url": page.url, "title": page.title, "new_text": page.new_text, "fields": fields, "actions": page.actions, "text": text});
    if !page.user_edits.is_empty() {
        v["user_edits"] = json!(page.user_edits);
    }
    v
}

fn history_view(turns: &[Turn]) -> serde_json::Value {
    let start = turns.len().saturating_sub(HISTORY_TURNS);
    let summary: Vec<String> = turns[..start].iter().map(|t| t.message.chars().take(120).collect()).collect();
    let items: Vec<_> = turns[start..]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let actions: Vec<String> = t
                .actions
                .iter()
                .enumerate()
                .map(|(j, a)| {
                    let r = match t.results.get(j) {
                        Some(r) if r.ok => "ok".to_string(),
                        Some(r) if r.skipped => format!("not run ({})", r.error.clone().unwrap_or_default()),
                        Some(r) => format!("FAILED: {}", r.error.clone().unwrap_or_default()),
                        None => "pending".to_string(),
                    };
                    format!("{} -> {r}", describe_value(a))
                })
                .collect();
            let mut v = json!({"turn": start + i + 1, "message": t.message, "actions": actions});
            if !t.events.is_empty() {
                v["events"] = json!(t.events.iter().map(|e| format!("+{}ms {}: {}", e.t_ms as u64, e.kind, e.detail)).collect::<Vec<_>>());
            }
            if let Some(why) = &t.interrupted {
                v["interrupted"] = json!(why);
            }
            if !t.rejected.is_empty() {
                v["rejected"] = json!(t.rejected);
            }
            v
        })
        .collect();
    if summary.is_empty() { json!(items) } else { json!({"earlier": summary, "recent": items}) }
}

fn describe_value(step: &Step) -> String {
    let lit = |v: &ValueRef| match v {
        ValueRef::Literal { literal } => literal.clone(),
        ValueRef::Profile { profile } => format!("{{{profile}}}"),
    };
    match step {
        Step::Fill { selector, value } | Step::Select { selector, value } | Step::Check { selector, value } => {
            format!("{} {} = {:?}", describe(step).split(' ').next().unwrap_or(""), selector, lit(value))
        }
        other => describe(other),
    }
}

fn context_view(docs: &[ContextDoc]) -> serde_json::Value {
    let mut budget = MAX_CONTEXT_CHARS;
    let mut out = Vec::new();
    for d in docs {
        if budget == 0 {
            break;
        }
        let text: String = d.text.chars().take(budget).collect();
        budget = budget.saturating_sub(text.chars().count());
        out.push(json!({"name": d.name, "text": text}));
    }
    json!(out)
}

impl MemoryController {
    fn session(&self, id: &str) -> Result<AgentSession> {
        self.sessions.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("agent session {id} not found"))
    }

    fn put_session(&self, s: AgentSession) {
        let mut g = self.sessions.lock().unwrap();
        // Keep memory bounded: drop finished sessions older than an hour.
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        g.retain(|_, v| !v.finished || v.created_at > cutoff);
        g.insert(s.id.clone(), s);
        let live: HashSet<String> = g.keys().cloned().collect();
        self.background.lock().unwrap().retain(|k, _| live.contains(k));
    }

    /// Map the page to a node, reusing the session's node for this URL pattern.
    async fn page_node(&self, session: &mut AgentSession, page: &AgentPage) -> Result<(String, String)> {
        let (host, pattern) = normalize_url(&page.url);
        let key = format!("{host}{pattern}");
        if let Some(id) = session.pages.get(&key) {
            return Ok((key, id.clone()));
        }
        let (obs, _, _) = self.resolve(&page.snapshot()).await?;
        session.pages.insert(key.clone(), obs.id.clone());
        Ok((key, obs.id))
    }

    fn bg(&self, session_id: &str) -> Background {
        self.background.lock().unwrap().get(session_id).cloned().unwrap_or_default()
    }

    /// Run a background-model job without blocking the micro-turn loop.
    /// Planning when `question` is None, otherwise a consult.
    fn spawn_background(&self, session_id: &str, question: Option<String>, context: serde_json::Value) {
        let Some(llm) = self.llm.clone() else { return };
        let map = self.background.clone();
        let sid = session_id.to_string();
        {
            let mut g = map.lock().unwrap();
            let slot = g.entry(sid.clone()).or_default();
            if slot.running.is_some() {
                return; // one background job at a time
            }
            slot.running = Some(match &question {
                None => "Reading your documents and planning the entries".into(),
                Some(q) => format!("Thinking about: {q}"),
            });
        }
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let (system, name, schema) = match &question {
                None => (PLANNER, "plan", plan_schema()),
                Some(_) => (CONSULT, "consult", consult_schema()),
            };
            let reply = llm
                .chat_metered(false, vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": context.to_string()})], Some((name, schema)))
                .await;
            let mut g = map.lock().unwrap();
            let slot = g.entry(sid.clone()).or_default();
            slot.running = None;
            let outcome = match reply {
                Ok((v, usage)) => {
                    slot.usage.background.add(&usage);
                    match &question {
                        None => serde_json::from_value::<RawPlan>(v).map(Ok).map_err(anyhow::Error::from),
                        Some(_) => serde_json::from_value::<RawConsult>(v).map(Err).map_err(anyhow::Error::from),
                    }
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok(Ok(raw)) => {
                    let n = raw.items.len();
                    match slot.plan.as_mut() {
                        // Follow-up: keep earlier items (and their statuses), append the new ones.
                        Some(plan) => {
                            plan.summary = raw.summary;
                            plan.questions = raw.questions;
                            plan.add(raw.items);
                        }
                        None => {
                            let mut plan = Plan { summary: raw.summary, items: Vec::new(), questions: raw.questions };
                            plan.add(raw.items);
                            slot.plan = Some(plan);
                        }
                    }
                    tracing::info!("background plan for {sid}: +{n} items in {:.1}s", started.elapsed().as_secs_f64());
                }
                Ok(Err(consult)) => {
                    if !consult.add_items.is_empty() {
                        slot.plan.get_or_insert_with(Plan::default).add(consult.add_items);
                    }
                    slot.notes.push(BgNote { question: question.unwrap_or_default(), answer: consult.answer, at: Utc::now() });
                }
                Err(e) => {
                    tracing::warn!("background job failed for {sid}: {e:#}");
                    if question.is_none() {
                        slot.planning_failed = true;
                    }
                    slot.notes.push(BgNote { question: question.unwrap_or_else(|| "plan".into()), answer: format!("(background model failed: {e:#})"), at: Utc::now() });
                }
            }
        });
    }

    /// Wait (bounded) for the in-flight background job to land.
    async fn await_background(&self, session_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(BACKGROUND_WAIT_SECS);
        while self.bg(session_id).running.is_some() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    pub async fn agent_step(&self, req: AgentStepRequest) -> Result<AgentStepResponse> {
        let llm = self.llm.as_ref().ok_or_else(|| anyhow!("no OPENROUTER_KEY or OPENAI_KEY configured: the agent needs a model"))?;
        let mut session = match &req.session_id {
            Some(id) => self.session(id)?,
            None => {
                let prompt = req.prompt.clone().filter(|p| !p.trim().is_empty()).ok_or_else(|| anyhow!("a prompt is required to start"))?;
                let (host, _) = normalize_url(&req.page.url);
                AgentSession {
                    id: new_id(),
                    prompt,
                    host,
                    turns: Vec::new(),
                    pages: BTreeMap::new(),
                    finished: false,
                    saved: Vec::new(),
                    saved_turns: 0,
                    created_at: Utc::now(),
                    last_turn_at: Utc::now(),
                    notes_seen: 0,
                    known_selectors: HashSet::new(),
                    last_delegate_turn: None,
                    idle_turns: 0,
                    user_edits: BTreeMap::new(),
            last_user_question_check: None,
                }
            }
        };
        let new_session = req.session_id.is_none();
        let follow_up = !new_session && req.prompt.as_ref().is_some_and(|p| !p.trim().is_empty());
        if session.finished {
            // A new message reopens the conversation (answering a question, asking for more).
            if !follow_up {
                return Err(anyhow!("agent session already finished"));
            }
            session.finished = false;
        }
        if let Some(last) = session.turns.last_mut() {
            last.results = req.results.clone();
            last.events = req.events.clone();
            last.interrupted = req.interrupted.clone();
        }
        if follow_up && let Some(p) = &req.prompt {
            session.prompt = format!("{}\nFollow-up from the user: {}", session.prompt, p.trim());
        }

        let (url_key, observation_id) = self.page_node(&mut session, &req.page).await?;
        for f in &req.page.fields {
            session.known_selectors.insert(f.sig.selector.clone());
        }
        for e in &req.page.user_edits {
            session.user_edits.insert(e.selector.clone(), e.clone());
        }
        for a in &req.page.actions {
            session.known_selectors.insert(a.selector.clone());
        }
        let docs = self.store.context_for(&site_of(&req.page.url)).await?;
        let profile = self.store.profile_all().await?;

        // Background: plan once at the start, and again when the user follows up.
        if new_session || follow_up {
            let ctx = json!({
                "request": if follow_up { req.prompt.clone().unwrap_or_default() } else { session.prompt.clone() },
                "earlier_requests": if follow_up { json!(session.prompt) } else { json!(null) },
                "documents": context_view(&docs),
                "profile": profile,
                "page": page_view(&req.page),
                "existing_plan": self.bg(&session.id).plan.map(|p| p.compact()),
            });
            // A follow-up is usually a new request ("now do the classes section"):
            // plan it, appending items to the existing plan.
            self.spawn_background(&session.id, None, ctx);
        }

        if session.turns.len() >= MAX_TURNS {
            return self.finish_early(session, observation_id, format!("Stopped after {MAX_TURNS} micro-turns. Saved what worked so far.")).await;
        }

        let current_sig = page_sig(&req.page);
        let repeats = repeat_count(&session.turns, current_sig);
        if repeats >= REPEAT_STOP {
            let last = session.turns.last().map(|t| t.actions.iter().map(describe).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            let hint = if req.page.new_text.is_empty() { String::new() } else { format!(" The page says: {}", req.page.new_text.join(" / ")) };
            let message = format!(
                "I'm stuck: I repeated \"{last}\" {repeats} times and the page didn't change.{hint} Please fix the highlighted field on the page or tell me what to enter, then send a message to continue."
            );
            return self.finish_early(session, observation_id, message).await;
        }
        let stuck_warning = (repeats >= REPEAT_WARN).then(|| {
            format!(
                "Your last {repeats} micro-turns did the same actions and the page did not change. Doing them again will not help. \
The background model has been asked for help; fix what new_text and missing/error fields point at, or try a different approach."
            )
        });
        if repeats >= REPEAT_WARN {
            let ctx = self.consult_context(&session, &docs, &profile, &req.page);
            self.spawn_background(&session.id, Some("The interaction layer is stuck repeating the same actions with no change on the page. What is wrong and what exactly should it do?".into()), ctx);
        }

        let now = Utc::now();
        let elapsed = (now - session.created_at).num_milliseconds() as f64 / 1000.0;
        let since_last = (now - session.last_turn_at).num_milliseconds() as f64 / 1000.0;

        // Micro-turn: ask the interaction model; if it has nothing to do while the
        // background model is still working, wait for that result and ask once more.
        let mut raw: RawReply;
        let mut attempt = 0;
        let mut finish_challenge: Option<String> = None;
        loop {
            attempt += 1;
            let bg = self.bg(&session.id);
            let new_notes: Vec<_> = bg.notes.iter().skip(session.notes_seen).map(|n| json!({"question": n.question, "answer": n.answer})).collect();
            let mut user = json!({
                "request": session.prompt,
                "timing": {"elapsed_s": elapsed.round(), "since_last_turn_s": (since_last * 10.0).round() / 10.0, "micro_turn": session.turns.len() + 1},
                "plan": bg.plan.as_ref().map(Plan::compact),
                "background": bg.running.clone().map(|r| format!("busy: {r}")).unwrap_or_else(|| "idle".into()),
                "background_notes": new_notes,
                "profile": profile,
                "page": page_view(&req.page),
                "environment_events": req.events.iter().map(|e| format!("+{}ms {}: {}", e.t_ms as u64, e.kind, e.detail)).collect::<Vec<_>>(),
                "interrupted": req.interrupted,
                "history": history_view(&session.turns),
            });
            if !session.user_edits.is_empty() {
                user["person_edited"] = json!(session.user_edits.values().collect::<Vec<_>>());
            }
            if let Some(w) = &finish_challenge {
                user["finish_check"] = json!(w);
            }
            // Without a plan (planning failed), fall back to reading the documents directly.
            if bg.plan.is_none() && bg.planning_failed {
                user["documents"] = context_view(&docs);
            }
            if let Some(w) = &stuck_warning {
                user["warning"] = json!(w);
            }
            let (reply, usage) = llm
                .chat_metered(
                    true,
                    vec![json!({"role": "system", "content": SYSTEM}), json!({"role": "user", "content": user.to_string()})],
                    Some(("micro_turn", reply_schema())),
                )
                .await?;
            self.background.lock().unwrap().entry(session.id.clone()).or_default().usage.interaction.add(&usage);
            raw = serde_json::from_value(reply)?;
            session.notes_seen = bg.notes.len();
            if !raw.item_updates.is_empty()
                && let Some(plan) = self.background.lock().unwrap().get_mut(&session.id).and_then(|b| b.plan.as_mut()) {
                    plan.apply(&raw.item_updates);
                }
            let turn_no = session.turns.len();
            let recently = session.last_delegate_turn.is_some_and(|t| turn_no < t + DELEGATE_EVERY);
            if !raw.delegate.trim().is_empty() && (!recently || stuck_warning.is_some()) {
                let ctx = self.consult_context(&session, &docs, &profile, &req.page);
                self.spawn_background(&session.id, Some(raw.delegate.trim().to_string()), ctx);
                session.last_delegate_turn = Some(turn_no);
            }
            let busy = self.bg(&session.id).running.is_some();
            // Values before the plan exists are guesses (e.g. "Me", "me@example.com").
            // Navigation (Add New, Next) may go ahead; anything that types or picks
            // a value waits for the background plan, then the turn is asked again.
            let sets_values = raw.actions.iter().any(|a| matches!(a.op.as_str(), "fill" | "select" | "check"));
            let bg_now = self.bg(&session.id);
            if sets_values && bg_now.plan.is_none() && !bg_now.planning_failed && busy && attempt <= 2 {
                self.await_background(&session.id).await;
                continue;
            }
            // Finish guard: "finished" while plan items are still open is usually a
            // false completion. Make the model look once more before accepting it.
            let unfinished = self.unfinished_items(&session.id);
            if raw.actions.is_empty() && raw.state == "finished" && !unfinished.is_empty() && finish_challenge.is_none() && attempt <= 2 {
                finish_challenge = Some(format!(
                    "You said finished, but these plan items are not marked done or skipped: {}. Check the page text for each one. \
If it is on the page, mark it done; if it is missing or wrong, enter it now; if it cannot be entered, mark it skipped with a note.",
                    unfinished.join("; ")
                ));
                continue;
            }
            // Before bothering the user, check the documents: the fast model only sees
            // the plan, so "I need X from you" often means "X was not in the plan".
            let recently_checked = session.last_user_question_check.is_some_and(|t| session.turns.len() < t + 5);
            if raw.actions.is_empty() && raw.state == "need_user" && !busy && !recently_checked && attempt <= 2 {
                session.last_user_question_check = Some(session.turns.len());
                let q = format!(
                    "Before asking the user, check the documents. The interaction layer wants to ask: \"{}\". \
If the documents, profile or request contain the answer, give the exact values to enter; otherwise say plainly that they do not.",
                    raw.message
                );
                let ctx = self.consult_context(&session, &docs, &profile, &req.page);
                self.spawn_background(&session.id, Some(q), ctx);
                self.await_background(&session.id).await;
                continue;
            }
            if raw.actions.is_empty() && attempt == 1 && (busy || raw.state == "working") {
                if !busy {
                    // "Working" with nothing to do: it lacks something. Ask the background model.
                    let q = if raw.delegate.trim().is_empty() {
                        format!("The interaction layer says: \"{}\" but has no actions to take. Give it exactly what it needs to continue (values from the documents, which items remain, what to click).", raw.message)
                    } else {
                        raw.delegate.trim().to_string()
                    };
                    let ctx = self.consult_context(&session, &docs, &profile, &req.page);
                    self.spawn_background(&session.id, Some(q), ctx);
                    session.last_delegate_turn = Some(session.turns.len());
                }
                self.await_background(&session.id).await;
                continue;
            }
            break;
        }

        let allowed: HashSet<String> = session.known_selectors.clone();
        let mut actions = Vec::new();
        let mut rejected = Vec::new();
        for a in raw.actions {
            let desc = format!("{} {}", a.op, a.selector);
            // The person's edits win: never type over a field they changed themselves.
            if matches!(a.op.as_str(), "fill" | "select" | "check") && session.user_edits.contains_key(&a.selector) {
                rejected.push(format!("{desc}: the person edited this field themselves; leave it as they set it"));
                continue;
            }
            // waitFor may target an element that does not exist yet.
            if a.op != "waitFor" && !allowed.contains(&a.selector) {
                rejected.push(format!("{desc}: selector never seen on this page; use selectors from the page"));
                continue;
            }
            match to_step(a) {
                Some(step) => actions.push(step),
                None => rejected.push(format!("{desc}: unknown op")),
            }
        }
        let bg = self.bg(&session.id);
        // Done when there is nothing to do here, nothing pending in the background,
        // and the model is not still working. A rejected batch is a mistake to correct.
        // Several idle "working" turns in a row means it is stuck: stop and ask the user.
        let idle = actions.is_empty() && rejected.is_empty();
        session.idle_turns = if idle && raw.state == "working" { session.idle_turns + 1 } else { 0 };
        let stalled = session.idle_turns >= MAX_IDLE_TURNS;
        let mut message = raw.message.clone();
        // The person's own edits must always be reported, even if the model forgot.
        if idle && raw.state != "working" {
            let lower = message.to_lowercase();
            let unmentioned: Vec<String> = session
                .user_edits
                .values()
                .filter(|e| {
                    let label = e.label.trim().trim_end_matches('*').trim().to_lowercase();
                    !(lower.contains(&e.value.to_lowercase()) || (!label.is_empty() && lower.contains(&label)))
                })
                // Only claim "I left your edit" if the page still shows it.
                .filter(|e| {
                    req.page.fields.iter().find(|f| f.sig.selector == e.selector).is_none_or(|f| f.value == e.value || e.value == "true" || e.value == "false")
                })
                .map(|e| {
                    let label = e.label.trim().trim_end_matches('*').trim();
                    let name = if label.is_empty() { e.selector.as_str() } else { label };
                    match e.value.as_str() {
                        "true" => format!("selected \"{name}\""),
                        "false" => format!("cleared \"{name}\""),
                        v => format!("set {name} to \"{v}\""),
                    }
                })
                .collect();
            if !unmentioned.is_empty() {
                message = format!("{message} Note: while I worked you {}; I left your edits.", unmentioned.join(" and "));
            }
        }
        let still_open = self.unfinished_items(&session.id);
        if idle && raw.state == "finished" && !still_open.is_empty() {
            message = format!("{message} Not confirmed on the page: {}.", still_open.join(", "));
        }
        if stalled {
            message = format!("{message} I'm not making progress on my own; tell me what to do next or give me the missing information.");
        }
        let done = idle && bg.running.is_none() && (raw.state != "working" || stalled);

        session.turns.push(Turn {
            url_key,
            observation_id: observation_id.clone(),
            message: message.clone(),
            actions: actions.clone(),
            results: Vec::new(),
            rejected,
            events: Vec::new(),
            interrupted: None,
            page_sig: current_sig,
        });
        session.last_turn_at = Utc::now();
        let saved = if done { self.save_session(&mut session).await? } else { Vec::new() };
        if done {
            let t = self.bg(&session.id).usage.total();
            tracing::info!(
                "session {} finished: {} micro-turns, {} model calls, {} prompt + {} completion tokens, ${:.5}",
                session.id, session.turns.len(), t.calls, t.prompt_tokens, t.completion_tokens, t.cost_usd
            );
        }
        let id = session.id.clone();
        let turn = session.turns.len();
        self.put_session(session);
        let bg = self.bg(&id);
        Ok(AgentStepResponse { session_id: id, observation_id, turn, message, actions, done, saved, plan: bg.plan, background: bg.running, usage: bg.usage })
    }

    async fn finish_early(&self, mut session: AgentSession, observation_id: String, message: String) -> Result<AgentStepResponse> {
        let saved = self.save_session(&mut session).await?;
        let bg = self.bg(&session.id);
        let (session_id, turn) = (session.id.clone(), session.turns.len());
        self.put_session(session);
        Ok(AgentStepResponse { session_id, observation_id, turn, message, actions: vec![], done: true, saved, plan: bg.plan, background: None, usage: bg.usage })
    }

    /// Titles of plan items still pending or in progress.
    fn unfinished_items(&self, session_id: &str) -> Vec<String> {
        self.bg(session_id)
            .plan
            .map(|p| p.items.iter().filter(|i| i.status == "pending" || i.status == "doing").map(|i| i.title.clone()).collect())
            .unwrap_or_default()
    }

    fn consult_context(&self, session: &AgentSession, docs: &[ContextDoc], profile: &Profile, page: &AgentPage) -> serde_json::Value {
        let bg = self.bg(&session.id);
        json!({
            "request": session.prompt,
            "documents": context_view(docs),
            "profile": profile,
            "plan": bg.plan.as_ref().map(Plan::compact),
            "earlier_notes": bg.notes.iter().map(|n| json!({"question": n.question, "answer": n.answer})).collect::<Vec<_>>(),
            "page": page_view(page),
            "history": history_view(&session.turns),
        })
    }

    /// Stop a session early (user pressed Stop) and keep what worked.
    pub async fn agent_finish(&self, id: &str, results: Vec<StepResult>) -> Result<Vec<RankedScript>> {
        let mut session = self.session(id)?;
        if session.finished {
            return Ok(Vec::new());
        }
        if let Some(last) = session.turns.last_mut()
            && last.results.is_empty() {
                last.results = results;
            }
        let saved = self.save_session(&mut session).await?;
        self.put_session(session);
        Ok(saved)
    }

    pub fn agent_session(&self, id: &str) -> Result<AgentSession> {
        self.session(id)
    }

    /// Turn the successful actions of a session into one script per page node,
    /// linked by `applies`, with `transition` edges between consecutive pages.
    async fn save_session(&self, session: &mut AgentSession) -> Result<Vec<RankedScript>> {
        session.finished = true;
        let profile = self.store.profile_all().await?;
        let mut order: Vec<String> = Vec::new();
        let mut per_page: BTreeMap<String, Vec<Step>> = BTreeMap::new();
        let start = session.saved_turns.min(session.turns.len());
        session.saved_turns = session.turns.len();
        for turn in &session.turns[start..] {
            if order.last() != Some(&turn.observation_id) {
                order.push(turn.observation_id.clone());
            }
            for (i, step) in turn.actions.iter().enumerate() {
                let ok = turn.results.get(i).map(|r| r.ok).unwrap_or(false);
                if !ok {
                    continue;
                }
                per_page.entry(turn.observation_id.clone()).or_default().push(generalize_step(step, &profile));
            }
        }
        let title: String = session.prompt.lines().next().unwrap_or("").chars().take(60).collect();
        let mut saved = Vec::new();
        for obs_id in order.iter().collect::<std::collections::BTreeSet<_>>() {
            let Some(steps) = per_page.remove(obs_id) else { continue };
            if steps.is_empty() {
                continue;
            }
            let script = ToolScript {
                id: new_id(),
                name: format!("Agent: {title}"),
                body: ScriptBody::Steps(steps),
                origin: Origin::Agent,
                created_at: Utc::now(),
            };
            let mut ranked = self.add_script(obs_id, script).await?;
            ranked.observation_id = obs_id.to_string();
            let edge = self.store.record_run(obs_id, &ranked.script.id, true).await?;
            ranked.confidence = edge.confidence();
            ranked.successes = edge.successes;
            saved.push(ranked);
        }
        let script_on: BTreeMap<String, String> = saved.iter().map(|s| (s.observation_id.clone(), s.script.id.clone())).collect();
        for pair in order.windows(2) {
            if pair[0] != pair[1] {
                self.store.add_transition(&pair[0], &pair[1], script_on.get(&pair[0]).map(String::as_str)).await?;
            }
        }
        session.saved.extend(saved.iter().cloned());
        Ok(saved)
    }
}

fn describe(step: &Step) -> String {
    match step {
        Step::Fill { selector, .. } => format!("fill {selector}"),
        Step::Select { selector, .. } => format!("select {selector}"),
        Step::Check { selector, .. } => format!("check {selector}"),
        Step::Click { selector } => format!("click {selector}"),
        Step::WaitFor { selector, .. } => format!("wait for {selector}"),
        Step::Submit { selector } => format!("submit {selector}"),
        Step::Navigate { url } => format!("go to {url}"),
    }
}

/// Replace literal values that equal a profile value with profile references.
fn generalize_step(step: &Step, profile: &Profile) -> Step {
    let generalized = |v: &ValueRef| match v {
        ValueRef::Literal { literal } => generalize(literal, profile),
        other => other.clone(),
    };
    match step {
        Step::Fill { selector, value } => Step::Fill { selector: selector.clone(), value: generalized(value) },
        Step::Select { selector, value } => Step::Select { selector: selector.clone(), value: generalized(value) },
        other => other.clone(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    fn fsig(sel: &str) -> FieldSig {
        FieldSig {
            selector: sel.into(),
            tag: "input".into(),
            input_type: "text".into(),
            name: sel.trim_start_matches('#').into(),
            id: String::new(),
            label: String::new(),
            placeholder: String::new(),
            autocomplete: String::new(),
        }
    }

    fn lit(s: &str) -> ValueRef {
        ValueRef::Literal { literal: s.into() }
    }

    fn ok() -> StepResult {
        StepResult { ok: true, skipped: false, error: None }
    }

    #[test]
    fn plan_add_apply_and_compact() {
        let kv = |f: &str, v: &str| Kv { field: f.into(), value: v.into() };
        let mut plan = Plan { summary: "6 classes".into(), items: vec![], questions: vec!["exam for AP Calc?".into()] };
        plan.add(vec![
            RawPlanItem { title: "Algebra II".into(), values: vec![kv("year taken", "9th"), kv("grade", "A-")] },
            RawPlanItem { title: "AP CSP".into(), values: vec![kv("level", "AP")] },
        ]);
        assert_eq!(plan.items[1].id, "i2");
        assert_eq!(plan.items[0].status, "pending");
        plan.apply(&[
            ItemUpdate { item: "i1".into(), status: "done".into(), note: "".into() },
            ItemUpdate { item: "i2".into(), status: "bogus".into(), note: "exam unknown".into() },
            ItemUpdate { item: "i9".into(), status: "done".into(), note: "".into() },
        ]);
        assert_eq!(plan.items[0].status, "done");
        assert_eq!(plan.items[1].status, "pending", "invalid status ignored");
        assert_eq!(plan.items[1].note, "exam unknown");
        let c = plan.compact();
        assert_eq!(c["items"][0], "i1 [done] Algebra II: year taken=9th; grade=A-");
        assert_eq!(c["items"][1], "i2 [pending] AP CSP: level=AP (exam unknown)");
        assert_eq!(c["open_questions"][0], "exam for AP Calc?");
    }

    #[test]
    fn repeat_count_detects_identical_turns_on_unchanged_page() {
        let t = |sel: &str, page: u64| Turn {
            url_key: String::new(),
            observation_id: String::new(),
            message: String::new(),
            actions: vec![Step::Click { selector: sel.into() }],
            results: vec![],
            rejected: vec![],
            events: vec![],
            interrupted: None,
            page_sig: page,
        };
        let turns = vec![t("text=Add New", 1), t("text=Save", 7), t("text=Save", 7), t("text=Save", 7)];
        assert_eq!(repeat_count(&turns, 7), 3);
        assert_eq!(repeat_count(&turns, 8), 0, "page changed: not stuck");
        assert_eq!(repeat_count(&turns[..2], 7), 1);
        assert_eq!(repeat_count(&[], 7), 0);
    }

    #[tokio::test]
    async fn finishing_saves_one_script_per_page_with_transition() {
        let c = MemoryController::new(Arc::new(MemoryStore::new()), None);
        c.store.profile_set("firstName", "Ada").await.unwrap();
        let (a, _, _) = c.resolve(&PageSnapshot { url: "https://x.edu/courses".into(), title: String::new(), fields: vec![fsig("#q")] }).await.unwrap();
        let (b, _, _) = c.resolve(&PageSnapshot { url: "https://x.edu/family".into(), title: String::new(), fields: vec![fsig("#p")] }).await.unwrap();
        let turn = |obs: &str, actions: Vec<Step>, results: Vec<StepResult>| Turn {
            url_key: String::new(),
            observation_id: obs.into(),
            message: String::new(),
            actions,
            results,
            rejected: vec![],
            events: vec![],
            interrupted: None,
            page_sig: 0,
        };
        let session = AgentSession {
            id: "s1".into(),
            prompt: "Fill my coursework\nfrom transcript".into(),
            host: "x.edu".into(),
            turns: vec![
                turn(&a.id, vec![Step::Click { selector: "text=Add New".into() }], vec![ok()]),
                turn(
                    &a.id,
                    vec![
                        Step::Fill { selector: "#name".into(), value: lit("Algebra I") },
                        Step::Fill { selector: "#grade".into(), value: lit("A") },
                        Step::Fill { selector: "#who".into(), value: lit("ada") },
                    ],
                    vec![ok(), StepResult { ok: false, skipped: false, error: Some("nope".into()) }, ok()],
                ),
                turn(&b.id, vec![Step::Fill { selector: "#p".into(), value: lit("x") }], vec![]),
            ],
            pages: BTreeMap::new(),
            finished: false,
            saved: vec![],
            saved_turns: 0,
            created_at: Utc::now(),
            last_turn_at: Utc::now(),
            notes_seen: 0,
            known_selectors: HashSet::new(),
            last_delegate_turn: None,
            idle_turns: 0,
            user_edits: BTreeMap::new(),
            last_user_question_check: None,
        };
        c.put_session(session);
        let saved = c.agent_finish("s1", vec![ok()]).await.unwrap();
        assert_eq!(saved.len(), 2, "one script per page");
        let on_a = saved.iter().find(|s| s.observation_id == a.id).unwrap();
        assert_eq!(on_a.script.origin, Origin::Agent);
        assert_eq!(on_a.script.name, "Agent: Fill my coursework");
        assert_eq!(on_a.successes, 1);
        let ScriptBody::Steps(steps) = &on_a.script.body else { panic!() };
        assert_eq!(
            steps,
            &vec![
                Step::Click { selector: "text=Add New".into() },
                Step::Fill { selector: "#name".into(), value: lit("Algebra I") },
                Step::Fill { selector: "#who".into(), value: ValueRef::Profile { profile: "firstName".into() } },
            ],
            "failed steps dropped, profile values generalized"
        );
        let next = c.store.transitions_from(&a.id).await.unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].to, b.id);
        assert_eq!(next[0].via.as_deref(), Some(on_a.script.id.as_str()));
        // Finishing twice saves nothing new.
        assert!(c.agent_finish("s1", vec![]).await.unwrap().is_empty());
        assert_eq!(c.store.all_scripts().await.unwrap().len(), 2);
        // A reopened session only saves turns added after the first save.
        let mut s = c.agent_session("s1").unwrap();
        s.finished = false;
        s.turns.push(turn(&a.id, vec![Step::Fill { selector: "#more".into(), value: lit("B") }], vec![ok()]));
        c.put_session(s);
        let again = c.agent_finish("s1", vec![]).await.unwrap();
        assert_eq!(again.len(), 1);
        let ScriptBody::Steps(steps) = &again[0].script.body else { panic!() };
        assert_eq!(steps.len(), 1);
    }
}
