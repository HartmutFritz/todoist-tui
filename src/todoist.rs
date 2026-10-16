use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::{
        Due, Operation, OutboxEntry, RemoteCollaborator, RemoteFilter, RemoteLabel, RemoteProject,
        RemoteTask, Task,
    },
    store::Store,
};

const API_BASE: &str = "https://api.todoist.com/api/v1";
const SYNC_RESOURCES: &str =
    r#"["projects","items","labels","filters","workspace_filters","collaborators","user"]"#;

#[derive(Debug, Deserialize)]
struct SyncResponse {
    #[serde(default)]
    full_sync: bool,
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
    #[serde(default)]
    collaborators: Vec<RemoteCollaborator>,
    #[serde(default)]
    user: Option<RemoteCollaborator>,
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
    verified: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub message: String,
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ApiFailure {}

pub fn retry_delay(error: &anyhow::Error) -> Option<Duration> {
    error
        .downcast_ref::<ApiFailure>()
        .and_then(|error| error.retry_after)
}

pub fn permanent_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiFailure>().is_some_and(|error| {
        (400..500).contains(&error.status)
            && !matches!(error.status, 401 | 408 | 409 | 429)
            && error.retry_after.is_none()
    })
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
            verified: Arc::new(AtomicBool::new(false)),
        })
    }

    fn check(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let header_delay = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .map(Duration::from_secs)
                    .or_else(|| {
                        chrono::DateTime::parse_from_rfc2822(value)
                            .ok()
                            .map(|date| {
                                (date.with_timezone(&Utc) - Utc::now())
                                    .to_std()
                                    .unwrap_or(Duration::ZERO)
                            })
                    })
            });
        let body = response.text().unwrap_or_default();
        let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let retry_after = header_delay.or_else(|| {
            json.pointer("/error_extra/retry_after")
                .and_then(Value::as_u64)
                .map(Duration::from_secs)
        });
        Err(ApiFailure {
            status: status.as_u16(),
            retry_after,
            message: format!("Todoist returned {status}: {body}"),
        }
        .into())
    }

    fn auth(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request.bearer_auth(&self.token)
    }

    pub fn verify_account(&self, store: &Store) -> Result<()> {
        if self.verified.load(Ordering::Acquire) {
            return Ok(());
        }
        let response = Self::check(
            self.auth(self.http.post(format!("{}/sync", self.base)))
                .form(&[("sync_token", "*"), ("resource_types", "[\"user\"]")])
                .send()?,
        )?;
        let value: Value = response.json().context("decode account identity")?;
        let user = value
            .get("user")
            .context("Todoist omitted the account identity")?;
        let id = id_value(user.get("id").context("Todoist omitted the account ID")?)?;
        let inbox = user.get("inbox_project_id").map(id_value).transpose()?;
        store.associate_account(&id, inbox.as_deref())?;
        if let Some(inbox_id) = inbox {
            store.merge_remote_snapshot(
                &[RemoteProject {
                    id: inbox_id,
                    name: "Inbox".into(),
                    parent_id: None,
                    child_order: 0,
                    workspace_id: None,
                    is_inbox_project: true,
                    shared: false,
                    can_assign_tasks: false,
                    is_deleted: false,
                    is_archived: false,
                }],
                &[],
                &[],
                &[],
                false,
                None,
            )?;
        }
        self.verified.store(true, Ordering::Release);
        Ok(())
    }

    pub fn push(&self, store: &Store, entry: &OutboxEntry) -> Result<()> {
        if !matches!(
            entry.operation,
            Operation::QuickAdd { .. } | Operation::QuickEdit { .. } | Operation::DeleteTask
        ) && store.task_is_deleted(&entry.entity_id)?
            && !matches!(
                entry.operation,
                Operation::AddProject { .. }
                    | Operation::UpdateProject { .. }
                    | Operation::AddFilter { .. }
                    | Operation::UpdateFilter { .. }
                    | Operation::ReorderProjects { .. }
                    | Operation::ReorderTasks { .. }
            )
        {
            store.finish_outbox(entry.seq)?;
            return Ok(());
        }
        match &entry.operation {
            Operation::QuickAdd {
                expression,
                description,
                project_id,
                project_name,
                parent_id,
                target_order,
            } => {
                let is_inbox = store
                    .project(project_id)?
                    .map(|p| p.is_inbox)
                    .unwrap_or(false);
                let (text, enforce_fallback_project) =
                    quick_add_request(expression, project_name, is_inbox);
                let mut checkpoint = checkpoint_object(entry);
                let deleted = store.task_is_deleted(&entry.entity_id)?;
                if deleted && checkpoint.get("parsed").is_none() && checkpoint["rejected"] == true {
                    store.cancel_rejected_creation(&entry.entity_id)?;
                    return Ok(());
                }
                let mut task = self.parse_task(store, entry, &text, &mut checkpoint)?;
                store.map_created_task_id(&entry.entity_id, &task.id)?;
                let follow_ups = store.task_follow_up_operations(&task.id, entry.seq)?;
                let superseded_project = checkpoint["rejected"] == true && follow_ups.iter().any(|operation|
                    matches!(operation, Operation::MoveTask { .. }) || matches!(operation, Operation::QuickEdit { expression, .. } if contains_explicit_project(expression)));
                let superseded_description = checkpoint["rejected"] == true && follow_ups.iter().any(|operation|
                    matches!(operation, Operation::QuickEdit { .. } | Operation::UpdateTask { .. }));
                let superseded_parent = superseded_project || (checkpoint["rejected"] == true && follow_ups.iter().any(|operation| matches!(operation, Operation::MoveTaskParent { .. })));
                if deleted {
                    store.replace_task_id(&task.id, &task)?;
                    store.finish_outbox(entry.seq)?;
                    return Ok(());
                }
                if enforce_fallback_project && task.project_id != *project_id && !superseded_project {
                    self.step(store, entry, &mut checkpoint, "fallback",
                        "item_move",
                        json!({"id": task.id, "project_id": project_id}),
                    )?;
                    task.project_id.clone_from(project_id);
                }
                if let Some(parent_id) = parent_id && !superseded_parent {
                    let resolved_parent = store.resolve_sync_task_id(parent_id)?;
                    self.step(store, entry, &mut checkpoint, "parent",
                        "item_move",
                        json!({"id": task.id, "parent_id": resolved_parent}),
                    )?;
                    task.project_id.clone_from(project_id);
                    task.parent_id = Some(resolved_parent);
                }
                if let Some(target_order) = target_order {
                    self.step(store, entry, &mut checkpoint, "order",
                        "item_reorder",
                        json!({"items": [{"id": task.id, "child_order": target_order}]}),
                    )?;
                    task.child_order = *target_order;
                }
                if !description.is_empty() && !superseded_description {
                    self.step(store, entry, &mut checkpoint, "description",
                        "item_update",
                        json!({"id": task.id, "description": description}),
                    )?;
                    task.description.clone_from(description);
                }
                store.replace_task_id(&task.id, &task)?;
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
            Operation::RestoreDue { due } => self.command(entry, "item_update",
                json!({"id": entry.entity_id, "due": exact_due_value(due.as_ref())}))?,
            Operation::RestoreRecurring { due } => self.command(entry, "item_update_date_complete",
                json!({"id": entry.entity_id, "due": exact_due_value(Some(due)), "is_forward": false}))?,
            Operation::RestoreCompletion { completed } => {
                let args = if *completed { json!({"id": entry.entity_id, "from_undo": true}) }
                    else { json!({"id": entry.entity_id}) };
                self.command(entry, if *completed { "item_complete" } else { "item_uncomplete" }, args)?;
            }
            Operation::UpdateAssignee { responsible_uid } => self.command(entry, "item_update",
                json!({"id": entry.entity_id, "responsible_uid": responsible_uid}))?,
            Operation::UpdateLabels { labels } => self.command(
                entry,
                "item_update",
                json!({
                    "id": entry.entity_id, "labels": labels
                }),
            )?,
            Operation::UpdatePriority { priority } => self.command(
                entry,
                "item_update",
                json!({"id": entry.entity_id, "priority": priority}),
            )?,
            Operation::MoveTask { project_id } => self.command(
                entry,
                "item_move",
                json!({"id": entry.entity_id, "project_id": project_id}),
            )?,
            Operation::MoveTaskParent { parent_id, project_id } => {
                if let Some(parent_id) = parent_id {
                    let resolved_parent = store.resolve_sync_task_id(parent_id)?;
                    self.command(
                        entry,
                        "item_move",
                        task_parent_move_args(&entry.entity_id, Some(&resolved_parent), ""),
                    )?;
                } else {
                    let destination = if project_id.is_empty() {
                        store.task_including_deleted(&entry.entity_id)?.context("outdented task no longer exists")?.project_id
                    } else { project_id.clone() };
                    self.command(
                        entry,
                        "item_move",
                        task_parent_move_args(&entry.entity_id, None, &destination),
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
                if entry.entity_id.starts_with("local:") {
                    store.finish_local_delete(entry)?;
                    return Ok(());
                }
                if let Err(error) = self.command(entry, "item_delete", json!({"id": entry.entity_id})) {
                    let already_missing = error.downcast_ref::<ApiFailure>().is_some_and(|failure|
                        failure.status == 404 || failure.message.contains("ITEM_NOT_FOUND")
                        || failure.message.contains("TASK_NOT_FOUND") || failure.message.contains("Item not found"));
                    if !already_missing { return Err(error); }
                }
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
            Operation::ReorderTasks { tasks } => {
                let tasks = tasks.iter().filter(|task| !task.id.starts_with("local:")).collect::<Vec<_>>();
                if !tasks.is_empty() { self.command(entry, "item_reorder", json!({"items": tasks}))?; }
            }
        }
        store.finish_outbox(entry.seq)?;
        Ok(())
    }

    fn parse_task(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        text: &str,
        checkpoint: &mut Value,
    ) -> Result<RemoteTask> {
        if let Some(task) = checkpoint.get("parsed") {
            return Ok(serde_json::from_value(task.clone())?);
        }
        checkpoint["request_started"] = json!(true);
        store.save_checkpoint(entry.seq, checkpoint)?;
        // Todoist's SDK uses X-Request-ID for Quick Add as well. Reuse it on
        // transport retries, and persist the result before any follow-up write.
        let response = Self::check(
            self.auth(self.http.post(format!("{}/tasks/quick", self.base)))
                .header("X-Request-Id", &entry.uuid)
                .json(&json!({"text": text, "meta": true}))
                .send()?,
        )?;
        let value: Value = response.json().context("decode Quick Add response")?;
        let task: RemoteTask = serde_json::from_value(
            value
                .get("task")
                .or_else(|| value.get("item"))
                .unwrap_or(&value)
                .clone(),
        )
        .context("Todoist Quick Add response did not contain a task")?;
        checkpoint["parsed"] = serde_json::to_value(&task)?;
        store.save_checkpoint(entry.seq, checkpoint)?;
        Ok(task)
    }

    fn step(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        checkpoint: &mut Value,
        name: &str,
        kind: &str,
        args: Value,
    ) -> Result<()> {
        if checkpoint[name] == true {
            return Ok(());
        }
        // A command UUID is an opaque string, and must be stable across retries.
        self.command_with_uuid(&format!("{}-{name}", entry.uuid), kind, args)?;
        checkpoint[name] = json!(true);
        store.save_checkpoint(entry.seq, checkpoint)
    }

    fn add_filter(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        name: &str,
        query: &str,
        is_favorite: bool,
    ) -> Result<()> {
        if !entry.entity_id.starts_with("local:") {
            return Ok(());
        }
        if let Some(id) = entry.checkpoint.get("mapped_id").and_then(Value::as_str) {
            return store.replace_filter_id(&entry.entity_id, id);
        }
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
            return Err(command_failure(status));
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
        store.save_checkpoint(entry.seq, &json!({"mapped_id": new_id}))?;
        store.replace_filter_id(&entry.entity_id, &new_id)?;
        Ok(())
    }

    fn add_project(&self, store: &Store, entry: &OutboxEntry, name: &str) -> Result<()> {
        if !entry.entity_id.starts_with("local:") {
            return Ok(());
        }
        if let Some(id) = entry.checkpoint.get("mapped_id").and_then(Value::as_str) {
            return store.replace_project_id(&entry.entity_id, id);
        }
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
            return Err(command_failure(status));
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
        store.save_checkpoint(entry.seq, &json!({"mapped_id": new_id}))?;
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
        let mut checkpoint = checkpoint_object(entry);
        if store.task_is_deleted(&entry.entity_id)? {
            if checkpoint.get("parsed").is_some()
                || (checkpoint["request_started"] == true && checkpoint["rejected"] != true)
            {
                let parsed = self.parse_task(store, entry, expression, &mut checkpoint)?;
                if checkpoint["cleanup"] != true {
                    self.command_with_uuid(delete_uuid, "item_delete", json!({"id": parsed.id}))?;
                    checkpoint["cleanup"] = json!(true);
                    store.save_checkpoint(entry.seq, &checkpoint)?;
                }
            }
            return Ok(());
        }
        let original: Task = if let Some(original) = checkpoint.get("original") {
            serde_json::from_value(original.clone())?
        } else {
            let original = store
                .task_including_deleted(&entry.entity_id)?
                .context("edited task no longer exists")?;
            checkpoint["original"] = serde_json::to_value(&original)?;
            store.save_checkpoint(entry.seq, &checkpoint)?;
            original
        };
        let parsed = self.parse_task(store, entry, expression, &mut checkpoint)?;

        let parsed_project_selected = store
            .project(&parsed.project_id)?
            .is_some_and(|project| expression_mentions_project(expression, &project.name));
        let parsed_due = parsed.due.is_some();
        let updated = if let Some(updated) = checkpoint.get("updated") {
            serde_json::from_value(updated.clone())?
        } else {
            let updated = merge_quick_edit(
                &original,
                &parsed,
                expression,
                description,
                parsed_project_selected,
            );
            checkpoint["updated"] = serde_json::to_value(&updated)?;
            store.save_checkpoint(entry.seq, &checkpoint)?;
            updated
        };
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
            args["due"] = exact_due_value(updated.due.as_ref());
        }

        if checkpoint["update"] != true {
            self.command_with_uuid(update_uuid, "item_update", args)?;
            checkpoint["update"] = json!(true);
            store.save_checkpoint(entry.seq, &checkpoint)?;
        }
        if let Some(project_id) = move_project
            && checkpoint["move"] != true
        {
            self.command_with_uuid(
                move_uuid,
                "item_move",
                json!({"id": entry.entity_id, "project_id": project_id}),
            )?;
            checkpoint["move"] = json!(true);
            store.save_checkpoint(entry.seq, &checkpoint)?;
        }
        if checkpoint["cleanup"] != true {
            self.command_with_uuid(delete_uuid, "item_delete", json!({"id": parsed.id}))
                .context("remove temporary Quick Edit parser task")?;
            checkpoint["cleanup"] = json!(true);
            store.save_checkpoint(entry.seq, &checkpoint)?;
        }
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
            return Err(command_failure(status));
        }
        Ok(())
    }

    pub fn pull(&self, store: &Store) -> Result<()> {
        let mut sync_token = store.sync_token()?;
        // A full sync may be generated from a slightly delayed snapshot, so
        // immediately follow the first one with an incremental pass.
        let mut retried_full = false;
        let mut full_passes = 0;
        loop {
            let response = Self::check(
                self.auth(self.http.post(format!("{}/sync", self.base)))
                    .form(&[
                        ("sync_token", sync_token.as_str()),
                        ("resource_types", SYNC_RESOURCES),
                    ])
                    .send()?,
            );
            let response = match response {
                Ok(response) => response,
                Err(error)
                    if sync_token != "*"
                        && !retried_full
                        && error.downcast_ref::<ApiFailure>().is_some_and(|failure| {
                            failure.message.contains("SYNC_TOKEN")
                                || failure
                                    .message
                                    .to_lowercase()
                                    .contains("invalid sync token")
                        }) =>
                {
                    sync_token = "*".into();
                    retried_full = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let value: Value = response.json().context("decode sync response")?;
            if value.get("sync_token").is_none()
                && sync_token != "*"
                && !retried_full
                && value
                    .get("error_tag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| tag.contains("SYNC_TOKEN"))
            {
                sync_token = "*".into();
                retried_full = true;
                continue;
            }
            let mut sync: SyncResponse =
                serde_json::from_value(value).context("decode sync response")?;
            sync.filters.append(&mut sync.workspace_filters);
            if let Some(user) = sync.user.take() {
                sync.collaborators.push(user);
            }
            store.merge_collaborators(&sync.collaborators)?;
            store.merge_remote_snapshot(
                &sync.projects,
                &sync.items,
                &sync.labels,
                &sync.filters,
                sync.full_sync,
                Some(&sync.sync_token),
            )?;
            sync_token = sync.sync_token;
            if !sync.full_sync {
                break;
            }
            full_passes += 1;
            if full_passes >= 2 {
                break;
            }
        }
        Ok(())
    }

    pub fn pull_auxiliary(&self, store: &Store) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        // Each filter is isolated: an unsupported query cannot fail the full refresh.
        for filter in store.favorite_filters()? {
            if store.outbox_len()? > 0 {
                return Ok(warnings);
            }
            match self.filtered_tasks(&filter.query, store) {
                Ok(Some(tasks)) => store.replace_filter_membership(&filter.id, &tasks)?,
                Ok(None) => return Ok(warnings),
                Err(error) => {
                    if retry_delay(&error).is_some() {
                        return Err(error);
                    }
                    warnings.push(format!("Filter ‘{}’ is stale: {error:#}", filter.name));
                }
            }
        }
        // Completed history is an auxiliary view. Keep normal synchronization
        // healthy if an account or server does not expose this endpoint.
        if store.outbox_len()? > 0 {
            return Ok(warnings);
        }
        let previous = store
            .meta("completed_history_refresh")?
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok());
        if previous.is_some_and(|date| {
            Utc::now().signed_duration_since(date) < ChronoDuration::minutes(15)
        }) {
            // Preserve any history warning until a history request actually
            // succeeds, rather than clearing it on a filter-only refresh.
            if let Some(warning) = store
                .meta("completed_history_warning")?
                .filter(|value| !value.is_empty())
            {
                warnings.push(warning);
            }
            return Ok(warnings);
        }
        match self.recent_completed_tasks(store) {
            Ok(Some(tasks)) => {
                store.merge_completed_tasks(&tasks)?;
                store.set_meta("completed_history_refresh", &Utc::now().to_rfc3339())?;
                store.set_meta("completed_history_warning", "")?;
            }
            Ok(None) => {}
            Err(error) => {
                if retry_delay(&error).is_some() {
                    return Err(error);
                }
                let warning = format!("Completed history is stale: {error:#}");
                store.set_meta("completed_history_warning", &warning)?;
                warnings.push(warning);
            }
        }
        Ok(warnings)
    }

    fn recent_completed_tasks(&self, store: &Store) -> Result<Option<Vec<RemoteTask>>> {
        let until = Utc::now();
        let previous = store
            .meta("completed_history_refresh")?
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok());
        let since = previous
            .map(|date| date.with_timezone(&Utc) - ChronoDuration::days(1))
            .unwrap_or(until - ChronoDuration::days(89))
            .max(until - ChronoDuration::days(89));
        let since = since.to_rfc3339_opts(SecondsFormat::Secs, true);
        let until = until.to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            if store.outbox_len()? > 0 {
                return Ok(None);
            }
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
        Ok(Some(all))
    }

    fn filtered_tasks(&self, query: &str, store: &Store) -> Result<Option<Vec<RemoteTask>>> {
        let query = assignment_negation_compat_query(query);
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            if store.outbox_len()? > 0 {
                return Ok(None);
            }
            let mut request = self
                .auth(self.http.get(format!("{}/tasks/filter", self.base)))
                .query(&[("query", query.as_str()), ("limit", "200")]);
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
        Ok(Some(all))
    }
}

fn checkpoint_object(entry: &OutboxEntry) -> Value {
    if entry.checkpoint.is_object() {
        entry.checkpoint.clone()
    } else {
        json!({})
    }
}

fn id_value(value: &Value) -> Result<String> {
    match value {
        Value::String(id) => Ok(id.clone()),
        Value::Number(id) => Ok(id.to_string()),
        _ => bail!("Todoist returned an invalid resource ID"),
    }
}

fn command_failure(status: &Value) -> anyhow::Error {
    ApiFailure {
        status: status
            .get("http_code")
            .and_then(Value::as_u64)
            .unwrap_or(400) as u16,
        retry_after: status
            .pointer("/error_extra/retry_after")
            .and_then(Value::as_u64)
            .map(Duration::from_secs),
        message: format!("Todoist rejected operation: {status}"),
    }
    .into()
}

fn assignment_negation_compat_query(query: &str) -> String {
    const PREDICATE: &str = "!assigned to:";
    let lowercase = query.to_ascii_lowercase();
    let mut rewritten = String::with_capacity(query.len());
    let mut cursor = 0;
    while let Some(relative_start) = lowercase[cursor..].find(PREDICATE) {
        let start = cursor + relative_start;
        let value_start = start + PREDICATE.len();
        let value_end = query[value_start..]
            .find(['&', '|', ')', ','])
            .map_or(query.len(), |offset| value_start + offset);
        let trimmed_end = query[value_start..value_end].trim_end().len() + value_start;
        if query[value_start..trimmed_end].trim().is_empty() {
            rewritten.push_str(&query[cursor..value_start]);
            cursor = value_start;
            continue;
        }
        rewritten.push_str(&query[cursor..start]);
        rewritten.push_str("(!assigned | ");
        rewritten.push_str(&query[start..trimmed_end]);
        rewritten.push(')');
        rewritten.push_str(&query[trimmed_end..value_end]);
        cursor = value_end;
    }
    rewritten.push_str(&query[cursor..]);
    rewritten
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

fn exact_due_value(due: Option<&Due>) -> Value {
    due.map_or(Value::Null, |due| {
        if due.datetime.is_none() && due.date.is_none() {
            return due_value(Some(due));
        }
        json!({"date": due.datetime.as_ref().or(due.date.as_ref()), "string": due.string,
            "timezone": due.timezone, "is_recurring": due.is_recurring})
    })
}

fn quick_add_request(expression: &str, project_name: &str, is_inbox: bool) -> (String, bool) {
    let has_explicit_project = contains_explicit_project(expression);
    if is_inbox || has_explicit_project {
        (expression.to_owned(), false)
    } else {
        (
            format!("{expression} #{}", project_name.replace(' ', "\\ ")),
            true,
        )
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
        if updated.project_id != original.project_id {
            updated.parent_id = None;
        }
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
        sync::{Mutex, mpsc},
        thread,
    };

    type MockResponse = (u16, Value, Option<u64>);

    struct MockApi {
        client: TodoistClient,
        responses: Arc<Mutex<VecDeque<MockResponse>>>,
        requests: mpsc::Receiver<String>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl MockApi {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::<(u16, Value, Option<u64>)>::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (tx, requests) = mpsc::channel();
            let queued = responses.clone();
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("accept mock request: {error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let count = stream.read(&mut buffer).unwrap();
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length: usize = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|value| value.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    tx.send(String::from_utf8(request).unwrap()).unwrap();
                    let (status, body, retry) = queued
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected HTTP request");
                    let body = body.to_string();
                    let retry = retry
                        .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                        .unwrap_or_default();
                    write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{retry}\r\n{body}", body.len()).unwrap();
                }
            });
            let mut client = TodoistClient::new("test-token".into()).unwrap();
            client.base = base;
            Self {
                client,
                responses,
                requests,
                stop,
                worker: Some(worker),
            }
        }
        fn respond(&self, status: u16, body: Value) {
            self.responses
                .lock()
                .unwrap()
                .push_back((status, body, None));
        }
        fn command(&self, uuid: &str) {
            self.respond(200, json!({"sync_status": {uuid: "ok"}}));
        }
        fn request(&self) -> String {
            self.requests.recv_timeout(Duration::from_secs(3)).unwrap()
        }
    }
    impl Drop for MockApi {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn quick_add_resumes_from_durable_checkpoint_without_recreating_or_reordering() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let store = Store::open(&path).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        let id = store
            .add_task_at_top_with_description("Buy milk", "Description", &inbox)
            .unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(
            200,
            json!({"id":"remote-task", "project_id":"inbox", "content":"Buy milk"}),
        );
        api.command(&format!("{}-order", entry.uuid));
        api.respond(500, json!({"error":"temporary failure"}));
        assert!(api.client.push(&store, &entry).is_err());
        store
            .edit_task(&id, "Newest local title", "Newest local description")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let resumed = store.claim_next_outbox().unwrap().unwrap();
        assert_eq!(resumed.uuid, entry.uuid);
        assert_eq!(resumed.checkpoint["order"], true);
        api.command(&format!("{}-description", entry.uuid));
        api.client.push(&store, &resumed).unwrap();
        assert_eq!(
            store.task(&id).unwrap().unwrap().content,
            "Newest local title"
        );
        assert_eq!(
            store.next_outbox().unwrap().unwrap().entity_id,
            "remote-task"
        );
        assert!(api.request().starts_with("POST /tasks/quick "));
        assert!(api.request().contains("item_reorder"));
        let first = api.request();
        let retried = api.request();
        assert!(first.contains(&format!("{}-description", entry.uuid)));
        assert!(retried.contains(&format!("{}-description", entry.uuid)));
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn quick_edit_retries_only_cleanup_and_preserves_newer_metadata() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let store = Store::open(&path).unwrap();
        let remote: RemoteTask =
            serde_json::from_value(json!({"id":"task", "project_id":"inbox", "content":"Old"}))
                .unwrap();
        store.merge_remote(&[], &[remote], &[], &[]).unwrap();
        store
            .edit_task("task", "New tomorrow", "Description")
            .unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        let Operation::QuickEdit {
            update_uuid,
            delete_uuid,
            ..
        } = &entry.operation
        else {
            panic!()
        };
        api.respond(200, json!({"id":"parser", "project_id":"inbox", "content":"New", "due":{"date":"2026-10-17", "string":"tomorrow", "is_recurring":false}}));
        api.command(update_uuid);
        api.respond(500, json!({"error":"cleanup unavailable"}));
        assert!(api.client.push(&store, &entry).is_err());
        store.update_labels("task", &["Local".into()]).unwrap();
        drop(store);
        let store = Store::open(path).unwrap();
        api.command(delete_uuid);
        api.client
            .push(&store, &store.claim_next_outbox().unwrap().unwrap())
            .unwrap();
        let task = store.task("task").unwrap().unwrap();
        assert_eq!(task.content, "New");
        assert_eq!(task.labels, ["Local"]);
        assert!(api.request().starts_with("POST /tasks/quick "));
        assert!(api.request().contains("item_update"));
        assert!(api.request().contains("item_delete"));
        assert!(api.request().contains(delete_uuid));
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn offline_edit_then_delete_does_not_require_a_live_parser_task() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let remote: RemoteTask =
            serde_json::from_value(json!({"id":"task", "project_id":"inbox", "content":"Old"}))
                .unwrap();
        store.merge_remote(&[], &[remote], &[], &[]).unwrap();
        store.edit_task("task", "New", "").unwrap();
        store.delete_task_tree("task").unwrap();
        let edit = store.claim_next_outbox().unwrap().unwrap();
        api.client.push(&store, &edit).unwrap();
        let delete = store.claim_next_outbox().unwrap().unwrap();
        api.command(&delete.uuid);
        api.client.push(&store, &delete).unwrap();
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.request().contains("item_delete"));
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn http_200_command_rejections_and_rate_limits_keep_complete_error_information() {
        let api = MockApi::new();
        let detail = "Detailed error ".repeat(50);
        api.respond(
            200,
            json!({"sync_status":{"command":{"http_code":400,"error":detail}}}),
        );
        let error = api
            .client
            .command_with_uuid("command", "item_update", json!({"id":"task"}))
            .unwrap_err();
        assert!(permanent_rejection(&error));
        assert!(error.to_string().contains(&detail));
        api.responses
            .lock()
            .unwrap()
            .push_back((429, json!({"error":"rate limited"}), Some(120)));
        let error = api
            .client
            .command_with_uuid("command", "item_update", json!({"id":"task"}))
            .unwrap_err();
        assert_eq!(retry_delay(&error), Some(Duration::from_secs(120)));
        assert!(!permanent_rejection(&error));
    }

    #[test]
    fn account_identity_is_checked_before_any_write_and_canonical_inbox_is_bound() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        store.add_task("Offline", &inbox).unwrap();
        api.respond(
            200,
            json!({"user":{"id":"account-a","inbox_project_id":"canonical"}}),
        );
        api.client.verify_account(&store).unwrap();
        assert!(
            matches!(store.next_outbox().unwrap().unwrap().operation, Operation::QuickAdd { project_id, .. } if project_id == "canonical")
        );
        assert!(!api.request().contains("commands="));
        api.client.verified.store(false, Ordering::Release);
        api.respond(
            200,
            json!({"user":{"id":"account-b","inbox_project_id":"other"}}),
        );
        assert!(api.client.verify_account(&store).is_err());
        assert_eq!(store.outbox_len().unwrap(), 1);
        assert!(!api.request().contains("commands="));
    }

    #[test]
    fn unusable_sync_tokens_recover_with_full_and_incremental_refresh() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        store.set_sync_token("expired-token").unwrap();
        api.respond(400, json!({"error_tag":"INVALID_SYNC_TOKEN"}));
        api.respond(
            200,
            json!({"full_sync":true,"sync_token":"full-token","projects":[],"items":[]}),
        );
        api.respond(
            200,
            json!({"full_sync":false,"sync_token":"incremental-token","items":[]}),
        );
        api.client.pull(&store).unwrap();
        assert_eq!(store.sync_token().unwrap(), "incremental-token");
        assert!(api.request().contains("expired-token"));
        assert!(api.request().contains("sync_token=*"));
        assert!(api.request().contains("full-token"));
    }

    #[test]
    fn paginated_filter_refresh_is_atomic_and_yields_to_local_writes() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        api.respond(200, json!({"results":[{"id":"one","project_id":"inbox","content":"One"}],"next_cursor":"cursor-two"}));
        api.respond(200, json!({"results":[{"id":"two","project_id":"inbox","content":"Two"}],"next_cursor":null}));
        let tasks = api.client.filtered_tasks("today", &store).unwrap().unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(api.request().contains("query=today"));
        assert!(api.request().contains("cursor=cursor-two"));
        let inbox = store.project("inbox").unwrap().unwrap();
        store.add_task("Local", &inbox).unwrap();
        assert!(
            api.client
                .filtered_tasks("today", &store)
                .unwrap()
                .is_none()
        );
        assert!(api.requests.try_recv().is_err());
    }

    fn wait_for_sync(manager: &crate::sync::SyncManager) -> crate::sync::SyncEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = manager.next_event()
                && matches!(
                    event,
                    crate::sync::SyncEvent::Finished { .. } | crate::sync::SyncEvent::Failed { .. }
                )
            {
                return event;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sync worker did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn background_worker_quarantines_a_rejection_pushes_unrelated_changes_and_recovers() {
        use crate::sync::{SyncEvent, SyncManager};
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let tasks: Vec<RemoteTask> = serde_json::from_value(json!([
            {"id":"first","project_id":"inbox","content":"First"},
            {"id":"second","project_id":"inbox","content":"Second"}]))
        .unwrap();
        store.merge_remote(&[], &tasks, &[], &[]).unwrap();
        store.update_due("first", "invalid").unwrap();
        store.update_labels("second", &["Local".into()]).unwrap();
        let queued = store.outbox().unwrap();
        api.respond(200, json!({"user":{"id":"account"}}));
        api.respond(
            200,
            json!({"sync_status":{&queued[0].uuid:{"http_code":400,"error":"Invalid due date"}}}),
        );
        api.command(&queued[1].uuid);
        api.respond(200, json!({"sync_token":"token"}));
        let manager = SyncManager::with_test_client(store.clone(), api.client.clone());
        assert!(manager.start());
        assert!(
            matches!(wait_for_sync(&manager), SyncEvent::Failed { message, .. } if message.contains("Invalid due date"))
        );
        assert_eq!(store.outbox_len().unwrap(), 1);
        assert_eq!(store.task("second").unwrap().unwrap().labels, ["Local"]);
        store.update_due("first", "tomorrow").unwrap();
        let corrected = store.next_outbox().unwrap().unwrap();
        api.command(&corrected.uuid);
        api.respond(200, json!({"sync_token":"new-token"}));
        assert!(manager.start());
        assert!(matches!(
            wait_for_sync(&manager),
            SyncEvent::Finished { pending: 0 }
        ));
        assert!(store.outbox_error().unwrap().is_none());
        assert_eq!(store.outbox_len().unwrap(), 0);
    }

    #[test]
    fn server_backoff_prevents_duplicate_runs_and_survives_restart() {
        use crate::sync::{SyncEvent, SyncManager};
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let store = Store::open(&path).unwrap();
        let task: RemoteTask =
            serde_json::from_value(json!({"id":"task","project_id":"inbox","content":"Task"}))
                .unwrap();
        store.merge_remote(&[], &[task], &[], &[]).unwrap();
        store.update_due("task", "tomorrow").unwrap();
        api.respond(200, json!({"user":{"id":"account"}}));
        api.responses
            .lock()
            .unwrap()
            .push_back((429, json!({"error":"Rate limited"}), Some(120)));
        let manager = SyncManager::with_test_client(store.clone(), api.client.clone());
        assert!(manager.start());
        assert!(matches!(wait_for_sync(&manager), SyncEvent::Failed { .. }));
        assert!(manager.retry_after() > Duration::from_secs(110));
        assert!(!manager.start());
        store
            .update_labels("task", &["Offline edit".into()])
            .unwrap();
        let restarted =
            SyncManager::with_test_client(Store::open(path).unwrap(), api.client.clone());
        assert!(restarted.retry_after() > Duration::from_secs(110));
        assert!(!restarted.start());
        assert_eq!(store.outbox_len().unwrap(), 2);
        assert_eq!(
            store.task("task").unwrap().unwrap().labels,
            ["Offline edit"]
        );
    }

    #[test]
    fn uncertain_quick_add_reuses_the_request_id_and_restores_only_one_cached_task() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let store = Store::open(&path).unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        store.add_task("Captured", &inbox).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(500, json!({"error":"response lost after server commit"}));
        assert!(api.client.push(&store, &entry).is_err());
        drop(store);
        let store = Store::open(path).unwrap();
        api.respond(
            200,
            json!({"id":"only-task","project_id":"inbox","content":"Captured"}),
        );
        api.client
            .push(&store, &store.claim_next_outbox().unwrap().unwrap())
            .unwrap();
        assert_eq!(
            store
                .tasks_for(&crate::model::NavId::Project("inbox".into()))
                .unwrap()
                .len(),
            1
        );
        for request in [api.request(), api.request()] {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("x-request-id: {}", entry.uuid))
            );
        }
        assert_eq!(store.outbox_len().unwrap(), 0);
    }

    #[test]
    fn failed_filter_refresh_retains_cached_membership_and_reports_staleness() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let filter: RemoteFilter = serde_json::from_value(
            json!({"id":"filter","name":"Today","query":"today","is_favorite":true}),
        )
        .unwrap();
        let task: RemoteTask =
            serde_json::from_value(json!({"id":"cached","project_id":"inbox","content":"Cached"}))
                .unwrap();
        store.merge_remote(&[], &[], &[], &[filter]).unwrap();
        store.replace_filter_membership("filter", &[task]).unwrap();
        store
            .set_meta("completed_history_refresh", &Utc::now().to_rfc3339())
            .unwrap();
        api.respond(400, json!({"error":"unsupported query"}));
        let warnings = api.client.pull_auxiliary(&store).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Today"));
        assert_eq!(
            store
                .tasks_for(&crate::model::NavId::Filter("filter".into()))
                .unwrap()
                .len(),
            1
        );
        api.respond(200, json!({"results":[],"next_cursor":null}));
        assert!(api.client.pull_auxiliary(&store).unwrap().is_empty());
        assert!(
            store
                .tasks_for(&crate::model::NavId::Filter("filter".into()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn restored_due_dates_send_concrete_values_instead_of_reparsing_relative_text() {
        let due = Due {
            string: Some("tomorrow at 9".into()),
            date: Some("2026-10-14T09:00:00Z".into()),
            timezone: Some("Europe/London".into()),
            is_recurring: true,
            ..Due::default()
        };
        let value = exact_due_value(Some(&due));
        assert_eq!(value["date"], "2026-10-14T09:00:00Z");
        assert_eq!(value["timezone"], "Europe/London");
        assert_eq!(value["is_recurring"], true);
        assert_eq!(exact_due_value(None), Value::Null);
        assert_eq!(
            quick_add_request("Plan", "Summer Holiday", false).0,
            "Plan #Summer\\ Holiday"
        );
    }

    #[test]
    fn rejected_parser_updates_queue_durable_cleanup_without_blocking_other_work() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let task: RemoteTask =
            serde_json::from_value(json!({"id":"task","project_id":"inbox","content":"Original"}))
                .unwrap();
        store.merge_remote(&[], &[task], &[], &[]).unwrap();
        store.edit_task("task", "Updated", "").unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        let Operation::QuickEdit {
            update_uuid,
            delete_uuid,
            ..
        } = &entry.operation
        else {
            panic!()
        };
        api.respond(
            200,
            json!({"id":"parser","project_id":"inbox","content":"Updated"}),
        );
        api.respond(
            200,
            json!({"sync_status":{update_uuid:{"http_code":403,"error":"Read only"}}}),
        );
        let error = api.client.push(&store, &entry).unwrap_err();
        assert!(permanent_rejection(&error));
        store.fail_outbox(entry.seq, &error.to_string()).unwrap();
        let cleanup = store.claim_next_outbox().unwrap().unwrap();
        assert_eq!(cleanup.entity_id, "parser");
        assert_eq!(cleanup.uuid, *delete_uuid);
        api.command(delete_uuid);
        api.client.push(&store, &cleanup).unwrap();
        assert_eq!(store.outbox_len().unwrap(), 1);
        assert!(store.claim_next_outbox().unwrap().is_none());
    }

    #[test]
    fn a_partial_creation_is_mapped_immediately_and_a_corrective_move_can_recover_it() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let projects: Vec<RemoteProject> = serde_json::from_value(
            json!([{"id":"source","name":"Source"},{"id":"destination","name":"Destination"}]),
        )
        .unwrap();
        store.merge_remote(&projects, &[], &[], &[]).unwrap();
        let source = store.project("source").unwrap().unwrap();
        let id = store.add_task("Task", &source).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(
            200,
            json!({"id":"created","project_id":"inbox","content":"Task"}),
        );
        api.respond(200, json!({"sync_status":{format!("{}-fallback", entry.uuid):{"http_code":400,"error":"Project no longer exists"}}}));
        let error = api.client.push(&store, &entry).unwrap_err();
        assert_eq!(store.task(&id).unwrap().unwrap().id, "created");
        store.fail_outbox(entry.seq, &error.to_string()).unwrap();
        store.move_task(&id, "destination").unwrap();
        let resumed = store.claim_next_outbox().unwrap().unwrap();
        api.client.push(&store, &resumed).unwrap();
        let moved = store.claim_next_outbox().unwrap().unwrap();
        api.command(&moved.uuid);
        api.client.push(&store, &moved).unwrap();
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert_eq!(store.task(&id).unwrap().unwrap().project_id, "destination");
        assert!(api.request().starts_with("POST /tasks/quick "));
        assert!(api.request().contains("source"));
        assert!(api.request().contains("destination"));
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn completed_history_accepts_items_pages_and_uses_an_incremental_overlap() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let previous = Utc::now() - ChronoDuration::hours(1);
        store
            .set_meta("completed_history_refresh", &previous.to_rfc3339())
            .unwrap();
        api.respond(200, json!({"items":[{"id":"one","project_id":"inbox","content":"One","completed_at":"2026-10-17T10:00:00Z"}],"next_cursor":"page-two"}));
        api.respond(200, json!({"items":[],"next_cursor":null}));
        let tasks = api.client.recent_completed_tasks(&store).unwrap().unwrap();
        store.merge_completed_tasks(&tasks).unwrap();
        assert!(store.task("one").unwrap().unwrap().checked);
        let first = api.request();
        assert!(first.starts_with("GET /tasks/completed/by_completion_date?"));
        let since = (previous - ChronoDuration::days(1))
            .format("%Y-%m-%dT")
            .to_string();
        assert!(first.contains(&since));
        assert!(api.request().contains("cursor=page-two"));
    }

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
            assignee: None,
        }
    }

    fn parsed_task() -> RemoteTask {
        RemoteTask {
            id: "parser-task".into(),
            project_id: "project-2".into(),
            parent_id: None,
            responsible_uid: None,
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
    fn filter_assignment_negation_includes_unassigned_tasks() {
        assert_eq!(
            assignment_negation_compat_query(
                "#today | ((today | overdue) & !assigned to: Lea) | (p1 & !assigned to:Lea)"
            ),
            "#today | ((today | overdue) & (!assigned | !assigned to: Lea)) | (p1 & (!assigned | !assigned to:Lea))"
        );
        assert_eq!(
            assignment_negation_compat_query("assigned to: me | today"),
            "assigned to: me | today"
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
