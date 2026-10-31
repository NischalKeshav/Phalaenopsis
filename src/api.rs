//! HTTP surface used by the Chrome extension.

use crate::controller::{LearnSegment, LearnSessionResponse, MemoryController, ObserveRequest, RankedScript, RunReport};
use crate::model::*;
use crate::agent::{AgentStepRequest, AgentStepResponse, StepResult};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, services::ServeDir};

#[derive(Clone)]
pub struct AppState {
    pub controller: Arc<MemoryController>,
}

pub struct ApiError(StatusCode, anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::warn!("{} {:#}", self.0, self.1);
        (self.0, Json(json!({ "error": format!("{:#}", self.1) }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else if msg.contains("reached for this API key") || msg.contains("429 Too Many Requests") {
            // provider rate limits
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        ApiError(status, e)
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/observe", post(observe))
        .route("/v1/observations/{id}", get(get_observation))
        .route("/v1/scripts", post(add_script))
        .route("/v1/scripts/{id}", get(get_script).delete(delete_script))
        .route("/v1/generate", post(generate))
        .route("/v1/learn", post(learn))
        .route("/v1/learn/session", post(learn_session))
        .route("/v1/runs", post(report_run))
        .route("/v1/profile", get(get_profile).put(put_profile))
        .route("/v1/profile/{key}", delete(delete_profile_key))
        .route("/v1/graph", get(graph))
        .route("/v1/agent/step", post(agent_step))
        .route("/v1/agent/{id}", get(agent_session))
        .route("/v1/agent/{id}/finish", post(agent_finish))
        .route("/v1/context", get(list_context).post(add_context))
        .route("/v1/context/{id}", get(get_context).delete(delete_context))
        .layer(DefaultBodyLimit::max(30 * 1024 * 1024))
        .nest_service("/test", ServeDir::new("autofill_extension/test"))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "store": s.controller.store.kind(),
        "llm": s.controller.llm.is_some(),
        "provider": s.controller.llm.as_ref().map(|l| l.provider.name()),
        "model": s.controller.llm.as_ref().map(|l| l.model.clone()),
        "fast_model": s.controller.llm.as_ref().map(|l| l.fast_model.clone()),
    }))
}

async fn observe(State(s): State<AppState>, Json(req): Json<ObserveRequest>) -> ApiResult<crate::controller::ObserveResponse> {
    match s.controller.observe(req).await? {
        Some(r) => Ok(Json(r)),
        None => Err(ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("no known node for this page"))),
    }
}

#[derive(Serialize)]
struct ObservationDetail {
    observation: Observation,
    scripts: Vec<RankedScript>,
    next: Vec<Transition>,
}

async fn get_observation(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<ObservationDetail> {
    let observation = s
        .controller
        .store
        .get_observation(&id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("observation {id} not found")))?;
    let scripts = s.controller.ranked_scripts(&id).await?;
    let next = s.controller.store.transitions_from(&id).await?;
    Ok(Json(ObservationDetail { observation, scripts, next }))
}

#[derive(Deserialize)]
struct AddScriptRequest {
    observation_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(flatten)]
    body: ScriptBody,
    #[serde(default)]
    origin: Option<Origin>,
}

async fn add_script(State(s): State<AppState>, Json(req): Json<AddScriptRequest>) -> ApiResult<RankedScript> {
    let script = ToolScript {
        id: new_id(),
        name: req.name.unwrap_or_else(|| format!("{} script", req.body.kind())),
        body: req.body,
        origin: req.origin.unwrap_or(Origin::Manual),
        created_at: chrono::Utc::now(),
    };
    Ok(Json(s.controller.add_script(&req.observation_id, script).await?))
}

async fn get_script(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<ToolScript> {
    let script = s
        .controller
        .store
        .get_script(&id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("script {id} not found")))?;
    Ok(Json(script))
}

