use crate::probe::build_probe_url;
use anyhow::{Context as _, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::time::{Duration, Instant};

const BENCHMARK_TIMEOUT: Duration = Duration::from_secs(20);
const BENCHMARK_PROMPT: &str = "Reply with exactly OK. Do not add anything else.";

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBenchmarkStats {
    pub rounds: u32,
    pub median_ms: u64,
    pub avg_ms: u64,
    pub success_rate: f64,
    pub stability_ms: u64,
    pub samples_ms: Vec<u64>,
    pub first_token_median_ms: Option<u64>,
    pub first_token_avg_ms: Option<u64>,
    pub first_token_samples_ms: Vec<u64>,
    pub detail: Option<String>,
}

impl ProviderBenchmarkStats {
    pub fn score(&self) -> i64 {
        (self.success_rate * 100000.0).round() as i64
            - self.median_ms as i64
            - (self.stability_ms as i64 * 2)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderBenchmarkOutcome {
    Success(ProviderBenchmarkStats),
    MissingBaseUrl,
    MissingToken,
    MissingModel,
    InvalidBaseUrl(String),
    Error(String),
}

impl ProviderBenchmarkOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Success(stats) => {
                let mut parts = vec![
                    "ok".to_string(),
                    format!("success={}%", (stats.success_rate * 100.0).round() as u64),
                    format!("median={}ms", stats.median_ms),
                    format!("avg={}ms", stats.avg_ms),
                    format!("stability={}ms", stats.stability_ms),
                ];
                if let Some(first_token_ms) = stats.first_token_median_ms {
                    parts.push(format!("firstToken={}ms", first_token_ms));
                }
                if let Some(detail) = &stats.detail {
                    parts.push(format!("detail={detail}"));
                }
                parts.join(" ")
            }
            Self::MissingBaseUrl => "error missing base_url".to_string(),
            Self::MissingToken => "error missing token".to_string(),
            Self::MissingModel => "error missing model".to_string(),
            Self::InvalidBaseUrl(message) => format!("error invalid base_url: {message}"),
            Self::Error(message) => format!("error {message}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderBenchmarkResult {
    pub id: String,
    pub outcome: ProviderBenchmarkOutcome,
}

impl ProviderBenchmarkResult {
    pub fn new(id: impl Into<String>, outcome: ProviderBenchmarkOutcome) -> Self {
        Self {
            id: id.into(),
            outcome,
        }
    }

    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }

    pub fn stats(&self) -> Option<&ProviderBenchmarkStats> {
        match &self.outcome {
            ProviderBenchmarkOutcome::Success(stats) => Some(stats),
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        self.outcome.summary()
    }
}

pub trait BenchmarkRunner {
    fn benchmark(
        &self,
        id: &str,
        base_url: &str,
        token: &str,
        model: &str,
        rounds: u32,
    ) -> ProviderBenchmarkResult;
}

pub struct HttpBenchmarkRunner {
    client: Client,
}

impl HttpBenchmarkRunner {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(BENCHMARK_TIMEOUT)
            .build()
            .with_context(|| "failed to build HTTP benchmark client")?;
        Ok(Self { client })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BenchmarkRoundSample {
    elapsed_ms: u64,
    first_token_ms: Option<u64>,
}

impl BenchmarkRunner for HttpBenchmarkRunner {
    fn benchmark(
        &self,
        id: &str,
        base_url: &str,
        token: &str,
        model: &str,
        rounds: u32,
    ) -> ProviderBenchmarkResult {
        let benchmark_url = match build_probe_url(base_url) {
            Ok(url) => url,
            Err(err) => {
                return ProviderBenchmarkResult::new(
                    id,
                    ProviderBenchmarkOutcome::InvalidBaseUrl(err.to_string()),
                )
            }
        };

        let rounds = rounds.max(1);
        let mut samples = Vec::new();
        let mut first_token_samples = Vec::new();
        let mut errors = Vec::new();

        for _ in 0..rounds {
            match run_benchmark_round(&self.client, &benchmark_url, token, model) {
                Ok(sample) => {
                    if let Some(first_token_ms) = sample.first_token_ms {
                        first_token_samples.push(first_token_ms);
                    }
                    samples.push(sample.elapsed_ms);
                }
                Err(err) => errors.push(err),
            }
        }

        if samples.is_empty() {
            let detail = dedupe_strings(errors).into_iter().next().unwrap_or_else(|| {
                "benchmark failed without a readable error".to_string()
            });
            return ProviderBenchmarkResult::new(id, ProviderBenchmarkOutcome::Error(detail));
        }

        let detail = dedupe_strings(errors).into_iter().next();
        let stats = ProviderBenchmarkStats {
            rounds,
            median_ms: median_of(&samples),
            avg_ms: average_of(&samples),
            success_rate: samples.len() as f64 / rounds as f64,
            stability_ms: compute_stability(&samples),
            samples_ms: samples,
            first_token_median_ms: (!first_token_samples.is_empty())
                .then(|| median_of(&first_token_samples)),
            first_token_avg_ms: (!first_token_samples.is_empty())
                .then(|| average_of(&first_token_samples)),
            first_token_samples_ms: first_token_samples,
            detail,
        };

        ProviderBenchmarkResult::new(id, ProviderBenchmarkOutcome::Success(stats))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkRanking {
    pub ordered_ids: Vec<String>,
    pub fastest_id: Option<String>,
    pub quickest_first_token_id: Option<String>,
    pub most_stable_id: Option<String>,
    pub recommended_id: Option<String>,
}

pub fn rank_benchmark_results(results: &[ProviderBenchmarkResult]) -> BenchmarkRanking {
    let mut successful = results
        .iter()
        .enumerate()
        .filter_map(|(index, result)| result.stats().map(|stats| (index, &result.id, stats)))
        .collect::<Vec<_>>();
    successful.sort_by(|left, right| {
        right
            .2
            .score()
            .cmp(&left.2.score())
            .then_with(|| left.0.cmp(&right.0))
    });

    let failed_ids = results
        .iter()
        .filter(|result| !result.is_success())
        .map(|result| result.id.clone());

    let mut ordered_ids = successful
        .iter()
        .map(|(_, id, _)| (*id).clone())
        .collect::<Vec<_>>();
    ordered_ids.extend(failed_ids);

    let fastest_id = results
        .iter()
        .filter_map(|result| result.stats().map(|stats| (&result.id, stats.median_ms)))
        .min_by_key(|(_, median_ms)| *median_ms)
        .map(|(id, _)| id.clone());

    let quickest_first_token_id = results
        .iter()
        .filter_map(|result| {
            result
                .stats()
                .and_then(|stats| stats.first_token_median_ms.map(|ms| (&result.id, ms)))
        })
        .min_by_key(|(_, first_token_ms)| *first_token_ms)
        .map(|(id, _)| id.clone());

    let most_stable_id = results
        .iter()
        .filter_map(|result| result.stats().map(|stats| (&result.id, stats.stability_ms)))
        .min_by_key(|(_, stability_ms)| *stability_ms)
        .map(|(id, _)| id.clone());

    let recommended_id = successful.first().map(|(_, id, _)| (*id).clone());

    BenchmarkRanking {
        ordered_ids,
        fastest_id,
        quickest_first_token_id,
        most_stable_id,
        recommended_id,
    }
}

fn run_benchmark_round(
    client: &Client,
    benchmark_url: &str,
    token: &str,
    model: &str,
) -> Result<BenchmarkRoundSample, String> {
    match request_stream_round(client, benchmark_url, token, model) {
        Ok(sample) => Ok(sample),
        Err(stream_err) => match request_fallback_round(client, benchmark_url, token, model) {
            Ok(sample) => Ok(sample),
            Err(fallback_err) => Err(dedupe_strings(vec![fallback_err, stream_err])
                .into_iter()
                .next()
                .unwrap_or_else(|| "benchmark failed".to_string())),
        },
    }
}

fn request_stream_round(
    client: &Client,
    benchmark_url: &str,
    token: &str,
    model: &str,
) -> Result<BenchmarkRoundSample, String> {
    let started_at = Instant::now();
    let mut response = client
        .post(benchmark_url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(CONTENT_TYPE, "application/json")
        .body(format!(
            r#"{{"model":"{}","messages":[{{"role":"user","content":"{}"}}],"max_tokens":8,"stream":true}}"#,
            escape_json_string(model),
            escape_json_string(BENCHMARK_PROMPT)
        ))
        .send()
        .map_err(|err| err.to_string())?;

    if !response.status().is_success() {
        return Err(response_error_message(response));
    }

    let mut buffer = [0_u8; 1024];
    let mut pending = String::new();
    let mut collected_text = String::new();
    let mut first_token_ms = None;

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|err| format!("stream read failed: {err}"))?;
        if read == 0 {
            break;
        }

        pending.push_str(&String::from_utf8_lossy(&buffer[..read]));
        pending = pending.replace("\r\n", "\n");

        while let Some(boundary) = pending.find("\n\n") {
            let chunk = pending[..boundary].to_string();
            pending = pending[boundary + 2..].to_string();
            for line in chunk.lines().map(str::trim).filter(|line| line.starts_with("data:")) {
                let data = line.trim_start_matches("data:").trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(payload) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let delta_text = extract_stream_delta_text(&payload);
                if delta_text.trim().is_empty() {
                    continue;
                }
                if first_token_ms.is_none() {
                    first_token_ms = Some(started_at.elapsed().as_millis() as u64);
                }
                collected_text.push_str(&delta_text);
            }
        }
    }

    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    if collected_text.trim().is_empty() {
        return Err("streaming response returned no readable content".to_string());
    }

    Ok(BenchmarkRoundSample {
        elapsed_ms,
        first_token_ms,
    })
}

fn request_fallback_round(
    client: &Client,
    benchmark_url: &str,
    token: &str,
    model: &str,
) -> Result<BenchmarkRoundSample, String> {
    let started_at = Instant::now();
    let response = client
        .post(benchmark_url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(CONTENT_TYPE, "application/json")
        .body(format!(
            r#"{{"model":"{}","messages":[{{"role":"user","content":"{}"}}],"max_tokens":8}}"#,
            escape_json_string(model),
            escape_json_string(BENCHMARK_PROMPT)
        ))
        .send()
        .map_err(|err| err.to_string())?;

    if !response.status().is_success() {
        return Err(response_error_message(response));
    }

    let body = response.text().map_err(|err| err.to_string())?;
    if !response_has_message_text(&body) {
        return Err("response returned no readable content".to_string());
    }

    Ok(BenchmarkRoundSample {
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        first_token_ms: None,
    })
}

fn response_error_message(response: Response) -> String {
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    if let Ok(payload) = serde_json::from_str::<Value>(&body) {
        if let Some(message) = extract_error_message(&payload) {
            return format!("http {status}: {message}");
        }
    }

    let detail = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.is_empty() {
        format!("http {status}")
    } else {
        format!("http {status}: {detail}")
    }
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
            (!combined.trim().is_empty()).then_some(combined)
        }
        _ => None,
    }
}

fn extract_stream_delta_text(payload: &Value) -> String {
    let Some(first_choice) = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return String::new();
    };
    let Some(delta) = first_choice.get("delta") else {
        return String::new();
    };

    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        return text.to_string();
    }

    if let Some(parts) = delta.get("content").and_then(Value::as_array) {
        return parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
    }

    delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn extract_error_message(payload: &Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|error| match error {
            Value::String(message) => Some(message.as_str()),
            Value::Object(map) => map.get("message").and_then(Value::as_str),
            _ => None,
        })
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty())
}

fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for value in values.into_iter().map(|value| value.trim().to_string()) {
        if value.is_empty() || !seen.insert(value.clone()) {
            continue;
        }
        deduped.push(value);
    }
    deduped
}

