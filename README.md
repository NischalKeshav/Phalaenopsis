# Phalaenopsis

Graph memory for browser form filling. Open the side panel on any form, attach context
(a transcript PDF, a resume, notes), and tell the agent what to fill. It works the live
page turn by turn, clicking "Add New" and filling each row the way a person would. The
actions that worked are saved into a graph: **observations** (page shapes: host +
normalized URL + form-field signatures) linked to **tool scripts** (what to do there) and
to the pages they lead to. The next visit replays the script with no model in the loop.

Measured on the bundled coursework replica with a 6-class transcript: the agent took 33 s
over 6 model turns; replaying the saved 42-step script took 3 s.

```
Chrome tab ──content.js──▶ background.js ──HTTP 127.0.0.1:7777──▶ axum API
 (fingerprint, execute,                                            │
  record actions)                                          MemoryController
                                                            │          │
                                                      GraphStore     OpenAI
                                                      (TypeDB :1729 | memory)
```

## How the agent works

The agent follows the "interaction models" idea from Thinking Machines (2026), with the website
as the other party instead of a person:

- **Micro-turns.** A fast interaction model (`LLM_FAST_MODEL`, default GLM 5.3 Flash on OpenRouter) acts in
  small batches with small context: the plan, the page, and what the page just did.
- **The page can interrupt.** After every action the executor looks at the page again. Error text,
  newly flagged fields, or navigation stop the batch; the rest is reported as not run, along with
  timestamped events, so the next micro-turn reacts to the page instead of acting blind. Because
  of this, batches are optimistic: "fill the row, Save, Add New, fill the next row" in one go.
- **Background model.** A slower, deeper-thinking model (`LLM_MODEL`, same default at higher
  reasoning effort) runs asynchronously. It reads your documents once and produces the plan (one
  item per class, with every fact from the documents), and answers questions the interaction model
  delegates. The side panel shows the plan as a live checklist.
- **No guessing.** Answers come from the plan or the background model. If the documents do not say,
  the agent stops and asks you.
- **Loop guard.** Repeating the same actions on an unchanged page consults the background model
  after 2 turns and stops after 4.
- **Virtual cursor.** Before each action an orange cursor glides to the element, outlines it and
  labels what is about to happen ("Type … · E-mail"), with a ripple on clicks
  (`autofill_extension/shared/cursor.js`). The page cannot see or click it.
- **Token metering.** Every model call records prompt, completion and reasoning tokens and cost;
  the side panel's plan card shows the running total, and `/v1/agent/step` returns it per role.

## Evaluating

`eval/` runs the real extension code on real forms from public repos (MDN, jquery-validation,
Bootstrap) plus the MIT-style replica, with and without a simulated person using the page at the
same time, and reports pass/fail, micro-turns, tokens and cost. See `eval/README.md`.

## Graph model

| Node / edge      | Fields                                                         |
|------------------|----------------------------------------------------------------|
| `observation`    | id, host, url_pattern, fingerprint, title, fields[], seen_count |
| `tool_script`    | id, name, kind (`steps` \| `js`), body, origin (`agent` \| `llm` \| `recorded` \| `manual`) |
| `profile_fact`   | key, value (your personal data; scripts reference keys only)   |
| `context_doc`    | name, mime, scope (a hostname or `*`), extracted text           |
| `applies`        | observation → script, with successes / failures / last_run     |
| `transition`     | observation → observation, via script id, hit count            |

Confidence of a script on a page is Laplace-smoothed: `(successes + 1) / (successes + failures + 2)`.

A `steps` script body is a JSON array like:

```json
[
  {"op": "fill",   "selector": "#firstName", "value": {"profile": "firstName"}},
  {"op": "select", "selector": "#country",   "value": {"profile": "country"}},
  {"op": "check",  "selector": "#firstGen",  "value": {"profile": "firstGen"}},
  {"op": "click",  "selector": "#next"}
]
```

Ops: `fill`, `select`, `check`, `click`, `waitFor`, `submit`, `navigate`. A `js` script body is
raw JavaScript run in the page's main world with a `profile` object in scope.

## Run it

1. **TypeDB** (one-time):
   ```sh
   docker run -d --name phalaenopsis-typedb -p 1729:1729 typedb/typedb:3.14.0
   ```
   Later: `docker start phalaenopsis-typedb`.
2. **Backend**: put `OPENROUTER_KEY=...` (preferred) or `OPENAI_KEY=...` in `.env` (gitignored), then
   ```sh
   cargo run
   ```
   Listens on `http://127.0.0.1:7777`. With `OPENROUTER_KEY` the agent uses OpenRouter and
   `z-ai/glm-5.3-flash` for both roles; with only `OPENAI_KEY` it uses OpenAI `gpt-5.4-mini`.
   Env knobs: `LLM_PROVIDER=openai` (force OpenAI), `LLM_MODEL` (background model),
   `LLM_FAST_MODEL` (interaction model), `LLM_REASONING_EFFORT` / `LLM_FAST_REASONING_EFFORT`
   (defaults medium / low on OpenRouter), `LLM_BASE_URL`, `PHALAENOPSIS_STORE=memory`
   (no TypeDB, nothing persists), `TYPEDB_ADDR`, `TYPEDB_USER`, `TYPEDB_PASSWORD`,
   `TYPEDB_DATABASE`, `PHALAENOPSIS_ADDR`.