async fn delete_script(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<serde_json::Value> {
    s.controller.store.delete_script(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
struct GenerateRequest {
    observation_id: String,
}

async fn generate(State(s): State<AppState>, Json(req): Json<GenerateRequest>) -> ApiResult<RankedScript> {
    let obs = s
        .controller
        .store
        .get_observation(&req.observation_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("observation {} not found", req.observation_id)))?;
    Ok(Json(s.controller.generate_script(&obs).await?))
}

#[derive(Deserialize)]
struct LearnRequest {
    observation_id: String,
    actions: Vec<RecordedAction>,
}

async fn learn(State(s): State<AppState>, Json(req): Json<LearnRequest>) -> ApiResult<RankedScript> {
    Ok(Json(s.controller.learn(&req.observation_id, &req.actions).await?))
}

#[derive(Deserialize)]
struct LearnSessionRequest {
    segments: Vec<LearnSegment>,
}

/// "Learn from user": a whole demonstration, possibly spanning several pages.
async fn learn_session(State(s): State<AppState>, Json(req): Json<LearnSessionRequest>) -> ApiResult<LearnSessionResponse> {
    Ok(Json(s.controller.learn_session(&req.segments).await?))
}

async fn report_run(State(s): State<AppState>, Json(req): Json<RunReport>) -> ApiResult<crate::controller::RunResponse> {
    Ok(Json(s.controller.report_run(req).await?))
}

async fn get_profile(State(s): State<AppState>) -> ApiResult<Profile> {
    Ok(Json(s.controller.store.profile_all().await?))
}

/// Merge the given keys into the profile. A null value deletes the key.
async fn put_profile(State(s): State<AppState>, Json(map): Json<serde_json::Map<String, serde_json::Value>>) -> ApiResult<Profile> {
    for (key, value) in map {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        match value {
            serde_json::Value::Null => s.controller.store.profile_delete(key).await?,
            serde_json::Value::String(v) => s.controller.store.profile_set(key, &v).await?,
            other => s.controller.store.profile_set(key, &other.to_string()).await?,
        }
    }
    Ok(Json(s.controller.store.profile_all().await?))
}

async fn delete_profile_key(State(s): State<AppState>, Path(key): Path<String>) -> ApiResult<Profile> {
    s.controller.store.profile_delete(&key).await?;
    Ok(Json(s.controller.store.profile_all().await?))
}

async fn agent_step(State(s): State<AppState>, Json(req): Json<AgentStepRequest>) -> ApiResult<AgentStepResponse> {
    Ok(Json(s.controller.agent_step(req).await?))
}

async fn agent_session(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<crate::agent::AgentSession> {
    Ok(Json(s.controller.agent_session(&id)?))
}

#[derive(Deserialize, Default)]
struct FinishRequest {
    #[serde(default)]
    results: Vec<StepResult>,
}

async fn agent_finish(State(s): State<AppState>, Path(id): Path<String>, body: Option<Json<FinishRequest>>) -> ApiResult<Vec<RankedScript>> {
    let results = body.map(|b| b.0.results).unwrap_or_default();
    Ok(Json(s.controller.agent_finish(&id, results).await?))
}

#[derive(Deserialize)]
struct AddContextRequest {
    name: String,
    #[serde(default)]
    mime: String,
    /// Host the document applies to, or "*" (default) for every site.
    #[serde(default)]
    scope: Option<String>,
    /// Plain text, if the client already has it.
    #[serde(default)]
    text: Option<String>,
    /// Raw file bytes, base64. PDFs and images are transcribed by the model.
    #[serde(default)]
    data_base64: Option<String>,
}

fn is_textual(mime: &str, name: &str) -> bool {
    let lower = name.to_lowercase();
    mime.starts_with("text/")
        || matches!(mime, "application/json" | "application/csv" | "application/xml")
        || [".txt", ".md", ".csv", ".tsv", ".json"].iter().any(|ext| lower.ends_with(ext))
}

async fn add_context(State(s): State<AppState>, Json(req): Json<AddContextRequest>) -> ApiResult<serde_json::Value> {
    use base64::Engine;
    let bad = |msg: String| ApiError(StatusCode::BAD_REQUEST, anyhow::anyhow!(msg));
    let mime = if req.mime.is_empty() { "text/plain".to_string() } else { req.mime.clone() };
    let text = match (req.text, req.data_base64) {
        (Some(t), _) => t,
        (None, Some(b64)) if is_textual(&mime, &req.name) => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| bad(format!("bad base64: {e}")))?;
            String::from_utf8_lossy(&bytes).into_owned()
        }
        (None, Some(b64)) => {
            let llm = s.controller.llm.as_ref().ok_or_else(|| bad("reading PDFs and images needs OPENROUTER_KEY or OPENAI_KEY".into()))?;
            llm.extract_text(&req.name, &mime, b64.trim()).await?
        }
        (None, None) => return Err(bad("send text or data_base64".into())),
    };
    if text.trim().is_empty() {
        return Err(bad("no text could be extracted from this document".into()));
    }
    let doc = ContextDoc {
        id: new_id(),
        name: req.name,
        mime,
        scope: match req.scope.as_deref().map(str::trim) {
            None | Some("") | Some("*") => "*".into(),
            Some(site) => crate::fingerprint::site_of(site),
        },
        text,
        created_at: chrono::Utc::now(),
    };
    s.controller.store.insert_context(&doc).await?;
    Ok(Json(doc.summary()))
}

async fn list_context(State(s): State<AppState>) -> ApiResult<Vec<serde_json::Value>> {
    Ok(Json(s.controller.store.all_context().await?.iter().map(ContextDoc::summary).collect()))
}

async fn get_context(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<ContextDoc> {
    let doc = s
        .controller
        .store
        .get_context(&id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, anyhow::anyhow!("context {id} not found")))?;
    Ok(Json(doc))
}

async fn delete_context(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<serde_json::Value> {
    s.controller.store.delete_context(&id).await?;
    Ok(Json(json!({ "deleted": id })))
}

async fn graph(State(s): State<AppState>) -> ApiResult<GraphExport> {
    Ok(Json(s.controller.store.export().await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        let controller = Arc::new(MemoryController::new(Arc::new(MemoryStore::new()), None));
        router(AppState { controller })
    }

    async fn call(app: &Router, method: &str, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() { json!(null) } else { serde_json::from_slice(&bytes).unwrap() };
        (status, value)
    }

    #[tokio::test]
    async fn observe_then_script_then_run_round_trip() {
        let app = app();
        let page = json!({
            "url": "https://apply.example.edu/app/42/profile",
            "title": "Profile",
            "fields": [{"selector": "#first", "tag": "input", "input_type": "text", "name": "first", "label": "First name"}]
        });
        let (st, r1) = call(&app, "POST", "/v1/observe", page.clone()).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r1["matched"], false);
        assert_eq!(r1["observation"]["url_pattern"], "/app/*/profile");
        let obs_id = r1["observation"]["id"].as_str().unwrap().to_string();

        let (st, s) = call(
            &app,
            "POST",
            "/v1/scripts",
            json!({"observation_id": obs_id, "name": "fill", "kind": "steps", "body": [{"op": "fill", "selector": "#first", "value": {"profile": "firstName"}}]}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{s}");
        assert_eq!(s["confidence"], 0.5);
        let script_id = s["id"].as_str().unwrap().to_string();

        let (st, run) = call(&app, "POST", "/v1/runs", json!({"observation_id": obs_id, "script_id": script_id, "ok": true})).await;
        assert_eq!(st, StatusCode::OK);
        assert!((run["confidence"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-9);

        let (_, r2) = call(&app, "POST", "/v1/observe", page).await;
        assert_eq!(r2["matched"], true);
        assert_eq!(r2["scripts"][0]["id"], script_id);
        assert_eq!(r2["scripts"][0]["kind"], "steps");

        let (st, _) = call(&app, "PUT", "/v1/profile", json!({"firstName": "Ada"})).await;
        assert_eq!(st, StatusCode::OK);
        let (_, g) = call(&app, "GET", "/v1/graph", json!(null)).await;
        assert_eq!(g["observations"].as_array().unwrap().len(), 1);
        assert_eq!(g["applies"].as_array().unwrap().len(), 1);
        assert_eq!(g["profile_keys"], json!(["firstName"]));
    }

    #[tokio::test]
    async fn context_upload_text_and_base64() {
        let app = app();
        let (st, d) = call(&app, "POST", "/v1/context", json!({"name": "notes.txt", "text": "Grade 9 | Algebra I | A", "scope": "https://www.apply.mit.edu:443/app"})).await;
        assert_eq!(st, StatusCode::OK, "{d}");
        assert_eq!(d["scope"], "apply.mit.edu");
        // "Grade 10 | Geometry | A-" in base64
        let (st, d2) = call(&app, "POST", "/v1/context", json!({"name": "t.csv", "mime": "text/csv", "data_base64": "R3JhZGUgMTAgfCBHZW9tZXRyeSB8IEEt"})).await;
        assert_eq!(st, StatusCode::OK, "{d2}");
        assert_eq!(d2["preview"], "Grade 10 | Geometry | A-");
        assert_eq!(d2["scope"], "*");
        let (st, _) = call(&app, "POST", "/v1/context", json!({"name": "x.pdf", "mime": "application/pdf", "data_base64": "AAAA"})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "pdf without an LLM is rejected");
        let (_, list) = call(&app, "GET", "/v1/context", json!(null)).await;
        assert_eq!(list.as_array().unwrap().len(), 2);
        let id = d["id"].as_str().unwrap();
        let (_, full) = call(&app, "GET", &format!("/v1/context/{id}"), json!(null)).await;
        assert_eq!(full["text"], "Grade 9 | Algebra I | A");
    }

    #[tokio::test]
    async fn agent_without_llm_is_an_error() {
        let (st, e) = call(&app(), "POST", "/v1/agent/step", json!({"prompt": "fill", "page": {"url": "https://a.com/x"}})).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(e["error"].as_str().unwrap().contains("OPENROUTER_KEY"));
    }

    #[tokio::test]
    async fn unknown_observation_is_404() {
        let (st, _) = call(&app(), "GET", "/v1/observations/nope", json!(null)).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}
