use anyhow::{Context as _, Result};
use base64::Engine as _;
use std::io::{self, IsTerminal, Read, Write};
#[cfg(test)]
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub source: &'static str,
    confidence: Confidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractionResult {
    base_url_candidates: Vec<Candidate>,
    key_candidates: Vec<Candidate>,
}

impl ExtractionResult {
    #[cfg(test)]
    pub fn best_base_url(&self) -> Option<&Candidate> {
        best_candidate(&self.base_url_candidates)
    }

    #[cfg(test)]
    pub fn best_key(&self) -> Option<&Candidate> {
        best_candidate(&self.key_candidates)
    }

    fn base_url_candidates(&self) -> &[Candidate] {
        &self.base_url_candidates
    }

    fn key_candidates(&self) -> &[Candidate] {
        &self.key_candidates
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AddAssistRequest<'a> {
    pub base_url: Option<&'a str>,
    pub key: Option<&'a str>,
    pub pasted_content: &'a str,
    pub interactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAddInputs {
    pub base_url: String,
    pub key: String,
    pub used_assisted_flow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    pub label: &'static str,
    pub initial: String,
    pub alternatives: Vec<String>,
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationRequest {
    pub base_url: String,
    pub masked_key: String,
}

pub trait AddPrompter {
    fn edit(&mut self, request: EditRequest) -> Result<String>;
    fn confirm(&mut self, request: ConfirmationRequest) -> Result<bool>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddCommandIO {
    pasted_content: String,
    interactive: bool,
}

impl AddCommandIO {
    #[cfg(test)]
    pub fn piped(content: &str) -> Self {
        Self {
            pasted_content: content.to_string(),
            interactive: false,
        }
    }

    pub fn direct() -> Self {
        Self::default()
    }

    pub fn from_stdio() -> Result<Self> {
        let stdin_is_terminal = io::stdin().is_terminal();
        let stdout_is_terminal = io::stdout().is_terminal();
        let mut stderr = io::stderr();
        let mut stdin = io::stdin();
        let pasted_content =
            read_pasted_content(&mut stdin, &mut stderr, stdin_is_terminal)?;

        Ok(Self {
            pasted_content,
            interactive: stdin_is_terminal && stdout_is_terminal,
        })
    }

    pub fn pasted_content(&self) -> &str {
        &self.pasted_content
    }

    pub fn interactive(&self) -> bool {
        self.interactive
    }
}

pub struct DialoguerPrompter;

impl AddPrompter for DialoguerPrompter {
    fn edit(&mut self, request: EditRequest) -> Result<String> {
        let mut prompt = request.label.to_string();
        if !request.alternatives.is_empty() {
            prompt.push_str(" (alternatives: ");
            prompt.push_str(&request.alternatives.join(", "));
            prompt.push(')');
        }

        let value = dialoguer::Input::<String>::new()
            .with_prompt(prompt)
            .with_initial_text(request.initial)
            .allow_empty(true)
            .interact_text()
            .with_context(|| format!("failed to read {}", request.label))?;

        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            anyhow::bail!("{} cannot be empty", request.label);
        }
        Ok(trimmed)
    }

    fn confirm(&mut self, request: ConfirmationRequest) -> Result<bool> {
        eprintln!("Detected base_url: {}", request.base_url);
        eprintln!("Detected key: {}", request.masked_key);

        dialoguer::Confirm::new()
            .with_prompt("Write this provider with the values above?")
            .default(true)
            .interact()
            .with_context(|| "failed to confirm add")
    }
}

pub fn read_pasted_content<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    interactive: bool,
) -> Result<String> {
    if interactive {
        let eof_hint = if cfg!(windows) {
            "Ctrl+Z then Enter"
        } else {
            "Ctrl+D"
        };
        writeln!(writer, "Paste provider content, then press {eof_hint} to finish.")
            .with_context(|| "failed to print paste instructions")?;
        writer.flush().ok();
    }

    let mut content = String::new();
    reader
        .read_to_string(&mut content)
        .with_context(|| "failed to read pasted content from stdin")?;
    Ok(content)
}

pub fn resolve_add_inputs(
    request: AddAssistRequest<'_>,
    prompt: &mut dyn AddPrompter,
) -> Result<ResolvedAddInputs> {
    let extraction = extract_candidates(request.pasted_content);

    let base_url = resolve_field(
        "base_url",
        request.base_url,
        extraction.base_url_candidates(),
        request.interactive,
        false,
        prompt,
    )?;
    let key = resolve_field(
        "key",
        request.key,
        extraction.key_candidates(),
        request.interactive,
        true,
        prompt,
    )?;

    if request.interactive {
        let confirmed = prompt.confirm(ConfirmationRequest {
            base_url: base_url.clone(),
            masked_key: mask_secret(&key),
        })?;
        if !confirmed {
            anyhow::bail!("add cancelled");
        }
    }

    Ok(ResolvedAddInputs {
        base_url,
        key,
        used_assisted_flow: true,
    })
}

pub fn extract_candidates(content: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();

    for payload in extract_cherry_studio_links(content) {
        push_candidate(
            &mut result.base_url_candidates,
            payload.base_url,
            "cherry-studio",
            Confidence::High,
        );
        push_candidate(
            &mut result.key_candidates,
            payload.api_key,
            "cherry-studio",
            Confidence::High,
        );
    }

    for line in content.lines() {
        if let Some((lhs, rhs)) = split_assignment(line) {
            let name = normalize_name(&lhs);
            if is_url_key(&name) {
                if let Some(value) = normalize_base_url_candidate(&rhs) {
                    push_candidate(
                        &mut result.base_url_candidates,
                        value,
                        "assignment",
                        Confidence::High,
                    );
                }
            }

            if let Some(value) = extract_explicit_key_value(&name, &rhs) {
                push_candidate(
                    &mut result.key_candidates,
                    value,
                    "assignment",
                    Confidence::High,
                );
            }
        }

        for token in extract_bearer_tokens(line) {
            push_candidate(
                &mut result.key_candidates,
                token,
                "authorization",
                Confidence::High,
            );
        }

        let url_confidence = if line.trim_start().starts_with("curl ") {
            Confidence::High
        } else {
            Confidence::Medium
        };
        let url_source = if line.trim_start().starts_with("curl ") {
            "curl"
        } else {
            "url"
        };
        for url in extract_url_literals(line) {
            push_candidate(
                &mut result.base_url_candidates,
                url,
                url_source,
                url_confidence,
            );
        }

        for token in extract_standalone_tokens(line) {
            push_candidate(
                &mut result.key_candidates,
                token,
                "token",
                Confidence::Low,
            );
        }
    }

    result
}

pub fn normalize_base_url_candidate(raw: &str) -> Option<String> {
    let token = extract_first_http_token(raw)?;
    let mut url = url::Url::parse(token).ok()?;
    url.set_query(None);
    url.set_fragment(None);

    let mut segments: Vec<String> = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect();

    trim_known_endpoint_suffixes(&mut segments);

    if segments.is_empty() {
        url.set_path("/");
        return Some(url.as_str().trim_end_matches('/').to_string());
    }

    let path = format!("/{}", segments.join("/"));
    url.set_path(&path);
    Some(url.to_string())
}

fn resolve_field(
    label: &'static str,
    explicit: Option<&str>,
    candidates: &[Candidate],
    interactive: bool,
    sensitive: bool,
    prompt: &mut dyn AddPrompter,
) -> Result<String> {
    if let Some(explicit) = explicit {
        let trimmed = explicit.trim();
        if trimmed.is_empty() {
            anyhow::bail!("{label} cannot be empty");
        }
        return Ok(trimmed.to_string());
    }

    if !interactive {
        return resolve_non_interactive_field(label, candidates);
    }

    let initial = best_candidate(candidates)
        .map(|candidate| candidate.value.clone())
        .unwrap_or_default();
    let alternatives = alternative_candidates(candidates, &initial);
    prompt.edit(EditRequest {
        label,
        initial,
        alternatives,
        sensitive,
    })
}

fn resolve_non_interactive_field(label: &'static str, candidates: &[Candidate]) -> Result<String> {
    let Some(best) = best_candidate(candidates) else {
        anyhow::bail!("missing {label} in pasted content");
    };

    let has_other_candidates = candidates.iter().any(|candidate| candidate.value != best.value);
    if best.confidence != Confidence::High || has_other_candidates {
        anyhow::bail!(
            "ambiguous {label} in pasted content; candidates: {}",
            summarize_candidates(candidates)
        );
    }

    Ok(best.value.clone())
}

fn best_candidate(candidates: &[Candidate]) -> Option<&Candidate> {
    let mut best: Option<&Candidate> = None;
    for candidate in candidates {
        let replace = match best {
            None => true,
            Some(current) => candidate.confidence > current.confidence,
        };
        if replace {
            best = Some(candidate);
        }
    }
    best
}

fn alternative_candidates(candidates: &[Candidate], best_value: &str) -> Vec<String> {
    candidates
        .iter()
        .filter(|candidate| candidate.value != best_value)
        .map(|candidate| candidate.value.clone())
        .take(3)
        .collect()
}

fn summarize_candidates(candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "(none)".to_string();
    }
    candidates
        .iter()
        .map(|candidate| candidate.value.as_str())
        .take(3)
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_candidate(
    candidates: &mut Vec<Candidate>,
    value: String,
    source: &'static str,
    confidence: Confidence,
) {
    if value.trim().is_empty() {
        return;
    }

    if let Some(existing) = candidates.iter_mut().find(|candidate| candidate.value == value) {
        if confidence > existing.confidence {
            existing.confidence = confidence;
            existing.source = source;
        }
        return;
    }

    candidates.push(Candidate {
        value,
        source,
        confidence,
    });
}

fn split_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim().trim_start_matches('-').trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some((lhs, rhs)) = trimmed.split_once('=') {
        return Some((lhs.to_string(), rhs.to_string()));
    }

    let (lhs, rhs) = trimmed.split_once(':')?;
    if !lhs
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '"' | '\'' | ' '))
    {
        return None;
    }
    Some((lhs.to_string(), rhs.to_string()))
}

