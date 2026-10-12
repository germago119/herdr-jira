//! Minimal Jira REST client (blocking, ureq).
//!
//! Works against both Jira Cloud and Server/Data Center:
//! - search first tries the new `/rest/api/2/search/jql` endpoint (Cloud replaced
//!   the classic `/search` with it in 2025), then falls back to the classic
//!   `/rest/api/2/search` (Server/DC and older instances);
//! - the v2 API returns descriptions as plain text on Server/DC, but some Cloud
//!   responses carry Atlassian Document Format (ADF) objects — both are handled.

use base64::Engine;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Issue {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub status_category: String, // "new" | "indeterminate" | "done"
    pub issue_type: String,
    pub priority: String,
    pub epic_link: String,
    pub epic_name: String,
    pub assignee: String,
    pub reporter: String,
    pub updated: String,
    pub labels: Vec<String>,
    pub description: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Transition {
    pub id: String,
    pub name: String,
    pub to_status: String,
}

#[derive(Clone)]
pub struct JiraClient {
    base: String,
    auth_header: String,
    max_results: u32,
    agent: ureq::Agent,
}

const FIELDS: &str = "summary,status,issuetype,priority,customfield_10000,assignee,reporter,updated,labels,description";

impl JiraClient {
    pub fn new(cfg: &crate::config::Config) -> Result<Self, String> {
        let token = cfg.resolve_token()?;
        let auth_header = match cfg.jira.auth.as_str() {
            "bearer" => format!("Bearer {token}"),
            "basic" => {
                if cfg.jira.email.trim().is_empty() {
                    return Err("auth = \"basic\" requires [jira].email".into());
                }
                let creds = format!("{}:{}", cfg.jira.email.trim(), token);
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(creds)
                )
            }
            other => {
                return Err(format!(
                    "unknown [jira].auth \"{other}\" (use basic|bearer)"
                ))
            }
        };
        Ok(Self {
            base: cfg.jira.base_url.clone(),
            auth_header,
            max_results: cfg.jira.max_results.max(1),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(20))
                .build(),
        })
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let mut req = self
            .agent
            .get(&format!("{}{}", self.base, path))
            .set("Authorization", &self.auth_header)
            .set("Accept", "application/json");
        for (k, v) in query {
            req = req.query(k, v);
        }
        Self::finish(req.call())
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let req = self
            .agent
            .post(&format!("{}{}", self.base, path))
            .set("Authorization", &self.auth_header)
            .set("Accept", "application/json");
        Self::finish(req.send_json(body))
    }

    fn finish(res: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
        match res {
            Ok(resp) => {
                let text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|e| format!("bad JSON from Jira: {e}"))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(format!("HTTP {code}: {}", extract_error(&body)))
            }
            Err(e) => Err(format!("request failed: {e}")),
        }
    }

    pub fn search(&self, jql: &str) -> Result<Vec<Issue>, String> {
        let result = self.search_response(jql, FIELDS)?;
        let mut issues: Vec<Issue> = result["issues"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|v| self.parse_issue(v))
            .collect();
        self.resolve_epic_names(&mut issues)?;
        Ok(issues)
    }

    fn search_response(&self, jql: &str, fields: &str) -> Result<Value, String> {
        let max = self.max_results.to_string();
        let query: &[(&str, &str)] = &[("jql", jql), ("maxResults", &max), ("fields", fields)];
        // New endpoint first (Jira Cloud), classic /search as fallback (Server/DC).
        match self.get("/rest/api/2/search/jql", query) {
            Ok(v) => Ok(v),
            Err(e)
                if e.starts_with("HTTP 404")
                    || e.starts_with("HTTP 405")
                    || e.starts_with("HTTP 410") =>
            {
                self.get("/rest/api/2/search", query)
            }
            Err(e) => Err(e),
        }
    }

    fn resolve_epic_names(&self, issues: &mut [Issue]) -> Result<(), String> {
        let keys: HashSet<&str> = issues
            .iter()
            .map(|issue| issue.epic_link.as_str())
            .filter(|key| *key != "—")
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        if keys.iter().any(|key| {
            !key.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        }) {
            return Err("Jira returned an invalid Epic Link key".into());
        }
        let mut keys: Vec<&str> = keys.into_iter().collect();
        keys.sort_unstable();
        let response = self.search_response(
            &format!("key in ({})", keys.join(",")),
            "summary,customfield_10002",
        )?;
        let names: HashMap<&str, String> = response["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|epic| {
                let key = epic["key"].as_str()?;
                Some((key, epic_display_name(&epic["fields"])))
            })
            .collect();
        for issue in issues {
            issue.epic_name = names
                .get(issue.epic_link.as_str())
                .cloned()
                .unwrap_or_else(|| issue.epic_link.clone());
        }
        Ok(())
    }

    fn parse_issue(&self, v: &Value) -> Issue {
        let f = &v["fields"];
        let key = v["key"].as_str().unwrap_or("?").to_string();
        let person = |p: &Value| -> String {
            p["displayName"]
                .as_str()
                .or_else(|| p["name"].as_str())
                .unwrap_or("—")
                .to_string()
        };
        Issue {
            url: format!("{}/browse/{}", self.base, key),
            key,
            summary: f["summary"].as_str().unwrap_or("").to_string(),
            status: f["status"]["name"].as_str().unwrap_or("?").to_string(),
            status_category: f["status"]["statusCategory"]["key"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            issue_type: f["issuetype"]["name"].as_str().unwrap_or("").to_string(),
            priority: f["priority"]["name"].as_str().unwrap_or("—").to_string(),
            epic_link: f["customfield_10000"].as_str().unwrap_or("—").to_string(),
            epic_name: f["customfield_10000"].as_str().unwrap_or("—").to_string(),
            assignee: person(&f["assignee"]),
            reporter: person(&f["reporter"]),
            updated: f["updated"]
                .as_str()
                .map(|s| s.chars().take(16).collect::<String>().replace('T', " "))
                .unwrap_or_default(),
            labels: f["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            description: description_text(&f["description"]),
        }
    }

    pub fn transitions(&self, key: &str) -> Result<Vec<Transition>, String> {
        let v = self.get(&format!("/rest/api/2/issue/{key}/transitions"), &[])?;
        Ok(v["transitions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|t| Transition {
                id: t["id"].as_str().unwrap_or("").to_string(),
                name: t["name"].as_str().unwrap_or("?").to_string(),
                to_status: t["to"]["name"].as_str().unwrap_or("?").to_string(),
            })
            .collect())
    }

    pub fn apply_transition(&self, key: &str, transition_id: &str) -> Result<(), String> {
        self.post(
            &format!("/rest/api/2/issue/{key}/transitions"),
            serde_json::json!({ "transition": { "id": transition_id } }),
        )?;
        Ok(())
    }
}

