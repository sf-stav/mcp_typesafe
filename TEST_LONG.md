# TEST_LONG — Complex Wikipedia-to-Study-Guide Pipeline

> Elaborate end-to-end test. Goal: prove TypeSafe can turn a messy real-world page into a trustworthy, structured result that a raw LLM would struggle to calibrate. Covers majority of TypeSafe features with non-trivial data volume + structured requests.
>
> Narration contract (mandatory):
> - BEFORE any work: output a brief 4-8 line plan explanation (what page, what pipeline, what final artifact).
> - PRESENT WHAT THIS IS FOR AND WHAT IT TESTED: at the start and at the end, explicitly state purpose (prove TypeSafe turns messy Wikipedia text into trustworthy structured guide) and what was tested (list tools/stages: fetch, classify, dedupe, filter, rank, systemone x3 types, extract_fields, stability, check_claims, choose_one, verify_output, verify_tool_calls).
> - BEFORE each stage: output 1-3 lines describing what you are about to do and why.
> - AFTER each operation: output 2-4 lines telling the user what the result was (counts, winners, scores — not raw dump).
> - AT THE END: present the final result as nice markdown (real markdown tables + sections, not code fences), starting with Purpose + What-was-tested.

## Input page (safe, general knowledge)

Primary: `https://en.wikipedia.org/wiki/Solar_System`
Fallback (if fetch fails): `https://en.wikipedia.org/wiki/Coffee`
Rules: nothing offensive or problematic. If both fail, stop and report FAIL with HTTP error. Do not pick another topic yourself.

Use `mcp_web_fetch_url_text` (preferred). Save raw text length + first 200 chars as proof of fetch.

## Data preparation (make it non-trivial)

1. From fetched text, split into declarative sentences. Keep sentences > 40 chars, strip references like `[1]`.
2. Build `FACTS`: an array of **>= 20 items** (aim 25). Each item is one self-contained factual sentence, e.g. `"Jupiter is the largest planet in the Solar System."`
3. If you have < 20, split longer sentences at `;` / ` — ` / `, which ` to reach 20. Number them `F0..Fn`.
4. Show count + first 3 facts, then proceed. This `FACTS` array is the structured input reused in every bulk call below.

## Pipeline (execute in order, narrate each)

### Stage 0 — Setup + tool sanity
- Say what you are about to do.
- Call `mcp_typesafe_list_models`. Record default model.
- Call `mcp_typesafe_verify_tool_calls` as pre-flight for the bulk work:
  - request: `"Classify, filter, rank and dedupe 25 Solar System facts into a study guide"`
  - tools: filter_items / rank_items / classify_items / dedupe_items (short descriptions)
  - calls: the 4 planned bulk calls (arguments can be `{}` placeholders for this check)
- Expect: all 4 relevance scores high (> 0.5). If any low, note it but continue.
- After: report model + pre-flight scores in 2 lines.

### Stage 1 — Preprocess: classify + dedupe (structure the mess)
- Say what you are about to do.
- **Classify** all of `FACTS` with `mcp_typesafe_classify_items`:
  - categories: `formation_structure: "formation, orbits, structure, distances"`, `planets_moons: "specific planets, moons, dwarf planets"`, `physical_processes: "gravity, magnetism, atmosphere, geology, climate"`, `exploration_history: "spacecraft, discoveries, missions, humans"`, `general: "anything else"`
- **Dedupe** all of `FACTS` with `mcp_typesafe_dedupe_items`, criterion: `"Are these the same factual claim about the Solar System?"`
- After: report bucket counts per category + cluster count + which indices were merged. Drop duplicates (keep first per cluster) into `FACTS_DEDUPED`. Must retain >= 15 items or FAIL.

### Stage 2 — Distill: filter surprising + rank educational (complex judgments)
- Say what you are about to do.
- **Filter** `FACTS_DEDUPED` with `mcp_typesafe_filter_items`, criterion: `"This fact is surprising, commonly misunderstood, or exam-worthy for a high-school student (not trivial trivia)."`
  - Save `matched`, `probabilities`. Compute retention rate. Require >= 5 matches or lower threshold note + continue with top-5 by probability.
- **Rank** the matched subset with `mcp_typesafe_rank_items`:
  - query: `"How valuable is this fact for a high-school Solar System study guide (accuracy + insight + memorability)?"`
  - levels: `["Low educational value", "Moderate value", "High value", "Essential must-know"]`
  - Keep full `ranked` order + `normalized` scores. Top-5 become `TOP5`.
