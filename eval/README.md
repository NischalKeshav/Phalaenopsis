# Eval harness

Runs the real extension code on real web forms and measures how well the agent does,
how many micro-turns and tokens it takes, and how it copes with a person using the page
at the same time.

```sh
cargo build                     # the harness starts its own backend from target/debug
bash eval/fetch-sites.sh        # downloads the test sites (static files only)
node eval/run.mjs               # every scenario, calm and busy
node eval/run.mjs --only=signup,wizard --modes=busy --pace=careful
node eval/run.mjs --headed      # watch it in a real Chrome window
```

Needs `OPENROUTER_KEY` (or `OPENAI_KEY`) in the repo's `.env`. Node 22+, Google Chrome.
No npm packages.

## What it does

- Starts an **isolated backend** (in-memory store, port 7787). Your real graph, profile and
  documents are never read or written.
- Serves the test sites on port 7790 and opens them in headless Chrome over the DevTools
  protocol.
- Injects the extension's own `shared/netwatch.js`, `shared/fingerprint.js`,
  `shared/cursor.js` and `content.js` into every page, then runs the same loop as the side
  panel: snapshot, `/v1/agent/step`, execute, repeat.
- In **busy** mode a simulated person uses the tab while the agent works: curved mouse
  movements, scrolling, clicks on empty space, pressing Escape, and once per run editing a
  field the agent already filled. All of it is trusted input (`Input.dispatchMouseEvent` /
  `dispatchKeyEvent`), the same as a real mouse and keyboard.
- Afterwards each scenario's `verify` code inspects the page and checks the result.

## Output

`eval/results/<timestamp>/` (gitignored):

- `report.md`: summary table plus per-run details (failed checks, final message, human actions).
- `index.html`: contact sheet of screenshots. The orange arrow is the agent's virtual cursor;
  the blue dot is the simulated human's mouse (red while clicking).
- `results.json`, per-run `result.json`, `backend.log`.

## Scenarios

| id | Site | What makes it hard |
|---|---|---|
| contact | MDN "your first HTML form" | free-text message from a request |
| signup | jquery-validation 1.19.5 Milk demo | live validation errors, password confirm, radio, terms checkbox |
| wizard | jquery-validation 1.19.5 multipart demo | 3-step accordion wizard, ~15 selects, odd radio labels |
| checkout | Bootstrap 5.3.3 checkout example | country/state selects; must leave card fields empty |
| coursework | `autofill_extension/test/coursework_mit.html` | Add New dialogs, radio groups, required-field popups, transcript |

Data comes from a fictional persona (Jordan Lee) and a fake transcript in `scenarios.mjs`.

## Learn from user

`node eval/learn-from-user.mjs` (after `cargo build`) checks the side panel's
"Learn from user" recording end to end without a model key. It runs the real
`background.js` in Node behind a fake `chrome.*` API, the real content scripts in
headless Chrome, and the backend with an in-memory store on port 7797. A simulated
person types and clicks (trusted DevTools input) through `form.html` -> `form2.html`.
The test then saves and checks that each page got a `recorded` script, a transition
edge links the two pages via the first page's script, profile values became
`{profile}` references, and neither a typed password nor the agent's own fills were stored.
