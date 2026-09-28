use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PeerScope {
    #[default]
    Actor,
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenVikingConfig {
    pub url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    pub recall: bool,
    pub capture: bool,
    pub skills: bool,
    pub peer_scope: PeerScope,
    pub max_context_tokens: u32,
}
impl Default for OpenVikingConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: None,
            timeout_secs: 15,
            recall: true,
            capture: true,
            skills: true,
            peer_scope: PeerScope::Actor,
            max_context_tokens: 1600,
        }
    }
}

#[derive(Debug, Error)]
pub enum OpenVikingError {
    #[error("OpenViking is disabled")]
    Disabled,
    #[error("invalid OpenViking URI: {0}")]
    InvalidUri(String),
    #[error("OpenViking HTTP status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("invalid OpenViking JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("OpenViking request timed out")]
    Timeout,
    #[error("OpenViking transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("OpenViking pending queue I/O: {0}")]
    PendingIo(#[from] std::io::Error),
    #[error("OpenViking server error {code}: {message} (trace {trace_id:?})")]
    Server {
        code: String,
        message: String,
        trace_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenVikingUri(String);
impl OpenVikingUri {
    pub fn parse(input: &str) -> Result<Self, OpenVikingError> {
        if input.chars().any(char::is_control) {
            return Err(OpenVikingError::InvalidUri("control character".into()));
        }
        let canonical = input
            .strip_prefix("ov://")
            .map(|value| format!("viking://{value}"))
            .or_else(|| {
                input
                    .strip_prefix("viking://")
                    .map(|value| format!("viking://{value}"))
            })
            .ok_or_else(|| {
                OpenVikingError::InvalidUri("scheme must be ov:// or viking://".into())
            })?;
        let path = canonical
            .strip_prefix("viking://")
            .ok_or_else(|| OpenVikingError::InvalidUri("missing scheme".into()))?;
        let segments: Vec<&str> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.is_empty()
            || segments
                .iter()
                .any(|segment| *segment == ".." || segment.contains(['\\', '?', '#']))
        {
            return Err(OpenVikingError::InvalidUri("invalid path".into()));
        }
        Ok(Self(canonical))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenVikingEntry {
    pub uri: String,
    pub name: String,
    pub is_dir: bool,
    #[serde(rename = "abstract", default)]
    pub abstract_text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenVikingDocument {
    pub uri: String,
    pub content: String,
}

#[derive(Clone)]
pub struct OpenVikingClient {
    config: OpenVikingConfig,
    http: reqwest::Client,
    pending_root: PathBuf,
}
impl OpenVikingClient {
    pub fn new(config: OpenVikingConfig) -> Result<Option<Self>, OpenVikingError> {
        if config.url.trim().is_empty() {
            return Ok(None);
        }
        let pending_root = std::env::var_os("MOC_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".moc")))
            .unwrap_or_else(|| PathBuf::from(".moc"))
            .join("openviking")
            .join("pending");
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()?;
        Ok(Some(Self {
            config,
            http,
            pending_root,
        }))
    }
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.config.url.trim_end_matches('/'), path)
    }
    fn request(&self, req: reqwest::RequestBuilder, peer: Option<&str>) -> reqwest::RequestBuilder {
        let req = req
            .header("Content-Type", "application/json")
            .header("User-Agent", concat!("moc/", env!("CARGO_PKG_VERSION")));
        let req = if let Some(key) = self.config.api_key.as_deref().filter(|v| !v.is_empty()) {
            req.bearer_auth(key)
        } else {
            req
        };
        if let Some(peer) = peer {
            req.header("X-OpenViking-Actor-Peer", peer)
        } else {
            req
        }
    }
    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: reqwest::RequestBuilder,
        peer: Option<&str>,
    ) -> Result<T, OpenVikingError> {
        let response = self.request(req, peer).send().await.map_err(|error| {
            if error.is_timeout() {
                OpenVikingError::Timeout
            } else {
                OpenVikingError::Transport(error)
            }
        })?;
        let status = response.status();
        let body = response.text().await.map_err(OpenVikingError::Transport)?;
        if !status.is_success() {
            return Err(OpenVikingError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        let envelope: serde_json::Value = serde_json::from_str(&body)?;
        if let Some(error) = envelope.get("error").filter(|value| !value.is_null()) {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("SERVER_ERROR")
                .to_owned();
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("OpenViking server error")
                .to_owned();
            let trace_id = envelope
                .get("trace_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            return Err(OpenVikingError::Server {
                code,
                message,
                trace_id,
            });
        }
        let result = envelope.get("result").ok_or_else(|| {
            OpenVikingError::InvalidJson(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing result",
            )))
        })?;
        serde_json::from_value(result.clone()).map_err(OpenVikingError::InvalidJson)
    }
    fn pending_path(&self, session_id: &str, peer: &str) -> PathBuf {
        let peer = sanitize_segment(peer);
        let session = sanitize_segment(session_id);
        self.pending_root
            .join(peer)
            .join(format!("{session}.jsonl"))
    }

    async fn queue_messages(
        &self,
        session_id: &str,
        messages: &[serde_json::Value],
        peer: &str,
    ) -> Result<(), OpenVikingError> {
        if messages.is_empty() {
            return Ok(());
        }
        let path = self.pending_path(session_id, peer);
        let parent = path.parent().ok_or_else(|| {
            OpenVikingError::PendingIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pending path has no parent",
            ))
        })?;
        tokio::fs::create_dir_all(parent).await?;
        let mut content = tokio::fs::read(&path).await.unwrap_or_default();
        for message in messages {
            content.extend_from_slice(&serde_json::to_vec(message)?);
            content.push(b'\n');
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("pending"),
            stamp
        ));
        tokio::fs::write(&temporary, content).await?;
        set_private_permissions(&temporary).await?;
        tokio::fs::rename(&temporary, &path).await?;
        Ok(())
    }

