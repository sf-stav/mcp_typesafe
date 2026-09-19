use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::{
        stdio,
        sse_server::{SseServer, SseServerConfig},
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::{BTreeMap, BTreeSet};
use tracing_subscriber::EnvFilter;

// ===== Constants =====

const MAX_BULK_ITEMS: usize = 1000;
const CHUNK_CHARS: usize = 64_000;
const MAX_QUESTIONS_PER_CALL: usize = 200;
const MAX_DEDUPE_ITEMS: usize = 64;
const PAIRS_PER_CALL: usize = 150;

// ===== CLI Arguments =====

#[derive(Debug, Clone, ValueEnum)]
enum Transport {
    Stdio,
    Sse,
}

#[derive(Debug, Clone, ValueEnum)]
enum LogDestination {
    Stdout,
    Stderr,
    File,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "MCP TypeSafe System One Server", long_about = None)]
struct Args {
    #[arg(short, long, value_enum, default_value = "stdio")]
    transport: Transport,

    #[arg(long, default_value = "127.0.0.1:8000")]
    bind: String,

    #[arg(long, default_value = "/sse")]
    sse_path: String,

    #[arg(long, default_value = "/message")]
    post_path: String,

    #[arg(long, value_enum, default_value = "stderr")]
    log_destination: LogDestination,

    #[arg(long, default_value = "mcp-typesafe.log")]
    log_file: String,

    #[arg(long, default_value = "info")]
    log_level: String,

    /// TypeSafe API key (required for tool calls); can also be provided via TYPESAFE_API_KEY
    #[arg(long, env = "TYPESAFE_API_KEY")]
    typesafe_api_key: Option<String>,

    /// TypeSafe API base URL (defaults to https://api.typesafe.ai; mainly useful for testing/staging)
    #[arg(long, env = "TYPESAFE_BASE_URL", default_value = "https://api.typesafe.ai")]
    typesafe_base_url: String,
}

// ===== Tool Parameter Schemas =====