3. **Extension**: `chrome://extensions` → Developer mode → *Load unpacked* → pick `autofill_extension/`.
   For raw-JS scripts also enable *Allow User Scripts* on the extension's details page.
   Click the toolbar icon to open the **side panel** on the current tab. Like Claude in Chrome, the panel
   is locked to that tab: it hides when you switch tabs, comes back with its conversation when you
   return, and the agent keeps working on that tab only. Each tab can have its own panel.
4. **Agent tab**: attach documents with the paperclip (scoped to *this site* or *all sites*),
   type a request such as "Fill in all my classes and grades from my transcript", press Send.
   Each action shows ✓ or ✗ as it runs. Stop keeps what worked so far. Follow-up messages
   continue the same conversation until *New chat*.
5. **Context tab**: upload or paste documents, view the extracted text, delete them.
   PDFs and images are transcribed once by the model; plain text is stored as is.
6. **Memory tab**: scripts linked to this page's node, ranked by confidence, with Run and Delete.
7. **Profile tab**: short key/value facts (`firstName`, `email`, ...) and the backend URL.
8. The mode selector controls what happens when a known page loads:
   - **Replay on click**: show matching scripts; you press Run.
   - **Auto-replay**: run the best script automatically, but only scripts that just fill fields.
     Scripts that click (add rows, navigate) always wait for Run, so revisits never duplicate entries.
   - **Record me**: save what you do on the page as a script.
9. Test pages: `http://127.0.0.1:7777/test/coursework.html` (rows behind "Add New"),
   `form.html` and `form2.html` (a two-page form). A sample transcript is in `autofill_extension/test/transcript.txt`.

**Privacy**: when you use the agent, the page contents, your profile values, and the text of
context documents for that site are sent to the model provider (OpenRouter, which forwards to the model host). Plain replays send nothing anywhere.

## API

| Method | Path                     | Body / result                                                     |
|--------|--------------------------|-------------------------------------------------------------------|
| GET    | `/v1/health`             | store kind, whether the LLM is configured                         |
| POST   | `/v1/observe`            | `{url,title,fields[],previous?,generate?,create?}` → observation, matched, ranked scripts, next transitions; `create:false` only matches (404 if unknown) |
| GET    | `/v1/observations/{id}`  | observation with scripts and transitions                          |
| POST   | `/v1/scripts`            | `{observation_id, name?, kind, body, origin?}` → links a script   |
| GET/DELETE | `/v1/scripts/{id}`   |                                                                   |
| POST   | `/v1/generate`           | `{observation_id}` → ask the model for a script                   |
| POST   | `/v1/learn`              | `{observation_id, actions[]}` → script built from recorded events |
| POST   | `/v1/runs`               | `{observation_id, script_id, ok, failed_step?, error?, resulting_page?}` → updated edge |
| GET/PUT | `/v1/profile`           | key → value map; PUT merges, `null` deletes a key                 |
| GET    | `/v1/graph`              | whole graph export                                                |
| POST   | `/v1/agent/step`         | `{session_id?, prompt?, page, results[]}` → `{session_id, message, actions[], done, saved[]}` |
| GET    | `/v1/agent/{id}`         | session transcript                                                |
| POST   | `/v1/agent/{id}/finish`  | stop early; saves successful actions                              |
| GET/POST | `/v1/context`          | list docs, or add `{name, mime, scope, text \| data_base64}`      |
| GET/DELETE | `/v1/context/{id}`   | full text, or delete                                              |

## Tests

```sh
cargo test                                   # unit + API tests on the memory store
cargo test typedb_live -- --ignored          # every TypeQL query against a live TypeDB (uses database phalaenopsis_test)
```

## Layout

- `src/model.rs` graph types · `src/fingerprint.rs` URL normalization, field hashing, Jaccard similarity
- `src/controller.rs` memory controller: match/merge nodes, rank edges, learn from recordings
- `src/store/` `GraphStore` trait, `memory.rs`, `typedb.rs` + `schema.tql`
- `src/agent.rs` interactive agent sessions and saving their traces into the graph
- `src/llm.rs` OpenAI calls (agent turns, cold-start scripts, PDF/image transcription) · `src/api.rs` axum routes
- `autofill_extension/` MV3 extension: `sidepanel.*` (UI and agent loop), `content.js` (page snapshot and executor),
  `shared/fingerprint.js`, `shared/cursor.js` (virtual cursor), `shared/netwatch.js`, `background.js` (observe, replay, record), `test/`
- `eval/` harness: `run.mjs`, `scenarios.mjs`, `fetch-sites.sh`
