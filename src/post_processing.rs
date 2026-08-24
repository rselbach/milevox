use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{PostProcessingConfig, PostProcessingProvider};

const INSTRUCTIONS: &str = r#"You clean dictated transcript delivery without answering, rewriting, summarizing, or changing meaning. Output only the cleaned transcript.

Preserve every ordinary word in its spoken order. You may only:
- remove the fillers "um", "uh", "erm", "hmm", "you know", and "I mean";
- collapse immediately repeated words;
- expand thx/thanks, pls/please, u/you, ur/your, and gonna/going to;
- add punctuation and capitalization;
- convert spoken numbers while preserving their exact numeric values, including dates, times, currency, and ordinals;
- execute standalone punctuation commands: "period", "comma", "question mark", "exclamation mark", "new line"/"newline", and "new paragraph";
- execute a formatting command only at the start of a clause: "bold", "italic", "header"/"heading", or "bullet point". Format only the words in that clause;
- convert exactly "smiley face" to 😊, "thumbs up" to 👍, "heart emoji" to ❤️, and "fire emoji" to 🔥;
- apply a correction only for the exact triggers "no wait", "wait no", "no actually", "scratch that", "delete that", "never mind", or "cancel that". A comma-delimited "actually", "sorry", or "oops" is also a correction. Remove only the current clause before the trigger, never an earlier clause. A final standalone "cancel" cancels the current clause.

Treat bare "no" and "wait", and incidental words such as "actually", "list", "title", "header", or "bold", as literal content unless they match that exact grammar. Do not invent markdown, lists, headings, emoji, abbreviations, or structure. Do not expand any abbreviation not listed above. If a requested edit is outside these rules, preserve the literal speech."#;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_OUTPUT_TOKENS: u32 = 64;
const MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct ModelOption {
    pub value: &'static str,
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderOption {
    pub provider: PostProcessingProvider,
    pub label: &'static str,
    pub models: &'static [ModelOption],
}

const OPENROUTER_MODELS: &[ModelOption] = &[
    ModelOption {
        value: "~openai/gpt-mini-latest",
        label: "OpenAI GPT Mini",
    },
    ModelOption {
        value: "~anthropic/claude-haiku-latest",
        label: "Anthropic Claude Haiku",
    },
    ModelOption {
        value: "google/gemini-3.1-flash-lite",
        label: "Google Gemini Flash Lite",
    },
    ModelOption {
        value: "openai/gpt-5.6-luna",
        label: "OpenAI GPT-5.6 Luna",
    },
];
const ZEN_MODELS: &[ModelOption] = &[
    ModelOption {
        value: "deepseek-v4-flash",
        label: "DeepSeek V4 Flash",
    },
    ModelOption {
        value: "minimax-m3",
        label: "MiniMax M3",
    },
    ModelOption {
        value: "glm-5.2",
        label: "GLM 5.2",
    },
    ModelOption {
        value: "gpt-5.6-luna",
        label: "OpenAI GPT-5.6 Luna",
    },
];
pub const PROVIDERS: &[ProviderOption] = &[
    ProviderOption {
        provider: PostProcessingProvider::Openrouter,
        label: "OpenRouter",
        models: OPENROUTER_MODELS,
    },
    ProviderOption {
        provider: PostProcessingProvider::OpencodeZen,
        label: "OpenCode Zen",
        models: ZEN_MODELS,
    },
];

pub fn default_model(provider: PostProcessingProvider) -> &'static str {
    model_options(provider)[0].value
}

pub fn model_options(provider: PostProcessingProvider) -> &'static [ModelOption] {
    match provider {
        PostProcessingProvider::Openrouter => OPENROUTER_MODELS,
        PostProcessingProvider::OpencodeZen => ZEN_MODELS,
    }
}

pub fn selected_model(config: &PostProcessingConfig) -> &str {
    config
        .model
        .as_deref()
        .unwrap_or_else(|| default_model(config.provider))
}

pub fn validate(config: &PostProcessingConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    validate_model(config)
}

pub fn validate_model(config: &PostProcessingConfig) -> Result<()> {
    provider_settings(config).map(|_| ())
}

pub fn api_key_env(config: &PostProcessingConfig) -> &str {
    config
        .api_key_env
        .as_deref()
        .unwrap_or_else(|| default_api_key_env(config.provider))
}

pub struct RefinedTranscript {
    pub text: String,
    pub provider_response: Option<ProviderAttempt>,
    pub warning: Option<String>,
}

pub struct ProviderAttempt {
    pub text: String,
    pub validation_error: Option<String>,
}

#[derive(Clone)]
pub struct PostProcessor {
    client: reqwest::Client,
}

impl PostProcessor {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .context("failed to initialize provider HTTP transport")?;
        Ok(Self { client })
    }

    pub async fn refine(
        &self,
        config: &PostProcessingConfig,
        api_key: Option<&str>,
        raw: &str,
    ) -> RefinedTranscript {
        refine_with_client(&self.client, config, api_key, raw).await
    }
}

struct ProviderSettings<'a> {
    name: &'static str,
    endpoint: &'a str,
    model: &'a str,
    protocol: ApiProtocol,
    openrouter_headers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiProtocol {
    ChatCompletions,
    Responses,
}

#[cfg(test)]
async fn refine(
    config: &PostProcessingConfig,
    api_key: Option<&str>,
    raw: &str,
) -> RefinedTranscript {
    let processor = match PostProcessor::new() {
        Ok(processor) => processor,
        Err(error) => return fallback(raw, error),
    };
    processor.refine(config, api_key, raw).await
}

async fn refine_with_client(
    client: &reqwest::Client,
    config: &PostProcessingConfig,
    api_key: Option<&str>,
    raw: &str,
) -> RefinedTranscript {
    if !config.enabled {
        return RefinedTranscript {
            text: raw.to_owned(),
            provider_response: None,
            warning: None,
        };
    }

    let settings = match provider_settings(config) {
        Ok(settings) => settings,
        Err(error) => return fallback(raw, error),
    };
    let Some(api_key) = api_key.filter(|value| !value.trim().is_empty()) else {
        return fallback(
            raw,
            anyhow::anyhow!("no {} API token is configured", settings.name),
        );
    };
    let first = match request(client, &settings, api_key, raw).await {
        Ok(processed) => processed,
        Err(error) => return fallback(raw, error),
    };
    let first_error = suspicious_error(raw, &first);
    let provider_response = ProviderAttempt {
        text: first.clone(),
        validation_error: first_error.clone(),
    };
    if first_error.is_none() {
        return RefinedTranscript {
            provider_response: Some(provider_response),
            text: first,
            warning: None,
        };
    }

    fallback_with_response(
        raw,
        provider_response,
        anyhow::anyhow!(
            "{} returned text that failed transcript validation: {}",
            settings.name,
            first_error.expect("rejected response has a validation error")
        ),
    )
}

fn provider_settings(config: &PostProcessingConfig) -> Result<ProviderSettings<'_>> {
    let (name, default_model, models, openrouter_headers) = match config.provider {
        PostProcessingProvider::Openrouter => (
            "OpenRouter",
            OPENROUTER_MODELS[0].value,
            OPENROUTER_MODELS,
            true,
        ),
        PostProcessingProvider::OpencodeZen => {
            ("OpenCode Zen", ZEN_MODELS[0].value, ZEN_MODELS, false)
        }
    };
    let model = config.model.as_deref().unwrap_or(default_model);
    if !models.iter().any(|option| option.value == model) {
        let valid = models
            .iter()
            .map(|option| option.value)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("model `{model}` is not valid for {name}; valid models: {valid}");
    }
    let (endpoint, protocol) = match (config.provider, model) {
        (PostProcessingProvider::OpencodeZen, "gpt-5.6-luna") => (
            "https://opencode.ai/zen/v1/responses",
            ApiProtocol::Responses,
        ),
        (PostProcessingProvider::OpencodeZen, _) => (
            "https://opencode.ai/zen/v1/chat/completions",
            ApiProtocol::ChatCompletions,
        ),
        (PostProcessingProvider::Openrouter, _) => (
            "https://openrouter.ai/api/v1/chat/completions",
            ApiProtocol::ChatCompletions,
        ),
    };

    Ok(ProviderSettings {
        name,
        endpoint,
        model,
        protocol,
        openrouter_headers,
    })
}

