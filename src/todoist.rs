use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::{Operation, OutboxEntry, RemoteFilter, RemoteLabel, RemoteProject, RemoteTask},
    store::Store,
};

const API_BASE: &str = "https://api.todoist.com/api/v1";
const SYNC_RESOURCES: &str = r#"["projects","items","labels","filters","workspace_filters"]"#;

#[derive(Debug, Deserialize)]
struct SyncResponse {
    #[serde(default)]
    projects: Vec<RemoteProject>,
    #[serde(default, alias = "items")]
    items: Vec<RemoteTask>,
    #[serde(default)]
    labels: Vec<RemoteLabel>,
    #[serde(default)]
    filters: Vec<RemoteFilter>,
    #[serde(default)]
    workspace_filters: Vec<RemoteFilter>,
    sync_token: String,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(alias = "items")]
    results: Vec<T>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TodoistClient {
    http: Client,
    token: String,
    base: String,
}

impl TodoistClient {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(25))
                .user_agent(concat!("todoist-tui/", env!("CARGO_PKG_VERSION")))
                .build()?,
            token,
            base: API_BASE.into(),
        })
    }

    fn check(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        let concise = body
            .lines()
            .next()
            .unwrap_or("request failed")
            .chars()
            .take(180)
            .collect::<String>();
        bail!("Todoist returned {status}: {concise}")
    }

    fn auth(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request.bearer_auth(&self.token)
    }

    pub fn push(&self, store: &Store, entry: &OutboxEntry) -> Result<()> {
        match &entry.operation {
            Operation::QuickAdd {
                expression,
                project_id,
                project_name,
            } => {
                let is_inbox = store
                    .project(project_id)?
                    .map(|p| p.is_inbox)
                    .unwrap_or(false);
                let (text, enforce_fallback_project) =
                    quick_add_request(expression, project_name, is_inbox);
                let response = Self::check(
                    self.auth(self.http.post(format!("{}/tasks/quick", self.base)))
                        .header("X-Request-Id", &entry.uuid)
                        .json(&json!({"text": text, "meta": true}))
                        .send()?,
                )?;
                let value: Value = response.json().context("decode Quick Add response")?;
                let task_value = value
                    .get("task")
                    .or_else(|| value.get("item"))
                    .unwrap_or(&value);
                let mut task: RemoteTask = serde_json::from_value(task_value.clone())
                    .context("Todoist Quick Add response did not contain a task")?;
                if enforce_fallback_project && task.project_id != *project_id {
                    self.command_with_uuid(
                        &uuid::Uuid::new_v4().to_string(),
                        "item_move",
                        json!({"id": task.id, "project_id": project_id}),
                    )?;
                    task.project_id.clone_from(project_id);
                }
                store.replace_task_id(&entry.entity_id, &task)?;
            }
            Operation::UpdateTask {
                content,
                description,
            } => self.command(
                entry,
                "item_update",
                json!({
                    "id": entry.entity_id, "content": content, "description": description
                }),
            )?,
            Operation::UpdateDue { due_string } => {
                self.command(
                    entry,
                    "item_update",
                    due_update_args(&entry.entity_id, due_string),
                )?;
            }
            Operation::UpdateLabels { labels } => self.command(
                entry,
                "item_update",
                json!({
                    "id": entry.entity_id, "labels": labels
                }),
            )?,
            Operation::MoveTask { project_id } => self.command(
                entry,
                "item_move",
                json!({"id": entry.entity_id, "project_id": project_id}),
            )?,
            Operation::CompleteTask => {
                self.command(entry, "item_close", json!({"id": entry.entity_id}))?
            }
            Operation::UncompleteTask => {
                self.command(entry, "item_uncomplete", json!({"id": entry.entity_id}))?
            }
            Operation::DeleteTask => {
                self.command(entry, "item_delete", json!({"id": entry.entity_id}))?
            }
        }
        store.finish_outbox(entry.seq)?;
        Ok(())
    }

    fn command(&self, entry: &OutboxEntry, kind: &str, args: Value) -> Result<()> {
        self.command_with_uuid(&entry.uuid, kind, args)
    }

    fn command_with_uuid(&self, uuid: &str, kind: &str, args: Value) -> Result<()> {
        let command = json!([{"type":kind,"uuid":uuid,"args":args}]);
        let response = Self::check(
            self.auth(self.http.post(format!("{}/sync", self.base)))
                .form(&[("commands", command.to_string())])
                .send()?,
        )?;
        let value: Value = response.json().context("decode command response")?;
        let status = value
            .get("sync_status")
            .and_then(|s| s.get(uuid))
            .context("Todoist command response omitted its status")?;
        if status != "ok" {
            bail!("Todoist rejected operation: {}", concise_json(status));
        }
        Ok(())
    }

    pub fn pull(&self, store: &Store) -> Result<()> {
        let mut sync_token = store.sync_token()?;
        let initial_sync = sync_token == "*";
        // A full sync may be generated from a slightly delayed snapshot, so
        // immediately follow the first one with an incremental pass.
        for _ in 0..if initial_sync { 2 } else { 1 } {
            let response = Self::check(
                self.auth(self.http.post(format!("{}/sync", self.base)))
                    .form(&[
                        ("sync_token", sync_token.as_str()),
                        ("resource_types", SYNC_RESOURCES),
                    ])
                    .send()?,
            )?;
            let mut sync: SyncResponse = response.json().context("decode sync response")?;
            sync.filters.append(&mut sync.workspace_filters);
            store.merge_remote(&sync.projects, &sync.items, &sync.labels, &sync.filters)?;
            store.set_sync_token(&sync.sync_token)?;
            sync_token = sync.sync_token;
        }

        // Each filter is isolated: an unsupported query cannot fail the full refresh.
        for filter in store.favorite_filters()? {
            if let Ok(tasks) = self.filtered_tasks(&filter.query) {
                store.replace_filter_membership(&filter.id, &tasks)?;
            }
        }
        // Completed history is an auxiliary view. Keep normal synchronization
        // healthy if an account or server does not expose this endpoint.
        if let Ok(tasks) = self.recent_completed_tasks() {
            store.merge_completed_tasks(&tasks)?;
        }
        Ok(())
    }

    fn recent_completed_tasks(&self) -> Result<Vec<RemoteTask>> {
        let until = Utc::now() + ChronoDuration::days(1);
        let since = until - ChronoDuration::days(89);
        let since = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let until = until.to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = self
                .auth(
                    self.http
                        .get(format!("{}/tasks/completed/by_completion_date", self.base)),
                )
                .query(&[
                    ("since", since.as_str()),
                    ("until", until.as_str()),
                    ("limit", "50"),
                ]);
            if let Some(value) = cursor.as_deref() {
                request = request.query(&[("cursor", value)]);
            }
            let page: Page<RemoteTask> = Self::check(request.send()?)?.json()?;
            all.extend(page.results);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    fn filtered_tasks(&self, query: &str) -> Result<Vec<RemoteTask>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = self
                .auth(self.http.get(format!("{}/tasks/filter", self.base)))
                .query(&[("query", query), ("limit", "200")]);
            if let Some(value) = cursor.as_deref() {
                request = request.query(&[("cursor", value)]);
            }
            let page: Page<RemoteTask> = Self::check(request.send()?)?.json()?;
            all.extend(page.results);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }
}

