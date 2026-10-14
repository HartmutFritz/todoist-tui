use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::{
        Due, Operation, OutboxEntry, RemoteFilter, RemoteLabel, RemoteProject, RemoteTask, Task,
    },
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
                parent_id,
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
                if let Some(parent_id) = parent_id {
                    let resolved_parent = store
                        .resolve_task_id(parent_id)?
                        .unwrap_or_else(|| parent_id.clone());
                    self.command_with_uuid(
                        &uuid::Uuid::new_v4().to_string(),
                        "item_move",
                        json!({"id": task.id, "parent_id": resolved_parent}),
                    )?;
                    task.project_id.clone_from(project_id);
                    task.parent_id = Some(resolved_parent);
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
            Operation::QuickEdit {
                expression,
                description,
                update_uuid,
                move_uuid,
                delete_uuid,
            } => self.quick_edit(
                store,
                entry,
                expression,
                description,
                update_uuid,
                move_uuid,
                delete_uuid,
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
            Operation::MoveTaskParent { parent_id } => {
                if let Some(parent_id) = parent_id {
                    let resolved_parent = store
                        .resolve_task_id(parent_id)?
                        .unwrap_or_else(|| parent_id.clone());
                    self.command(
                        entry,
                        "item_move",
                        task_parent_move_args(&entry.entity_id, Some(&resolved_parent), ""),
                    )?;
                } else {
                    let task = store
                        .task(&entry.entity_id)?
                        .context("outdented task no longer exists")?;
                    self.command(
                        entry,
                        "item_move",
                        task_parent_move_args(&entry.entity_id, None, &task.project_id),
                    )?;
                }
            }
            Operation::CompleteTask => {
                self.command(entry, "item_close", json!({"id": entry.entity_id}))?
            }
            Operation::UncompleteTask => {
                self.command(entry, "item_uncomplete", json!({"id": entry.entity_id}))?
            }
            Operation::DeleteTask => {
                self.command(entry, "item_delete", json!({"id": entry.entity_id}))?
            }
            Operation::AddFilter {
                name,
                query,
                is_favorite,
            } => self.add_filter(store, entry, name, query, *is_favorite)?,
            Operation::UpdateFilter {
                name,
                query,
                is_favorite,
                is_workspace,
            } => self.command(
                entry,
                if *is_workspace {
                    "workspace_filter_update"
                } else {
                    "filter_update"
                },
                json!({
                    "id": entry.entity_id,
                    "name": name,
                    "query": query,
                    "is_favorite": is_favorite
                }),
            )?,
            Operation::AddProject { name } => self.add_project(store, entry, name)?,
            Operation::UpdateProject { name } => self.command(
                entry,
                "project_update",
                json!({"id": entry.entity_id, "name": name}),
            )?,
            Operation::ReorderProjects { projects } => {
                self.command(entry, "project_reorder", project_reorder_args(projects))?
            }
        }
        store.finish_outbox(entry.seq)?;
        Ok(())
    }

    fn add_filter(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        name: &str,
        query: &str,
        is_favorite: bool,
    ) -> Result<()> {
        let command = json!([{
            "type": "filter_add",
            "temp_id": entry.uuid,
            "uuid": entry.uuid,
            "args": {"name": name, "query": query, "is_favorite": is_favorite}
        }]);
        let response = Self::check(
            self.auth(self.http.post(format!("{}/sync", self.base)))
                .form(&[("commands", command.to_string())])
                .send()?,
        )?;
        let value: Value = response.json().context("decode filter-add response")?;
        let status = value
            .get("sync_status")
            .and_then(|statuses| statuses.get(&entry.uuid))
            .context("Todoist filter-add response omitted its status")?;
        if status != "ok" {
            bail!("Todoist rejected operation: {}", concise_json(status));
        }
        let mapped = value
            .get("temp_id_mapping")
            .and_then(|mapping| mapping.get(&entry.uuid))
            .context("Todoist filter-add response omitted the new filter ID")?;
        let new_id = match mapped {
            Value::String(id) => id.clone(),
            Value::Number(id) => id.to_string(),
            _ => bail!("Todoist returned an invalid filter ID"),
        };
        store.replace_filter_id(&entry.entity_id, &new_id)?;
        Ok(())
    }

    fn add_project(&self, store: &Store, entry: &OutboxEntry, name: &str) -> Result<()> {
        let command = json!([{
            "type": "project_add",
            "temp_id": entry.uuid,
            "uuid": entry.uuid,
            "args": {"name": name}
        }]);
        let response = Self::check(
            self.auth(self.http.post(format!("{}/sync", self.base)))
                .form(&[("commands", command.to_string())])
                .send()?,
        )?;
        let value: Value = response.json().context("decode project-add response")?;
        let status = value
            .get("sync_status")
            .and_then(|statuses| statuses.get(&entry.uuid))
            .context("Todoist project-add response omitted its status")?;
        if status != "ok" {
            bail!("Todoist rejected operation: {}", concise_json(status));
        }
        let mapped = value
            .get("temp_id_mapping")
            .and_then(|mapping| mapping.get(&entry.uuid))
            .context("Todoist project-add response omitted the new project ID")?;
        let new_id = match mapped {
            Value::String(id) => id.clone(),
            Value::Number(id) => id.to_string(),
            _ => bail!("Todoist returned an invalid project ID"),
        };
        store.replace_project_id(&entry.entity_id, &new_id)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn quick_edit(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        expression: &str,
        description: &str,
        update_uuid: &str,
        move_uuid: &str,
        delete_uuid: &str,
    ) -> Result<()> {
        let original = store
            .task(&entry.entity_id)?
            .context("edited task no longer exists")?;
        let response = Self::check(
            self.auth(self.http.post(format!("{}/tasks/quick", self.base)))
                .header("X-Request-Id", &entry.uuid)
                .json(&json!({"text": expression, "meta": true}))
                .send()?,
        )?;
        let value: Value = response
            .json()
            .context("decode Quick Edit parsing response")?;
        let task_value = value
            .get("task")
            .or_else(|| value.get("item"))
            .unwrap_or(&value);
        let parsed: RemoteTask = serde_json::from_value(task_value.clone())
            .context("Todoist Quick Edit response did not contain a task")?;

        let parsed_project_selected = store
            .project(&parsed.project_id)?
            .is_some_and(|project| expression_mentions_project(expression, &project.name));
        let parsed_due = parsed.due.is_some();
        let updated = merge_quick_edit(
            &original,
            &parsed,
            expression,
            description,
            parsed_project_selected,
        );
        let move_project =
            (updated.project_id != original.project_id).then(|| updated.project_id.clone());

        let mut args = json!({
            "id": entry.entity_id,
            "content": updated.content,
            "description": updated.description,
            "labels": updated.labels,
            "priority": updated.priority,
        });
        if parsed_due {
            args["due"] = due_value(updated.due.as_ref());
        }

        let update_result = (|| -> Result<()> {
            self.command_with_uuid(update_uuid, "item_update", args)?;
            if let Some(project_id) = move_project {
                self.command_with_uuid(
                    move_uuid,
                    "item_move",
                    json!({"id": entry.entity_id, "project_id": project_id}),
                )?;
            }
            Ok(())
        })();
        let cleanup_result =
            self.command_with_uuid(delete_uuid, "item_delete", json!({"id": parsed.id}));
        update_result?;
        cleanup_result.context("remove temporary Quick Edit parser task")?;
        store.apply_synced_task_edit(&updated, entry.seq)?;
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
    let due = (!due_string.is_empty()).then(|| Due {
        string: Some(due_string.to_owned()),
        ..Due::default()
    });
    json!({"id": task_id, "due": due_value(due.as_ref())})
}

fn task_parent_move_args(task_id: &str, parent_id: Option<&str>, project_id: &str) -> Value {
    if let Some(parent_id) = parent_id {
        json!({"id": task_id, "parent_id": parent_id})
    } else {
        json!({"id": task_id, "project_id": project_id})
    }
}

fn project_reorder_args(projects: &[crate::model::ProjectOrder]) -> Value {
    json!({"projects": projects})
}

fn due_value(due: Option<&Due>) -> Value {
    due.and_then(|due| due.string.as_deref().or_else(|| due.display()))
        .map_or(Value::Null, |value| json!({"string": value}))
}

fn quick_add_request(expression: &str, project_name: &str, is_inbox: bool) -> (String, bool) {
    let has_explicit_project = contains_explicit_project(expression);
    if is_inbox || has_explicit_project {
        (expression.to_owned(), false)
    } else {
        (format!("{expression} #{project_name}"), true)
    }
}

fn contains_explicit_project(expression: &str) -> bool {
    expression.split_whitespace().any(|word| {
        word.strip_prefix('#')
            .is_some_and(|project| !project.is_empty())
    })
}

fn contains_priority(expression: &str) -> bool {
    expression.split_whitespace().any(|word| {
        matches!(
            word.to_ascii_lowercase().as_str(),
            "p1" | "p2" | "p3" | "p4" | "!!1" | "!!2" | "!!3" | "!!4"
        )
    })
}

fn expression_mentions_project(expression: &str, project_name: &str) -> bool {
    let expression = expression.to_lowercase();
    let project = format!("#{}", project_name.replace(' ', "\\ ")).to_lowercase();
    expression.match_indices(&project).any(|(index, _)| {
        expression[index + project.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    })
}

fn merge_quick_edit(
    original: &Task,
    parsed: &RemoteTask,
    expression: &str,
    description: &str,
    parsed_project_selected: bool,
) -> Task {
    let mut updated = original.clone();
    updated.content.clone_from(&parsed.content);
    updated.description = if description.is_empty() {
        parsed.description.clone()
    } else {
        description.to_owned()
    };
    if let Some(due) = parsed.due.clone() {
        updated.due = Some(due);
    }
    for label in &parsed.labels {
        if !updated
            .labels
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(label))
        {
            updated.labels.push(label.clone());
        }
    }
    if contains_priority(expression) {
        updated.priority = parsed.priority;
    }
    if parsed_project_selected {
        updated.project_id.clone_from(&parsed.project_id);
    }
    updated
}

fn concise_json(value: &Value) -> String {
    value.to_string().chars().take(180).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task {
            id: "task-1".into(),
            project_id: "inbox".into(),
            parent_id: None,
            content: "Original".into(),
            description: "Old description".into(),
            labels: vec!["Work".into()],
            due: Some(Due {
                string: Some("next week".into()),
                ..Due::default()
            }),
            priority: 2,
            child_order: 7,
            checked: false,
        }
    }

    fn parsed_task() -> RemoteTask {
        RemoteTask {
            id: "parser-task".into(),
            project_id: "project-2".into(),
            parent_id: None,
            content: "Buy milk".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        }
    }

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
    fn hierarchy_moves_use_parent_ids_and_root_promotions_use_project_ids() {
        assert_eq!(
            task_parent_move_args("task", Some("parent"), "project"),
            json!({"id": "task", "parent_id": "parent"})
        );
        assert_eq!(
            task_parent_move_args("task", None, "project"),
            json!({"id": "task", "project_id": "project"})
        );
    }

    #[test]
    fn project_reorder_uses_todoists_bulk_order_shape() {
        let projects = vec![
            crate::model::ProjectOrder {
                id: "first".into(),
                child_order: 0,
            },
            crate::model::ProjectOrder {
                id: "second".into(),
                child_order: 1,
            },
        ];
        assert_eq!(
            project_reorder_args(&projects),
            json!({"projects": [
                {"id": "first", "child_order": 0},
                {"id": "second", "child_order": 1}
            ]})
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

    #[test]
    fn quick_edit_preserves_metadata_that_was_not_present_in_the_expression() {
        let original = task();
        let updated = merge_quick_edit(&original, &parsed_task(), "Buy milk", "", false);

        assert_eq!(updated.content, "Buy milk");
        assert!(updated.description.is_empty());
        assert_eq!(updated.project_id, original.project_id);
        assert_eq!(updated.labels, original.labels);
        assert_eq!(updated.due, original.due);
        assert_eq!(updated.priority, original.priority);
    }

    #[test]
    fn quick_edit_applies_metadata_parsed_from_the_expression() {
        let original = task();
        let mut parsed = parsed_task();
        parsed.labels = vec!["Errands".into(), "work".into()];
        parsed.due = Some(Due {
            string: Some("tomorrow at 9".into()),
            ..Due::default()
        });
        parsed.priority = 4;

        let updated = merge_quick_edit(
            &original,
            &parsed,
            "Buy milk tomorrow #Shopping @Errands p1",
            "Two bottles",
            true,
        );

        assert_eq!(updated.project_id, "project-2");
        assert_eq!(updated.labels, ["Work", "Errands"]);
        assert_eq!(
            updated.due.unwrap().string.as_deref(),
            Some("tomorrow at 9")
        );
        assert_eq!(updated.priority, 4);
        assert_eq!(updated.description, "Two bottles");
    }

    #[test]
    fn project_detection_handles_case_and_escaped_spaces() {
        assert!(expression_mentions_project(
            "Plan trip #Summer\\ Holiday tomorrow",
            "summer holiday"
        ));
        assert!(expression_mentions_project("Do it #✅TODO", "✅todo"));
        assert!(!expression_mentions_project("Do it #Unknown", "Inbox"));
    }
}