fn normalize_name(raw: &str) -> String {
    raw.trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
        .replace('-', "_")
        .to_ascii_lowercase()
}

fn is_url_key(name: &str) -> bool {
    matches!(
        name,
        "base_url" | "api_base" | "api_url" | "openai_base_url" | "openai_api_base"
    ) || name.ends_with("_base_url")
        || name.ends_with("_api_base")
        || name.ends_with("_api_url")
}

fn extract_explicit_key_value(name: &str, raw: &str) -> Option<String> {
    if name == "authorization" {
        return extract_bearer_token(raw);
    }

    let is_key_name = matches!(
        name,
        "api_key" | "apikey" | "key" | "token" | "bearer_token"
    ) || name.ends_with("_api_key")
        || name.ends_with("_apikey")
        || name.ends_with("_key")
        || name.ends_with("_token");

    if !is_key_name {
        return None;
    }

    clean_secret_value(raw)
}

fn extract_bearer_tokens(text: &str) -> Vec<String> {
    extract_bearer_token(text).into_iter().collect()
}

fn extract_bearer_token(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let marker = "bearer ";
    let start = lower.find(marker)?;
    let token_start = start + marker.len();
    let token = &text[token_start..];
    clean_secret_value(token)
}

fn extract_url_literals(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(normalize_base_url_candidate)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CherryStudioApiKeysPayload {
    base_url: String,
    api_key: String,
}

fn extract_cherry_studio_links(text: &str) -> Vec<CherryStudioApiKeysPayload> {
    text.split_whitespace()
        .filter_map(parse_cherry_studio_link)
        .collect()
}

fn parse_cherry_studio_link(raw: &str) -> Option<CherryStudioApiKeysPayload> {
    let token = raw.trim().trim_matches(|ch| {
        matches!(ch, '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}')
    });
    if !token.starts_with("cherrystudio://") {
        return None;
    }

    let url = url::Url::parse(token).ok()?;
    if url.scheme() != "cherrystudio"
        || url.host_str() != Some("providers")
        || url.path() != "/api-keys"
    {
        return None;
    }

    let raw_data = extract_raw_query_param(url.query()?, "data")?;
    let decoded = percent_decode_query_value(raw_data)?;
    let normalized = decoded.replace('_', "+").replace('-', "/").replace(' ', "+");
    let payload = base64::engine::general_purpose::STANDARD
        .decode(normalized)
        .ok()?;
    let payload = String::from_utf8(payload).ok()?;
    let payload = payload
        .replace('\'', "\"")
        .replace(['(', ')'], "");
    let parsed: serde_json::Value = serde_json::from_str(&payload).ok()?;

    let base_url = parsed
        .get("baseUrl")
        .and_then(serde_json::Value::as_str)
        .and_then(normalize_base_url_candidate)?;
    let api_key = parsed
        .get("apiKey")
        .and_then(serde_json::Value::as_str)
        .and_then(clean_secret_value)?;

    Some(CherryStudioApiKeysPayload { base_url, api_key })
}

fn extract_raw_query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (candidate_key, candidate_value) = pair.split_once('=')?;
        (candidate_key == key).then_some(candidate_value)
    })
}

