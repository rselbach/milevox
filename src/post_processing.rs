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
    pub provider_attempts: Vec<ProviderAttempt>,
    pub warning: Option<String>,
}

pub struct ProviderAttempt {
    pub text: String,
    pub validation_error: Option<String>,
}

struct ProviderSettings<'a> {
    name: &'static str,
    endpoint: &'static str,
    model: &'a str,
    protocol: ApiProtocol,
    openrouter_headers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiProtocol {
    ChatCompletions,
    Responses,
}

pub async fn refine(
    config: &PostProcessingConfig,
    api_key: Option<&str>,
    raw: &str,
) -> RefinedTranscript {
    if !config.enabled {
        return RefinedTranscript {
            text: raw.to_owned(),
            provider_attempts: Vec::new(),
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
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(error) => return fallback(raw, error.into()),
    };

    let first = match request(&client, &settings, api_key, raw).await {
        Ok(processed) => processed,
        Err(error) => return fallback(raw, error),
    };
    let first_error = suspicious_error(raw, &first);
    let provider_attempts = vec![ProviderAttempt {
        text: first.clone(),
        validation_error: first_error.clone(),
    }];
    if first_error.is_none() {
        return RefinedTranscript {
            provider_attempts,
            text: first,
            warning: None,
        };
    }

    fallback_with_attempts(
        raw,
        provider_attempts,
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
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read {} response", settings.name))?;
    if !status.is_success() {
        return http_error(settings.name, status, &bytes);
    }

    parse_response(settings, &bytes)
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
        .map(|error| error.error.message.chars().take(200).collect::<String>());
    match detail {
        Some(detail) => bail!("{name} returned HTTP {status}: {detail}"),
        None => bail!("{name} returned HTTP {status}"),
    }
}

fn fallback(raw: &str, error: anyhow::Error) -> RefinedTranscript {
    RefinedTranscript {
        text: raw.to_owned(),
        provider_attempts: Vec::new(),
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn fallback_with_attempts(
    raw: &str,
    provider_attempts: Vec<ProviderAttempt>,
    error: anyhow::Error,
) -> RefinedTranscript {
    RefinedTranscript {
        text: raw.to_owned(),
        provider_attempts,
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn suspicious_error(raw: &str, processed: &str) -> Option<String> {
    let analysis = analyze_raw(raw);
    if contains_markdown(processed) && !analysis.formatting_allowed {
        return Some("output introduces markdown that was not dictated".into());
    }
    let processed_emoji = emoji_symbols(processed);
    if analysis.emoji != processed_emoji {
        return Some("output changes or introduces an emoji that was not dictated".into());
    }
    if currency_mentions(raw) != currency_mentions(processed) {
        return Some("output changes or introduces a currency marker that was not dictated".into());
    }
    let processed_words = analyze_processed(processed);
    if analysis.words == processed_words {
        return None;
    }
    let mismatch = analysis
        .words
        .iter()
        .zip(&processed_words)
        .position(|(raw, processed)| raw != processed)
        .unwrap_or_else(|| analysis.words.len().min(processed_words.len()));
    let raw_word = analysis
        .words
        .get(mismatch)
        .map_or("end of text", String::as_str);
    let processed_word = processed_words
        .get(mismatch)
        .map_or("end of text", String::as_str);
    Some(format!(
        "output changes dictated item {} (`{raw_word}` became `{processed_word}`)",
        mismatch + 1
    ))
}

struct RawAnalysis {
    words: Vec<String>,
    emoji: Vec<char>,
    formatting_allowed: bool,
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
    let mut formatting_allowed = false;
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

        if at_clause_start
            && (matches!(word, "bold" | "italic" | "header" | "heading")
                || (word == "bullet" && next == Some("point")))
        {
            formatting_allowed = true;
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
            index += 2;
            at_clause_start = false;
            continue;
        }
        if is_filler(word) || matches!((word, next), ("you", Some("know")) | ("i", Some("mean"))) {
            index += if is_filler(word) { 1 } else { 2 };
            continue;
        }
        append_expanded(&mut words, word);
        at_clause_start = false;
        index += 1;
    }

    RawAnalysis {
        words: canonicalize_numbers(collapse_repetitions(words), true),
        emoji: expected_emoji,
        formatting_allowed,
    }
}

fn analyze_processed(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let tokens = spoken_words(text);
    let mut index = 0;
    while index < tokens.len() {
        let word = tokens[index].text.as_str();
        let next = tokens.get(index + 1).map(|token| token.text.as_str());
        if is_filler(word) || matches!((word, next), ("you", Some("know")) | ("i", Some("mean"))) {
            index += if is_filler(word) { 1 } else { 2 };
            continue;
        }
        append_expanded(&mut words, word);
        index += 1;
    }
    // Number conversion is optional. Canonicalize spoken and written forms on both sides so
    // an otherwise unchanged transcript remains valid while changed values are rejected.
    canonicalize_numbers(collapse_repetitions(words), true)
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
    for (index, character) in text.char_indices() {
        if character.is_alphanumeric() {
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

fn append_expanded(words: &mut Vec<String>, word: &str) {
    match word {
        "thx" => words.push("thanks".into()),
        "pls" => words.push("please".into()),
        "u" => words.push("you".into()),
        "ur" => words.push("your".into()),
        "gonna" => words.extend(["going".into(), "to".into()]),
        word => words.push(word.into()),
    }
}

fn collapse_repetitions(words: Vec<String>) -> Vec<String> {
    let mut output = Vec::with_capacity(words.len());
    for word in words {
        if output.last() != Some(&word) {
            output.push(word);
        }
    }
    output
}

fn canonicalize_numbers(words: Vec<String>, spoken: bool) -> Vec<String> {
    let mut output = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        if let Some(number) = canonical_numeric_literal(&words[index]) {
            output.push(format!("#{number}"));
            index += 1;
            continue;
        }
        if spoken && is_number_word(&words[index]) {
            let start = index;
            while index < words.len()
                && (is_number_word(&words[index])
                    || words[index] == "and"
                    || words[index] == "point")
            {
                index += 1;
            }
            let sequence = &words[start..index];
            let currency = words
                .get(index)
                .is_some_and(|word| matches!(word.as_str(), "dollar" | "dollars"));
            let time = output.last().is_some_and(|word| word == "at")
                && sequence.len() == 2
                && number_atom(&sequence[0]).is_some_and(|value| value <= 23)
                && number_atom(&sequence[1]).is_some_and(|value| (10..=59).contains(&value));
            let decimal_currency = currency
                && sequence.len() == 2
                && number_atom(&sequence[1]).is_some_and(|value| value <= 99);
            if time || decimal_currency {
                output.push(format!("#{}", number_atom(&sequence[0]).unwrap_or(0)));
                output.push(format!("#{}", number_atom(&sequence[1]).unwrap_or(0)));
            } else {
                for part in sequence.split(|word| word == "point") {
                    if !part.is_empty() {
                        output.push(format!("#{}", parse_number(part)));
                    }
                }
            }
            if currency {
                index += 1;
            }
            continue;
        }
        output.push(words[index].clone());
        index += 1;
    }
    output
}

fn canonical_numeric_literal(word: &str) -> Option<String> {
    if word.chars().all(|character| character.is_ascii_digit()) {
        let trimmed = word.trim_start_matches('0');
        return Some(if trimmed.is_empty() { "0" } else { trimmed }.to_owned());
    }
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(number) = word.strip_suffix(suffix)
            && number.chars().all(|character| character.is_ascii_digit())
        {
            return canonical_numeric_literal(number);
        }
    }
    None
}

fn parse_number(words: &[String]) -> u64 {
    let mut total = 0_u64;
    let mut current = 0_u64;
    for word in words {
        match word.as_str() {
            "and" => {}
            "hundred" => current = current.max(1) * 100,
            "thousand" => {
                total += current.max(1) * 1_000;
                current = 0;
            }
            "million" => {
                total += current.max(1) * 1_000_000;
                current = 0;
            }
            "billion" => {
                total += current.max(1) * 1_000_000_000;
                current = 0;
            }
            word => current += number_atom(word).unwrap_or(0),
        }
    }
    total + current
}

fn is_number_word(word: &str) -> bool {
    number_atom(word).is_some() || matches!(word, "hundred" | "thousand" | "million" | "billion")
}

fn number_atom(word: &str) -> Option<u64> {
    Some(match word {
        "zero" => 0,
        "one" | "first" => 1,
        "two" | "second" => 2,
        "three" | "third" => 3,
        "four" | "fourth" => 4,
        "five" | "fifth" => 5,
        "six" | "sixth" => 6,
        "seven" | "seventh" => 7,
        "eight" | "eighth" => 8,
        "nine" | "ninth" => 9,
        "ten" | "tenth" => 10,
        "eleven" | "eleventh" => 11,
        "twelve" | "twelfth" => 12,
        "thirteen" | "thirteenth" => 13,
        "fourteen" | "fourteenth" => 14,
        "fifteen" | "fifteenth" => 15,
        "sixteen" | "sixteenth" => 16,
        "seventeen" | "seventeenth" => 17,
        "eighteen" | "eighteenth" => 18,
        "nineteen" | "nineteenth" => 19,
        "twenty" | "twentieth" => 20,
        "thirty" | "thirtieth" => 30,
        "forty" | "fortieth" => 40,
        "fifty" | "fiftieth" => 50,
        "sixty" | "sixtieth" => 60,
        "seventy" | "seventieth" => 70,
        "eighty" | "eightieth" => 80,
        "ninety" | "ninetieth" => 90,
        _ => return None,
    })
}

fn emoji_symbols(text: &str) -> Vec<char> {
    text.chars()
        .filter(|character| matches!(character, '😊' | '👍' | '❤' | '🔥'))
        .collect()
}

fn contains_markdown(text: &str) -> bool {
    if text.contains("**") || text.contains("__") {
        return true;
    }
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#')
            || line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("+ ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_undictated_markdown_and_emoji() {
        assert!(suspicious_error("study at Greendale", "# Study at Greendale").is_some());
        assert!(suspicious_error("study at Greendale", "Study at Greendale 😊").is_some());
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
        assert!(result.warning.unwrap().contains("valid models"));
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
}