fn median_of(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        ((sorted[middle - 1] + sorted[middle]) as f64 / 2.0).round() as u64
    }
}

fn average_of(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    (values.iter().sum::<u64>() as f64 / values.len() as f64).round() as u64
}

fn compute_stability(values: &[u64]) -> u64 {
    if values.len() <= 1 {
        return 0;
    }
    values.iter().max().copied().unwrap_or(0) - values.iter().min().copied().unwrap_or(0)
}

fn escape_json_string(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn http_benchmark_runner_records_stream_first_token() {
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
            assert!(request.contains(r#""stream":true"#));
            assert!(request.contains(r#""model":"gpt-5.4""#));
            assert!(request.contains(BENCHMARK_PROMPT));

            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: [DONE]\n\n";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .expect("response should write");
        });

        let runner = HttpBenchmarkRunner::new().expect("runner should build");
        let result = runner.benchmark("demo", &format!("http://{addr}/v1"), "sk-test", "gpt-5.4", 1);
        handle.join().expect("server thread should finish");

        let stats = result.stats().expect("benchmark should succeed");
        assert_eq!(stats.rounds, 1);
        assert_eq!(stats.success_rate, 1.0);
        assert_eq!(stats.samples_ms.len(), 1);
        assert_eq!(stats.first_token_samples_ms.len(), 1);
        assert!(stats.first_token_median_ms.is_some());
    }

    #[test]
    fn http_benchmark_runner_falls_back_to_non_stream_request() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener.local_addr().expect("listener should expose addr");

        let handle = std::thread::spawn(move || {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept should succeed");
                let mut buf = [0_u8; 4096];
                let read = stream.read(&mut buf).expect("request should read");
                let request = String::from_utf8_lossy(&buf[..read]);
                if request_index == 0 {
                    assert!(request.contains(r#""stream":true"#));
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"stream unavailable\"}}",
                        )
                        .expect("stream failure response should write");
                } else {
                    assert!(!request.contains(r#""stream":true"#));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"OK\"}}]}",
                        )
                        .expect("fallback response should write");
                }
            }
        });

        let runner = HttpBenchmarkRunner::new().expect("runner should build");
        let result = runner.benchmark("demo", &format!("http://{addr}/v1"), "sk-test", "gpt-5.4", 1);
        handle.join().expect("server thread should finish");

        let stats = result.stats().expect("benchmark should succeed");
        assert_eq!(stats.rounds, 1);
        assert_eq!(stats.first_token_median_ms, None);
        assert_eq!(stats.detail, None);
    }

    #[test]
    fn rank_benchmark_results_matches_recommended_score_and_failed_order() {
        let results = vec![
            ProviderBenchmarkResult::new(
                "slow",
                ProviderBenchmarkOutcome::Success(ProviderBenchmarkStats {
                    rounds: 2,
                    median_ms: 320,
                    avg_ms: 325,
                    success_rate: 1.0,
                    stability_ms: 10,
                    samples_ms: vec![320, 330],
                    first_token_median_ms: Some(120),
                    first_token_avg_ms: Some(125),
                    first_token_samples_ms: vec![120, 130],
                    detail: None,
                }),
            ),
            ProviderBenchmarkResult::new(
                "failfirst",
                ProviderBenchmarkOutcome::Error("timeout".to_string()),
            ),
            ProviderBenchmarkResult::new(
                "fast",
                ProviderBenchmarkOutcome::Success(ProviderBenchmarkStats {
                    rounds: 2,
                    median_ms: 180,
                    avg_ms: 185,
                    success_rate: 1.0,
                    stability_ms: 5,
                    samples_ms: vec![180, 190],
                    first_token_median_ms: Some(70),
                    first_token_avg_ms: Some(75),
                    first_token_samples_ms: vec![70, 80],
                    detail: None,
                }),
            ),
            ProviderBenchmarkResult::new(
                "failsecond",
                ProviderBenchmarkOutcome::Error("http 503".to_string()),
            ),
        ];

        let ranking = rank_benchmark_results(&results);

        assert_eq!(
            ranking.ordered_ids,
            vec![
                "fast".to_string(),
                "slow".to_string(),
                "failfirst".to_string(),
                "failsecond".to_string()
            ]
        );
        assert_eq!(ranking.fastest_id.as_deref(), Some("fast"));
        assert_eq!(ranking.quickest_first_token_id.as_deref(), Some("fast"));
        assert_eq!(ranking.most_stable_id.as_deref(), Some("fast"));
        assert_eq!(ranking.recommended_id.as_deref(), Some("fast"));
    }
}