    async fn send_batch(
        &self,
        session_id: &str,
        messages: &[serde_json::Value],
        peer: &str,
    ) -> Result<serde_json::Value, OpenVikingError> {
        self.send_json(
            self.http
                .post(self.endpoint(&format!("/api/v1/sessions/{session_id}/messages/batch")))
                .json(&serde_json::json!({"peer_id":peer,"messages":messages})),
            Some(peer),
        )
        .await
    }

    async fn replay_pending(&self, session_id: &str, peer: &str) -> Result<(), OpenVikingError> {
        let path = self.pending_path(session_id, peer);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut messages = Vec::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            messages.push(serde_json::from_slice::<serde_json::Value>(line)?);
        }
        for chunk in messages.chunks(100) {
            self.send_batch(session_id, chunk, peer).await?;
        }
        tokio::fs::remove_file(path).await?;
        Ok(())
    }
    pub async fn list(
        &self,
        uri: &str,
        peer: Option<&str>,
        recursive: bool,
        limit: usize,
    ) -> Result<Vec<OpenVikingEntry>, OpenVikingError> {
        let uri = OpenVikingUri::parse(uri)?;
        let url = format!(
            "{}?uri={}&recursive={}&limit={}",
            self.endpoint("/api/v1/fs/ls"),
            encode_query(uri.as_str()),
            recursive,
            limit
        );
        self.send_json(self.http.get(url), peer).await
    }
    pub async fn read(
        &self,
        uri: &str,
        peer: Option<&str>,
        offset: usize,
        limit: i64,
    ) -> Result<OpenVikingDocument, OpenVikingError> {
        let uri = OpenVikingUri::parse(uri)?;
        let url = format!(
            "{}?uri={}&offset={}&limit={}",
            self.endpoint("/api/v1/content/read"),
            encode_query(uri.as_str()),
            offset,
            limit
        );
        let result: serde_json::Value = self.send_json(self.http.get(url), peer).await?;
        if let Some(content) = result.as_str() {
            return Ok(OpenVikingDocument {
                uri: uri.as_str().to_owned(),
                content: content.to_owned(),
            });
        }
        serde_json::from_value(result).map_err(OpenVikingError::InvalidJson)
    }
    pub async fn write(
        &self,
        uri: &str,
        content: &str,
        peer: Option<&str>,
    ) -> Result<serde_json::Value, OpenVikingError> {
        let uri = OpenVikingUri::parse(uri)?;
        self.send_json(self.http.post(self.endpoint("/api/v1/content/write")).json(&serde_json::json!({"uri": uri.as_str(), "content": content, "mode":"replace", "wait":false})), peer).await
    }
    pub async fn delete(
        &self,
        uri: &str,
        peer: Option<&str>,
        recursive: bool,
    ) -> Result<serde_json::Value, OpenVikingError> {
        let uri = OpenVikingUri::parse(uri)?;
        let url = format!(
            "{}?uri={}&recursive={}",
            self.endpoint("/api/v1/fs"),
            encode_query(uri.as_str()),
            recursive
        );
        self.send_json(self.http.delete(url), peer).await
    }
    pub async fn search_context(
        &self,
        query: &str,
        session_id: Option<&str>,
        peer: Option<&str>,
    ) -> Result<serde_json::Value, OpenVikingError> {
        self.send_json(self.http.post(self.endpoint("/api/v1/search/search")).json(&serde_json::json!({"query": query, "mode":"context", "purpose":"coding", "peer_scope":"actor", "max_tokens": self.config.max_context_tokens, "session_id":session_id})), peer).await
    }
    pub async fn append_messages(
        &self,
        session_id: &str,
        messages: &[serde_json::Value],
        peer: &str,
    ) -> Result<serde_json::Value, OpenVikingError> {
        let _ = self.replay_pending(session_id, peer).await;
        let mut last = serde_json::Value::Null;
        for (index, chunk) in messages.chunks(100).enumerate() {
            match self.send_batch(session_id, chunk, peer).await {
                Ok(result) => last = result,
                Err(OpenVikingError::HttpStatus {
                    status: 404 | 405, ..
                }) => {
                    for message in chunk {
                        last = self
                            .send_json(
                                self.http
                                    .post(self.endpoint(&format!(
                                        "/api/v1/sessions/{session_id}/messages"
                                    )))
                                    .json(&serde_json::json!({"peer_id":peer,"message":message})),
                                Some(peer),
                            )
                            .await?;
                    }
                }
                Err(error) => {
                    self.queue_messages(session_id, &messages[index * 100..], peer)
                        .await?;
                    return Err(error);
                }
            }
        }
        Ok(last)
    }
    pub async fn create_session(
        &self,
        session_id: &str,
        peer: &str,
    ) -> Result<serde_json::Value, OpenVikingError> {
        self.send_json(
            self.http
                .post(self.endpoint("/api/v1/sessions"))
                .json(&serde_json::json!({"session_id":session_id,"peer_id":peer})),
            Some(peer),
        )
        .await
    }
    pub async fn commit_session(
        &self,
        session_id: &str,
        peer: &str,
    ) -> Result<serde_json::Value, OpenVikingError> {
        self.send_json(
            self.http
                .post(self.endpoint(&format!("/api/v1/sessions/{session_id}/commit")))
                .json(&serde_json::json!({"peer_id":peer})),
            Some(peer),
        )
        .await
    }
    pub async fn list_skills(
        &self,
        peer: Option<&str>,
    ) -> Result<serde_json::Value, OpenVikingError> {
        self.send_json(self.http.get(self.endpoint("/api/v1/skills")), peer)
            .await
    }
}
fn sanitize_segment(value: &str) -> String {
    let result: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if result.is_empty() {
        "unknown".into()
    } else {
        result
    }
}