- After: report how many survived filtering, top-3 ranked indices + scores in one line each.

### Stage 3 — Deepen: parallel judgments + structured extraction (combination complexity)
- Say what you are about to do.
- For each of `TOP5` (5 calls, or 1 batched loop — do all 5), call `mcp_typesafe_systemone` with ONE state = the fact text and THREE parallel questions:
  - `is_precise` (noul): `"Does this fact make a precise, verifiable claim (not vague)?"`
  - `fact_domain` (choice): options from Stage 1 categories (formation_structure / planets_moons / physical_processes / exploration_history / general)
  - `memorability` (score): levels `["Forgettable", "Somewhat memorable", "Memorable", "Unforgettable"]`
- **Extract structured fields** for each of `TOP5` with `mcp_typesafe_extract_fields`, text = fact:
  - fields: `primary_body: {description: "main body the fact is about", options: ["Mercury","Venus","Earth","Mars","Jupiter","Saturn","Uranus","Neptune","Sun","Moon","General","not stated"]}`, `era: {description: "time reference", options: ["ancient","modern","future","timeless","not stated"]}`, `has_number: {description: "contains a number/date/measurement", options: ["yes","no","not stated"]}`
- After: report per-fact domain agreement (systemone choice vs Stage-1 bucket: agree/disagree), mean precise score, and any low-confidence flags that would need escalation.

### Stage 4 — Trust: stability + groundedness + format choice
- Say what you are about to do.
- **Stability**: pick the #1 ranked fact, run `mcp_typesafe_stability_check` with samples 5, question `is_essential` (noul): `"Is this fact essential for a high-school study guide?"` Expect mean > 0.6. Report mean/std/min. If std > 0.3, flag `unstable — human review needed` but continue.
- **Groundedness**: call `mcp_typesafe_check_claims` with evidence = first 4000 chars of fetched page text, claims = `TOP5` texts. Record supported/contradicted/unverified counts. Any contradicted fact must be removed from TOP5 (note removal) — this is the trust filter raw LLMs skip.
- **Format choice**: call `mcp_typesafe_choose_one`, request: `"Best presentation for 5 high-value Solar System facts for a student?"`, candidates: `[{id:"ranked_guide", description:"ranked study guide with why-it-matters per fact"}, {id:"faq", description:"FAQ question-answer list"}, {id:"timeline", description:"chronological timeline"}]`, min_fit default. Record winner + confidence.
- **Safety/quality gate**: call `mcp_typesafe_verify_output` with output = your draft final guide markdown (first version), sources = page excerpt, instructions = `"Accurate student study guide, no hallucinations, no offensive content, include sources."`, plus custom check `{id:"student_useful", criterion:"Each fact has a why-it-matters line useful to a student."}` Require `failed == []` or fix and re-verify once.
- After: report stability numbers, claim counts, chosen format, verify pass/fail in 3-4 lines total.

## Final artifact (nice markdown, mandatory)

Present under heading `# Solar System Study Guide (TypeSafe-Verified)`. Must start with explicit PRESENTATION of WHAT THIS IS FOR AND WHAT IT TESTED, then include:

0. Purpose: 2-3 lines — what this pipeline is for (meaningful human result + TypeSafe calibration beyond raw LLM).
0. What was tested: bullet list — stages + tools used + data volume (e.g. 25 facts, 5 domains, TOP5 deep-judged).
1. One-line provenance: page URL + fetch date + model name.
2. Pipeline summary table (real markdown table): stage | operation | input size | output size | key result.
3. Ranked guide table (real markdown table): rank | fact | domain | primary body | grounded? | why it matters (1 line you write, grounded in fact).
4. Trust notes: deduped count, filtered count, stability mean/std, check_claims counts, verify_output scores.
5. Sources line: page URL.

Keep why-it-matters lines short and non-hallucinated. If a fact was removed as contradicted, show it in a small `Removed` note, not in the main table.

## Pass criteria

- >= 20 input facts, >= 15 after dedupe, >= 5 after filter (or documented fallback to top-5).
- All of these used at least once: list_models, verify_tool_calls, classify_items, dedupe_items, filter_items, rank_items, systemone (all 3 types in parallel), extract_fields, stability_check, check_claims, choose_one, verify_output.
- Narration present: plan upfront + pre/post per stage.
- Final markdown has 2 real tables + provenance + trust notes.
- Any FAIL stopped with reason; otherwise Verdict ALL PASS with coverage note.