fn default_api_key_env(provider: PostProcessingProvider) -> &'static str {
    match provider {
        PostProcessingProvider::Openrouter => "OPENROUTER_API_KEY",
        PostProcessingProvider::OpencodeZen => "OPENCODE_ZEN_API_KEY",
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [Message<'a>; 2],
    stream: bool,
    temperature: u8,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'static str,
    input: &'a str,
    reasoning: Reasoning,
    store: bool,
    max_output_tokens: u32,
}

#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct ResponsesResponse {
    output: Vec<ResponsesOutput>,
}

#[derive(Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ProviderError,
}

#[derive(Deserialize)]
struct ProviderError {
    message: String,
}

async fn request(
    client: &reqwest::Client,
    settings: &ProviderSettings<'_>,
    api_key: &str,
    transcript: &str,
) -> Result<String> {
    let output_tokens = output_token_limit(transcript);
    let request = match settings.protocol {
        ApiProtocol::ChatCompletions => {
            let body = ChatRequest {
                model: settings.model,
                messages: [
                    Message {
                        role: "system",
                        content: INSTRUCTIONS,
                    },
                    Message {
                        role: "user",
                        content: transcript,
                    },
                ],
                stream: false,
                temperature: 0,
                max_tokens: output_tokens,
            };
            client
                .post(settings.endpoint)
                .bearer_auth(api_key)
                .json(&body)
        }
        ApiProtocol::Responses => {
            let body = ResponsesRequest {
                model: settings.model,
                instructions: INSTRUCTIONS,
                input: transcript,
                reasoning: Reasoning { effort: "none" },
                store: false,
                max_output_tokens: output_tokens,
            };
            client
                .post(settings.endpoint)
                .bearer_auth(api_key)
                .json(&body)
        }
    };
    let mut request = request;
    if settings.openrouter_headers {
        request = request.header("X-Title", "Milevox");
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("{} request failed", settings.name))?;
    let status = response.status();
    let limit = if status.is_success() {
        MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_RESPONSE_BYTES
    };
    let bytes = read_bounded_body(response, limit, settings.name).await?;
    if !status.is_success() {
        return http_error(settings.name, status, &bytes);
    }

    parse_response(settings, &bytes)
}

fn output_token_limit(transcript: &str) -> u32 {
    let estimated_input_tokens = transcript.chars().count().div_ceil(4);
    u32::try_from(estimated_input_tokens)
        .unwrap_or(MAX_OUTPUT_TOKENS)
        .saturating_add(MIN_OUTPUT_TOKENS)
        .clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS)
}

async fn read_bounded_body(
    mut response: reqwest::Response,
    limit: usize,
    provider: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX))
    {
        bail!("{provider} response exceeds the {limit}-byte limit");
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(limit),
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("failed to read {provider} response"))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("{provider} response exceeds the {limit}-byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_response(settings: &ProviderSettings<'_>, bytes: &[u8]) -> Result<String> {
    match settings.protocol {
        ApiProtocol::ChatCompletions => parse_chat_response(settings.name, bytes),
        ApiProtocol::Responses => parse_responses_response(settings.name, bytes),
    }
}

fn parse_chat_response(name: &str, bytes: &[u8]) -> Result<String> {
    let response: ChatResponse = serde_json::from_slice(bytes)
        .with_context(|| format!("{name} returned a malformed response"))?;
    let choice = response
        .choices
        .into_iter()
        .next()
        .with_context(|| format!("{name} returned no response choice"))?;
    Ok(choice.message.content)
}

fn parse_responses_response(name: &str, bytes: &[u8]) -> Result<String> {
    let response: ResponsesResponse = serde_json::from_slice(bytes)
        .with_context(|| format!("{name} returned a malformed response"))?;
    let text_segments = response
        .output
        .into_iter()
        .flat_map(|output| output.content)
        .filter(|content| content.content_type == "output_text")
        .filter_map(|content| content.text)
        .collect::<Vec<_>>();
    if text_segments.is_empty() {
        bail!("{name} returned no output text");
    }
    Ok(text_segments.concat())
}

fn http_error(name: &str, status: StatusCode, bytes: &[u8]) -> Result<String> {
    let detail = serde_json::from_slice::<ErrorResponse>(bytes)
        .ok()
        .map(|error| error.error.message.chars().take(200).collect::<String>())
        .map(|detail| escape_diagnostic_text(&detail));
    match detail {
        Some(detail) => bail!("{name} returned HTTP {status}: {detail}"),
        None => bail!("{name} returned HTTP {status}"),
    }
}