fn default_model() -> String {
    "jev-latest".to_string()
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SystemOneArgs {
    #[schemars(description = "Required. The content to evaluate: a plain string, or a JSON object/array for structured context such as records, conversations, policies, or application state. Include only the context the questions need.")]
    state: JsonValue,

    #[schemars(description = "Required. Map of question id -> question object, e.g. {\"urgent\": {\"type\": \"noul\", \"instructions\": \"Does this message express urgency?\"}}. You choose the ids; they are not sent to the model, and answers come back under the same keys. Ask every independent question you might need in one call - questions are evaluated in parallel against the same state.")]
    questions: BTreeMap<String, Question>,

    #[schemars(description = "Optional. Model name or alias that handles the request, e.g. \"jev-latest\" (default) or \"jev-preview\". Use the list_models tool to see the names your account can send.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Question {
    Choice {
        #[schemars(description = "Required. The question the model answers - state exactly what to decide. A string is usual; an object or array of labeled fields can add structure when it aids clarity.")]
        instructions: JsonValue,

        #[schemars(description = "Required. Map of option name -> description of that option. Values may be a string, an object/array, or null when the name is self-explanatory. Option names and descriptions are both sent to the model; write descriptions that separate the options. Up to 255 options.")]
        criteria: BTreeMap<String, JsonValue>,
    },

    Score {
        #[schemars(description = "Required. The question the model answers - state exactly what to rate. A string is usual; an object or array of labeled fields can add structure when it aids clarity.")]
        instructions: JsonValue,

        #[schemars(length(min = 2, max = 10), description = "Required. Ordered array of level descriptions, from the low end of the scale to the high end (2-10 entries). Each entry is a string or a structured object with labeled fields. Describe concrete situations, not degrees; each level is judged on its own and the model does not see level numbers.")]
        criteria: Vec<JsonValue>,
    },

    Noul {
        #[schemars(description = "Required. The yes/no question to evaluate. Phrase it so a high value means \"yes\" for the thing you are checking.")]
        instructions: JsonValue,

        #[schemars(description = "Optional. Descriptions of what a yes (value near 1) and a no (value near 0) mean. Use when the boundary between yes and no is subtle.")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "What a yes (value near 1) means.")]
    true_meaning: Option<JsonValue>,

    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "What a no (value near 0) means.")]
    false_meaning: Option<JsonValue>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FilterItemsArgs {
    #[schemars(description = "Required. The items to evaluate: an array of strings or objects (search results, messages, documents, log lines, ...). Each item is judged independently; results are reported by index (`items[0]`, `items[1]`, ...). Items are batched server-side.")]
    items: Vec<JsonValue>,

    #[schemars(description = "Required. What each item is checked for, written as a yes/no question or a statement to verify (e.g. \"Does this describe a payment failure?\" or \"This text contains a date\"). A string is usual; structured criteria are allowed. A high probability means the criterion holds for the item.")]
    criterion: JsonValue,

    #[schemars(description = "Optional. Probability at or above which an item is reported as matched (0-1). Defaults to 0.5. The full per-item probability list is always returned, so you can re-threshold yourself.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    threshold: Option<f64>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct RankItemsArgs {
    #[schemars(description = "Required. The items to rank: an array of strings or objects. Each item is scored independently against the query; results are reported by index (`items[0]`, `items[1]`, ...). Items are batched server-side.")]
    items: Vec<JsonValue>,

    #[schemars(description = "Required. Natural-language description of what 'best' means, e.g. \"how relevant this search result is to the user's question about refunds\".")]
    query: String,

    #[schemars(length(min = 2, max = 10), description = "Required. Ordered level descriptions (2-10) from the low end of the scale to the high end, scored per item. Describe concrete situations, not degrees. E.g. [\"Not relevant at all\", \"Tangentially related\", \"Directly answers the query\"].")]
    levels: Vec<JsonValue>,

    #[schemars(description = "Optional. How many top-ranked items to report in detail. Defaults to 10. The full descending index order is always returned under 'ranked'.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_k: Option<usize>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ClassifyItemsArgs {
    #[schemars(description = "Required. The items to classify: an array of strings or objects. Each item is judged independently; results are reported by index (`items[0]`, `items[1]`, ...). Items are batched server-side.")]
    items: Vec<JsonValue>,

    #[schemars(description = "Required. Map of category name -> short description (or null when the name is self-explanatory). Write descriptions that separate the categories. Up to 255 categories.")]
    categories: BTreeMap<String, JsonValue>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CandidateSpec {
    #[schemars(description = "Required. Short identifier for this candidate (used as the Choice option name and returned as the result). Must be unique.")]
    id: String,

    #[schemars(description = "Optional. One-line description used in the first (wide skim) pass.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    #[schemars(description = "Optional. Longer detail (full description, instructions, rationale, ...) used in the second (verify) pass for the shortlist. Falls back to 'description' when omitted.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Candidate {
    Plain(String),
    Detailed(CandidateSpec),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ChooseOneArgs {
    #[schemars(description = "Required. The request or task the best candidate must serve, described in natural language.")]
    request: String,

    #[schemars(description = "Required. The candidates to choose between: an array of strings, or of objects {id, description?, details?}. Up to 255. With many candidates, a wider 'description' helps the skim pass and 'details' sharpens the verify pass.")]
    candidates: Vec<Candidate>,

    #[schemars(description = "Optional. How many candidates the second pass re-examines. Defaults to 3.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shortlist_size: Option<usize>,

    #[schemars(description = "Optional. Minimum absolute 'fit' probability (0-1) the winner must reach; below it, the result is rejected (choice: null). Defaults to 0.3.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_fit: Option<f64>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct StabilityCheckArgs {
    #[schemars(description = "Required. The content to evaluate, same as the systemone tool's 'state'.")]
    state: JsonValue,

    #[schemars(description = "Required. The questions to sample (same typed questions as the systemone tool, e.g. noul/choice/score with instructions and criteria). Ask coherent judgments; a fresh throwaway value is added to object states so each sample is an independent draw.")]
    questions: BTreeMap<String, Question>,

    #[schemars(description = "Optional. Number of samples to draw (2-20). Defaults to 5.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    samples: Option<u32>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum CheckSpec {
    Plain(String),
    Detailed {
        #[schemars(description = "Optional. Identifier for this check in the results. Defaults to custom_N.")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[schemars(description = "Required. The check to run, phrased so a HIGH probability means the check PASSES (e.g. \"Does the answer cite concrete evidence from `sources`?\").")]
        criterion: JsonValue,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct VerifyOutputArgs {
    #[schemars(description = "Required. The artifact to verify: the draft answer, summary, message, code snippet, or other content you are about to rely on or send.")]
    output: JsonValue,

    #[schemars(description = "Optional. What the output was supposed to be or do (the original task instructions). When provided, a compliance check is added.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<JsonValue>,

    #[schemars(description = "Optional. Supporting evidence the output should be grounded in (documents, search results, tool outputs, ...). When provided, groundedness and consistency checks are added.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sources: Option<JsonValue>,

    #[schemars(description = "Optional. Extra checks to run, as strings or {id, criterion} objects. Phrase each so a HIGH probability means the check PASSES.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checks: Option<Vec<CheckSpec>>,

    #[schemars(description = "Optional. Probability below which a check is listed as failed (0-1). Defaults to 0.5.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flag_threshold: Option<f64>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ToolInvocation {
    #[schemars(description = "Required. The tool name exactly as it appears in the tool list.")]
    name: String,

    #[schemars(description = "Required. The arguments object the call would use (may be empty).")]
    #[serde(default)]
    arguments: JsonValue,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ToolDefinition {
    #[schemars(description = "Optional. The tool's description.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<JsonValue>,

    #[schemars(description = "Optional. The tool's parameter schema (JSON Schema). When present, each call's arguments are checked against it.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parameters: Option<JsonValue>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct VerifyToolCallsArgs {
    #[schemars(description = "Required. The user request the calls are meant to serve.")]
    request: String,

    #[schemars(description = "Required. The tool calls to check before executing: an array of {name, arguments} objects.")]
    calls: Vec<ToolInvocation>,

    #[schemars(description = "Required. The available tools, as a map of tool name -> {description?, parameters?}. Calls naming tools outside this map are reported as warnings.")]
    tools: BTreeMap<String, ToolDefinition>,

    #[schemars(description = "Optional. Probability below which a check is listed as failed (0-1). Defaults to 0.5.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flag_threshold: Option<f64>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum ClaimSpec {
    Plain(String),
    Detailed {
        #[schemars(description = "Required. The claim text.")]
        claim: String,
        #[schemars(description = "Optional. Evidence specific to this claim (quote, passage, record). Falls back to the global 'evidence' when omitted.")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<JsonValue>,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CheckClaimsArgs {
    #[schemars(description = "Required. The claims to check: an array of strings, or of {claim, source?} objects.")]
    claims: Vec<ClaimSpec>,

    #[schemars(description = "Optional (but required when no claim carries its own 'source'). The global evidence the claims are checked against: text, quotes, records, or documents.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<JsonValue>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct DedupeItemsArgs {
    #[schemars(description = "Required. The items to compare pairwise: an array of strings or objects (records, names, search results, ...). Up to 64 items (all pairs are compared).")]
    items: Vec<JsonValue>,

    #[schemars(description = "Optional. What 'same thing' means, written as a yes/no question. Defaults to same real-world entity detection (duplicate, alias, same thing under different wording).")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    criterion: Option<JsonValue>,

    #[schemars(description = "Optional. Probability at or above which a pair is treated as a duplicate edge (0-1). Defaults to 0.75.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    threshold: Option<f64>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FieldOptionObj {
    #[schemars(description = "Required. The option value (a short string, e.g. \"open\", \"net_30\", \"March\").")]
    value: String,

    #[schemars(description = "Optional. Description of when this option applies.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum FieldOption {
    Plain(String),
    Detailed(FieldOptionObj),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FieldSpecObj {
    #[schemars(description = "Optional. What the field means (helps disambiguate).")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    #[schemars(description = "Required. The allowed values this field can take.")]
    options: Vec<FieldOption>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum FieldSpec {
    Options(Vec<FieldOption>),
    Detailed(FieldSpecObj),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ExtractFieldsArgs {
    #[schemars(description = "Required. The text to extract from (a string or structured content holding the text).")]
    text: JsonValue,

    #[schemars(description = "Required. Map of field name -> either a plain array of allowed values, or {description?, options}. Each field becomes one Choice question over its options. Include a 'not stated' option yourself when absence is possible.")]
    fields: BTreeMap<String, FieldSpec>,

    #[schemars(description = "Optional. Model name or alias. Defaults to \"jev-latest\".")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

// ===== Server Struct =====

#[derive(Clone)]
pub struct TypeSafeServer {
    http_client: reqwest::Client,
    typesafe_api_key: Option<String>,
    typesafe_base_url: String,
    tool_router: ToolRouter<TypeSafeServer>,
}

impl TypeSafeServer {
    pub fn new(typesafe_api_key: Option<String>, typesafe_base_url: String) -> Self {
        let http_client = reqwest::Client::builder()
            .user_agent(concat!("mcp-typesafe/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            http_client,
            typesafe_api_key,
            typesafe_base_url,
            tool_router: Self::tool_router(),
        }
    }

    fn require_api_key(&self) -> Result<&str, McpError> {
        self.typesafe_api_key.as_deref().ok_or_else(|| {
            McpError::internal_error(
                "TypeSafe API key not configured",
                Some(json!({"error": "Pass --typesafe-api-key or set TYPESAFE_API_KEY environment variable"})),
            )
        })
    }

    async fn api_systemone(
        &self,
        state: &JsonValue,
        questions: &BTreeMap<String, Question>,
        model: &str,
    ) -> Result<JsonValue, McpError> {
        let api_key = self.require_api_key()?;
        let body = json!({
            "state": state,
            "model": model,
            "questions": questions,
        });

        let url = format!("{}/v1/systemone", self.typesafe_base_url.trim_end_matches('/'));
        let response = self
            .http_client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await
            .map_err(internal_error)?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 => " (check the configured TypeSafe API key)",
                429 => " (rate limit exceeded; retry after a short delay)",
                529 => " (TypeSafe is temporarily overloaded; retry after a short delay)",
                _ => "",
            };
            return Err(internal_error(format!(
                "TypeSafe API returned HTTP {}{}: {}",
                status, hint, error_text
            )));
        }

        response.json().await.map_err(internal_error)
    }

    async fn api_get_models(&self) -> Result<JsonValue, McpError> {
        let api_key = self.require_api_key()?;
        let url = format!("{}/v1/models", self.typesafe_base_url.trim_end_matches('/'));
        let response = self
            .http_client
            .get(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(internal_error)?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(internal_error(format!(
                "TypeSafe API returned HTTP {}: {}",
                status, error_text
            )));
        }

        response.json().await.map_err(internal_error)
    }
}

// ===== Tool Implementations =====

#[tool_router]
impl TypeSafeServer {
    #[tool(description = "Evaluate a state against one or more typed questions using TypeSafe's System One model (Jev). Returns structured answers your code can branch on directly - no generated text. Question types: 'choice' (pick one option from a fixed set; answer: chosen option + probability per option + confidence), 'score' (position on ordered, described levels; answer: score + probability per level + confidence), 'noul' (yes/no; answer: probability that it is true). All questions are evaluated in parallel against the same state, so ask every independent question you might need together (speculative questions are fine; use only the answers you need). Only 'state' and 'questions' are required; 'model' defaults to \"jev-latest\".")]
    async fn systemone(
        &self,
        Parameters(args): Parameters<SystemOneArgs>,
    ) -> Result<CallToolResult, McpError> {
        validate_questions(&args.questions)?;

        let model = args.model.clone().unwrap_or_else(default_model);

        tracing::info!(
            "System One request: {} question(s), model '{}'",
            args.questions.len(),
            model
        );

        let output = self.api_systemone(&args.state, &args.questions, &model).await?;

        ok_json(&output)
    }

    #[tool(description = "List the TypeSafe models and aliases your account can send in the 'model' field of the systemone tool. Returns each model's name, description, and release date.")]
    async fn list_models(&self) -> Result<CallToolResult, McpError> {
        tracing::info!("Listing TypeSafe models");
        let output = self.api_get_models().await?;
        ok_json(&output)
    }

    // ===== High-Level Tools =====

    #[tool(description = "Screen a list of items against one criterion in a fast batched call and return which ones match. One yes/no judgment is evaluated per item (items are chunked and parallelized server-side), so this handles hundreds of items far more cheaply and reliably than reading the list yourself. Use for triage, filtering search results, finding relevant messages/documents/log lines. Returns {threshold, matched_count, matched: [[index, probability], ...], probabilities: [p per item, by index]}.")]
    async fn filter_items(
        &self,
        Parameters(args): Parameters<FilterItemsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.items.is_empty() {
            return Err(invalid_params_error("'items' must contain at least one item"));
        }
        if args.items.len() > MAX_BULK_ITEMS {
            return Err(invalid_params_error(format!(
                "'items' has {} entries; the maximum is {} (split the list into several calls)",
                args.items.len(),
                MAX_BULK_ITEMS
            )));
        }
        let threshold = args.threshold.unwrap_or(0.5).clamp(0.0, 1.0);
        let model = args.model.clone().unwrap_or_else(default_model);
        let ranges = chunk_ranges(&args.items);
        tracing::info!(
            "filter_items: {} item(s) in {} batch(es), threshold {}",
            args.items.len(),
            ranges.len(),
            threshold
        );

        let mut probabilities = vec![0.0f64; args.items.len()];
        for (start, end) in &ranges {
            let chunk = &args.items[*start..*end];
            let mut questions: BTreeMap<String, Question> = BTreeMap::new();
            for i in 0..chunk.len() {
                questions.insert(
                    format!("item_{i}"),
                    noul_qv(scoped_instructions(
                        args.criterion.clone(),
                        format!("`items[{i}]`"),
                        "Judge only the scoped item; ignore every other item in `items`.",
                    )),
                );
            }
            let state = json!({ "items": chunk });
            let body = self.api_systemone(&state, &questions, &model).await?;
            for i in 0..chunk.len() {
                probabilities[*start + i] = answer_noul(&body, &format!("item_{i}"))?;
            }
        }

        let matched: Vec<JsonValue> = probabilities
            .iter()
            .enumerate()
            .filter(|(_, p)| **p >= threshold)
            .map(|(i, p)| json!([i, p]))
            .collect();

        ok_json(&json!({
            "threshold": threshold,
            "matched_count": matched.len(),
            "matched": matched,
            "probabilities": probabilities,
        }))
    }

    #[tool(description = "Rank a list of items against a natural-language query and return the order. One TypeSafe 'score' question is evaluated per item using your ordered levels (chunked server-side), then items are sorted by score. Use for reranking retrieval results, prioritizing work items, or shortlisting candidates. Returns {level_count, top: [{i, score, normalized, confidence}], ranked: [all indices, best first]}. 'normalized' = score / (level_count - 1).")]
    async fn rank_items(
        &self,
        Parameters(args): Parameters<RankItemsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.items.is_empty() {
            return Err(invalid_params_error("'items' must contain at least one item"));
        }
        if args.items.len() > MAX_BULK_ITEMS {
            return Err(invalid_params_error(format!(
                "'items' has {} entries; the maximum is {} (split the list into several calls)",
                args.items.len(),
                MAX_BULK_ITEMS
            )));
        }
        if args.levels.len() < 2 || args.levels.len() > 10 {
            return Err(invalid_params_error(format!(
                "'levels' has {} entries; ranking needs between 2 and 10 ordered levels",
                args.levels.len()
            )));
        }
        let level_count = args.levels.len();
        let model = args.model.clone().unwrap_or_else(default_model);
        let ranges = chunk_ranges(&args.items);
        tracing::info!(
            "rank_items: {} item(s) in {} batch(es), {} levels",
            args.items.len(),
            ranges.len(),
            level_count
        );

        let mut entries: Vec<(usize, f64, f64, f64)> = Vec::with_capacity(args.items.len());
        for (start, end) in &ranges {
            let chunk = &args.items[*start..*end];
            let mut questions: BTreeMap<String, Question> = BTreeMap::new();
            for i in 0..chunk.len() {
                questions.insert(
                    format!("item_{i}"),
                    Question::Score {
                        instructions: scoped_instructions(
                            json!(&args.query),
                            format!("`items[{i}]`"),
                            "Score the scoped item against the criterion; ignore every other item in `items`.",
                        ),
                        criteria: args.levels.clone(),
                    },
                );
            }
            let state = json!({ "items": chunk });
            let body = self.api_systemone(&state, &questions, &model).await?;
            for i in 0..chunk.len() {
                let id = format!("item_{i}");
                let score = answer_score(&body, &id)?;
                let confidence = answer_confidence(&body, &id)?;
                let normalized = score / (level_count - 1) as f64;
                entries.push((*start + i, score, normalized, confidence));
            }
        }

        entries.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let top_k = args.top_k.unwrap_or(10).clamp(1, entries.len());
        let top: Vec<JsonValue> = entries
            .iter()
            .take(top_k)
            .map(|(i, score, normalized, confidence)| {
                json!({
                    "i": i,
                    "score": score,
                    "normalized": normalized,
                    "confidence": confidence,
                })
            })
            .collect();
        let ranked: Vec<usize> = entries.iter().map(|(i, ..)| *i).collect();

        ok_json(&json!({
            "level_count": level_count,
            "top": top,
            "ranked": ranked,
        }))
    }

    #[tool(description = "Bucket a list of items into categories (one 'choice' question per item, batched server-side). Use for triage, routing, tagging, or labeling at scale. Returns {buckets: {category: [item indices]}, items: [{i, choice, confidence}]}.")]
    async fn classify_items(
        &self,
        Parameters(args): Parameters<ClassifyItemsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.items.is_empty() {
            return Err(invalid_params_error("'items' must contain at least one item"));
        }
        if args.items.len() > MAX_BULK_ITEMS {
            return Err(invalid_params_error(format!(
                "'items' has {} entries; the maximum is {} (split the list into several calls)",
                args.items.len(),
                MAX_BULK_ITEMS
            )));
        }
        if args.categories.is_empty() {
            return Err(invalid_params_error("'categories' must contain at least one category"));
        }
        if args.categories.len() > 255 {
            return Err(invalid_params_error(format!(
                "'categories' has {} entries; the maximum is 255",
                args.categories.len()
            )));
        }
        let model = args.model.clone().unwrap_or_else(default_model);
        let ranges = chunk_ranges(&args.items);
        tracing::info!(
            "classify_items: {} item(s) in {} batch(es), {} categories",
            args.items.len(),
            ranges.len(),
            args.categories.len()
        );

        let mut buckets: BTreeMap<String, Vec<usize>> = args
            .categories
            .keys()
            .map(|key| (key.clone(), Vec::new()))
            .collect();
        let mut items_out: Vec<JsonValue> = Vec::with_capacity(args.items.len());

        for (start, end) in &ranges {
            let chunk = &args.items[*start..*end];
            let mut questions: BTreeMap<String, Question> = BTreeMap::new();
            for i in 0..chunk.len() {
                questions.insert(
                    format!("item_{i}"),
                    Question::Choice {
                        instructions: scoped_instructions(
                            json!("Which category best applies to the scoped item?"),
                            format!("`items[{i}]`"),
                            "Choose the single best-fitting category; ignore every other item in `items`.",
                        ),
                        criteria: args.categories.clone(),
                    },
                );
            }
            let state = json!({ "items": chunk });
            let body = self.api_systemone(&state, &questions, &model).await?;
            for i in 0..chunk.len() {
                let id = format!("item_{i}");
                let choice = answer_choice(&body, &id)?;
                let confidence = answer_confidence(&body, &id)?;
                if let Some(bucket) = buckets.get_mut(&choice) {
                    bucket.push(*start + i);
                }
                items_out.push(json!({
                    "i": *start + i,
                    "choice": choice,
                    "confidence": confidence,
                }));
            }
        }

        ok_json(&json!({
            "buckets": buckets,
            "items": items_out,
        }))
    }

    #[tool(description = "Pick the single best candidate (plan, tool, answer, name) from a list, with calibrated probabilities. Two passes: a wide skim of all candidates, then a closer Choice over the shortlist using fuller descriptions plus one absolute 'does it fit' probability per shortlist candidate. Set min_fit to reject all candidates when none fits. Returns {choice (or null), rejected, confidence, selection_probabilities, shortlist, fits, passes}.")]
    async fn choose_one(
        &self,
        Parameters(args): Parameters<ChooseOneArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.candidates.is_empty() {
            return Err(invalid_params_error("'candidates' must contain at least one candidate"));
        }
        if args.candidates.len() > 255 {
            return Err(invalid_params_error(format!(
                "'candidates' has {} entries; the maximum is 255 (pre-filter with rank_items or a first Choice)",
                args.candidates.len()
            )));
        }
        let shortlist_size = args.shortlist_size.unwrap_or(3).clamp(1, 10);
        let min_fit = args.min_fit.unwrap_or(0.3).clamp(0.0, 1.0);
        let model = args.model.clone().unwrap_or_else(default_model);

        let candidates: Vec<(String, Option<String>, Option<String>)> = args
            .candidates
            .into_iter()
            .map(|candidate| match candidate {
                Candidate::Plain(id) => (id, None, None),
                Candidate::Detailed(spec) => (spec.id, spec.description, spec.details),
            })
            .collect();

        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (id, ..) in &candidates {
            if !seen.insert(id.to_string()) {
                return Err(invalid_params_error(format!("duplicate candidate id '{id}'")));
            }
        }

        tracing::info!(
            "choose_one: {} candidate(s), shortlist {}, min_fit {}",
            candidates.len(),
            shortlist_size,
            min_fit
        );

        let two_passes = candidates.len() > shortlist_size;
        let shortlist: Vec<String> = if two_passes {
            let criteria: BTreeMap<String, JsonValue> = candidates
                .iter()
                .map(|(id, description, _)| {
                    (
                        id.to_string(),
                        description.clone().map(JsonValue::String).unwrap_or(JsonValue::Null),
                    )
                })
                .collect();
            let mut questions: BTreeMap<String, Question> = BTreeMap::new();
            questions.insert(
                "which".to_string(),
                Question::Choice {
                    instructions: json!("Which candidate best fits `request`? Judge each candidate by what its description says; pick the single best fit."),
                    criteria,
                },
            );
            let state = json!({ "request": &args.request });
            let body = self.api_systemone(&state, &questions, &model).await?;
            let mut ranked: Vec<(String, f64)> =
                answer_probabilities(&body, "which")?.into_iter().collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ranked
                .into_iter()
                .take(shortlist_size)
                .map(|(id, _)| id)
                .collect()
        } else {
            candidates.iter().map(|(id, ..)| id.to_string()).collect()
        };

        let shortlist_candidates: Vec<(String, Option<String>, Option<String>)> = shortlist
            .iter()
            .filter_map(|id| {
                candidates
                    .iter()
                    .find(|(candidate_id, ..)| candidate_id == id)
                    .cloned()
            })
            .collect();

        let criteria: BTreeMap<String, JsonValue> = shortlist_candidates
            .iter()
            .map(|(id, description, details)| {
                (
                    id.to_string(),
                    details
                        .clone()
                        .or_else(|| description.clone())
                        .map(JsonValue::String)
                        .unwrap_or(JsonValue::Null),
                )
            })
            .collect();
        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        questions.insert(
            "which".to_string(),
            Question::Choice {
                instructions: json!("Exactly one of these candidates is the best fit for `request`. Which one? Judge what each candidate actually is or does, not just its name."),
                criteria,
            },
        );

        let shortlist_state: Vec<JsonValue> = shortlist_candidates
            .iter()
            .map(|(id, description, details)| {
                json!({
                    "id": id,
                    "text": details.clone().or_else(|| description.clone()),
                })
            })
            .collect();
        for (i, (id, ..)) in shortlist_candidates.iter().enumerate() {
            questions.insert(
                format!("fits_{id}"),
                noul_qv(json!({
                    "criterion": format!("Does `shortlist[{i}]` (candidate '{id}') do the specific thing that `request` asks for, well enough to be chosen?"),
                    "focus": "Judge the capability actually described, not the name.",
                })),
            );
        }

        let state = json!({ "request": &args.request, "shortlist": shortlist_state });
        let body = self.api_systemone(&state, &questions, &model).await?;

        let choice = answer_choice(&body, "which")?;
        let confidence = answer_confidence(&body, "which")?;
        let selection_probabilities = answer_probabilities(&body, "which")?;
        let mut fits: BTreeMap<String, f64> = BTreeMap::new();
        let mut best_fit = 0.0f64;
        for (id, ..) in &shortlist_candidates {
            let p = answer_noul(&body, &format!("fits_{id}"))?;
            best_fit = best_fit.max(p);
            fits.insert(id.to_string(), p);
        }
        let rejected = best_fit < min_fit;

        ok_json(&json!({
            "choice": if rejected { JsonValue::Null } else { JsonValue::String(choice) },
            "rejected": rejected,
            "confidence": confidence,
            "selection_probabilities": selection_probabilities,
            "shortlist": shortlist,
            "fits": fits,
            "passes": if two_passes { 2 } else { 1 },
        }))
    }

    #[tool(description = "Run the same question(s) several times over one state and report how stable each answer is: mean/std/min/max for noul and score, option means and modal share for choice. TypeSafe is fast enough to sample: use this before acting on a high-stakes judgment or when deciding whether an answer is reliable enough to automate. A fresh throwaway value is added to object states so each sample is an independent draw. Returns {samples, answers: {id: {...}}}.")]
    async fn stability_check(
        &self,
        Parameters(args): Parameters<StabilityCheckArgs>,
    ) -> Result<CallToolResult, McpError> {
        validate_questions(&args.questions)?;
        let samples = args.samples.unwrap_or(5).clamp(2, 20);
        let model = args.model.clone().unwrap_or_else(default_model);
        tracing::info!(
            "stability_check: {} question(s), {} sample(s), model '{}'",
            args.questions.len(),
            samples,
            model
        );

        let mut noul_values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut score_values: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
        let mut choice_values: BTreeMap<String, Vec<(String, BTreeMap<String, f64>, f64)>> =
            BTreeMap::new();
        for (id, question) in &args.questions {
            match question {
                Question::Noul { .. } => {
                    noul_values.insert(id.to_string(), Vec::new());
                }
                Question::Score { .. } => {
                    score_values.insert(id.to_string(), Vec::new());
                }
                Question::Choice { .. } => {
                    choice_values.insert(id.to_string(), Vec::new());
                }
            }
        }

        for sample in 0..samples {
            let state = with_sample_token(&args.state, sample);
            let body = self.api_systemone(&state, &args.questions, &model).await?;
            for (id, question) in &args.questions {
                match question {
                    Question::Noul { .. } => {
                        let p = answer_noul(&body, id)?;
                        if let Some(values) = noul_values.get_mut(id) {
                            values.push(p);
                        }
                    }
                    Question::Score { .. } => {
                        let score = answer_score(&body, id)?;
                        let confidence = answer_confidence(&body, id)?;
                        if let Some(values) = score_values.get_mut(id) {
                            values.push((score, confidence));
                        }
                    }
                    Question::Choice { .. } => {
                        let choice = answer_choice(&body, id)?;
                        let probabilities = answer_probabilities(&body, id)?;
                        let confidence = answer_confidence(&body, id)?;
                        if let Some(values) = choice_values.get_mut(id) {
                            values.push((choice, probabilities, confidence));
                        }
                    }
                }
            }
        }

        let mut answers: BTreeMap<String, JsonValue> = BTreeMap::new();

        for (id, values) in &noul_values {
            answers.insert(
                id.to_string(),
                json!({
                    "type": "noul",
                    "mean": mean(values),
                    "std": std_dev(values),
                    "min": values.iter().cloned().fold(f64::INFINITY, f64::min),
                    "max": values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    "values": values,
                }),
            );
        }

        for (id, values) in &score_values {
            let scores: Vec<f64> = values.iter().map(|(score, _)| *score).collect();
            let confidences: Vec<f64> = values.iter().map(|(_, confidence)| *confidence).collect();
            answers.insert(
                id.to_string(),
                json!({
                    "type": "score",
                    "score_mean": mean(&scores),
                    "score_std": std_dev(&scores),
                    "score_min": scores.iter().cloned().fold(f64::INFINITY, f64::min),
                    "score_max": scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    "confidence_mean": mean(&confidences),
                    "values": scores,
                }),
            );
        }

        for (id, values) in &choice_values {
            let mut option_means: BTreeMap<String, Vec<f64>> = BTreeMap::new();
            for (_, probabilities, _) in values {
                for (option, p) in probabilities {
                    option_means.entry(option.to_string()).or_default().push(*p);
                }
            }
            let mut best_option = String::new();
            let mut best_mean = f64::NEG_INFINITY;
            for (option, probs) in &option_means {
                let m = mean(probs);
                if m > best_mean {
                    best_mean = m;
                    best_option = option.to_string();
                }
            }
            let modal_share = if values.is_empty() {
                0.0
            } else {
                values
                    .iter()
                    .filter(|(choice, ..)| *choice == best_option)
                    .count() as f64
                    / values.len() as f64
            };
            let means: BTreeMap<String, f64> = option_means
                .iter()
                .map(|(option, probs)| (option.to_string(), mean(probs)))
                .collect();
            let confidences: Vec<f64> =
                values.iter().map(|(_, _, confidence)| *confidence).collect();
            answers.insert(
                id.to_string(),
                json!({
                    "type": "choice",
                    "choice": best_option,
                    "modal_share": modal_share,
                    "probabilities": means,
                    "confidence_mean": mean(&confidences),
                }),
            );
        }

        ok_json(&json!({
            "samples": samples,
            "answers": answers,
        }))
    }

    #[tool(description = "Independent verification battery over an artifact (draft answer, summary, message, code...) in one fast call. Checks are phrased so a HIGH probability means the check PASSES: 'safe_to_share' (no credentials/secrets/personal data) always; 'grounded' and 'consistent_with_sources' when `sources` is provided; 'complies_with_instructions' when `instructions` is provided; plus custom checks. Returns {checks: {id: probability}, failed: [ids below flag_threshold], flag_threshold}.")]
    async fn verify_output(
        &self,
        Parameters(args): Parameters<VerifyOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let threshold = args.flag_threshold.unwrap_or(0.5).clamp(0.0, 1.0);
        let model = args.model.clone().unwrap_or_else(default_model);

        let mut state = serde_json::Map::new();
        state.insert("output".to_string(), args.output.clone());
        if let Some(instructions) = &args.instructions {
            state.insert("instructions".to_string(), instructions.clone());
        }
        if let Some(sources) = &args.sources {
            state.insert("sources".to_string(), sources.clone());
        }
        let state = JsonValue::Object(state);

        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        questions.insert(
            "safe_to_share".to_string(),
            noul_q("Is `output` safe to share externally - free of credentials, secrets, API keys, or personal data?"),
        );
        if args.sources.is_some() {
            questions.insert(
                "grounded".to_string(),
                noul_q("Are the factual claims in `output` supported by `sources`?"),
            );
            questions.insert(
                "consistent_with_sources".to_string(),
                noul_q("Is `output` consistent with `sources`, with no contradictions?"),
            );
        }
        if args.instructions.is_some() {
            questions.insert(
                "complies_with_instructions".to_string(),
                noul_q("Does `output` comply with `instructions`?"),
            );
        }
        if let Some(custom) = &args.checks {
            for (index, spec) in custom.iter().enumerate() {
                let (id, criterion) = match spec {
                    CheckSpec::Plain(criterion) => (format!("custom_{}", index + 1), json!(criterion)),
                    CheckSpec::Detailed { id, criterion } => (
                        id.clone().unwrap_or_else(|| format!("custom_{}", index + 1)),
                        criterion.clone(),
                    ),
                };
                if questions.contains_key(&id) {
                    return Err(invalid_params_error(format!("duplicate check id '{id}'")));
                }
                questions.insert(id, noul_qv(criterion));
            }
        }

        tracing::info!("verify_output: {} check(s)", questions.len());

        let body = self.api_systemone(&state, &questions, &model).await?;
        let mut checks: BTreeMap<String, f64> = BTreeMap::new();
        let mut failed: Vec<String> = Vec::new();
        for id in questions.keys() {
            let p = answer_noul(&body, id)?;
            if p < threshold {
                failed.push(id.to_string());
            }
            checks.insert(id.to_string(), p);
        }

        ok_json(&json!({
            "checks": checks,
            "failed": failed,
            "flag_threshold": threshold,
        }))
    }

    #[tool(description = "Pre-flight check of planned tool calls: is each call's tool appropriate for the request, and do the arguments conform to the tool's parameter schema? All checks run in parallel in one TypeSafe call. Unknown tools and tools without a parameters schema are reported as warnings. Returns {checks: {id: probability} (high = pass), failed: [below flag_threshold], warnings, flag_threshold}. Use right before executing a batch of tool calls.")]
    async fn verify_tool_calls(
        &self,
        Parameters(args): Parameters<VerifyToolCallsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.calls.is_empty() {
            return Err(invalid_params_error("'calls' must contain at least one tool call"));
        }
        let threshold = args.flag_threshold.unwrap_or(0.5).clamp(0.0, 1.0);
        let model = args.model.clone().unwrap_or_else(default_model);

        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        let mut warnings: Vec<String> = Vec::new();
        for (index, call) in args.calls.iter().enumerate() {
            match args.tools.get(&call.name) {
                Some(definition) => {
                    questions.insert(
                        format!("call_{index}_relevant"),
                        noul_q(&format!(
                            "Is `calls[{index}].name` an appropriate tool to make progress on `request`?"
                        )),
                    );
                    if definition.parameters.is_some() {
                        questions.insert(
                            format!("call_{index}_arguments_conform"),
                            noul_q(&format!(
                                "Do `calls[{index}].arguments` conform to the parameter schema `tools.{}.parameters`?",
                                call.name
                            )),
                        );
                    } else {
                        warnings.push(format!(
                            "call_{index}: tool '{}' has no parameters schema; argument conformance not checked",
                            call.name
                        ));
                    }
                }
                None => {
                    warnings.push(format!(
                        "call_{index}: unknown tool '{}' (not present in `tools`)",
                        call.name
                    ));
                }
            }
        }

        tracing::info!(
            "verify_tool_calls: {} call(s), {} check(s), {} warning(s)",
            args.calls.len(),
            questions.len(),
            warnings.len()
        );

        let mut checks: BTreeMap<String, f64> = BTreeMap::new();
        let mut failed: Vec<String> = Vec::new();
        if !questions.is_empty() {
            let state = json!({
                "request": args.request,
                "calls": args.calls,
                "tools": args.tools,
            });
            let body = self.api_systemone(&state, &questions, &model).await?;
            for id in questions.keys() {
                let p = answer_noul(&body, id)?;
                if p < threshold {
                    failed.push(id.to_string());
                }
                checks.insert(id.to_string(), p);
            }
        }

        ok_json(&json!({
            "checks": checks,
            "failed": failed,
            "warnings": warnings,
            "flag_threshold": threshold,
        }))
    }

    #[tool(description = "Check claims against evidence and classify each as supported / contradicted / unverified (one TypeSafe 'choice' per claim, batched in one call). Provide global `evidence` and/or a per-claim `source`. Use after summarization or research to catch unsupported or contradicted statements. Returns {claims: [{i, status, confidence}], counts: {supported, contradicted, unverified}}.")]
    async fn check_claims(
        &self,
        Parameters(args): Parameters<CheckClaimsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.claims.is_empty() {
            return Err(invalid_params_error("'claims' must contain at least one claim"));
        }
        let model = args.model.clone().unwrap_or_else(default_model);

        let claims: Vec<(String, Option<JsonValue>)> = args
            .claims
            .into_iter()
            .map(|spec| match spec {
                ClaimSpec::Plain(claim) => (claim, None),
                ClaimSpec::Detailed { claim, source } => (claim, source),
            })
            .collect();

        if args.evidence.is_none() && claims.iter().all(|(_, source)| source.is_none()) {
            return Err(invalid_params_error(
                "provide 'evidence', or a 'source' on individual claims, to check against",
            ));
        }

        let claims_state: Vec<JsonValue> = claims
            .iter()
            .map(|(claim, source)| {
                let mut entry = json!({ "claim": claim });
                if let Some(source) = source {
                    entry["source"] = source.clone();
                }
                entry
            })
            .collect();
        let state = json!({ "claims": claims_state, "evidence": args.evidence });

        let criteria: BTreeMap<String, JsonValue> = [
            ("supported", json!("The evidence supports the claim.")),
            ("contradicted", json!("The evidence contradicts the claim.")),
            ("unverified", json!("The evidence neither supports nor contradicts the claim.")),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();

        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        for (index, (_, source)) in claims.iter().enumerate() {
            let evidence_ref = if source.is_some() {
                format!("`claims[{index}].source`")
            } else {
                "`evidence`".to_string()
            };
            questions.insert(
                format!("claim_{index}"),
                Question::Choice {
                    instructions: json!({
                        "question": format!("How does the evidence relate to the claim in `claims[{index}]`?"),
                        "evidence": evidence_ref,
                        "focus": "Judge only this claim; do not use outside knowledge.",
                    }),
                    criteria: criteria.clone(),
                },
            );
        }

        tracing::info!("check_claims: {} claim(s)", claims.len());

        let body = self.api_systemone(&state, &questions, &model).await?;
        let mut counts: BTreeMap<String, u32> = [("supported", 0), ("contradicted", 0), ("unverified", 0)]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        let mut claims_out: Vec<JsonValue> = Vec::with_capacity(claims.len());
        for index in 0..claims.len() {
            let id = format!("claim_{index}");
            let status = answer_choice(&body, &id)?;
            let confidence = answer_confidence(&body, &id)?;
            if let Some(count) = counts.get_mut(&status) {
                *count += 1;
            }
            claims_out.push(json!({
                "i": index,
                "status": status,
                "confidence": confidence,
            }));
        }

        ok_json(&json!({
            "claims": claims_out,
            "counts": counts,
        }))
    }

    #[tool(description = "Find duplicate / same-entity pairs in a list and cluster them: one yes/no judgment per candidate pair (batched server-side) asks whether the two items are the same thing; pairs at or above the threshold become edges and connected components become clusters. Use for cleaning search results, merging contact/company lists, reconciling records. Returns {threshold, edge_count, edges: [[i, j, probability], ...], clusters: [[indices], ...]}. Supports up to 64 items (pairwise); pre-cluster larger lists.")]
    async fn dedupe_items(
        &self,
        Parameters(args): Parameters<DedupeItemsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.items.len() < 2 {
            return Err(invalid_params_error("'items' must contain at least two items"));
        }
        if args.items.len() > MAX_DEDUPE_ITEMS {
            return Err(invalid_params_error(format!(
                "'items' has {} entries; pairwise comparison supports at most {} (split the list or pre-cluster it first)",
                args.items.len(),
                MAX_DEDUPE_ITEMS
            )));
        }
        let threshold = args.threshold.unwrap_or(0.75).clamp(0.0, 1.0);
        let criterion = args.criterion.clone().unwrap_or_else(|| {
            json!("Do these two items refer to the same real-world entity (a duplicate, an alias, or the same thing under different wording)?")
        });
        let model = args.model.clone().unwrap_or_else(default_model);

        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for i in 0..args.items.len() {
            for j in (i + 1)..args.items.len() {
                pairs.push((i, j));
            }
        }
        tracing::info!(
            "dedupe_items: {} item(s), {} pair(s), {} batch(es), threshold {}",
            args.items.len(),
            pairs.len(),
            pairs.len().div_ceil(PAIRS_PER_CALL),
            threshold
        );

        let state = json!({ "items": &args.items });
        let mut edges: Vec<(usize, usize, f64)> = Vec::new();
        for batch in pairs.chunks(PAIRS_PER_CALL) {
            let mut questions: BTreeMap<String, Question> = BTreeMap::new();
            for (i, j) in batch {
                questions.insert(
                    format!("pair_{i}_{j}"),
                    noul_qv(scoped_instructions(
                        criterion.clone(),
                        format!("`items[{i}]` and `items[{j}]`"),
                        "Judge only the two scoped items; ignore every other item in `items`.",
                    )),
                );
            }
            let body = self.api_systemone(&state, &questions, &model).await?;
            for (i, j) in batch {
                let p = answer_noul(&body, &format!("pair_{i}_{j}"))?;
                if p >= threshold {
                    edges.push((*i, *j, p));
                }
            }
        }

        let clusters = clusters_from_edges(args.items.len(), &edges);
        let edges_json: Vec<JsonValue> = edges.iter().map(|(i, j, p)| json!([i, j, p])).collect();

        ok_json(&json!({
            "threshold": threshold,
            "edge_count": edges.len(),
            "edges": edges_json,
            "clusters": clusters,
        }))
    }

    #[tool(description = "Extract fields from text into fixed option sets (one 'choice' question per field, batched in one call). Use when you need normalized, code-consumable values (status, category, currency, date parts...) instead of free-form text; normalization and arithmetic stay in your code. Each field takes a list of options (strings, or {value, description} objects); include your own 'not stated' option when absence is possible. Returns {fields: {name: {value, confidence}}}.")]
    async fn extract_fields(
        &self,
        Parameters(args): Parameters<ExtractFieldsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.fields.is_empty() {
            return Err(invalid_params_error("'fields' must contain at least one field"));
        }
        let model = args.model.clone().unwrap_or_else(default_model);
        let state = json!({ "text": &args.text });

        let mut questions: BTreeMap<String, Question> = BTreeMap::new();
        for (field, spec) in &args.fields {
            let (description, options) = match spec {
                FieldSpec::Options(options) => (None, options),
                FieldSpec::Detailed(detailed) => (detailed.description.clone(), &detailed.options),
            };
            if options.is_empty() {
                return Err(invalid_params_error(format!(
                    "field '{field}' needs at least one option"
                )));
            }
            let mut criteria: BTreeMap<String, JsonValue> = BTreeMap::new();
            for option in options {
                let (value, option_description) = match option {
                    FieldOption::Plain(value) => (value.clone(), None),
                    FieldOption::Detailed(detailed) => {
                        (detailed.value.clone(), detailed.description.clone())
                    }
                };
                let entry = option_description
                    .map(JsonValue::String)
                    .unwrap_or(JsonValue::Null);
                if criteria.insert(value.clone(), entry).is_some() {
                    return Err(invalid_params_error(format!(
                        "field '{field}' has duplicate option value '{value}'"
                    )));
                }
            }
            if criteria.len() > 255 {
                return Err(invalid_params_error(format!(
                    "field '{field}' has {} options; the maximum is 255",
                    criteria.len()
                )));
            }
            let mut instructions = json!({
                "question": format!("Which option is the value of the '{field}' field in `text`?"),
                "focus": "Choose the single best option; options are mutually exclusive.",
            });
            if let Some(description) = description {
                instructions["field_description"] = json!(description);
            }
            questions.insert(
                field.to_string(),
                Question::Choice {
                    instructions,
                    criteria,
                },
            );
        }

        tracing::info!("extract_fields: {} field(s)", questions.len());

        let body = self.api_systemone(&state, &questions, &model).await?;
        let mut values: BTreeMap<String, JsonValue> = BTreeMap::new();
        for field in questions.keys() {
            let value = answer_choice(&body, field)?;
            let confidence = answer_confidence(&body, field)?;
            values.insert(
                field.to_string(),
                json!({ "value": value, "confidence": confidence }),
            );
        }

        ok_json(&json!({ "fields": values }))
    }
}

// ===== Server Handler =====

#[tool_handler]
impl ServerHandler for TypeSafeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mcp-typesafe".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("MCP TypeSafe System One Server".to_string()),
                website_url: None,
                icons: Some(vec![]),
            },
            instructions: Some(
                "MCP Server for TypeSafe System One models (Jev): turn natural language and application state into fast, typed judgments your code can act on directly. Calls complete in about 100ms, so these tools also work inside tight loops.\n\
                 \n\
                 CORE:\n\
                 - systemone: evaluate one state against any typed questions (choice/score/noul) in a single call - use it for custom or one-off judgments. All questions run in parallel, so ask everything you might need together.\n\
                 - list_models: model names and aliases for the 'model' field (default 'jev-latest').\n\
                 \n\
                 BULK OVER MANY ITEMS (use these instead of reading long lists yourself):\n\
                 - filter_items: which of N items match a criterion (probability per item).\n\
                 - rank_items: order N items by a query against ordered levels; returns sorted indices + top details.\n\
                 - classify_items: bucket N items into categories with per-item confidence.\n\
                 - dedupe_items: find duplicate/entity-match pairs and clusters in a list.\n\
                 Items are referenced by index (`items[0]`); the tools batch, chunk, and threshold server-side, and return raw probabilities so code (or you) can re-threshold.\n\
                 \n\
                 SELECTION & STABILITY:\n\
                 - choose_one: pick the best candidate (plan, tool, answer) from a list via skim-then-verify, with calibrated probabilities; can reject all.\n\
                 - stability_check: run a judgment several times and report mean/spread - use before acting on high-stakes answers.\n\
                 \n\
                 VERIFICATION (independent checks; checks are phrased so HIGH probability = pass):\n\
                 - verify_output: groundedness/consistency vs sources, instruction compliance, share-safety.\n\
                 - verify_tool_calls: are planned tool calls relevant and schema-conformant (check before executing).\n\
                 - check_claims: supported / contradicted / unverified per claim against evidence.\n\
                 Each returns per-check probabilities and a 'failed' list below the threshold.\n\
                 \n\
                 STRUCTURE:\n\
                 - extract_fields: pull fields out of text into fixed option sets; normalization stays in code.\n\
                 \n\
                 ANSWERS: choice = option + probability per option + confidence; score = weighted position + per-level probabilities + confidence; noul = probability that it is true. Low confidence or a mid-range noul means 'do not guess': escalate, review, or ask. Keep arithmetic, counting, and date math in code - extract with questions, compute yourself. Questions are independent; do not assume arithmetic relationships between them."
                    .to_string(),
            ),
        }
    }
}

// ===== Helper Functions =====

fn internal_error(msg: impl std::fmt::Display) -> McpError {
    McpError::internal_error("Internal error", Some(json!({"error": msg.to_string()})))
}

fn invalid_params_error(msg: impl std::fmt::Display) -> McpError {
    McpError::invalid_params("Invalid params", Some(json!({"error": msg.to_string()})))
}

fn ok_json(value: &JsonValue) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).map_err(internal_error)?,
    )]))
}

fn noul_q(instruction: &str) -> Question {
    noul_qv(json!(instruction))
}

fn noul_qv(instructions: JsonValue) -> Question {
    Question::Noul {
        instructions,
        criteria: None,
    }
}

fn scoped_instructions(criterion: JsonValue, scope: impl Into<String>, focus: &str) -> JsonValue {
    json!({
        "criterion": criterion,
        "scope": scope.into(),
        "focus": focus,
    })
}

fn validate_questions(questions: &BTreeMap<String, Question>) -> Result<(), McpError> {
    if questions.is_empty() {
        return Err(invalid_params_error(
            "At least one question is required in 'questions'",
        ));
    }
    for (id, question) in questions {
        match question {
            Question::Choice { criteria, .. } => {
                if criteria.is_empty() {
                    return Err(invalid_params_error(format!(
                        "Choice question '{id}' needs at least one option in criteria"
                    )));
                }
                if criteria.len() > 255 {
                    return Err(invalid_params_error(format!(
                        "Choice question '{id}' has {} options; the maximum is 255",
                        criteria.len()
                    )));
                }
            }
            Question::Score { criteria, .. } => {
                if criteria.len() < 2 || criteria.len() > 10 {
                    return Err(invalid_params_error(format!(
                        "Score question '{id}' has {} levels; it needs between 2 and 10",
                        criteria.len()
                    )));
                }
            }
            Question::Noul { .. } => {}
        }
    }
    Ok(())
}

fn chunk_ranges(items: &[JsonValue]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut chars = 0usize;
    for (i, item) in items.iter().enumerate() {
        let size = serde_json::to_string(item).map(|s| s.len() + 8).unwrap_or(64);
        if i > start && (chars + size > CHUNK_CHARS || i - start >= MAX_QUESTIONS_PER_CALL) {
            ranges.push((start, i));
            start = i;
            chars = 0;
        }
        chars += size;
    }
    if start < items.len() {
        ranges.push((start, items.len()));
    }
    ranges
}

fn answer_object<'a>(body: &'a JsonValue, id: &str) -> Result<&'a JsonValue, McpError> {
    body.get("answers")
        .and_then(|answers| answers.get(id))
        .ok_or_else(|| internal_error(format!("TypeSafe response is missing answer '{id}'")))
}

