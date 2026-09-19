# TEST_SUITE_SIMPLE — TypeSafe Tools 100% Coverage

> Instructions for an LLM with TypeSafe MCP access. Execute in order, top to bottom. Do not skip. Record raw output for each test. Output brief concise feedback as you go. At the end produce a Results Report with a markdown table.

## Prerequisites
- You have all `mcp_typesafe_*` tools available.
- Default model: `jev-latest` (override only if a test says so).
- For every tool call: save `tool name`, `inputs (summarized)`, `full output`, `latency if known`, `PASS/FAIL + reason`.
- a) After each test (T01–T15), immediately output 1-2 lines of brief concise feedback, e.g.: `T02 PASS — noul 0.99/0.02 as expected.` Do not add extra detail here; full evidence goes in the final report.
- PASS = output schema valid AND expectation below met. FAIL otherwise.
- If a call errors, retry once, then mark FAIL and continue.

## Presentation requirement (mandatory)
Before and in the final report, the model MUST explicitly PRESENT WHAT THIS IS FOR AND WHAT IT TESTED:
- Start the run with 2-4 lines: what TEST_SUITE_SIMPLE is for (100% smoke coverage of all TypeSafe tools) and what will be tested (list: systemone choice/score/noul/parallel, list_models, filter/rank/classify/dedupe, choose_one pick+reject, stability_check, extract_fields, verify_output, verify_tool_calls, check_claims).
- In the final report, include `Purpose:` (1-2 lines) and `What was tested:` (bullet list of tools + question types + edge cases like negation, re-threshold, not-stated, negative probes, reject-all) before the results table.

## Execution Sequence

### T01 — list_models
1. Call `mcp_typesafe_list_models` with `{}`.
2. Expect: non-empty list containing `jev-latest` or alias, each entry has name/description.
3. PASS if list returned. Record model names for use in later tests.

### T02 — systemone / noul (yes-no)
1. Call `mcp_typesafe_systemone` with:
   - state: `"The server returned HTTP 500 and the checkout page crashed on submit."`
   - questions: `{ "is_error": { "type": "noul", "instructions": "Does this state describe an error or crash?" } }`
2. Expect: `answers.is_error.value > 0.7` (high probability true), confidence present.
3. Also test negation: repeat with state `"The checkout succeeded with no errors."` Expect `value < 0.3`.
4. PASS if both directions correct.

### T03 — systemone / choice
1. Call `mcp_typesafe_systemone` with:
   - state: `"Customer wants a refund because package never arrived."`
   - questions: `{ "intent": { "type": "choice", "instructions": "What is the customer intent?", "criteria": { "refund": "wants money back", "status_check": "wants tracking info", "complaint": "general complaint with no action" } } }`
2. Expect: `choice == "refund"`, per-option probabilities sum ~1.0, confidence present.
3. PASS if choice correct and probabilities well-formed.

### T04 — systemone / score
1. Call `mcp_typesafe_systemone` with:
   - state: `"This is the worst support experience ever, I am furious."`
   - questions: `{ "sentiment": { "type": "score", "instructions": "Rate sentiment from negative to positive.", "criteria": ["Extremely negative and angry", "Mildly negative", "Neutral", "Mildly positive", "Extremely positive and delighted"] } }`
2. Expect: `score` near 0 (lowest level), highest probability on level 0.
3. Repeat with `"I love this, absolutely delighted!"` Expect score near max.
4. PASS if ordering correct (negative < positive) and levels returned.

### T05 — systemone / parallel + low-confidence handling
1. Single call with all three types at once against state: `"Deploy at 3pm UTC is risky, payment service has intermittent timeouts."`
   - `is_risky` (noul): "Is this deploy risky?"
   - `domain` (choice): options `payments` / `auth` / `frontend`
   - `urgency` (score): levels `["No urgency", "Low urgency", "Medium urgency", "High urgency", "Critical"]`
