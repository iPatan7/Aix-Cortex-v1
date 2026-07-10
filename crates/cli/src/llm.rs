//! LLM command generation for hybrid workflows.
//!
//! `cortex workflow llm "description"` sends the description plus current
//! system facts to a chat endpoint and gets back a single shell command,
//! which the workflow then runs inside an OverlayFS transaction with the
//! usual verify/journal/commit discipline.
//!
//! Two endpoint dialects are supported, selected by the URL path:
//!
//! - `.../api/chat` — the cortex-server relay (axum on :36702 forwarding to
//!   the Python bridge on :8766). Request `{"message": ...}`, response
//!   `{"response": ...}`. Point `CORTEX_LLM_ENDPOINT` at the relay and the
//!   CLI inherits its provider keys — nothing secret lives on this side.
//! - anything else — OpenAI-compatible chat completions (Ollama's
//!   `/v1/chat/completions`, OpenAI, vLLM, ...). Request
//!   `{"model", "messages"}`, response `choices[0].message.content`.
//!
//! Transport is `curl` rather than an HTTP crate: the CLI already drives
//! everything through subprocesses, and this keeps cortex-core's
//! dependency tree unchanged.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::process::{Command, Stdio};

/// Environment variables understood by [`LlmClient::from_env`].
pub const ENV_ENDPOINT: &str = "CORTEX_LLM_ENDPOINT";
pub const ENV_MODEL: &str = "CORTEX_LLM_MODEL";
pub const ENV_API_KEY: &str = "CORTEX_LLM_API_KEY";

pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1/chat/completions";
const DEFAULT_MODEL: &str = "llama3";

const SYSTEM_PROMPT: &str = "\
You are the command generator for Cortex, a transactional Linux DevOps tool. \
Given a task description and system facts, respond with EXACTLY ONE shell \
command that performs the task. The command runs as root inside an OverlayFS \
transaction and is verified, journaled and reversible. Output the bare \
command only: no markdown fences, no comments, no explanation, no leading $.";

/// The router prompt: pick a reversible workflow, not a bare command. Every
/// option here journals an inverse, so whatever the model chooses can be
/// undone. `safe-run` is the escape hatch and is the one case where the
/// model must state the inverse itself.
const PLAN_PROMPT: &str = r#"You are the planner for Cortex, a transactional Linux DevOps tool.
Every action Cortex takes must be reversible: it journals an inverse before it commits.
Given a task description and system facts, reply with EXACTLY ONE JSON object choosing one workflow.
No markdown fences, no commentary — JSON only.

Workflows and their exact argument shapes:
{"workflow":"safe-service","op":"start|stop|restart|enable|disable","service":"<unit>"}
    Start/stop/restart a systemd service. Use this for "run/start/stop <service>".
{"workflow":"safe-install","package":"<pkg>"}
    apt-get install a package.
{"workflow":"safe-dependency-upgrade","manager":"apt|pip|npm","package":"<pkg>"}
    Upgrade an already-installed package.
{"workflow":"safe-file-edit","file":"<path>","cmd":"<shell command that edits it>"}
    Edit a text file (useradd, sed, appending a line, ...).
{"workflow":"safe-config","service":"<svc>","cmd":"<shell command>"}
    Edit a service's config; verified with `<svc> -t` and the service is reloaded.
{"workflow":"safe-symlink-swap","link":"<path>","target":"<path>"}
    Repoint a symlink (blue/green deploys).
{"workflow":"safe-cron-install","user":"<user>","entry":"<5 fields then command>"}
    Install a crontab entry.
{"workflow":"safe-db-migration","db":"<db>","sql":"<forward SQL>","undo_sql":"<inverse SQL>"}
    Run a SQL migration. undo_sql MUST exactly reverse sql.
{"workflow":"safe-run","cmd":"<shell command>","undo_cmd":"<shell command that exactly reverses it>"}
    Anything else, including docker. undo_cmd is REQUIRED and must truly reverse cmd.
    Example: docker: cmd "docker run -d --name web -p 8080:80 nginx", undo_cmd "docker rm -f web".
    Always name containers (--name) so the inverse can address them.

Rules:
- Prefer the specific workflow over safe-run whenever one fits.
- Never invent a workflow name or an argument key.
- "run/start the X server" means safe-service start on unit X, not a raw command.
- If the task cannot be made reversible, reply {"error":"<one sentence why>"}."#;

