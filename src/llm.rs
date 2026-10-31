//! LLM client for the agent and cold-start scripts. Talks the OpenAI chat
//! completions protocol to either OpenRouter (default when `OPENROUTER_KEY` is
//! set; model `z-ai/glm-5.3-flash`) or OpenAI directly.

use crate::model::{Observation, Step, ValueRef};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    OpenRouter,
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::OpenRouter => "OpenRouter",
        }
    }
}

/// First non-empty value among env vars.
fn env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    key: String,
    pub provider: Provider,
    /// Background model: planning, documents, harder reasoning.
    pub model: String,
    /// Interaction model: fast, small-context micro-turns on the live page.
    pub fast_model: String,
    reasoning_effort: Option<String>,
    fast_reasoning_effort: Option<String>,
    base_url: String,
}

/// Tokens and cost of model calls, as reported by the provider.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Part of completion_tokens spent on hidden reasoning (when reported).
    pub reasoning_tokens: u64,
    /// USD, when the provider reports it (OpenRouter does).
    pub cost_usd: f64,
    pub latency_ms: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.calls += o.calls;
        self.prompt_tokens += o.prompt_tokens;
        self.completion_tokens += o.completion_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
        self.cost_usd += o.cost_usd;
        self.latency_ms += o.latency_ms;
    }

    fn from_response(v: &serde_json::Value, latency_ms: u64) -> Self {
        let u = &v["usage"];
        let n = |x: &serde_json::Value| x.as_u64().unwrap_or(0);
        Usage {
            calls: 1,
            prompt_tokens: n(&u["prompt_tokens"]),
            completion_tokens: n(&u["completion_tokens"]),
            reasoning_tokens: n(&u["completion_tokens_details"]["reasoning_tokens"]),
            cost_usd: u["cost"].as_f64().unwrap_or(0.0),
            latency_ms,
        }
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct RawStep {
    op: String,
    selector: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct RawScript {
    steps: Vec<RawStep>,
}

impl LlmClient {
    /// Build from the environment. `OPENROUTER_KEY` wins unless `LLM_PROVIDER=openai`;
    /// otherwise `OPENAI_KEY`/`OPENAI_API_KEY`. Returns None when no key is set.
    ///
    /// Models: `LLM_MODEL` (background) and `LLM_FAST_MODEL` (interaction); the older
    /// `OPENAI_MODEL`/`OPENAI_FAST_MODEL` names still work. Efforts: `LLM_REASONING_EFFORT`,
    /// `LLM_FAST_REASONING_EFFORT`.
    pub fn from_env() -> Option<Self> {
        let want_openai = env_any(&["LLM_PROVIDER"]).is_some_and(|p| p.eq_ignore_ascii_case("openai"));
        let openrouter = env_any(&["OPENROUTER_KEY", "OPENROUTER_API_KEY"]).filter(|_| !want_openai);
        let (provider, key) = match openrouter {
            Some(k) => (Provider::OpenRouter, k),
            None => (Provider::OpenAi, env_any(&["OPENAI_KEY", "OPENAI_API_KEY"])?),
        };
        let (default_model, default_fast, base_default) = match provider {
            // GLM 5.3 Flash: strict structured outputs, image/PDF input, reasoning always on.
            Provider::OpenRouter => ("z-ai/glm-5.3-flash", "z-ai/glm-5.3-flash", "https://openrouter.ai/api/v1"),
            // Measured: gpt-5.4-nano dithered and duplicated rows; gpt-5.4-mini was correct.
            Provider::OpenAi => ("gpt-5.4-mini", "gpt-5.4-mini", "https://api.openai.com/v1"),
        };
        let model = env_any(&["LLM_MODEL", "OPENAI_MODEL"]).unwrap_or_else(|| default_model.into());
        let fast_model = env_any(&["LLM_FAST_MODEL", "OPENAI_FAST_MODEL"]).unwrap_or_else(|| default_fast.into());
        let reasons = |m: &str| provider == Provider::OpenRouter || m.starts_with("gpt-5") || m.starts_with('o');
        // Background work (planning, documents, consults) is rare, so it can think harder.
        let reasoning_effort = env_any(&["LLM_REASONING_EFFORT", "OPENAI_REASONING_EFFORT"])
            .or_else(|| reasons(&model).then(|| "medium".into()));
        // OpenRouter's GLM cannot disable reasoning, so keep it low for speed;
        // OpenAI's mini is fastest with its own default.
        let fast_reasoning_effort = env_any(&["LLM_FAST_REASONING_EFFORT", "OPENAI_FAST_REASONING_EFFORT"])
            .or_else(|| (provider == Provider::OpenRouter).then(|| "low".into()));
        Some(Self {
            http: reqwest::Client::new(),
            key,
            provider,
            model,
            fast_model,
            reasoning_effort,
            fast_reasoning_effort,
            base_url: env_any(&["LLM_BASE_URL", "OPENAI_BASE_URL"]).unwrap_or_else(|| base_default.into()),
        })
    }

    /// One chat completion. With `schema`, the reply is constrained JSON and parsed;
    /// without, the raw text is returned as a JSON string.
    pub async fn chat(&self, messages: Vec<serde_json::Value>, schema: Option<(&str, serde_json::Value)>) -> Result<serde_json::Value> {
        Ok(self.chat_metered(false, messages, schema).await?.0)
    }

    /// One chat completion plus the tokens/cost it used. `fast` picks the
    /// interaction model, otherwise the background model.
    pub async fn chat_metered(&self, fast: bool, messages: Vec<serde_json::Value>, schema: Option<(&str, serde_json::Value)>) -> Result<(serde_json::Value, Usage)> {
        self.chat_with(fast, messages, schema).await
    }

    async fn chat_with(&self, fast: bool, messages: Vec<serde_json::Value>, schema: Option<(&str, serde_json::Value)>) -> Result<(serde_json::Value, Usage)> {
        let started = std::time::Instant::now();
        let (model, effort) = if fast { (&self.fast_model, &self.fast_reasoning_effort) } else { (&self.model, &self.reasoning_effort) };
        let mut body = json!({ "model": model, "messages": messages });
        if let Some((name, schema)) = schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {"name": name, "strict": true, "schema": schema}
            });
        }
        match self.provider {
            Provider::OpenAi => {
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            Provider::OpenRouter => {
                if let Some(effort) = effort {
                    body["reasoning"] = json!({ "effort": effort, "exclude": true });
                }
                // Only route to hosts that honour every parameter (strict JSON schema).
                if body.get("response_format").is_some() {
                    body["provider"] = json!({ "require_parameters": true });
                }
                // Ask OpenRouter to report the cost of each call.
                body["usage"] = json!({ "include": true });
            }
        }
        let provider = self.provider.name();
        // Rate limits (429) and transient server errors are retried with the
        // wait OpenAI suggests, so a burst of agent turns slows down instead of failing.
        let mut attempt = 0;
        let text = loop {
            attempt += 1;
            let mut req = self.http.post(format!("{}/chat/completions", self.base_url)).bearer_auth(&self.key).json(&body);
            if self.provider == Provider::OpenRouter {
                req = req.header("X-Title", "Phalaenopsis");
            }
            let resp = req.send().await.with_context(|| format!("{provider} request failed"))?;
            let status = resp.status();
            let header_wait = resp
                .headers()
                .get("retry-after-ms")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .map(|ms| ms / 1000.0)
                .or_else(|| resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok()));
            let text = resp.text().await.with_context(|| format!("{provider} response unreadable"))?;
            if status.is_success() {
                break text;
            }
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable || attempt >= MAX_ATTEMPTS {
                return Err(anyhow!("{provider} {status}: {text}"));
            }
            let suggested = suggested_wait(&text).or(header_wait);
            if let Some(w) = suggested
                && w > MAX_INLINE_WAIT_SECS
            {
                return Err(long_limit_error(&text, w, provider));
            }
            let wait = suggested.unwrap_or(2.0 * attempt as f64).clamp(0.5, MAX_INLINE_WAIT_SECS) + 0.25;
            tracing::info!("{provider} {status}, retrying in {wait:.1}s (attempt {attempt}/{MAX_ATTEMPTS})");
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("{provider} response not json"))?;
        // OpenRouter can return 200 with an error object from the upstream host.
        if let Some(err) = parsed.get("error") {
            return Err(anyhow!("{provider} error: {err}"));
        }
        let usage = Usage::from_response(&parsed, started.elapsed().as_millis() as u64);
        let content = parsed["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("{provider} response missing content: {text}"))?;
        let value = if body.get("response_format").is_some() {
            // Some models wrap JSON in a code fence even under a schema.
            let trimmed = content.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
            serde_json::from_str(trimmed).context("model output did not match schema")?
        } else {
            serde_json::Value::String(content.to_string())
        };
        Ok((value, usage))
    }

    /// Transcribe a PDF or image into plain text so it can be used as context.
    pub async fn extract_text(&self, name: &str, mime: &str, data_base64: &str) -> Result<String> {
        let data_url = format!("data:{mime};base64,{data_base64}");
        let part = if mime == "application/pdf" {
            json!({"type": "file", "file": {"filename": name, "file_data": data_url}})
        } else if mime.starts_with("image/") {
            json!({"type": "image_url", "image_url": {"url": data_url}})
        } else {
            return Err(anyhow!("cannot extract text from {mime}"));
        };
        let instruction = "Transcribe this document into plain text for later use filling out forms. \
Preserve every fact exactly: names, dates, course titles, grade levels, grades, credits, scores, terms. \
Render tables as one row per line with ' | ' between columns and a header line. Output only the transcription.";
        let reply = self
            .chat(vec![json!({"role": "user", "content": [{"type": "text", "text": instruction}, part]})], None)
            .await?;
        Ok(reply.as_str().unwrap_or_default().trim().to_string())
    }

    pub async fn generate_steps(&self, obs: &Observation, profile_keys: &[String]) -> Result<Vec<Step>> {
        let fields: Vec<_> = obs
            .fields
            .iter()
            .map(|f| {
                json!({
                    "selector": f.selector, "tag": f.tag, "type": f.input_type, "name": f.name,
                    "id": f.id, "label": f.label, "placeholder": f.placeholder, "autocomplete": f.autocomplete,
                })
            })
            .collect();
        let system = "You write deterministic browser autofill scripts for a form the user has to fill. \
You are given the form fields (with CSS selectors) and the list of profile KEYS available (you never see the values). \
Return JSON {\"steps\": [...]} where each step is {\"op\", \"selector\", \"value\"}. \
op is one of: fill (text inputs, textareas, email, tel, date, number), select (<select> elements), check (checkboxes/radios), click (buttons or links). \
value is either \"profile:<key>\" to use a profile value, or \"literal:<text>\" for a fixed value. \
For check, the value resolves to a boolean: use profile:<key> when a profile key describes that fact, literal:true/literal:false only for non-personal consents such as terms of service. \
Only use selectors that appear in the field list. Map a field to a profile key only when the match is clear from its label/name/autocomplete; skip fields with no matching key. \
Never click submit, next, continue, or save buttons. Do not invent or guess personal data.";
        let user = json!({
            "page": {"host": obs.host, "url_pattern": obs.url_pattern, "title": obs.title},
            "profile_keys": profile_keys,
            "fields": fields,
        });
        let schema = json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["fill", "select", "check", "click"]},
                            "selector": {"type": "string"},
                            "value": {"type": "string"}
                        },
                        "required": ["op", "selector", "value"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["steps"],
            "additionalProperties": false
        });
        let reply = self
            .chat(
                vec![json!({"role": "system", "content": system}), json!({"role": "user", "content": user.to_string()})],
                Some(("autofill_script", schema)),
            )
            .await?;
        let raw: RawScript = serde_json::from_value(reply).context("model output did not match schema")?;
        let known: std::collections::HashSet<&str> = obs.fields.iter().map(|f| f.selector.as_str()).collect();
        Ok(raw.steps.into_iter().filter(|s| known.contains(s.selector.as_str()) || s.op == "click").filter_map(convert_step).collect())
    }
}

