# Eval results, 2026-10-31

Provider OpenRouter, `z-ai/glm-5.3-flash` for both the interaction and background roles, pace normal.
Five scenarios, each run **calm** (agent alone) and **busy** (a simulated person moves the mouse,
scrolls, clicks, presses Escape and edits one filled field while the agent works). Raw output,
screenshots and per-run JSON are in `eval/results/` (gitignored).

## Final full run (`2026-10-31T18-38-57`)

| Scenario | Mode | Result | Checks | Micro-turns | Interrupts | Model calls | Prompt tok | Completion tok (reasoning) | Cost | Time | Human actions |
|---|---|---|---|---|---|---|---|---|---|---|---|
| contact | calm | ✅ pass | 3/3 | 3 | 0 | 4 | 4,954 | 556 (144) | $0.0006 | 7.9s | 0 |
| contact | busy | ✅ pass | 5/5 | 3 | 0 | 4 | 4,939 | 502 (132) | $0.0006 | 7.9s | 3 |
| signup | calm | ✅ pass | 9/9 | 3 | 0 | 5 | 7,102 | 1,111 (413) | $0.0010 | 11.5s | 0 |
| signup | busy | ✅ pass | 10/10 | 4 | 0 | 6 | 8,942 | 2,648 (1,840) | $0.0008 | 22.8s | 8 |
| wizard | calm | ❌ fail | 11/12 | 4 | 0 | 8 | 20,326 | 1,621 (231) | $0.0021 | 28.5s | 0 |
| wizard | busy | ✅ pass | 12/12 | 6 | 0 | 15 | 38,704 | 5,374 (3,489) | $0.0029 | 54.5s | 17 |
| checkout | calm | ✅ pass | 11/11 | 4 | 0 | 7 | 15,039 | 1,800 (1,083) | $0.0014 | 17.8s | 0 |
| checkout | busy | ✅ pass | 11/11 | 4 | 0 | 10 | 21,473 | 3,017 (1,762) | $0.0022 | 24.1s | 8 |
| coursework | calm | ✅ pass | 7/7 | 8 | 1 | 15 | 36,830 | 6,136 (2,684) | $0.0046 | 74.2s | 0 |
| coursework | busy | ✅ pass | 7/7 | 6 | 2 | 12 | 31,345 | 6,303 (3,263) | $0.0037 | 89.9s | 31 |
| **total** | | **9/10** | | 45 | 3 | 86 | 189,654 | 29,068 (15,041) | **$0.0198** | 339s | 67 |

The one failure (wizard calm: buying state left on the page's preselected "Pennsylvania") was
then fixed by flagging selects still on a page default. Wizard reruns after that fix: calm pass,
calm pass, busy pass, and one busy run lost to an OpenRouter response that stalled mid-body,
which is now retried.

## Progress across runs

| Run | Passed | Cost | What changed before it |
|---|---|---|---|
| 1 | 6/10 | $0.0244 | baseline |
| 2 | 4/10 | $0.0227 | person-edit tracking, finish guard, keyboard typing for masked inputs |
| 3 | 5/10 | $0.0157 | wait for the plan before entering values, always report person's edits |
| 4 | 7/10 | $0.0228 | request timeouts and retries, self-correction, fairer harness checks |
| 5 | 6/10 | $0.0203 | person's edits protected from overwrite; agent's own radio clicks no longer counted as edits |
| 6 | 9/10 | $0.0198 | that filter limited to checkboxes/radios; harness only expects a mention if the agent saw the edit |

Run 2's dip came mostly from harness bugs the new checks exposed, not from agent regressions.

## What the eval found and fixed

Agent and extension:
- **False completion.** The agent said "finished" with plan items still open. Now the backend
  makes it check again, and names anything still unconfirmed.
- **Guessing before the plan.** It typed "Me" / "me@example.com" before the background plan
  arrived. Value-setting actions now wait for the plan; navigation does not.
- **Asking the user for data that was in the documents.** Questions for the user are now checked
  against the documents by the background model first.
- **Masked inputs** (jquery.maskedinput ZIP/phone) wiped values set by script. The executor now
  verifies the characters stuck and falls back to keyboard-style typing with legacy key codes.
- **The person's own edits.** Trusted input events are reported as `person_edited`; the agent may
  not overwrite those fields, and the final message always mentions them (only claiming "I left
  it" when the page still shows it).
- **The agent's own checkbox/radio clicks** fire trusted change events in Chrome and were
  mistaken for the person's edits. Filtered.
- **Pages' preselected defaults** in selects looked like answers. Now flagged `page_default`.
- **Hung model requests** stalled a run for 5+ minutes. 90 s timeout, and retries on connect,
  timeout and body errors.
- **Planner shortened names** ("English 9" for "English 9 Honors") and dropped facts the page did
  not ask for yet. Now told to copy names verbatim and keep every fact.

Harness:
- End in a textarea only goes to the end of the line, dialogs rebuilt per row reuse field names,
  and an edit after the agent's last look cannot be mentioned; the checks now account for all three.
- Blocking browser dialogs are auto-dismissed and logged.
- The checkout demo only lists California as a state. Leaving it blank and asking is now the
  correct answer; picking California is a guess and fails.

## Still open

- **Model reasoning slips** that self-checking cannot catch because the plan itself is wrong: one
  coursework run entered every year one grade too high ("Grade 9" became "10th"); one wizard run
  typed ZIP 02140 instead of 02139. A second, independent check of the plan against the
  documents would catch these.
- **Variance.** GLM 5.3 Flash is not deterministic; single runs swing by a scenario or two.
  Running the suite 3+ times and reporting pass rates would be more reliable than one run.

## Cost

About **$0.02 per full suite** (10 runs, ~190k prompt + ~30k completion tokens, ~86 model calls).
Per scenario: simple forms (contact, signup) ~5k tokens and well under $0.001; multi-step or
row-by-row forms (wizard, coursework) 20k-60k tokens and $0.002-0.005. Roughly half the completion
tokens are hidden reasoning.