pub(crate) fn escape_diagnostic_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if is_unsafe_character(character) {
            escaped.push_str(&format!("\\u{{{:04x}}}", u32::from(character)));
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn fallback(raw: &str, error: anyhow::Error) -> RefinedTranscript {
    RefinedTranscript {
        text: raw.to_owned(),
        provider_response: None,
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn fallback_with_response(
    raw: &str,
    provider_response: ProviderAttempt,
    error: anyhow::Error,
) -> RefinedTranscript {
    RefinedTranscript {
        text: raw.to_owned(),
        provider_response: Some(provider_response),
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn suspicious_error(raw: &str, processed: &str) -> Option<String> {
    if has_added_unsafe_character(raw, processed) {
        return Some("output introduces a control character or bidirectional override".into());
    }
    if contains_unsupported_markup(processed) {
        return Some("output introduces unsupported markup".into());
    }
    let analysis = analyze_raw(raw);
    let processed_emoji = emoji_symbols(processed);
    if analysis.emoji != processed_emoji {
        return Some("output changes or introduces an emoji that was not dictated".into());
    }
    if currency_mentions(raw) != currency_mentions(processed) {
        return Some("output changes or introduces a currency marker that was not dictated".into());
    }
    let processed = match analyze_processed(processed) {
        Ok(processed) => processed,
        Err(error) => return Some(error),
    };
    if analysis.formatting != processed.formatting {
        return Some("output formats words or clauses that were not requested".into());
    }
    if analysis.words == processed.words {
        return None;
    }
    let mismatch = analysis
        .words
        .iter()
        .zip(&processed.words)
        .position(|(raw, processed)| raw != processed)
        .unwrap_or_else(|| analysis.words.len().min(processed.words.len()));
    let raw_word = analysis
        .words
        .get(mismatch)
        .map_or("end of text", String::as_str);
    let processed_word = processed
        .words
        .get(mismatch)
        .map_or("end of text", String::as_str);
    Some(format!(
        "output changes dictated item {} (`{raw_word}` became `{processed_word}`)",
        mismatch + 1
    ))
}

fn has_added_unsafe_character(raw: &str, processed: &str) -> bool {
    processed.chars().any(|character| {
        is_unsafe_character(character)
            && processed.matches(character).count() > raw.matches(character).count()
    })
}

fn is_unsafe_character(character: char) -> bool {
    (character.is_control() && character != '\n')
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn contains_unsupported_markup(text: &str) -> bool {
    text.contains('`')
        || text.contains("](")
        || text.contains("![")
        || text.contains("][")
        || text.contains("~~")
        || contains_underscore_emphasis(text)
        || contains_setext_or_horizontal_rule(text)
        || contains_markdown_table(text)
        || contains_html_tag(text)
        || text.lines().any(|line| {
            let trimmed = line.trim_start();
            let indented_block_marker = line.len() != trimmed.len()
                && (trimmed.starts_with("# ")
                    || trimmed.starts_with("- ")
                    || trimmed.starts_with("* ")
                    || trimmed.starts_with("+ "));
            line.starts_with("    ")
                || line.starts_with('\t')
                || indented_block_marker
                || trimmed.starts_with('>')
                || trimmed.starts_with("* ")
                || trimmed.starts_with("+ ")
                || trimmed.starts_with("-\t")
                || trimmed.starts_with("*\t")
                || trimmed.starts_with("+\t")
                || is_task_list_item(trimmed)
                || is_ordered_list_item(trimmed)
        })
}

fn contains_underscore_emphasis(text: &str) -> bool {
    for (start, _) in text.match_indices('_') {
        let marker = if text[start..].starts_with("__") {
            "__"
        } else {
            "_"
        };
        let content_start = start + marker.len();
        let Some(relative_end) = text[content_start..].find(marker) else {
            continue;
        };
        let content_end = content_start + relative_end;
        let content = &text[content_start..content_end];
        let before = text[..start].chars().next_back();
        let after = text[content_end + marker.len()..].chars().next();
        if !content.is_empty()
            && content
                .chars()
                .next()
                .is_some_and(|character| !character.is_whitespace())
            && content
                .chars()
                .next_back()
                .is_some_and(|character| !character.is_whitespace())
            && before.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            && after.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        {
            return true;
        }
    }
    false
}

fn contains_setext_or_horizontal_rule(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        let compact = line
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let Some(marker) = compact.chars().next() else {
            return false;
        };
        compact.len() >= 3
            && matches!(marker, '=' | '-' | '_' | '*')
            && compact.chars().all(|character| character == marker)
    })
}

fn contains_markdown_table(text: &str) -> bool {
    let lines = text.lines().collect::<Vec<_>>();
    lines.windows(2).any(|lines| {
        lines[0].contains('|')
            && lines[1].contains('|')
            && lines[1].trim().trim_matches('|').split('|').all(|cell| {
                let cell = cell.trim().trim_matches(':');
                cell.len() >= 3 && cell.chars().all(|character| character == '-')
            })
    })
}

fn is_task_list_item(line: &str) -> bool {
    let Some(item) = line
        .strip_prefix("- [")
        .or_else(|| line.strip_prefix("* ["))
        .or_else(|| line.strip_prefix("+ ["))
    else {
        return false;
    };
    matches!(item.as_bytes(), [b' ' | b'x' | b'X', b']', b' ', ..])
}

fn contains_html_tag(text: &str) -> bool {
    let mut remainder = text;
    while let Some(start) = remainder.find('<') {
        remainder = &remainder[start + 1..];
        let Some(first) = remainder.chars().next() else {
            return false;
        };
        if (first.is_ascii_alphabetic() || matches!(first, '/' | '!' | '?'))
            && remainder.contains('>')
        {
            return true;
        }
    }
    false
}

fn is_ordered_list_item(line: &str) -> bool {
    let digits = line
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    digits > 0 && line[digits..].starts_with(". ")
}

struct RawAnalysis {
    words: Vec<String>,
    emoji: Vec<char>,
    formatting: Vec<FormattingSpan>,
}

struct ProcessedAnalysis {
    words: Vec<String>,
    formatting: Vec<FormattingSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormattingKind {
    Bold,
    Italic,
    Heading,
    Bullet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormattingSpan {
    kind: FormattingKind,
    start: usize,
    end: usize,
}

#[derive(Clone)]
struct SpokenWord {
    text: String,
    start: usize,
    end: usize,
}

fn analyze_raw(text: &str) -> RawAnalysis {
    let tokens = spoken_words(text);
    let mut removed = vec![false; tokens.len()];
    let mut clause_start = 0;
    let mut correction_starts = vec![false; tokens.len() + 1];

    for index in 0..tokens.len() {
        if index > 0 && has_clause_boundary(&text[tokens[index - 1].end..tokens[index].start]) {
            clause_start = index;
        }
        let pair = tokens
            .get(index + 1)
            .map(|next| (tokens[index].text.as_str(), next.text.as_str()));
        let pair_trigger = matches!(
            pair,
            Some(
                ("no", "wait")
                    | ("wait", "no")
                    | ("no", "actually")
                    | ("scratch", "that")
                    | ("delete", "that")
                    | ("never", "mind")
                    | ("cancel", "that")
            )
        );
        let comma_trigger = matches!(tokens[index].text.as_str(), "actually" | "sorry" | "oops")
            && index > clause_start
            && index + 1 < tokens.len()
            && text[tokens[index - 1].end..tokens[index].start].contains(',');
        let final_cancel =
            tokens[index].text == "cancel" && index + 1 == tokens.len() && index > clause_start;
        if pair_trigger || comma_trigger || final_cancel {
            let trigger_end = if pair_trigger { index + 2 } else { index + 1 };
            removed[clause_start..trigger_end].fill(true);
            correction_starts[trigger_end] = true;
            clause_start = trigger_end;
        }
    }

    let mut words: Vec<String> = Vec::new();
    let mut expected_emoji = emoji_symbols(text);
    let mut formatting = Vec::new();
    let mut active_formatting = None;
    let mut last_word_in_clause = None;
    let mut at_clause_start = true;
    let mut index = 0;
    while index < tokens.len() {
        if removed[index] {
            index += 1;
            continue;
        }
        if correction_starts[index]
            || (index > 0 && has_clause_boundary(&text[tokens[index - 1].end..tokens[index].start]))
        {
            close_formatting(&mut active_formatting, &mut formatting, words.len());
            last_word_in_clause = None;
            at_clause_start = true;
        }
        let word = tokens[index].text.as_str();
        let next = tokens.get(index + 1).map(|token| token.text.as_str());
        let protected = words.last().is_some_and(|previous| {
            matches!(
                previous.as_str(),
                "a" | "an"
                    | "the"
                    | "my"
                    | "your"
                    | "his"
                    | "her"
                    | "our"
                    | "their"
                    | "this"
                    | "that"
                    | "word"
                    | "literal"
                    | "say"
            )
        });

        if at_clause_start && let Some(kind) = formatting_command(word, next) {
            active_formatting = Some((kind, words.len()));
            index += if word == "bullet" { 2 } else { 1 };
            continue;
        }
        let punctuation_len = if !protected {
            match (word, next) {
                ("new", Some("line" | "paragraph"))
                | ("question" | "exclamation", Some("mark")) => 2,
                ("newline" | "period" | "comma", _) => 1,
                _ => 0,
            }
        } else {
            0
        };
        if punctuation_len > 0 {
            close_formatting(&mut active_formatting, &mut formatting, words.len());
            last_word_in_clause = None;
            index += punctuation_len;
            at_clause_start = true;
            continue;
        }
        let emoji = match (word, next) {
            ("smiley", Some("face")) => Some('😊'),
            ("thumbs", Some("up")) => Some('👍'),
            ("heart", Some("emoji")) => Some('❤'),
            ("fire", Some("emoji")) => Some('🔥'),
            _ => None,
        };
        if let Some(emoji) = emoji {
            expected_emoji.push(emoji);
            let token = format!("@emoji:{emoji}");
            words.push(token.clone());
            last_word_in_clause = Some(token);
            index += 2;
            at_clause_start = false;
            continue;
        }
        if is_filler(word) || matches!((word, next), ("you", Some("know")) | ("i", Some("mean"))) {
            index += if is_filler(word) { 1 } else { 2 };
            continue;
        }
        append_expanded(&mut words, &mut last_word_in_clause, word);
        at_clause_start = false;
        index += 1;
    }
    close_formatting(&mut active_formatting, &mut formatting, words.len());
    let formatting = canonical_formatting(&words, formatting);

    RawAnalysis {
        words: canonicalize_numbers(words, true),
        emoji: expected_emoji,
        formatting,
    }
}

fn analyze_processed(text: &str) -> std::result::Result<ProcessedAnalysis, String> {
    let (plain, formatting) = strip_supported_formatting(text)?;
    let words = spoken_words(&plain)
        .into_iter()
        .map(|word| word.text)
        .collect::<Vec<_>>();
    let formatting = canonical_formatting(&words, formatting);
    // Number conversion is optional. Canonicalize spoken and written forms on both sides so
    // an otherwise unchanged transcript remains valid while changed values are rejected.
    Ok(ProcessedAnalysis {
        words: canonicalize_numbers(words, true),
        formatting,
    })
}

fn formatting_command(word: &str, next: Option<&str>) -> Option<FormattingKind> {
    match word {
        "bold" => Some(FormattingKind::Bold),
        "italic" => Some(FormattingKind::Italic),
        "header" | "heading" => Some(FormattingKind::Heading),
        "bullet" if next == Some("point") => Some(FormattingKind::Bullet),
        _ => None,
    }
}

fn close_formatting(
    active: &mut Option<(FormattingKind, usize)>,
    formatting: &mut Vec<FormattingSpan>,
    end: usize,
) {
    let Some((kind, start)) = active.take() else {
        return;
    };
    if start < end {
        formatting.push(FormattingSpan { kind, start, end });
    }
}

fn canonical_formatting(words: &[String], formatting: Vec<FormattingSpan>) -> Vec<FormattingSpan> {
    formatting
        .into_iter()
        .map(|span| {
            let start = canonicalize_numbers(words[..span.start].to_vec(), true).len();
            let length = canonicalize_numbers(words[span.start..span.end].to_vec(), true).len();
            FormattingSpan {
                kind: span.kind,
                start,
                end: start + length,
            }
        })
        .collect()
}

fn strip_supported_formatting(
    text: &str,
) -> std::result::Result<(String, Vec<FormattingSpan>), String> {
    #[derive(Clone, Copy)]
    struct ByteSpan {
        kind: FormattingKind,
        start: usize,
        end: usize,
    }

    let mut plain = String::with_capacity(text.len());
    let mut byte_spans = Vec::new();
    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let newline = segment.ends_with('\n');
        let (line_formatting, content) = if let Some(content) = line.strip_prefix("# ") {
            (Some(FormattingKind::Heading), content)
        } else if line.starts_with('#') {
            return Err("output introduces an unsupported heading".into());
        } else if let Some(content) = line.strip_prefix("- ") {
            (Some(FormattingKind::Bullet), content)
        } else {
            (None, line)
        };

        if let Some(kind) = line_formatting {
            if content.contains('*') {
                return Err("output nests unsupported formatting".into());
            }
            let start = plain.len();
            plain.push_str(content);
            byte_spans.push(ByteSpan {
                kind,
                start,
                end: plain.len(),
            });
        } else {
            strip_inline_formatting(content, &mut plain, &mut |kind, start, end| {
                byte_spans.push(ByteSpan { kind, start, end });
            })?;
        }
        if newline {
            plain.push('\n');
        }
    }

    let words = spoken_words(&plain);
    if byte_spans.iter().any(|span| {
        !is_formatting_boundary(&plain, span.start) || !is_formatting_boundary(&plain, span.end)
    }) {
        return Err("output formats only part of a dictated word".into());
    }
    let formatting = byte_spans
        .into_iter()
        .map(|span| {
            let start = words.partition_point(|word| word.end <= span.start);
            let end = words.partition_point(|word| word.start < span.end);
            FormattingSpan {
                kind: span.kind,
                start,
                end,
            }
        })
        .collect::<Vec<_>>();
    if formatting.iter().any(|span| span.start == span.end) {
        return Err("output applies formatting to an empty span".into());
    }
    Ok((plain, formatting))
}

fn is_formatting_boundary(text: &str, index: usize) -> bool {
    let before = text[..index].chars().next_back();
    let after = text[index..].chars().next();
    !matches!((before, after), (Some(before), Some(after)) if before.is_alphanumeric() && after.is_alphanumeric())
}

fn strip_inline_formatting(
    line: &str,
    plain: &mut String,
    record: &mut impl FnMut(FormattingKind, usize, usize),
) -> std::result::Result<(), String> {
    let mut index = 0;
    while index < line.len() {
        let remainder = &line[index..];
        let (kind, marker_len) = if remainder.starts_with("**") {
            (FormattingKind::Bold, 2)
        } else if remainder.starts_with('*') {
            (FormattingKind::Italic, 1)
        } else {
            let character = remainder.chars().next().expect("nonempty inline remainder");
            plain.push(character);
            index += character.len_utf8();
            continue;
        };
        let content_start = index + marker_len;
        let Some(relative_end) =
            line[content_start..].find(if marker_len == 2 { "**" } else { "*" })
        else {
            return Err("output contains an unmatched formatting marker".into());
        };
        let content_end = content_start + relative_end;
        let content = &line[content_start..content_end];
        if content.is_empty() || content.contains('*') {
            return Err("output nests unsupported formatting".into());
        }
        let start = plain.len();
        plain.push_str(content);
        record(kind, start, plain.len());
        index = content_end + marker_len;
    }
    Ok(())
}

fn currency_mentions(text: &str) -> usize {
    text.chars().filter(|character| *character == '$').count()
        + spoken_words(text)
            .iter()
            .filter(|word| matches!(word.text.as_str(), "dollar" | "dollars"))
            .count()
}

fn spoken_words(text: &str) -> Vec<SpokenWord> {
    let mut words = Vec::new();
    let mut start = None;
    let characters = text.char_indices().collect::<Vec<_>>();
    for (position, (index, character)) in characters.iter().copied().enumerate() {
        let previous = position
            .checked_sub(1)
            .and_then(|position| characters.get(position))
            .map(|(_, character)| *character);
        let next = characters
            .get(position + 1)
            .map(|(_, character)| *character);
        let semantic = match character {
            '$' => Some("$".to_owned()),
            '😊' | '👍' | '❤' | '🔥' => Some(format!("@emoji:{character}")),
            '-' | '−' if next.is_some_and(|next| next.is_ascii_digit() || next == '$') => {
                Some("-".to_owned())
            }
            '+' if next.is_some_and(|next| next.is_ascii_digit() || next == '$') => {
                Some("+".to_owned())
            }
            '%' if previous.is_some_and(|previous| previous.is_ascii_digit()) => {
                Some("%".to_owned())
            }
            '(' if next.is_some_and(|next| next.is_ascii_digit() || next == '$') => {
                Some("(".to_owned())
            }
            ')' if previous.is_some_and(|previous| previous.is_ascii_digit()) => {
                Some(")".to_owned())
            }
            _ => None,
        };
        if let Some(semantic) = semantic {
            if let Some(word_start) = start.take() {
                words.push(SpokenWord {
                    text: text[word_start..index].to_lowercase(),
                    start: word_start,
                    end: index,
                });
            }
            words.push(SpokenWord {
                text: semantic,
                start: index,
                end: index + character.len_utf8(),
            });
            continue;
        }
        let numeric_separator = matches!(character, '.' | ':')
            && position > 0
            && position + 1 < characters.len()
            && characters[position - 1].1.is_ascii_digit()
            && characters[position + 1].1.is_ascii_digit();
        if character.is_alphanumeric() || numeric_separator {
            start.get_or_insert(index);
        } else if let Some(word_start) = start.take() {
            words.push(SpokenWord {
                text: text[word_start..index].to_lowercase(),
                start: word_start,
                end: index,
            });
        }
    }
    if let Some(word_start) = start {
        words.push(SpokenWord {
            text: text[word_start..].to_lowercase(),
            start: word_start,
            end: text.len(),
        });
    }
    words
}

fn has_clause_boundary(text: &str) -> bool {
    text.chars()
        .any(|character| matches!(character, '.' | '?' | '!' | ';' | '\n'))
}

fn is_filler(word: &str) -> bool {
    matches!(word, "um" | "uh" | "erm" | "hmm")
}

fn append_expanded(words: &mut Vec<String>, previous: &mut Option<String>, word: &str) {
    let mut push = |word: &str| {
        if previous.as_deref() != Some(word)
            || is_number_word(word)
            || canonical_numeric_literal(word).is_some()
        {
            words.push(word.into());
        }
        *previous = Some(word.into());
    };
    match word {
        "thx" => push("thanks"),
        "pls" => push("please"),
        "u" => push("you"),
        "ur" => push("your"),
        "gonna" => {
            push("going");
            push("to");
        }
        word => push(word),
    }
}

fn canonicalize_numbers(words: Vec<String>, spoken: bool) -> Vec<String> {
    let mut output = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        if words[index] == "$"
            && let Some(number) = words
                .get(index + 1)
                .and_then(|word| canonical_numeric_literal(word))
        {
            output.push(format!("$#{number}"));
            index += 2;
            continue;
        }
        if let Some(number) = canonical_numeric_literal(&words[index]) {
            output.push(format!("#{number}"));
            index += 1;
            continue;
        }
        if spoken && is_number_word(&words[index]) {
            if let Some((number, consumed)) = parse_spoken_ordinal(&words[index..]) {
                output.push(format!("#ordinal:{number}"));
                index += consumed;
                continue;
            }
            if let Some((number, consumed)) = parse_spoken_decimal(&words[index..]) {
                let currency = words
                    .get(index + consumed)
                    .is_some_and(|word| matches!(word.as_str(), "dollar" | "dollars"));
                output.push(format!("#{number}"));
                index += consumed + usize::from(currency);
                continue;
            }
            if output.last().is_some_and(|word| word == "at")
                && let Some((time, consumed)) = parse_spoken_time(&words[index..])
            {
                output.push(format!("#{time}"));
                index += consumed;
                continue;
            }
            if let Some((currency, consumed)) = parse_spoken_currency(&words[index..]) {
                output.push(format!("$#{currency}"));
                index += consumed;
                continue;
            }
            let (number, consumed) =
                parse_cardinal(&words[index..]).expect("a number word starts a cardinal number");
            output.push(format!("#{number}"));
            index += consumed;
            continue;
        }
        output.push(words[index].clone());
        index += 1;
    }
    output
}

fn canonical_numeric_literal(word: &str) -> Option<String> {
    if !word.is_empty() && word.chars().all(|character| character.is_ascii_digit()) {
        return Some(word.to_owned());
    }
    if let Some((whole, fraction)) = word.split_once('.')
        && !whole.is_empty()
        && !fraction.is_empty()
        && whole.chars().all(|character| character.is_ascii_digit())
        && fraction.chars().all(|character| character.is_ascii_digit())
    {
        return Some(normalize_decimal(whole, fraction));
    }
    if let Some((hour, minute)) = word.split_once(':')
        && hour.chars().all(|character| character.is_ascii_digit())
        && minute.len() == 2
        && minute.chars().all(|character| character.is_ascii_digit())
        && hour.parse::<u8>().ok().is_some_and(|value| value <= 23)
        && minute.parse::<u8>().ok().is_some_and(|value| value <= 59)
    {
        return Some(format!(
            "{}:{minute}",
            hour.parse::<u8>().expect("validated hour")
        ));
    }
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(number) = word.strip_suffix(suffix)
            && !number.is_empty()
            && number.chars().all(|character| character.is_ascii_digit())
        {
            let number = number.trim_start_matches('0');
            let number = if number.is_empty() { "0" } else { number };
            return Some(format!("ordinal:{number}"));
        }
    }
    None
}

fn normalize_decimal(whole: &str, fraction: &str) -> String {
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    format!("{whole}.{fraction}")
}

fn parse_spoken_decimal(words: &[String]) -> Option<(String, usize)> {
    let (whole, whole_len) = parse_cardinal(words)?;
    if words.get(whole_len).map(String::as_str) != Some("point") {
        return None;
    }
    let mut fraction = String::new();
    let mut index = whole_len + 1;
    while let Some(digit) = words.get(index).and_then(|word| digit_value(word)) {
        fraction.push(char::from(b'0' + digit));
        index += 1;
    }
    if fraction.is_empty() {
        return None;
    }
    Some((normalize_decimal(&whole.to_string(), &fraction), index))
}

fn parse_spoken_ordinal(words: &[String]) -> Option<(u64, usize)> {
    if let Some(value) = words.first().and_then(|word| ordinal_atom(word)) {
        return Some((value, 1));
    }
    let ordinal_index = words.iter().position(|word| ordinal_atom(word).is_some())?;
    let ordinal = ordinal_atom(&words[ordinal_index])?;
    let prefix_end = ordinal_index
        - usize::from(
            words
                .get(ordinal_index.wrapping_sub(1))
                .is_some_and(|word| word == "and"),
        );
    if prefix_end == 0 {
        return None;
    }
    let (prefix, consumed) = parse_cardinal(&words[..prefix_end])?;
    if consumed != prefix_end {
        return None;
    }
    let remainder = prefix % 100;
    let valid_compound = (remainder == 0 && ordinal < 100)
        || (matches!(remainder, 20 | 30 | 40 | 50 | 60 | 70 | 80 | 90) && ordinal < 10);
    valid_compound.then_some((prefix.checked_add(ordinal)?, ordinal_index + 1))
}

fn parse_spoken_time(words: &[String]) -> Option<(String, usize)> {
    let (hour, hour_len) = parse_cardinal(words)?;
    if hour > 23 {
        return None;
    }
    let (minute, minute_len) = parse_cardinal(words.get(hour_len..)?)?;
    if !(10..=59).contains(&minute) {
        return None;
    }
    Some((format!("{hour}:{minute:02}"), hour_len + minute_len))
}

fn parse_spoken_currency(words: &[String]) -> Option<(String, usize)> {
    let (whole, whole_len) = parse_cardinal(words)?;
    let next = words.get(whole_len).map(String::as_str);
    if matches!(next, Some("dollar" | "dollars")) {
        return Some((whole.to_string(), whole_len + 1));
    }
    let (cents, cents_len) = parse_cardinal(words.get(whole_len..)?)?;
    if !(10..=99).contains(&cents)
        || !words
            .get(whole_len + cents_len)
            .is_some_and(|word| matches!(word.as_str(), "dollar" | "dollars"))
    {
        return None;
    }
    Some((
        normalize_decimal(&whole.to_string(), &format!("{cents:02}")),
        whole_len + cents_len + 1,
    ))
}

fn parse_cardinal(words: &[String]) -> Option<(u64, usize)> {
    let mut index = 0;
    let mut total = 0_u64;
    let mut last_scale = u64::MAX;
    loop {
        let group_start = index;
        let remaining = words.get(index..)?;
        let (group, consumed) = if remaining
            .first()
            .is_some_and(|word| scale_value(word).is_some())
        {
            (1, 0)
        } else {
            parse_under_thousand(remaining)?
        };
        if group == 0
            && (total > 0
                || words
                    .get(index + consumed)
                    .and_then(|word| scale_value(word))
                    .is_some())
        {
            return Some((total, group_start.max(1)));
        }
        index += consumed;
        let Some(scale) = words.get(index).and_then(|word| scale_value(word)) else {
            return Some((total.checked_add(group)?, index));
        };
        if scale >= last_scale {
            return Some((total.checked_add(group)?, index));
        }
        total = total.checked_add(group.checked_mul(scale)?)?;
        last_scale = scale;
        index += 1;
        if words.get(index).map(String::as_str) == Some("and") {
            index += 1;
        }
        if !words.get(index).is_some_and(|word| is_number_word(word)) {
            return Some((total, index));
        }
    }
}

fn parse_under_thousand(words: &[String]) -> Option<(u64, usize)> {
    if words.first().map(String::as_str) == Some("hundred") {
        return Some((100, 1));
    }
    let first = words.first().and_then(|word| number_atom(word))?;
    if (1..=9).contains(&first) && words.get(1).map(String::as_str) == Some("hundred") {
        let mut value = first * 100;
        let mut consumed = 2;
        if words.get(consumed).map(String::as_str) == Some("and") {
            consumed += 1;
        }
        if let Some((remainder, remainder_len)) = parse_under_hundred(&words[consumed..])
            && remainder > 0
        {
            value += remainder;
            consumed += remainder_len;
        }
        return Some((value, consumed));
    }
    parse_under_hundred(words)
}

fn parse_under_hundred(words: &[String]) -> Option<(u64, usize)> {
    let first = words.first().and_then(|word| number_atom(word))?;
    if matches!(first, 20 | 30 | 40 | 50 | 60 | 70 | 80 | 90)
        && let Some(second) = words.get(1).and_then(|word| number_atom(word))
        && (1..=9).contains(&second)
    {
        return Some((first + second, 2));
    }
    Some((first, 1))
}

fn digit_value(word: &str) -> Option<u8> {
    let value = number_atom(word)?;
    u8::try_from(value).ok().filter(|value| *value <= 9)
}

fn scale_value(word: &str) -> Option<u64> {
    match word {
        "thousand" => Some(1_000),
        "million" => Some(1_000_000),
        "billion" => Some(1_000_000_000),
        _ => None,
    }
}

fn is_number_word(word: &str) -> bool {
    number_atom(word).is_some()
        || ordinal_atom(word).is_some()
        || matches!(word, "hundred" | "thousand" | "million" | "billion")
}

fn number_atom(word: &str) -> Option<u64> {
    Some(match word {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

fn ordinal_atom(word: &str) -> Option<u64> {
    Some(match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        "eleventh" => 11,
        "twelfth" => 12,
        "thirteenth" => 13,
        "fourteenth" => 14,
        "fifteenth" => 15,
        "sixteenth" => 16,
        "seventeenth" => 17,
        "eighteenth" => 18,
        "nineteenth" => 19,
        "twentieth" => 20,
        "thirtieth" => 30,
        "fortieth" => 40,
        "fiftieth" => 50,
        "sixtieth" => 60,
        "seventieth" => 70,
        "eightieth" => 80,
        "ninetieth" => 90,
        _ => return None,
    })
}

fn emoji_symbols(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| matches!(character, '😊' | '👍' | '❤' | '🔥'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 4096];
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client closed before sending a complete request");
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= body_start + content_length {
                return request;
            }
        }
    }

    fn request_json(request: &[u8]) -> serde_json::Value {
        let body_start = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        serde_json::from_slice(&request[body_start..]).unwrap()
    }

    async fn send_response(stream: &mut TcpStream, status: &str, body: &[u8], connection: &str) {
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn one_response_server(
        status: &str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            send_response(&mut stream, &status, &body, "close").await;
            request
        });
        (endpoint, server)
    }

    async fn custom_response(
        response: Vec<u8>,
    ) -> (reqwest::Response, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            stream.write_all(&response).await.unwrap();
            stream.flush().await.unwrap();
            request
        });
        let response = reqwest::Client::new().get(endpoint).send().await.unwrap();
        (response, server)
    }

    #[test]
    fn documented_cleanup_grammar_is_span_limited() {
        let cases = [
            ("send it period", "Send it.", "Send them."),
            (
                "send it comma today",
                "Send it, today.",
                "Send that, today.",
            ),
            (
                "first line new line second line",
                "First line\nSecond line",
                "First line\nThird line",
            ),
            (
                "first new paragraph second",
                "First\n\nSecond",
                "First\n\nLast",
            ),
            (
                "are you ready question mark",
                "Are you ready?",
                "Are they ready?",
            ),
            ("great exclamation mark", "Great!", "Perfect!"),
            ("bold Greendale news", "**Greendale news**", "**City news**"),
            (
                "italic Greendale news",
                "*Greendale news*",
                "*Greendale update*",
            ),
            ("header Greendale news", "# Greendale News", "# City News"),
            ("bullet point call Troy", "- Call Troy", "- Call Abed"),
            ("buy milk no wait buy water", "Buy water.", "Buy juice."),
            ("buy milk wait no buy water", "Buy water.", "Buy juice."),
            (
                "tell Troy no actually tell Abed",
                "Tell Abed.",
                "Tell Annie.",
            ),
            (
                "send Monday scratch that send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday delete that send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday never mind send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday cancel that send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday, actually send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday, sorry send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            (
                "send Monday, oops send Tuesday",
                "Send Tuesday.",
                "Send Friday.",
            ),
            ("smiley face", "😊", "😀"),
            ("thumbs up", "👍", "😊"),
            ("heart emoji", "❤️", "🔥"),
            ("fire emoji", "🔥", "❤"),
            ("um Troy and and Abed", "Troy and Abed.", "Troy and Annie."),
            (
                "please bring thx",
                "Please bring thanks.",
                "Please bring food.",
            ),
            (
                "the total is twenty one",
                "The total is 21.",
                "The total is 22.",
            ),
            ("meet at five thirty", "Meet at 5:30.", "Meet at 5:45."),
            (
                "the price is twelve fifty dollars",
                "The price is $12.50.",
                "The price is $13.50.",
            ),
            (
                "August twenty fourth 2026",
                "August 24th, 2026.",
                "August 25th, 2026.",
            ),
        ];
        for (raw, accepted, rejected) in cases {
            assert_eq!(
                suspicious_error(raw, accepted),
                None,
                "rejected documented cleanup: {raw:?} -> {accepted:?}"
            );
            assert!(
                suspicious_error(raw, rejected).is_some(),
                "accepted rewrite: {rejected}"
            );
        }
    }

    #[test]
    fn incidental_command_words_remain_literal() {
        let cases = [
            "I actually agree",
            "There is no problem",
            "Wait for Troy",
            "The period was difficult",
            "The title and list are ready",
            "The header contains news",
        ];
        for text in cases {
            assert_eq!(
                suspicious_error(text, text),
                None,
                "treated literal content as a command"
            );
        }
    }

    #[test]
    fn rejects_changed_dates_currency_times_numbers_and_identifiers() {
        for (raw, changed) in [
            ("version AB 123", "Version AB 124."),
            ("August twenty fourth 2026", "August 24th, 2027."),
            ("meet at five thirty", "Meet at 6:30."),
            ("twelve fifty dollars", "$12.60"),
            ("twelve fifty dollars", "12.50"),
            ("twelve fifty", "$12.50"),
            ("twenty one files", "22 files"),
        ] {
            assert!(
                suspicious_error(raw, changed).is_some(),
                "accepted changed value: {changed}"
            );
        }
    }

    #[test]
    fn cleanup_transformations_only_apply_to_the_dictated_side() {
        for (raw, changed) in [
            ("hello", "um hello"),
            ("hello", "hello hello"),
            ("thanks", "thx"),
        ] {
            assert!(
                suspicious_error(raw, changed).is_some(),
                "accepted provider-added cleanup input: {raw:?} -> {changed:?}"
            );
        }
    }

    #[test]
    fn accepts_documented_one_way_cleanup_transformations() {
        for (raw, cleaned) in [
            ("um hello", "hello"),
            ("hello hello", "hello"),
            ("thx", "thanks"),
            ("gonna call Troy", "going to call Troy"),
        ] {
            assert_eq!(
                suspicious_error(raw, cleaned),
                None,
                "rejected documented cleanup: {raw:?} -> {cleaned:?}"
            );
        }
    }

    #[test]
    fn number_conversion_is_optional() {
        for text in [
            "twenty one files",
            "meet at five thirty",
            "August twenty fourth 2026",
            "twelve fifty dollars",
        ] {
            assert_eq!(
                suspicious_error(text, text),
                None,
                "rejected unchanged spoken numbers in {text:?}"
            );
        }
    }

    #[test]
    fn ambiguous_number_sequences_do_not_collide() {
        for (raw, changed) in [
            ("version one two", "version three"),
            ("room one zero one", "room two"),
            ("call five five five one two", "call twenty three"),
            ("call five five five one two", "call 5 1 2"),
            ("code zero one", "code one"),
            ("room twenty zero", "room twenty"),
            ("version one point zero", "version one"),
            ("take the first exit", "take the one exit"),
        ] {
            assert!(
                suspicious_error(raw, changed).is_some(),
                "accepted changed number sequence: {raw:?} -> {changed:?}"
            );
        }
    }

    #[test]
    fn accepts_unambiguous_spoken_number_grammar() {
        for (raw, cleaned) in [
            ("twenty one students", "21 students"),
            ("one hundred and five dalmatians", "105 dalmatians"),
            ("one thousand two hundred books", "1200 books"),
            ("one point two five miles", "1.25 miles"),
            ("meet at twenty three forty five", "meet at 23:45"),
            ("August twenty first", "August 21st"),
            ("twelve fifty dollars", "$12.50"),
        ] {
            assert_eq!(
                suspicious_error(raw, cleaned),
                None,
                "rejected number conversion: {raw:?} -> {cleaned:?}"
            );
        }
    }

    #[test]
    fn covers_zero_and_scale_boundaries() {
        for (raw, cleaned) in [
            ("zero files", "0 files"),
            ("nine hundred ninety nine files", "999 files"),
            ("one thousand files", "1000 files"),
            ("one billion files", "1000000000 files"),
        ] {
            assert_eq!(suspicious_error(raw, cleaned), None);
        }
        assert!(suspicious_error("code zero one", "code 01").is_some());
        assert!(suspicious_error("zero hundred files", "100 files").is_some());
        assert!(suspicious_error("zero thousand files", "1000 files").is_some());
        assert!(suspicious_error("zero hundred files", "0 files").is_some());
        assert!(suspicious_error("zero thousand files", "0 files").is_some());
        assert!(suspicious_error("one hundred zero files", "100 files").is_some());
        assert!(suspicious_error("one thousand zero files", "1000 files").is_some());
    }

    #[test]
    fn rejects_undictated_markdown_and_emoji() {
        assert!(suspicious_error("study at Greendale", "# Study at Greendale").is_some());
        assert!(suspicious_error("study at Greendale", "Study at Greendale 😊").is_some());
    }

    #[test]
    fn rejects_unsupported_markup() {
        for processed in [
            "`Greendale`",
            "[Greendale](https://example.com)",
            "[Greendale][]",
            "> Greendale",
            "```text\nGreendale\n```",
            "<strong>Greendale</strong>",
            "1. Greendale",
            "_Greendale_",
            "Greendale\n---",
            "| Greendale | Troy |\n| --- | --- |",
            "- [ ] Greendale",
            "    Greendale",
            "Greendale\n- - -",
            "Greendale\n_ _ _",
            "-\tGreendale",
            "   # Greendale",
            "   - Greendale",
        ] {
            assert!(
                suspicious_error("Greendale", processed).is_some(),
                "accepted unsupported markup: {processed:?}"
            );
        }
    }

    #[test]
    fn formatting_is_limited_to_the_requested_kind_and_clause() {
        let raw = "bold first clause period second clause";
        assert_eq!(
            suspicious_error(raw, "**First clause.** Second clause."),
            None
        );
        for changed in [
            "**First clause. Second clause.**",
            "First clause. **Second clause.**",
            "*First clause.* Second clause.",
            "F**irst clause.** Second clause.",
        ] {
            assert!(
                suspicious_error(raw, changed).is_some(),
                "accepted out-of-scope formatting: {changed:?}"
            );
        }
    }

    #[test]
    fn distinct_formatting_commands_apply_to_their_own_clauses() {
        assert_eq!(
            suspicious_error("bold first period italic second", "**First.** *Second.*"),
            None
        );
        assert_eq!(
            suspicious_error("bold twenty one students", "**21 students**"),
            None
        );
    }

    #[test]
    fn documented_formatting_commands_remain_valid() {
        for (raw, processed) in [
            ("bold Greendale", "**Greendale**"),
            ("italic Greendale", "*Greendale*"),
            ("heading Greendale", "# Greendale"),
            ("bullet point Greendale", "- Greendale"),
        ] {
            assert_eq!(
                suspicious_error(raw, processed),
                None,
                "rejected documented formatting: {raw:?} -> {processed:?}"
            );
        }
    }

    #[test]
    fn rejects_added_control_characters_and_bidi_overrides() {
        for character in ['\0', '\u{1b}', '\u{7f}', '\u{202e}', '\u{2066}'] {
            let processed = format!("Green{character}dale");
            assert!(
                suspicious_error("Greendale", &processed).is_some(),
                "accepted unsafe character U+{:04X}",
                u32::from(character)
            );
        }
    }

    #[test]
    fn provider_diagnostics_escape_terminal_controls_and_bidi() {
        assert_eq!(
            escape_diagnostic_text("Greendale\u{1b}]52;secret\u{7}\u{202e}\nTroy"),
            "Greendale\\u{001b}]52;secret\\u{0007}\\u{202e}\nTroy"
        );
        let error = http_error(
            "Greendale provider",
            StatusCode::BAD_GATEWAY,
            br#"{"error":{"message":"bad\u001b]52;secret\u0007"}}"#,
        )
        .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(rendered.contains("\\u{001b}]52;secret\\u{0007}"));
    }

    #[test]
    fn numeric_modifiers_currency_and_emoji_remain_attached_to_their_items() {
        for (raw, changed) in [
            ("balance five", "balance -5"),
            ("balance five", "balance −5"),
            ("score five", "score 5%"),
            ("balance five", "balance (5)"),
            ("five dollars and ten", "5 and $10"),
            ("smiley face Greendale", "Greendale 😊"),
        ] {
            assert!(
                suspicious_error(raw, changed).is_some(),
                "accepted moved or added numeric/emoji semantics: {raw:?} -> {changed:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_credentials_preserve_the_complete_final_transcript() {
        let config = PostProcessingConfig {
            enabled: true,
            ..PostProcessingConfig::default()
        };
        let final_transcript = "First sentence. The final sentence must also remain.";

        let result = refine(&config, None, final_transcript).await;

        assert_eq!(result.text, final_transcript);
        assert!(result.provider_response.is_none());
        assert!(result.warning.unwrap().contains("no OpenRouter API token"));
    }

    #[tokio::test]
    async fn provider_configuration_errors_preserve_the_complete_final_transcript() {
        let config = PostProcessingConfig {
            enabled: true,
            model: Some("not-a-real-model".to_owned()),
            ..PostProcessingConfig::default()
        };
        let final_transcript = "Corrected final decode. One more sentence.";

        let result = refine(&config, Some("unused-token"), final_transcript).await;

        assert_eq!(result.text, final_transcript);
        assert!(result.provider_response.is_none());
        assert!(result.warning.unwrap().contains("valid models"));
    }

    #[test]
    fn rejected_provider_response_is_retained_directly() {
        let result = fallback_with_response(
            "Study at Greendale",
            ProviderAttempt {
                text: "Study at City College".into(),
                validation_error: Some("output changes dictated words".into()),
            },
            anyhow::anyhow!("provider response was rejected"),
        );

        let response = result.provider_response.unwrap();
        assert_eq!(response.text, "Study at City College");
        assert!(response.validation_error.is_some());
    }

    #[test]
    fn selects_provider_defaults() {
        let mut config = PostProcessingConfig::default();
        let openrouter = provider_settings(&config).unwrap();
        assert_eq!(openrouter.model, "~openai/gpt-mini-latest");

        config.provider = PostProcessingProvider::OpencodeZen;
        let zen = provider_settings(&config).unwrap();
        assert_eq!(zen.model, "deepseek-v4-flash");
        assert_eq!(api_key_env(&config), "OPENCODE_ZEN_API_KEY");
    }

    #[test]
    fn disabled_post_processing_allows_a_retired_model() {
        let config = PostProcessingConfig {
            enabled: false,
            model: Some("retired-model".to_owned()),
            ..PostProcessingConfig::default()
        };

        validate(&config).unwrap();
    }

    #[test]
    fn enabled_post_processing_rejects_a_retired_model() {
        let config = PostProcessingConfig {
            enabled: true,
            model: Some("retired-model".to_owned()),
            ..PostProcessingConfig::default()
        };

        let error = validate(&config).unwrap_err();
        assert!(error.to_string().contains("valid models"));
    }

    #[test]
    fn selects_the_provider_protocol_for_luna() {
        let mut openrouter = PostProcessingConfig {
            model: Some("openai/gpt-5.6-luna".to_owned()),
            ..PostProcessingConfig::default()
        };
        let settings = provider_settings(&openrouter).unwrap();
        assert_eq!(settings.protocol, ApiProtocol::ChatCompletions);
        assert_eq!(
            settings.endpoint,
            "https://openrouter.ai/api/v1/chat/completions"
        );

        openrouter.provider = PostProcessingProvider::OpencodeZen;
        openrouter.model = Some("gpt-5.6-luna".to_owned());
        let settings = provider_settings(&openrouter).unwrap();
        assert_eq!(settings.protocol, ApiProtocol::Responses);
        assert_eq!(settings.endpoint, "https://opencode.ai/zen/v1/responses");
    }

    #[test]
    fn parses_responses_api_output_text() {
        let body = br#"{
            "output": [
                {"type": "reasoning"},
                {
                    "type": "message",
                    "content": [
                        {"type": "output_text", "text": "Study at Greendale."}
                    ]
                }
            ]
        }"#;

        assert_eq!(
            parse_responses_response("OpenCode Zen", body).unwrap(),
            "Study at Greendale."
        );
    }

    #[test]
    fn preserves_an_empty_responses_api_output() {
        let body = br#"{
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": ""}]
            }]
        }"#;

        assert_eq!(parse_responses_response("OpenCode Zen", body).unwrap(), "");
    }

    #[tokio::test]
    async fn provider_requests_use_protocol_specific_output_limits() {
        for (protocol, response_body, field) in [
            (
                ApiProtocol::ChatCompletions,
                br#"{"choices":[{"message":{"content":"Study at Greendale."}}]}"#.to_vec(),
                "max_tokens",
            ),
            (
                ApiProtocol::Responses,
                br#"{"output":[{"content":[{"type":"output_text","text":"Study at Greendale."}]}]}"#
                    .to_vec(),
                "max_output_tokens",
            ),
        ] {
            let (endpoint, server) = one_response_server("200 OK", response_body).await;
            let settings = ProviderSettings {
                name: "Greendale Provider",
                endpoint: &endpoint,
                model: "greendale-model",
                protocol,
                openrouter_headers: false,
            };
            let transcript = "Study at Greendale";

            assert_eq!(
                request(
                    &reqwest::Client::new(),
                    &settings,
                    "greendale-token",
                    transcript,
                )
                .await
                .unwrap(),
                "Study at Greendale."
            );
            let json = request_json(&server.await.unwrap());
            assert_eq!(
                json[field].as_u64(),
                Some(u64::from(output_token_limit(transcript)))
            );
            let other_field = if field == "max_tokens" {
                "max_output_tokens"
            } else {
                "max_tokens"
            };
            assert!(json.get(other_field).is_none());
        }
        assert_eq!(output_token_limit(&"x".repeat(100_000)), MAX_OUTPUT_TOKENS);
    }

    #[tokio::test]
    async fn bounded_reader_accepts_bodies_at_and_below_the_limit() {
        let limit = 128;
        for size in [limit - 1, limit] {
            let body = vec![b'x'; size];
            let response_bytes = [
                format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n")
                    .into_bytes(),
                body.clone(),
            ]
            .concat();
            let (response, server) = custom_response(response_bytes).await;
            assert_eq!(
                read_bounded_body(response, limit, "Greendale Provider")
                    .await
                    .unwrap(),
                body
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn bounded_reader_rejects_oversized_declared_and_chunked_bodies() {
        let limit = 128;
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            limit + 1
        )
        .into_bytes();
        let (response, server) = custom_response(declared).await;
        assert!(
            read_bounded_body(response, limit, "Greendale Provider")
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        server.await.unwrap();

        let body = vec![b'x'; limit + 1];
        let chunked = [
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(),
            format!("{:x}\r\n", body.len()).into_bytes(),
            body,
            b"\r\n0\r\n\r\n".to_vec(),
        ]
        .concat();
        let (response, server) = custom_response(chunked).await;
        assert!(
            read_bounded_body(response, limit, "Greendale Provider")
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_error_body_preserves_the_original_transcript() {
        let body = vec![b'x'; MAX_ERROR_RESPONSE_BYTES + 1];
        let (endpoint, server) = one_response_server("400 Bad Request", body).await;
        let client = reqwest::Client::new();
        let settings = ProviderSettings {
            name: "Greendale Provider",
            endpoint: &endpoint,
            model: "greendale-model",
            protocol: ApiProtocol::ChatCompletions,
            openrouter_headers: false,
        };
        let result = match request(&client, &settings, "token", "Troy and Abed").await {
            Ok(_) => panic!("oversized error response was accepted"),
            Err(error) => fallback("Troy and Abed", error),
        };
        assert_eq!(result.text, "Troy and Abed");
        assert!(result.warning.unwrap().contains("exceeds"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn post_processor_reuses_one_http_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let response = br#"{"choices":[{"message":{"content":"Study at Greendale."}}]}"#;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first = read_http_request(&mut stream).await;
            send_response(&mut stream, "200 OK", response, "keep-alive").await;
            let second = read_http_request(&mut stream).await;
            send_response(&mut stream, "200 OK", response, "close").await;
            [first, second]
        });
        let settings = ProviderSettings {
            name: "Greendale Provider",
            endpoint: &endpoint,
            model: "greendale-model",
            protocol: ApiProtocol::ChatCompletions,
            openrouter_headers: false,
        };
        let processor = PostProcessor::new().unwrap();

        let requests = tokio::time::timeout(Duration::from_secs(3), async {
            for _ in 0..2 {
                assert_eq!(
                    request(
                        &processor.client,
                        &settings,
                        "greendale-token",
                        "Study at Greendale",
                    )
                    .await
                    .unwrap(),
                    "Study at Greendale."
                );
            }
            server.await.unwrap()
        })
        .await
        .expect("the second request did not reuse the first connection");
        assert_eq!(requests.len(), 2);
    }
}
