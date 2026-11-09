//! Client for ZeroClaw's A2A `SendMessage` endpoint (`POST /a2a/<alias>`).
//!
//! Each pipeline role is a ZeroClaw agent. Calling it through the gateway,
//! rather than calling a model vendor directly, keeps provider routing,
//! retries, and cost recording in ZeroClaw: every token the harness spends
//! lands in the daemon's ledger under the role agent's alias.

use std::future::Future;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use url::Url;

/// Sends one message to a named agent and returns its text reply.
pub trait AgentClient {
    fn send(&self, agent: &str, text: &str) -> impl Future<Output = Result<String>> + Send;
}

/// The gateway A2A client. The bearer token is the gateway pairing token.
#[derive(Clone)]
pub struct A2aClient {
    client: reqwest::Client,
    base: Url,
    token: String,
}

impl std::fmt::Debug for A2aClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aClient")
            .field("base", &self.base.as_str())
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl A2aClient {
    pub fn new(base: Url, token: String, timeout_secs: u64) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("the gateway token is empty");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            base,
            token,
        })
    }

    fn endpoint(&self, agent: &str) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow::Error::msg("gateway_url cannot be a base URL"))?
            .pop_if_empty()
            .push("a2a")
            .push(agent);
        Ok(url)
    }
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<RpcResult>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RpcResult {
    task: Option<Task>,
}

#[derive(Debug, Deserialize)]
struct Task {
    status: TaskStatus,
    #[serde(default)]
    artifacts: Vec<Artifact>,
}

#[derive(Debug, Deserialize)]
struct TaskStatus {
    state: String,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
struct Part {
    text: Option<String>,
}

/// Extract the reply text from a `SendMessage` response body.
fn reply_text(body: &str) -> Result<String> {
    let response: RpcResponse =
        serde_json::from_str(body).context("gateway reply is not JSON-RPC")?;
    if let Some(err) = response.error {
        bail!("agent call failed: JSON-RPC {} {}", err.code, err.message);
    }
    let Some(task) = response.result.and_then(|r| r.task) else {
        bail!("gateway reply carries no task");
    };
    if task.status.state != "TASK_STATE_COMPLETED" {
        bail!("agent task ended in state {}", task.status.state);
    }
    let text = task
        .artifacts
        .iter()
        .flat_map(|a| a.parts.iter())
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        bail!("agent returned an empty reply");
    }
    Ok(text)
}

impl AgentClient for A2aClient {
    async fn send(&self, agent: &str, text: &str) -> Result<String> {
        let url = self.endpoint(agent)?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "SendMessage",
            "params": {
                "message": {
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "role": "ROLE_USER",
                    "parts": [{ "text": text }],
                }
            }
        });
        let response = self
            .client
            .post(url.clone())
            .bearer_auth(&self.token)
            .header("A2A-Version", "1.0")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("calling agent {agent}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("reading reply from agent {agent}"))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("the gateway rejected the token for agent {agent}");
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "agent {agent} is not published over A2A; set [a2a.server] enabled and \
                 [agents.{agent}.a2a] published"
            );
        }
        if !status.is_success() {
            bail!("agent {agent} answered HTTP {status}");
        }
        reply_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn completed(text: &str) -> serde_json::Value {
        json!({"id":1,"jsonrpc":"2.0","result":{"task":{
            "id":"t","contextId":"c",
            "status":{"state":"TASK_STATE_COMPLETED"},
            "artifacts":[{"artifactId":"a","parts":[{"text":text}]}]
        }}})
    }

    #[test]
    fn parses_completed_task_and_rejects_others() {
        assert_eq!(reply_text(&completed("PONG").to_string()).unwrap(), "PONG");
        let failed = json!({"id":1,"jsonrpc":"2.0","result":{"task":{
            "status":{"state":"TASK_STATE_FAILED"},"artifacts":[]}}});
        assert!(reply_text(&failed.to_string()).is_err());
        let rpc_err = json!({"id":1,"jsonrpc":"2.0","error":{"code":-32602,"message":"empty"}});
        assert!(
            reply_text(&rpc_err.to_string())
                .unwrap_err()
                .to_string()
                .contains("-32602")
        );
        assert!(reply_text(&completed("  ").to_string()).is_err());
    }

    #[tokio::test]
    async fn sends_spec_request_with_auth_and_version() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/a2a/brief_writer"))
            .and(header("authorization", "Bearer pair-token"))
            .and(header("a2a-version", "1.0"))
            .and(body_partial_json(
                json!({"jsonrpc":"2.0","method":"SendMessage"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(completed("{\"ok\":true}")))
            .expect(1)
            .mount(&server)
            .await;
        let client = A2aClient::new(server.uri().parse().unwrap(), "pair-token".into(), 5).unwrap();
        let reply = client.send("brief_writer", "hello").await.unwrap();
        assert_eq!(reply, "{\"ok\":true}");
    }

    #[tokio::test]
    async fn unauthorized_and_unpublished_are_explained() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/a2a/locked"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/a2a/hidden"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = A2aClient::new(server.uri().parse().unwrap(), "t".into(), 5).unwrap();
        let locked = client.send("locked", "x").await.unwrap_err().to_string();
        assert!(locked.contains("token"), "{locked}");
        let hidden = client.send("hidden", "x").await.unwrap_err().to_string();
        assert!(hidden.contains("published"), "{hidden}");
    }

    #[test]
    fn debug_redacts_token() {
        let client =
            A2aClient::new("http://127.0.0.1:1".parse().unwrap(), "secret".into(), 5).unwrap();
        assert!(!format!("{client:?}").contains("secret"));
    }
}