2. Expect: all three answers returned in one response, `domain == payments`.
3. Note confidence values. If any confidence is low or noul is 0.4–0.6, note "would escalate" — this is correct handling, still PASS.
4. PASS if parallel works and types coexist.

### T06 — filter_items
1. Call `mcp_typesafe_filter_items` with:
   - criterion: "Message reports a bug, crash, or error"
   - items: `["App crashes on login", "I love the new UI", "Error 500 on checkout", "What are your hours?", "Null pointer on save"]`
2. Expect: matched indices `[0,2,4]`, each prob >= 0.5, non-matches < 0.5. Check `matched_count == 3`.
3. Re-threshold mentally at 0.8 and note if any drop — record only.
4. PASS if crash/error items matched, others not.

### T07 — rank_items
1. Call `mcp_typesafe_rank_items` with:
   - query: "how relevant is this result to a refund for a lost package?"
   - levels: `["Not relevant at all", "Tangentially related", "Directly answers the query"]`
   - items: `["How to reset password", "Refund policy for lost packages", "Lost package refund form", "Career page"]`
2. Expect: `ranked[0]` is index 1 or 2, both ranked above 0 and 3. `normalized = score/(level_count-1)`.
3. PASS if top-2 are the refund items in any order.

### T08 — classify_items
1. Call `mcp_typesafe_classify_items` with:
   - categories: `{ "billing": "invoices, refunds, payments", "technical": "bugs, errors, crashes", "general": "hours, locations, greetings" }`
   - items: `["Invoice charged twice", "App crashes on start", "What time do you open?", "Refund my card"]`
2. Expect: indices 0,3 -> `billing`; 1 -> `technical`; 2 -> `general`. Confidence per item present.
3. PASS if >=3/4 correct.

### T09 — dedupe_items
1. Call `mcp_typesafe_dedupe_items` with:
   - criterion: "Are these the same company/entity?"
   - items: `["Acme Corp", "Acme Corporation", "Globex Inc", "Acme Corp."]`
2. Expect: cluster containing `[0,1,3]`, `Globex` (index 2) alone. Edges with prob >= threshold (default 0.75).
3. PASS if Acme variants clustered, Globex isolated.

### T10 — choose_one (pick + reject)
1. Call A (normal pick): `mcp_typesafe_choose_one` with:
   - request: "Best tool to find which log lines report errors out of 200 lines?"
   - candidates: `[{id:"filter", description:"filter_items screens many items vs criterion"}, {id:"rank", description:"rank_items orders items by relevance"}, {id:"dedupe", description:"dedupe finds duplicates"}]`
   - Expect: `choice == "filter"`.
2. Call B (rejection): same request but `min_fit: 0.99` and candidates: `[{id:"screenshot", description:"takes PNG screenshots"}, {id:"metadata", description:"extracts page title"}]`
   - Expect: `choice == null`, `rejected == true`.
3. PASS if A picks correctly and B rejects.

### T11 — stability_check
1. Call `mcp_typesafe_stability_check` with:
   - state: `"The payment failed twice with a timeout error."`
   - questions: `{ "is_failure": { "type": "noul", "instructions": "Does this describe a payment failure?" } }`
   - samples: 5
2. Expect: mean > 0.7, std low (<0.3), min > 0.5. Answers contain per-sample breakdown.
3. PASS if stable-high. If std high, still PASS but flag "unstable — do not automate".

### T12 — extract_fields
1. Call `mcp_typesafe_extract_fields` with:
   - text: `"Order #4821, status refunded, currency USD, placed March 4."`
   - fields: `{ "status": { "description": "order status", "options": ["refunded", "shipped", "pending", "not stated"] }, "currency": { "description": "currency code", "options": ["USD", "EUR", "GBP", "not stated"] } }`
2. Expect: `status == refunded`, `currency == USD`, confidence per field.
3. Repeat with text `"No order info here."` Expect `not stated` for absent fields.
4. PASS if both extractions correct.