fn percent_decode_query_value(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hi = *bytes.get(index + 1)?;
                let lo = *bytes.get(index + 2)?;
                decoded.push((decode_hex_digit(hi)? << 4) | decode_hex_digit(lo)?);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(decoded).ok()
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn extract_standalone_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(clean_secret_value)
        .filter(|value| value.starts_with("sk-"))
        .collect()
}

fn clean_secret_value(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'));

    if trimmed.is_empty() || trimmed.contains("://") {
        return None;
    }

    let without_bearer = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();

    if without_bearer.is_empty() {
        return None;
    }

    if without_bearer.contains(char::is_whitespace) {
        return None;
    }

    Some(without_bearer.to_string())
}

fn extract_first_http_token(raw: &str) -> Option<&str> {
    let start = raw.find("http://").or_else(|| raw.find("https://"))?;
    let token = &raw[start..];
    Some(
        token
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}')),
    )
}

fn trim_known_endpoint_suffixes(segments: &mut Vec<String>) {
    loop {
        let removed = if segments.len() >= 2
            && segments[segments.len() - 2] == "chat"
            && segments[segments.len() - 1] == "completions"
        {
            segments.truncate(segments.len() - 2);
            true
        } else if segments
            .last()
            .map(|segment| matches!(segment.as_str(), "responses" | "embeddings" | "models"))
            .unwrap_or(false)
        {
            segments.pop();
            true
        } else {
            false
        };

        if !removed {
            break;
        }
    }
}

fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        return "<empty>".to_string();
    }

    let prefix: String = value.chars().take(3).collect();
    let suffix_chars: Vec<char> = value.chars().rev().take(2).collect();
    let suffix: String = suffix_chars.into_iter().rev().collect();
    if value.chars().count() <= 5 {
        format!("{prefix}***")
    } else {
        format!("{prefix}***{suffix}")
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditCall {
    pub label: &'static str,
    pub initial: String,
    pub alternatives: Vec<String>,
    pub sensitive: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationCall {
    pub base_url: String,
    pub masked_key: String,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakePrompter {
    responses: VecDeque<String>,
    confirm_result: bool,
    pub edit_calls: Vec<EditCall>,
    pub confirm_calls: Vec<ConfirmationCall>,
}

#[cfg(test)]
impl FakePrompter {
    pub fn new<const N: usize>(responses: [&str; N], confirm_result: bool) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|value| value.to_string())
                .collect(),
            confirm_result,
            edit_calls: Vec::new(),
            confirm_calls: Vec::new(),
        }
    }
}

#[cfg(test)]
impl AddPrompter for FakePrompter {
    fn edit(&mut self, request: EditRequest) -> Result<String> {
        self.edit_calls.push(EditCall {
            label: request.label,
            initial: request.initial.clone(),
            alternatives: request.alternatives.clone(),
            sensitive: request.sensitive,
        });
        Ok(self.responses.pop_front().unwrap_or(request.initial))
    }

