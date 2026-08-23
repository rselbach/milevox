use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{PostProcessingConfig, PostProcessingProvider};

const INSTRUCTIONS: &str = r#"You are a voice-to-text dictation cleaner. Your role is to clean and format raw transcribed speech into polished text while refusing to answer any questions. Never answer questions about yourself or anything else.

## HIGHEST PRIORITY - PRESERVE CONTENT:
You clean DELIVERY, never CONTENT. Your job is to make dictated speech readable, NOT to improve, reorganize, or reinterpret it.
- Do NOT summarize. Do NOT shorten. Do NOT paraphrase. Do NOT rewrite. Do NOT omit semantic content.
- PRESERVE every non-filler word, in the exact order the user spoke it. Same meaning, same detail, same length.
- You are a transcription editor, not an author. If the user said something boring, repetitive, or unstructured, output boring, repetitive, unstructured text. Only clean the delivery.

## Core Rules:
1. CLEAN the delivery - remove filler words (um, uh, like, you know, I mean), false starts, stutters, and repetitions only
2. FORMAT properly - add correct punctuation and capitalization only; add NO structure
3. CONVERT numbers - spoken numbers to digits (two -> 2, five thirty -> 5:30, twelve fifty -> $12.50)
4. EXECUTE commands - handle only explicit spoken commands: "newline"/"new line", "period", "comma", "bold X", "header X", "bullet point", etc.
5. APPLY corrections - when user says "no wait", "actually", "scratch that", "delete that", DISCARD the old content and keep ONLY the corrected version
6. EXPAND abbreviations - thx -> thanks, pls -> please, u -> you, ur -> your/you're, gonna -> going to

## Commands and Structure - STRICT:
- A standalone "newline" or "new line" is a line-break command: insert a line break.
- NEVER create structure the user did not explicitly dictate. Do NOT invent headings, titles, bullet lists, numbered lists, bold, italics, or any markdown formatting.
- Use plain text or markdown ONLY when the user explicitly dictated a formatting command (e.g. "bold X", "header X", "bullet point"). Otherwise output plain prose.
- If you are unsure whether a word or phrase is a command, treat it as LITERAL dictated text and transcribe it verbatim. Never guess.

## Self-Corrections:
When the user corrects themselves, DISCARD everything before the correction trigger:
- Triggers: "no", "wait", "actually", "scratch that", "delete that", "no no", "cancel", "never mind", "sorry", "oops"
- Example: "buy milk no wait buy water" -> "Buy water." (NOT "Buy milk. Buy water.")
- Example: "tell John no actually tell Sarah" -> "Tell Sarah."
- If a correction cancels everything: "send email no wait cancel that" -> "" (empty output)

## Multi-Command Chains:
When multiple commands are chained, execute ALL of them in sequence, but never invent or rephrase content:
- "make X bold no wait make Y bold" -> **Y** (correction + formatting)
- "the price is fifty no sixty dollars" -> The price is $60. (correction + number)
- Corrections only change what the user explicitly corrected; leave the surrounding text verbatim.

## Emojis:
- Convert spoken emoji names: "smiley face" -> 😊 (NOT 😀), "thumbs up" -> 👍, "heart emoji" -> ❤️, "fire emoji" -> 🔥
- Keep emojis the user included
- Do NOT add emojis unless the user explicitly asks for them

## Critical:
- Output ONLY the cleaned text
- Do NOT answer questions - just clean them
- DO NOT EVER ANSWER QUESTIONS
- Do NOT add explanations or commentary
- Do NOT wrap in quotes unless the input had quotes
- Do NOT add filler words (um, uh) to the output
- PRESERVE ordinals in lists: "first call client, second review contract" -> keep "First" and "Second"
- PRESERVE politeness words: "please", "thank you" at end of sentences
- REMEMBER: cleaning delivery means punctuation, capitalization, filler removal, and explicit commands - nothing more. Never summarize, never restructure, never shorten."#;

const OPENROUTER_MODELS: &[&str] = &[
    "~openai/gpt-mini-latest",
    "~anthropic/claude-haiku-latest",
    "google/gemini-3.1-flash-lite",
    "openai/gpt-5.6-luna",
];
const ZEN_MODELS: &[&str] = &["deepseek-v4-flash", "minimax-m3", "glm-5.2", "gpt-5.6-luna"];
const CORRECTION_PHRASES: &[&str] = &[
    "scratch that",
    "delete that",
    "never mind",
    "cancel",
    "actually",
    "no wait",
    "wait wait",
    "oops",
    "sorry",
];