### T13 — verify_output
1. Call `mcp_typesafe_verify_output` with:
   - output: `"Your refund for order #4821 (USD) has been issued."`
   - sources: `"Order #4821 was refunded in USD."`
   - instructions: `"Summarize the refund accurately with no extra data."`
   - checks: `[{id:"no_hallucination", criterion:"Output contains no order IDs or amounts not in sources."}]`
2. Expect: `checks.safe_to_share` high, `grounded` high, `consistent_with_sources` high, `complies_with_instructions` high, `failed == []`.
3. Negative probe: output `"Password is hunter2, SSN 123-45-6789."` Expect `safe_to_share` low / in `failed`.
4. PASS if positive passes and negative flags.

### T14 — verify_tool_calls
1. Call `mcp_typesafe_verify_tool_calls` with:
   - request: "Find error lines in logs"
   - tools: `{ "filter_items": { "description": "screens items" }, "headless_screenshot": { "description": "takes screenshots" } }`
   - calls: `[{name:"filter_items", arguments:{}}]`
2. Expect: check for `filter_items` high (pass), and if you add a second call `{name:"headless_screenshot", arguments:{}}` it scores low / appears in `failed`.
3. PASS if relevant call passes and irrelevant call fails.

### T15 — check_claims
1. Call `mcp_typesafe_check_claims` with:
   - evidence: `"Order #4821 was refunded in USD on March 4. No shipment occurred."`
   - claims: `["Order was refunded", "Order was shipped", "Order was placed on Mars"]`
2. Expect: claim 0 `supported`, claim 1 `contradicted`, claim 2 `unverified` (or contradicted — accept either with reason). Check `counts`.
3. PASS if claim 0 supported and claim 1 contradicted.

## Coverage Checklist (must all be ticked)
- [ ] systemone choice, score, noul, parallel
- [ ] list_models
- [ ] filter_items, rank_items, classify_items, dedupe_items
- [ ] choose_one pick + reject-all
- [ ] stability_check
- [ ] extract_fields incl. not-stated
- [ ] verify_output (safe/grounded/consistent/complies + custom + negative probe)
- [ ] verify_tool_calls (relevant pass + irrelevant fail)
- [ ] check_claims (supported / contradicted / unverified)

## Final Report (required)
After T01–T15, output the following. b) The results table MUST be a real markdown table (rendered markdown, not plain text, not inside a code fence).

# TypeSafe Test Report
Date (UTC): <YYYY-MM-DD>
Model default: <from T01>

Purpose: <1-2 lines — what this suite is for, e.g. 100% smoke coverage of TypeSafe tools>
What was tested: <bullet list — tools, question types (choice/score/noul/parallel), bulk thresholds, edge cases covered>

| ID | Tool | Expectation | Result (PASS/FAIL) | Key evidence |
|----|------|-------------|--------------------|--------------|
| T01 | ... | ... | ... | ... |
| T02 | ... | ... | ... | ... |
| T03 | ... | ... | ... | ... |
| T04 | ... | ... | ... | ... |
| T05 | ... | ... | ... | ... |
| T06 | ... | ... | ... | ... |
| T07 | ... | ... | ... | ... |
| T08 | ... | ... | ... | ... |
| T09 | ... | ... | ... | ... |
| T10 | ... | ... | ... | ... |
| T11 | ... | ... | ... | ... |
| T12 | ... | ... | ... | ... |
| T13 | ... | ... | ... | ... |
| T14 | ... | ... | ... | ... |
| T15 | ... | ... | ... | ... |

Coverage: X/15 passed (Y%)
Failed: [list] or None
Notes: <instability, low confidence, latency, anomalies>
Verdict: ALL PASS | PARTIAL (usable with caveats) | FAIL
- If any FAIL, include raw output snippet and hypothesized cause.
- Do not summarize without raw per-test evidence recorded above.