fn due_update_args(task_id: &str, due_string: &str) -> Value {
    let due = if due_string.is_empty() {
        Value::Null
    } else {
        // Todoist parses this user-provided string with its natural-language
        // date parser, using the account language when no language is given.
        json!({"string": due_string})
    };
    json!({"id": task_id, "due": due})
}

fn quick_add_request(expression: &str, project_name: &str, is_inbox: bool) -> (String, bool) {
    let has_explicit_project = expression.split_whitespace().any(|word| {
        word.strip_prefix('#')
            .is_some_and(|project| !project.is_empty())
    });
    if is_inbox || has_explicit_project {
        (expression.to_owned(), false)
    } else {
        (format!("{expression} #{project_name}"), true)
    }
}

fn concise_json(value: &Value) -> String {
    value.to_string().chars().take(180).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_update_preserves_natural_language_for_todoist() {
        assert_eq!(
            due_update_args("task-1", "tod"),
            json!({"id": "task-1", "due": {"string": "tod"}})
        );
        assert_eq!(
            due_update_args("task-1", ""),
            json!({"id": "task-1", "due": null})
        );
    }

    #[test]
    fn quick_add_project_is_a_fallback_not_an_override() {
        assert_eq!(
            quick_add_request("Plan the week", "week", false),
            ("Plan the week #week".into(), true)
        );
        assert_eq!(
            quick_add_request("Plan trip #Travel tomorrow", "week", false),
            ("Plan trip #Travel tomorrow".into(), false)
        );
        assert_eq!(
            quick_add_request("Buy milk", "Inbox", true),
            ("Buy milk".into(), false)
        );
    }
}