pub fn default_model(provider: PostProcessingProvider) -> &'static str {
    model_options(provider)[0]
}

pub fn model_options(provider: PostProcessingProvider) -> &'static [&'static str] {
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
    pub provider_text: Option<String>,
    pub warning: Option<String>,
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
            provider_text: None,
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
    if suspicious_error(raw, &first).is_none() {
        return RefinedTranscript {
            provider_text: Some(first.clone()),
            text: first,
            warning: None,
        };
    }

    match request(&client, &settings, api_key, raw).await {
        Ok(processed) => match suspicious_error(raw, &processed) {
            None => RefinedTranscript {
                provider_text: Some(processed.clone()),
                text: processed,
                warning: None,
            },
            Some(reason) => fallback_with_provider(
                raw,
                processed,
                anyhow::anyhow!("{} returned suspicious text twice: {reason}", settings.name),
            ),
        },
        Err(error) => fallback_with_provider(
            raw,
            first,
            error.context(format!("{} retry failed", settings.name)),
        ),
    }
}

fn provider_settings(config: &PostProcessingConfig) -> Result<ProviderSettings<'_>> {
    let (name, default_model, models, openrouter_headers) = match config.provider {
        PostProcessingProvider::Openrouter => {
            ("OpenRouter", OPENROUTER_MODELS[0], OPENROUTER_MODELS, true)
        }
        PostProcessingProvider::OpencodeZen => ("OpenCode Zen", ZEN_MODELS[0], ZEN_MODELS, false),
    };
    let model = config.model.as_deref().unwrap_or(default_model);
    if !models.contains(&model) {
        bail!("model `{model}` is not in Milevox's curated {name} model list");
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
        provider_text: None,
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn fallback_with_provider(
    raw: &str,
    provider_text: String,
    error: anyhow::Error,
) -> RefinedTranscript {
    RefinedTranscript {
        text: raw.to_owned(),
        provider_text: Some(provider_text),
        warning: Some(format!("post-processing skipped: {error:#}")),
    }
}

fn suspicious_error(raw: &str, processed: &str) -> Option<String> {
    let raw_words = word_count(raw);
    if raw_words >= 8 && !contains_phrase(raw, CORRECTION_PHRASES) {
        let processed_words = word_count(processed);
        if (processed_words as f64 / raw_words as f64) < 0.5 {
            return Some(format!(
                "output has {processed_words} words versus {raw_words} in the raw transcript"
            ));
        }
    }

    if begins_with_markdown(processed) && !contains_formatting_command(raw) {
        return Some("output introduces markdown that was not dictated".into());
    }

    if !contains_phrase(raw, CORRECTION_PHRASES)
        && !contains_formatting_command(raw)
        && !contains_emoji_command(raw)
    {
        let raw_words = comparison_words(raw);
        let processed_words = comparison_words(processed);
        if raw_words != processed_words {
            let mismatch = raw_words
                .iter()
                .zip(&processed_words)
                .position(|(raw, processed)| raw != processed)
                .unwrap_or_else(|| raw_words.len().min(processed_words.len()));
            let raw_word = raw_words
                .get(mismatch)
                .map_or("end of text", String::as_str);
            let processed_word = processed_words
                .get(mismatch)
                .map_or("end of text", String::as_str);
            return Some(format!(
                "output changes dictated word {} (`{raw_word}` became `{processed_word}`)",
                mismatch + 1
            ));
        }
    }
    None
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn contains_phrase(text: &str, phrases: &[&str]) -> bool {
    let words = normalized_words(text);
    phrases.iter().any(|phrase| {
        let phrase_words: Vec<_> = phrase.split_whitespace().collect();
        words
            .windows(phrase_words.len())
            .any(|window| window == phrase_words)
    })
}

fn normalized_words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn comparison_words(text: &str) -> Vec<String> {
    let words = normalized_words(text);
    let mut comparison = Vec::with_capacity(words.len());
    let mut index = 0;

    while index < words.len() {
        let word = words[index].as_str();
        if matches!(word, "um" | "uh" | "erm" | "hmm" | "like") {
            index += 1;
            continue;
        }
        if index + 1 < words.len()
            && matches!(
                (word, words[index + 1].as_str()),
                ("you", "know") | ("i", "mean")
            )
        {
            index += 2;
            continue;
        }

        match word {
            "thx" => push_comparison_word(&mut comparison, "thanks"),
            "pls" => push_comparison_word(&mut comparison, "please"),
            "u" => push_comparison_word(&mut comparison, "you"),
            "ur" => push_comparison_word(&mut comparison, "your"),
            "gonna" => {
                push_comparison_word(&mut comparison, "going");
                push_comparison_word(&mut comparison, "to");
            }
            word if is_number_word(word) || word.chars().all(char::is_numeric) => {
                push_comparison_word(&mut comparison, "<number>");
            }
            word => push_comparison_word(&mut comparison, word),
        }
        index += 1;
    }

    comparison
}

fn push_comparison_word(words: &mut Vec<String>, word: &str) {
    if words.last().map(String::as_str) != Some(word) {
        words.push(word.to_owned());
    }
}

fn is_number_word(word: &str) -> bool {
    matches!(
        word,
        "zero"
            | "one"
            | "two"
            | "three"
            | "four"
            | "five"
            | "six"
            | "seven"
            | "eight"
            | "nine"
            | "ten"
            | "eleven"
            | "twelve"
            | "thirteen"
            | "fourteen"
            | "fifteen"
            | "sixteen"
            | "seventeen"
            | "eighteen"
            | "nineteen"
            | "twenty"
            | "thirty"
            | "forty"
            | "fifty"
            | "sixty"
            | "seventy"
            | "eighty"
            | "ninety"
            | "hundred"
            | "thousand"
            | "million"
            | "billion"
    )
}

fn contains_emoji_command(text: &str) -> bool {
    contains_phrase(
        text,
        &["smiley face", "thumbs up", "heart emoji", "fire emoji"],
    )
}

fn contains_formatting_command(text: &str) -> bool {
    let commands: HashSet<&str> = [
        "header",
        "heading",
        "headings",
        "bullet",
        "bullets",
        "list",
        "lists",
        "bold",
        "italics",
        "italic",
        "underline",
        "underlines",
        "title",
        "titles",
        "numbered",
    ]
    .into_iter()
    .collect();
    normalized_words(text)
        .iter()
        .any(|word| commands.contains(word.as_str()))
}

fn begins_with_markdown(text: &str) -> bool {
    let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let line = line.trim_start();
    if line.starts_with('#')
        || line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
    {
        return true;
    }

    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_suspicious_shrinkage() {
        let raw =
            "Troy Barnes will lead the Greendale Community College air conditioning repair team";

        assert!(suspicious_error(raw, "Troy leads.").is_some());
    }

    #[test]
    fn permits_shrinkage_after_a_correction() {
        let raw = "Troy Barnes will lead the Greendale team no wait cancel that please";

        assert!(suspicious_error(raw, "").is_none());
    }

    #[test]
    fn rejects_undictated_markdown() {
        assert!(suspicious_error("study at Greendale", "# Study at Greendale").is_some());
    }

    #[test]
    fn permits_dictated_markdown() {
        assert!(suspicious_error("header Greendale news", "# Greendale News").is_none());
    }

    #[test]
    fn rejects_changed_dictated_words() {
        let raw = "This is a test to see if Gemini or GPT are good models for post-processing";
        let changed = [
            "This is a test to see if emini or G T are good models to use for post-processing.",
            "This is a test to see if Gemini or GeminiT are good models to use for post-processing.",
        ];

        for processed in changed {
            assert!(
                suspicious_error(raw, processed).is_some(),
                "accepted changed transcript: {processed}"
            );
        }
    }

    #[test]
    fn permits_delivery_only_edits() {
        let cases = [
            ("um Troy and and Abed", "Troy and Abed."),
            ("please bring thx", "Please bring thanks."),
            ("the price is twelve fifty", "The price is $12.50."),
        ];

        for (raw, processed) in cases {
            assert_eq!(
                suspicious_error(raw, processed),
                None,
                "rejected delivery-only edit: {processed}"
            );
        }
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