fn answer_noul(body: &JsonValue, id: &str) -> Result<f64, McpError> {
    answer_object(body, id)?
        .get("noul")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| internal_error(format!("answer '{id}' has no numeric 'noul'")))
}

fn answer_choice(body: &JsonValue, id: &str) -> Result<String, McpError> {
    answer_object(body, id)?
        .get("choice")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| internal_error(format!("answer '{id}' has no 'choice'")))
}

fn answer_score(body: &JsonValue, id: &str) -> Result<f64, McpError> {
    answer_object(body, id)?
        .get("score")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| internal_error(format!("answer '{id}' has no numeric 'score'")))
}

fn answer_confidence(body: &JsonValue, id: &str) -> Result<f64, McpError> {
    answer_object(body, id)?
        .get("confidence")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| internal_error(format!("answer '{id}' has no numeric 'confidence'")))
}

fn answer_probabilities(body: &JsonValue, id: &str) -> Result<BTreeMap<String, f64>, McpError> {
    let obj = answer_object(body, id)?
        .get("probabilities")
        .and_then(|v| v.as_object())
        .ok_or_else(|| internal_error(format!("answer '{id}' has no 'probabilities' object")))?;
    let mut out = BTreeMap::new();
    for (key, value) in obj {
        let p = value.as_f64().ok_or_else(|| {
            internal_error(format!("answer '{id}' has a non-numeric probability for '{key}'"))
        })?;
        out.insert(key.clone(), p);
    }
    Ok(out)
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = mean(values);
    (values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

fn with_sample_token(state: &JsonValue, sample: u32) -> JsonValue {
    match state {
        JsonValue::Object(map) => {
            let mut clone = map.clone();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            clone.insert(
                "_stability_sample".to_string(),
                json!(format!("{sample}-{nanos}")),
            );
            JsonValue::Object(clone)
        }
        other => other.clone(),
    }
}

fn clusters_from_edges(n: usize, edges: &[(usize, usize, f64)]) -> Vec<Vec<usize>> {
    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut root = x;
        while parent[root] != root {
            root = parent[root];
        }
        let mut cur = x;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }
    let mut parent: Vec<usize> = (0..n).collect();
    for (i, j, _) in edges {
        let ri = find(&mut parent, *i);
        let rj = find(&mut parent, *j);
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for x in 0..n {
        let root = find(&mut parent, x);
        groups.entry(root).or_default().push(x);
    }
    groups.into_values().filter(|group| group.len() > 1).collect()
}

// ===== Main Function =====

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    setup_logging(&args)?;

    tracing::info!("Starting MCP TypeSafe Server");
    tracing::info!("Transport mode: {:?}", args.transport);

    let server = TypeSafeServer::new(
        args.typesafe_api_key.clone(),
        args.typesafe_base_url.clone(),
    );

    match args.transport {
        Transport::Stdio => {
            tracing::info!("Using stdio transport");
            let service = server.serve(stdio()).await.inspect_err(|e| {
                tracing::error!("Server error: {:?}", e);
            })?;

            service.waiting().await?;
        }
        Transport::Sse => {
            tracing::info!("Using SSE transport on {}", args.bind);
            tracing::info!("SSE endpoint: {}", args.sse_path);
            tracing::info!("POST endpoint: {}", args.post_path);

            let config = SseServerConfig {
                bind: args.bind.parse()?,
                sse_path: args.sse_path,
                post_path: args.post_path,
                ct: tokio_util::sync::CancellationToken::new(),
                sse_keep_alive: None,
            };

            let (sse_server, router) = SseServer::new(config);

            let listener = tokio::net::TcpListener::bind(sse_server.config.bind).await?;
            let ct = sse_server.config.ct.child_token();

            let server_task = axum::serve(listener, router).with_graceful_shutdown(async move {
                ct.cancelled().await;
                tracing::info!("SSE server shutting down");
            });

            tokio::spawn(async move {
                if let Err(e) = server_task.await {
                    tracing::error!(error = %e, "SSE server error");
                }
            });

            let ct = sse_server.with_service(move || server.clone());

            tracing::info!("SSE server running. Press Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
            ct.cancel();
        }
    }

    Ok(())
}

fn setup_logging(args: &Args) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    match args.log_destination {
        LogDestination::Stdout => {
            if matches!(args.transport, Transport::Stdio) {
                anyhow::bail!(
                    "Cannot log to stdout when using stdio transport. Use stderr or file logging."
                );
            }
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stdout)
                .init();
        }
        LogDestination::Stderr => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();
        }
        LogDestination::File => {
            let log_dir = std::env::current_dir().unwrap_or_default();
            let file_appender = tracing_appender::rolling::never(&log_dir, &args.log_file);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(file_appender)
                .with_ansi(false)
                .init();
        }
    }

    Ok(())
}