pub struct LlmClient {
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

impl LlmClient {
    /// Configure from the environment. Returns `None` when no endpoint is
    /// set — the caller must then require a manual command instead.
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var(ENV_ENDPOINT).ok()?;
        Some(Self {
            endpoint,
            model: std::env::var(ENV_MODEL).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            api_key: std::env::var(ENV_API_KEY).ok(),
        })
    }

    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: None,
        }
    }

    /// Ask the LLM for the single shell command that implements
    /// `description` on this system.
    pub fn generate_command(&self, description: &str) -> Result<String> {
        let user = format!("{}\n\nTask: {description}", system_facts());
        let body = build_request(&self.endpoint, &self.model, SYSTEM_PROMPT, &user);
        let response = self.post(&body)?;
        let content = extract_content(&self.endpoint, &response)?;
        let command = sanitize_command(&content);
        if command.is_empty() {
            bail!("LLM returned no usable command (response: {content:?})");
        }
        Ok(command)
    }

    /// Ask the LLM which *reversible workflow* implements `description`.
    /// Returns the raw plan object; [`crate::workflow::from_plan`] validates
    /// and dispatches it.
    pub fn generate_plan(&self, description: &str) -> Result<Value> {
        let user = format!("{}\n\nTask: {description}", system_facts());
        let body = build_request(&self.endpoint, &self.model, PLAN_PROMPT, &user);
        let response = self.post(&body)?;
        let content = extract_content(&self.endpoint, &response)?;
        parse_plan(&content)
    }

    fn post(&self, body: &Value) -> Result<Value> {
        let mut curl = Command::new("curl");
        curl.args(["-sS", "--fail-with-body", "-m", "90", "-X", "POST"])
            .args(["-H", "Content-Type: application/json"]);
        if let Some(key) = &self.api_key {
            curl.args(["-H", &format!("Authorization: Bearer {key}")]);
        }
        curl.args(["--data-binary", "@-"]).arg(&self.endpoint);
        curl.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = curl.spawn().context("failed to spawn curl")?;
        use std::io::Write;
        child
            .stdin
            .take()
            .context("curl stdin unavailable")?
            .write_all(body.to_string().as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "LLM request to {} failed ({}): {}{}",
                self.endpoint,
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "LLM endpoint {} returned non-JSON: {}",
                self.endpoint,
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }
}

/// Facts the model needs to pick correct tooling (apt vs dnf, service names).
fn system_facts() -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("PRETTY_NAME=")).map(|l| {
                l.trim_start_matches("PRETTY_NAME=")
                    .trim_matches('"')
                    .to_string()
            })
        })
        .unwrap_or_else(|| "unknown Linux".to_string());
    let kernel = Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    format!("System facts: OS={os}; kernel={kernel}; package manager=apt; init=systemd.")
}

fn is_bridge(endpoint: &str) -> bool {
    endpoint.contains("/api/chat")
}

fn build_request(endpoint: &str, model: &str, system: &str, user: &str) -> Value {
    if is_bridge(endpoint) {
        // The bridge takes one message string; fold the system prompt in.
        json!({ "message": format!("{system}\n\n{user}") })
    } else {
        json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
            "stream": false,
            "temperature": 0
        })
    }
}

fn extract_content(endpoint: &str, response: &Value) -> Result<String> {
    let content = if is_bridge(endpoint) {
        response["response"].as_str()
    } else {
        response["choices"][0]["message"]["content"].as_str()
    };
    content
        .map(str::to_string)
        .with_context(|| format!("unexpected LLM response shape: {response}"))
}

/// Pull the JSON object out of a plan reply, tolerating a chatty model that
/// wraps it in fences or prose.
fn parse_plan(raw: &str) -> Result<Value> {
    let text = raw.trim();
    let slice = match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => bail!("LLM plan reply contained no JSON object: {text:?}"),
    };
    let plan: Value = serde_json::from_str(slice)
        .with_context(|| format!("LLM plan reply is not valid JSON: {slice}"))?;
    if let Some(err) = plan.get("error").and_then(Value::as_str) {
        bail!("the planner refused: {err}");
    }
    Ok(plan)
}

/// Strip markdown fences, prompts, and blank lines down to the command.
fn sanitize_command(raw: &str) -> String {
    let mut lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect();
    if let Some(first) = lines.first_mut() {
        *first = first.trim_start_matches("$ ").trim_start_matches("$");
    }
    lines.join(" && ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_shape_round_trip() {
        let req = build_request(DEFAULT_ENDPOINT, "llama3", "sys", "user");
        assert_eq!(req["messages"][1]["content"], "user");
        let resp = json!({"choices": [{"message": {"role": "assistant",
            "content": "apt-get install -y nginx"}}]});
        assert_eq!(
            extract_content(DEFAULT_ENDPOINT, &resp).unwrap(),
            "apt-get install -y nginx"
        );
    }

    #[test]
    fn bridge_shape_round_trip() {
        let ep = "http://127.0.0.1:36702/api/chat";
        let req = build_request(ep, "llama3", "sys", "user");
        assert!(req["message"].as_str().unwrap().contains("user"));
        assert!(req.get("model").is_none());
        let resp = json!({"response": "ln -sfn /var/www/v2 /var/www/html"});
        assert_eq!(
            extract_content(ep, &resp).unwrap(),
            "ln -sfn /var/www/v2 /var/www/html"
        );
    }

    #[test]
    fn sanitize_strips_fences_and_prompt() {
        assert_eq!(
            sanitize_command("```bash\n$ apt-get update\n```"),
            "apt-get update"
        );
        assert_eq!(sanitize_command("  echo hi  \n"), "echo hi");
        assert_eq!(sanitize_command("cd /tmp\nls"), "cd /tmp && ls");
        assert_eq!(sanitize_command("```\n```"), "");
    }
}