fn epic_display_name(fields: &Value) -> String {
    let name = fields["customfield_10002"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| fields["summary"].as_str())
        .unwrap_or("—");
    name.to_string()
}

/// Jira error payloads look like {"errorMessages":[...],"errors":{...}}.
fn extract_error(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let mut parts: Vec<String> = Vec::new();
        if let Some(msgs) = v["errorMessages"].as_array() {
            parts.extend(msgs.iter().filter_map(|m| m.as_str().map(String::from)));
        }
        if let Some(errs) = v["errors"].as_object() {
            parts.extend(
                errs.iter()
                    .map(|(k, val)| format!("{k}: {}", val.as_str().unwrap_or_default())),
            );
        }
        if !parts.is_empty() {
            return parts.join("; ");
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no error body".into()
    } else {
        trimmed.chars().take(300).collect()
    }
}

/// Description is a plain string on API v2 (Server/DC), but may arrive as an
/// ADF document object from Cloud — flatten either to displayable text.
fn description_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            let mut out = String::new();
            adf_walk(v, &mut out);
            out.trim().to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> JiraClient {
        JiraClient {
            base: "https://jira.example".into(),
            auth_header: "Bearer test".into(),
            max_results: 50,
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    #[test]
    fn issue_parses_epic_link() {
        let issue = test_client().parse_issue(&serde_json::json!({
            "key": "PROJ-7",
            "fields": {
                "summary": "Child issue",
                "status": {"name": "Backlog", "statusCategory": {"key": "new"}},
                "issuetype": {"name": "Story"},
                "customfield_10000": "PROJ-1"
            }
        }));
        assert_eq!(issue.epic_link, "PROJ-1");
        assert_eq!(issue.epic_name, "PROJ-1");
    }

    #[test]
    fn issue_without_epic_link_uses_placeholder() {
        let issue = test_client().parse_issue(&serde_json::json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "Epic",
                "status": {"name": "In Progress", "statusCategory": {"key": "indeterminate"}},
                "issuetype": {"name": "Epic"},
                "customfield_10000": null
            }
        }));
        assert_eq!(issue.epic_link, "—");
        assert_eq!(issue.epic_name, "—");
    }

    #[test]
    fn epic_name_prefers_jira_epic_name_over_summary() {
        assert_eq!(
            epic_display_name(&serde_json::json!({
                "summary": "DPF Migration",
                "customfield_10002": "DPF->CTD"
            })),
            "DPF->CTD"
        );
        assert_eq!(
            epic_display_name(&serde_json::json!({
                "summary": "Fallback summary",
                "customfield_10002": null
            })),
            "Fallback summary"
        );
        assert_eq!(
            epic_display_name(&serde_json::json!({
                "summary": "Ignored summary",
                "customfield_10002": "Payments Platform"
            })),
            "Payments Platform"
        );
    }

    #[test]
    fn description_plain_string_passes_through() {
        assert_eq!(
            description_text(&Value::String("hi\nthere".into())),
            "hi\nthere"
        );
        assert_eq!(description_text(&Value::Null), "");
    }

    #[test]
    fn description_adf_flattens_to_text() {
        let adf: Value = serde_json::json!({
            "type": "doc", "version": 1,
            "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "first"},
                    {"type": "hardBreak"},
                    {"type": "text", "text": "second"}
                ]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "item"}]}
                    ]}
                ]}
            ]
        });
        assert_eq!(description_text(&adf), "first\nsecond\n- item");
    }

    #[test]
    fn jira_error_payload_is_extracted() {
        let body = r#"{"errorMessages":["Issue does not exist"],"errors":{"status":"bad"}}"#;
        assert_eq!(extract_error(body), "Issue does not exist; status: bad");
        assert_eq!(extract_error("plain"), "plain");
    }
}

fn adf_walk(v: &Value, out: &mut String) {
    match v["type"].as_str() {
        Some("text") => {
            out.push_str(v["text"].as_str().unwrap_or(""));
            return;
        }
        Some("hardBreak") => {
            out.push('\n');
            return;
        }
        Some("listItem") => out.push_str("- "),
        _ => {}
    }
    if let Some(children) = v["content"].as_array() {
        for c in children {
            adf_walk(c, out);
        }
    }
    // Block-level nodes end with a blank line.
    if matches!(
        v["type"].as_str(),
        Some("paragraph" | "heading" | "codeBlock" | "blockquote" | "listItem")
    ) && !out.ends_with('\n')
    {
        out.push('\n');
    }
}
