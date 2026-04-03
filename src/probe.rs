use anyhow::{Context as _, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Success(u16),
    HttpStatus(u16),
    MissingBaseUrl,
    MissingToken,
    MissingModel,
    InvalidBaseUrl(String),
    TransportError(String),
    MissingResponseText,
}

impl ProbeOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Success(status) => format!("ok {status}"),
            Self::HttpStatus(status) => format!("http {status}"),
            Self::MissingBaseUrl => "missing base_url".to_string(),
            Self::MissingToken => "missing token".to_string(),
            Self::MissingModel => "missing model".to_string(),
            Self::InvalidBaseUrl(message) => format!("invalid base_url: {message}"),
            Self::TransportError(message) => format!("transport: {message}"),
            Self::MissingResponseText => "missing response text".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub id: String,
    pub outcome: ProbeOutcome,
}

impl ProbeResult {
    pub fn new(id: impl Into<String>, outcome: ProbeOutcome) -> Self {
        Self {
            id: id.into(),
            outcome,
        }
    }

    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }

    pub fn summary(&self) -> String {
        self.outcome.summary()
    }
}

pub trait ProbeRunner {
    fn probe(&self, id: &str, base_url: &str, token: &str, model: &str) -> ProbeResult;
}

pub struct HttpProbeRunner {
    client: Client,
}

impl HttpProbeRunner {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .with_context(|| "failed to build HTTP probe client")?;
        Ok(Self { client })
    }
}

impl ProbeRunner for HttpProbeRunner {
    fn probe(&self, id: &str, base_url: &str, token: &str, model: &str) -> ProbeResult {
        let probe_url = match build_probe_url(base_url) {
            Ok(url) => url,
            Err(err) => return ProbeResult::new(id, ProbeOutcome::InvalidBaseUrl(err.to_string())),
        };

        match self
            .client
            .post(&probe_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json")
            .body(format!(
                r#"{{"model":"{}","messages":[{{"role":"user","content":"你好，请回复：ok"}}],"max_tokens":12}}"#,
                model.replace('\\', "\\\\").replace('"', "\\\"")
            ))
            .send()
        {
            Ok(response) if response.status().is_success() => {
                let status = response.status().as_u16();
                match response.text() {
                Ok(body) if response_has_message_text(&body) => {
                    ProbeResult::new(id, ProbeOutcome::Success(status))
                }
                Ok(_) => ProbeResult::new(id, ProbeOutcome::MissingResponseText),
                Err(err) => ProbeResult::new(id, ProbeOutcome::TransportError(err.to_string())),
                }
            }
            Ok(response) => ProbeResult::new(id, ProbeOutcome::HttpStatus(response.status().as_u16())),
            Err(err) => ProbeResult::new(id, ProbeOutcome::TransportError(err.to_string())),
        }
    }
}

pub fn build_probe_url(base_url: &str) -> Result<String> {
    let mut url =
        url::Url::parse(base_url).with_context(|| format!("invalid base_url '{base_url}'"))?;
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("base_url cannot be used as a base URL"))?;
        segments.push("chat");
        segments.push("completions");
    }
    Ok(url.to_string())
}

fn response_has_message_text(body: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        return false;
    };

    extract_message_text(&payload)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

fn extract_message_text(payload: &Value) -> Option<String> {
    let choices = payload.get("choices")?.as_array()?;
    let first_choice = choices.first()?;
    let message = first_choice.get("message")?;
    let content = message.get("content")?;

    match content {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Array(parts) => {
            let combined = parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(map) => map
                        .get("text")
                        .and_then(|text| text.as_str())
                        .map(|text| text.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            if combined.trim().is_empty() {
                None
            } else {
                Some(combined)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn build_probe_url_appends_chat_completions() {
        assert_eq!(
            build_probe_url("https://api.example.com/v1").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn http_probe_runner_sends_bearer_to_chat_completions() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener.local_addr().expect("listener should expose addr");

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept should succeed");
            let mut buf = [0_u8; 4096];
            let read = stream.read(&mut buf).expect("request should read");
            let request = String::from_utf8_lossy(&buf[..read]);
            let request_lower = request.to_ascii_lowercase();
            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            assert!(request_lower.contains("authorization: bearer sk-test"));
            assert!(request_lower.contains("content-type: application/json"));
            assert!(request.contains(r#""model":"gpt-5.4""#));
            assert!(request.contains(r#""content":"你好，请回复：ok""#));

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}",
                )
                .expect("response should write");
        });

        let runner = HttpProbeRunner::new().expect("runner should build");
        let result = runner.probe("demo", &format!("http://{addr}/v1"), "sk-test", "gpt-5.4");
        handle.join().expect("server thread should finish");

        assert_eq!(result.outcome, ProbeOutcome::Success(200));
    }

    #[test]
    fn http_probe_runner_reports_missing_response_text() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener.local_addr().expect("listener should expose addr");

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept should succeed");
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).expect("request should read");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"\"}}]}",
                )
                .expect("response should write");
        });

        let runner = HttpProbeRunner::new().expect("runner should build");
        let result = runner.probe("demo", &format!("http://{addr}/v1"), "sk-test", "gpt-5.4");
        handle.join().expect("server thread should finish");

        assert_eq!(result.outcome, ProbeOutcome::MissingResponseText);
    }
}