#[cfg(unix)]
async fn set_private_permissions(path: &Path) -> Result<(), OpenVikingError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_permissions(_path: &Path) -> Result<(), OpenVikingError> {
    Ok(())
}

fn encode_query(value: &str) -> String {
    value
        .bytes()
        .fold(String::with_capacity(value.len()), |mut encoded, byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
                encoded.push(byte as char);
            } else {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
            encoded
        })
}

#[derive(Debug, Clone, Copy)]
pub enum PeerSource {
    GitRemote,
    GitRoot,
    Workspace,
}
#[derive(Debug, Clone)]
pub struct WorkspaceIdentity {
    pub canonical_root: PathBuf,
    pub peer_id: String,
    pub source: PeerSource,
}
pub fn resolve_peer_id(cwd: &Path) -> Result<WorkspaceIdentity, std::io::Error> {
    let canonical_root = cwd.canonicalize()?;
    let git_root = std::process::Command::new("git")
        .args([
            "-C",
            canonical_root.to_string_lossy().as_ref(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| PathBuf::from(value.trim()));
    if let Some(root) = git_root {
        let remote = std::process::Command::new("git")
            .args([
                "-C",
                root.to_string_lossy().as_ref(),
                "remote",
                "get-url",
                "origin",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned());
        if let Some(remote) = remote.filter(|value| !value.is_empty()) {
            return Ok(WorkspaceIdentity {
                canonical_root,
                peer_id: sanitize_peer(&remote),
                source: PeerSource::GitRemote,
            });
        }
        let canonical_root = root.canonicalize()?;
        return Ok(WorkspaceIdentity {
            peer_id: sanitize_peer(&canonical_root.to_string_lossy()),
            canonical_root,
            source: PeerSource::GitRoot,
        });
    }
    Ok(WorkspaceIdentity {
        peer_id: sanitize_peer(&crate_path_encoding(&canonical_root)),
        canonical_root,
        source: PeerSource::Workspace,
    })
}

fn crate_path_encoding(path: &Path) -> String {
    path.to_string_lossy().replace(['/', '\\', ':'], "-")
}
fn sanitize_peer(value: &str) -> String {
    let value = value
        .trim()
        .trim_end_matches('/')
        .replace("git@", "")
        .replace("https://", "")
        .replace("http://", "");
    value
        .replace(['/', '\\', ':', '@', '.'], "-")
        .trim_matches('-')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonicalizes_alias_and_rejects_traversal() {
        assert_eq!(
            OpenVikingUri::parse("ov://resources/x")
                .expect("valid URI")
                .as_str(),
            "viking://resources/x"
        );
        assert_eq!(
            OpenVikingUri::parse("viking://resources/x")
                .expect("valid URI")
                .as_str(),
            "viking://resources/x"
        );
        assert!(OpenVikingUri::parse("viking://resources/../secret").is_err());
        assert!(OpenVikingUri::parse("/tmp/file").is_err());
    }
    #[test]
    fn empty_url_disables_client() {
        assert!(
            OpenVikingClient::new(OpenVikingConfig::default())
                .expect("config builds")
                .is_none()
        );
    }
    #[test]
    fn absolute_workspace_paths_are_distinct() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let left = resolve_peer_id(a.path()).expect("identity");
        let right = resolve_peer_id(b.path()).expect("identity");
        assert_ne!(left.peer_id, right.peer_id);
        assert!(left.peer_id.contains("tmp"));
    }
}

#[cfg(test)]
mod wiremock_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    #[tokio::test]
    async fn sends_envelope_headers_and_context_contract() {
        let server = MockServer::start().await;
        let config = OpenVikingConfig {
            url: server.uri(),
            api_key: Some("secret".into()),
            ..Default::default()
        };
        let client = OpenVikingClient::new(config)
            .expect("client config")
            .expect("enabled");
        Mock::given(method("POST")).and(path("/api/v1/search/search")).and(header("authorization", "Bearer secret")).and(header("x-openviking-actor-peer", "peer-a")).and(body_json(serde_json::json!({"query":"hello","mode":"context","purpose":"coding","peer_scope":"actor","max_tokens":1600,"session_id":"moc-s"}))).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status":"ok","result":{"entries":[]},"error":null,"trace_id":"t"}))).mount(&server).await;
        let result = client
            .search_context("hello", Some("moc-s"), Some("peer-a"))
            .await
            .expect("context response");
        assert_eq!(result["entries"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn maps_server_envelope_error() {
        let server = MockServer::start().await;
        let client = OpenVikingClient::new(OpenVikingConfig {
            url: server.uri(),
            ..Default::default()
        })
        .expect("client config")
        .expect("enabled");
        Mock::given(method("GET")).and(path("/api/v1/skills")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status":"error","result":null,"error":{"code":"NOPE","message":"offline"},"trace_id":"trace"}))).mount(&server).await;
        assert!(
            matches!(client.list_skills(None).await, Err(OpenVikingError::Server { code, trace_id: Some(_), .. }) if code == "NOPE")
        );
    }
    #[tokio::test]
    async fn queues_failed_batch_and_replays_in_order() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().expect("pending directory");
        let client = OpenVikingClient::new(OpenVikingConfig {
            url: server.uri(),
            ..Default::default()
        })
        .expect("client config")
        .expect("enabled");
        let mut client = client;
        client.pending_root = directory.path().to_owned();
        Mock::given(method("POST"))
            .and(path("/api/v1/sessions/moc-s/messages/batch"))
            .respond_with(ResponseTemplate::new(503).set_body_string("offline"))
            .expect(1)
            .mount(&server)
            .await;
        let first = client
            .append_messages("moc-s", &[serde_json::json!({"seq":1})], "peer-a")
            .await;
        assert!(matches!(
            first,
            Err(OpenVikingError::HttpStatus { status: 503, .. })
        ));
        assert!(client.pending_path("moc-s", "peer-a").exists());
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/sessions/moc-s/messages/batch"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"result":{"accepted":true}})),
            )
            .expect(2)
            .mount(&server)
            .await;
        client
            .append_messages("moc-s", &[serde_json::json!({"seq":2})], "peer-a")
            .await
            .expect("replay and append");
        assert!(!client.pending_path("moc-s", "peer-a").exists());
    }
}