const MAX_ATTEMPTS: u32 = 6;

/// Longest rate-limit wait worth sitting through inside one request.
const MAX_INLINE_WAIT_SECS: f64 = 40.0;

/// Pull the wait out of "try again in 2.887s", "in 450ms", "in 28m48s", "in 1h2m".
fn suggested_wait(body: &str) -> Option<f64> {
    let re = regex::Regex::new(r"try again in ((?:[0-9.]+(?:ms|h|m|s))+)").ok()?;
    let spec = re.captures(body)?.get(1)?.as_str().to_string();
    let part = regex::Regex::new(r"([0-9.]+)(ms|h|m|s)").ok()?;
    let mut total = 0.0;
    for c in part.captures_iter(&spec) {
        let n: f64 = c[1].parse().ok()?;
        total += match &c[2] {
            "ms" => n / 1000.0,
            "h" => n * 3600.0,
            "m" => n * 60.0,
            _ => n,
        };
    }
    Some(total)
}

fn human_wait(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 { format!("{}h {}m", s / 3600, (s % 3600) / 60) } else if s >= 60 { format!("{}m {}s", s / 60, s % 60) } else { format!("{s}s") }
}

/// A friendly error for limits that will not clear soon (e.g. requests per day).
fn long_limit_error(body: &str, wait: f64, provider: &str) -> anyhow::Error {
    let msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_default();
    let kind = if msg.contains("requests per day") {
        let limit = regex::Regex::new(r"Limit (\d+)").ok().and_then(|r| r.captures(&msg)).map(|c| c[1].to_string());
        format!("daily request limit{}", limit.map(|l| format!(" ({l} requests/day)")).unwrap_or_default())
    } else if msg.contains("tokens per day") {
        "daily token limit".to_string()
    } else {
        "rate limit".to_string()
    };
    anyhow!(
        "{provider} {kind} reached for this API key. It resets in about {}. Raising the account's limit or adding credits fixes it. Saved scripts still replay without the model.",
        human_wait(wait)
    )
}