    fn confirm(&mut self, request: ConfirmationRequest) -> Result<bool> {
        self.confirm_calls.push(ConfirmationCall {
            base_url: request.base_url,
            masked_key: request.masked_key,
        });
        Ok(self.confirm_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_env_style_values() {
        let extraction = extract_candidates(
            "OPENAI_BASE_URL=https://env.example/v1\nOPENAI_API_KEY=sk-env\n",
        );

        assert_eq!(
            extraction.best_base_url().map(|candidate| candidate.value.as_str()),
            Some("https://env.example/v1")
        );
        assert_eq!(
            extraction.best_key().map(|candidate| candidate.value.as_str()),
            Some("sk-env")
        );
    }

    #[test]
    fn normalizes_known_openai_compatible_endpoints() {
        assert_eq!(
            normalize_base_url_candidate("https://api.example.com/v1/chat/completions?foo=1"),
            Some("https://api.example.com/v1".to_string())
        );
        assert_eq!(
            normalize_base_url_candidate("'https://api.example.com/v1/responses'"),
            Some("https://api.example.com/v1".to_string())
        );
        assert_eq!(
            normalize_base_url_candidate("https://api.example.com/v1/models"),
            Some("https://api.example.com/v1".to_string())
        );
    }

    #[test]
    fn assisted_prompt_prefills_best_candidates() {
        let mut prompt = FakePrompter::new(["https://api.example.com/v1", "sk-curl"], true);
        let request = AddAssistRequest {
            base_url: None,
            key: None,
            pasted_content:
                "curl https://api.example.com/v1/chat/completions -H 'Authorization: Bearer sk-curl'",
            interactive: true,
        };

        let resolved = resolve_add_inputs(request, &mut prompt).expect("assist should succeed");

        assert_eq!(resolved.base_url, "https://api.example.com/v1");
        assert_eq!(resolved.key, "sk-curl");
        assert_eq!(prompt.edit_calls.len(), 2);
        assert_eq!(prompt.edit_calls[0].initial, "https://api.example.com/v1");
        assert_eq!(prompt.edit_calls[1].initial, "sk-curl");
        assert!(prompt.confirm_calls[0].masked_key.contains("sk-"));
    }

    #[test]
    fn extracts_cherry_studio_api_keys_link() {
        let extraction = extract_candidates(
            "cherrystudio://providers/api-keys?v=1&data=eyJpZCI6Im5ldy1hcGkiLCJiYXNlVXJsIjoiaHR0cHM6Ly9vcGVuYWkuYXBpLXRlc3QudXMuY2kiLCJhcGlLZXkiOiJzay04OG9pSmZIb1FjWU5PczYzYnFYY2E3c01CR01wVk5IT28xeWtuQWpERDl1T0hFRnYifQ%3D%3D",
        );

        assert_eq!(
            extraction.best_base_url().map(|candidate| candidate.value.as_str()),
            Some("https://openai.api-test.us.ci")
        );
        assert_eq!(
            extraction.best_key().map(|candidate| candidate.value.as_str()),
            Some("sk-88oiJfHoQcYNOs63bqXca7sMBGMpVNHOo1yknAjDD9uOHEFv")
        );
    }

    #[test]
    fn ignores_invalid_cherry_studio_data() {
        let extraction = extract_candidates(
            "cherrystudio://providers/api-keys?v=1&data=not-valid-base64",
        );

        assert_eq!(extraction.best_base_url(), None);
        assert_eq!(extraction.best_key(), None);
    }

    #[test]
    fn ignores_non_api_keys_cherry_studio_link() {
        let extraction = extract_candidates(
            "cherrystudio://providers/models?v=1&data=eyJmb28iOiJiYXIifQ%3D%3D",
        );

        assert_eq!(extraction.best_base_url(), None);
        assert_eq!(extraction.best_key(), None);
    }

    #[test]
    fn extracts_url_safe_cherry_studio_api_keys_link() {
        let payload = r#"{"id":"new-api","baseUrl":"https://openai.api-test.us.ci/v1/chat/completions","apiKey":"sk-url-safe"}"#;
        let data = base64::engine::general_purpose::URL_SAFE.encode(payload);
        let extraction = extract_candidates(&format!(
            "cherrystudio://providers/api-keys?v=1&data={data}"
        ));

        assert_eq!(
            extraction.best_base_url().map(|candidate| candidate.value.as_str()),
            Some("https://openai.api-test.us.ci/v1")
        );
        assert_eq!(
            extraction.best_key().map(|candidate| candidate.value.as_str()),
            Some("sk-url-safe")
        );
    }

    #[test]
    fn ignores_cherry_studio_link_with_invalid_json_payload() {
        let payload =
            base64::engine::general_purpose::STANDARD.encode("not-json");
        let extraction = extract_candidates(&format!(
            "cherrystudio://providers/api-keys?v=1&data={payload}"
        ));

        assert_eq!(extraction.best_base_url(), None);
        assert_eq!(extraction.best_key(), None);
    }

    #[test]
    fn ignores_cherry_studio_link_without_required_fields() {
        let payload = base64::engine::general_purpose::STANDARD
            .encode(r#"{"id":"new-api","baseUrl":"https://openai.api-test.us.ci"}"#);
        let extraction = extract_candidates(&format!(
            "cherrystudio://providers/api-keys?v=1&data={payload}"
        ));

        assert_eq!(extraction.best_base_url(), None);
        assert_eq!(extraction.best_key(), None);
    }
}
