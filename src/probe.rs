use anyhow::{Context as _, Result};
use reqwest::blocking::Client;
use reqwest::header::AUTHORIZATION;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Success(u16),
    HttpStatus(u16),
    MissingBaseUrl,
    MissingToken,
    InvalidBaseUrl(String),
    TransportError(String),
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
            Self::InvalidBaseUrl(message) => format!("invalid base_url: {message}"),
            Self::TransportError(message) => format!("transport: {message}"),
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
    fn probe(&self, id: &str, base_url: &str, token: &str) -> ProbeResult;
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
    fn probe(&self, id: &str, base_url: &str, token: &str) -> ProbeResult {
        let probe_url = match build_probe_url(base_url) {
            Ok(url) => url,
            Err(err) => return ProbeResult::new(id, ProbeOutcome::InvalidBaseUrl(err.to_string())),
        };

        match self
            .client
            .get(&probe_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
        {
            Ok(response) if response.status().is_success() => {
                ProbeResult::new(id, ProbeOutcome::Success(response.status().as_u16()))
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
        segments.push("models");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn build_probe_url_appends_models() {
        assert_eq!(
            build_probe_url("https://api.example.com/v1").unwrap(),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn http_probe_runner_sends_bearer_to_models() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener.local_addr().expect("listener should expose addr");

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept should succeed");
            let mut buf = [0_u8; 4096];
            let read = stream.read(&mut buf).expect("request should read");
            let request = String::from_utf8_lossy(&buf[..read]);
            let request_lower = request.to_ascii_lowercase();
            assert!(request.starts_with("GET /v1/models HTTP/1.1"));
            assert!(request_lower.contains("authorization: bearer sk-test"));

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .expect("response should write");
        });

        let runner = HttpProbeRunner::new().expect("runner should build");
        let result = runner.probe("demo", &format!("http://{addr}/v1"), "sk-test");
        handle.join().expect("server thread should finish");

        assert_eq!(result.outcome, ProbeOutcome::Success(200));
    }
}