fn parse_value(v: &str) -> ValueRef {
    if let Some(key) = v.strip_prefix("profile:") {
        ValueRef::Profile { profile: key.trim().to_string() }
    } else if let Some(lit) = v.strip_prefix("literal:") {
        ValueRef::Literal { literal: lit.to_string() }
    } else {
        ValueRef::Literal { literal: v.to_string() }
    }
}

fn convert_step(raw: RawStep) -> Option<Step> {
    let selector = raw.selector;
    match raw.op.as_str() {
        "fill" => Some(Step::Fill { selector, value: parse_value(&raw.value) }),
        "select" => Some(Step::Select { selector, value: parse_value(&raw.value) }),
        "check" => Some(Step::Check { selector, value: parse_value(&raw.value) }),
        "click" => Some(Step::Click { selector }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_openrouter_response() {
        let v = serde_json::json!({"usage": {"prompt_tokens": 1200, "completion_tokens": 300,
            "completion_tokens_details": {"reasoning_tokens": 180}, "cost": 0.000063}});
        let u = Usage::from_response(&v, 900);
        assert_eq!((u.calls, u.prompt_tokens, u.completion_tokens, u.reasoning_tokens, u.latency_ms), (1, 1200, 300, 180, 900));
        assert!((u.cost_usd - 0.000063).abs() < 1e-12);
        let mut total = Usage::default();
        total.add(&u);
        total.add(&u);
        assert_eq!(total.prompt_tokens, 2400);
        assert_eq!(Usage::from_response(&serde_json::json!({}), 5).prompt_tokens, 0);
    }

    #[test]
    fn reads_suggested_wait() {
        assert_eq!(suggested_wait("Please try again in 2.887s. Visit"), Some(2.887));
        assert_eq!(suggested_wait("try again in 450ms."), Some(0.45));
        assert_eq!(suggested_wait("no hint"), None);
        assert_eq!(suggested_wait("Please try again in 28m48s. Visit"), Some(28.0 * 60.0 + 48.0));
        assert_eq!(suggested_wait("try again in 1h2m."), Some(3720.0));
    }

    #[test]
    fn daily_limit_message_is_friendly() {
        let body = r#"{"error":{"message":"Rate limit reached for gpt-5-mini in organization org-x on requests per day (RPD): Limit 50, Used 50, Requested 1. Please try again in 28m48s."}}"#;
        let e = long_limit_error(body, 1728.0, "OpenAI").to_string();
        assert!(e.contains("daily request limit (50 requests/day)"), "{e}");
        assert!(e.contains("28m 48s"), "{e}");
    }

    #[test]
    fn parses_value_prefixes() {
        assert_eq!(parse_value("profile:firstName"), ValueRef::Profile { profile: "firstName".into() });
        assert_eq!(parse_value("literal:US"), ValueRef::Literal { literal: "US".into() });
        assert_eq!(parse_value("plain"), ValueRef::Literal { literal: "plain".into() });
    }

    #[test]
    fn converts_check_values() {
        let s = convert_step(RawStep { op: "check".into(), selector: "#x".into(), value: "profile:firstGen".into() }).unwrap();
        assert_eq!(s, Step::Check { selector: "#x".into(), value: ValueRef::Profile { profile: "firstGen".into() } });
    }
}
