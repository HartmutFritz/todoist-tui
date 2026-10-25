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
const SYNC_RESOURCES: &str = r#"["projects","items","sections","labels","filters","workspace_filters","collaborators","user"]"#;

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
                    | Operation::OrderResources { .. }
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
                placement,
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
                if checkpoint["created_order_recorded"] != true {
                    store.record_created_order(&checkpoint["parsed"])?;
                    checkpoint["created_order_recorded"]=json!(true);
                    store.save_checkpoint(entry.seq,&checkpoint)?;
                }
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
                    store.set_meta("ordering_dirty","true")?;
                    self.step(store, entry, &mut checkpoint, "fallback",
                        "item_move",
                        json!({"id": task.id, "project_id": project_id}),
                    )?;
                    task.project_id.clone_from(project_id);
                }
                if let Some(parent_id) = parent_id && !superseded_parent {
                    store.set_meta("ordering_dirty","true")?;
                    let resolved_parent = store.resolve_sync_task_id(parent_id)?;
                    self.step(store, entry, &mut checkpoint, "parent",
                        "item_move",
                        json!({"id": task.id, "parent_id": resolved_parent}),
                    )?;
                    task.project_id.clone_from(project_id);
                    task.parent_id = Some(resolved_parent);
                }
                if checkpoint["order"] != true {
                    let mut key_placement=*placement;
                    if key_placement.is_none() && let Some(target_order)=target_order {
                        if checkpoint["key_migration"] != true {
                            store.set_meta("ordering_dirty","true")?;
                            match self.step(store,entry,&mut checkpoint,"order","item_reorder",json!({"items":[{"id":task.id,"child_order":target_order}]})) {
                                Ok(())=>{},
                                Err(error) if permanent_rejection(&error)=>key_placement=Some(crate::ordering::Placement::Legacy(*target_order)),
                                Err(error)=>return Err(error),
                            }
                        }else{key_placement=Some(crate::ordering::Placement::Legacy(*target_order));}
                    }
                    if let Some(placement)=key_placement {
                        if checkpoint.get("order-keys_writes").is_none() {
                            self.ensure_order_metadata(store)?;
                            store.set_meta("ordering_dirty","true")?;
                            let plan=store.placement_plan(&task.id,placement)?;
                            checkpoint["placed_index"]=json!(plan.positions[0].child_order);
                            checkpoint["order_plan"]=serde_json::to_value(&plan)?;
                            let writes=store.prepare_key_writes(&[plan])?;
                            checkpoint["order-keys_writes"]=serde_json::to_value(writes)?;
                            store.save_checkpoint(entry.seq,&checkpoint)?;
                        }
                        self.send_key_writes(store,entry,&mut checkpoint,"order-keys")?;
                        checkpoint["order"]=json!(true);
                        store.save_checkpoint(entry.seq,&checkpoint)?;
                    }
                }
                if let Some(index)=checkpoint.get("placed_index").and_then(Value::as_i64) {task.child_order=index;}
                if checkpoint.get("placed_index").is_some() && let Ok(crate::ordering::OrderScope::Tasks{project_id,parent_id,..})=store.task_order_scope(&task.id) {
                    task.project_id=project_id;task.parent_id=parent_id;
                }
                if !description.is_empty() && !superseded_description {
                    self.step(store, entry, &mut checkpoint, "description",
                        "item_update",
                        json!({"id": task.id, "description": description}),
                    )?;
                    task.description.clone_from(description);
                }
                store.replace_task_id(&task.id, &task)?;
                if let Some(plan)=checkpoint.get("order_plan") {
                    store.finish_order_outbox(entry,&[serde_json::from_value(plan.clone())?])?;
                    return Ok(());
                }
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
            Operation::MoveTask { project_id } => {
                store.set_meta("ordering_dirty","true")?;
                self.command(entry,"item_move",json!({"id":entry.entity_id,"project_id":project_id}))?;
            },
            Operation::MoveTaskParent { parent_id, project_id, section_id } => {
                store.set_meta("ordering_dirty","true")?;
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
                        if let Some(section)=section_id {json!({"id":entry.entity_id,"section_id":section})}else{task_parent_move_args(&entry.entity_id, None, &destination)},
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
                let mut checkpoint=checkpoint_object(entry);
                if checkpoint["key_migration"] != true {
                    match self.command(entry,"project_reorder",project_reorder_args(projects)) {
                        Ok(())=>{store.set_meta("ordering_dirty","true")?;store.finish_outbox(entry.seq)?;return Ok(());},
                        Err(error) if permanent_rejection(&error)=>{},Err(error)=>return Err(error),
                    }
                }
                self.ensure_order_metadata(store)?;
                let plans=store.legacy_order_plans(projects.iter().map(|project|crate::model::TaskOrder{id:project.id.clone(),child_order:project.child_order}).collect(),true)?;
                self.push_order_plans(store,entry,&plans,&mut checkpoint)?;
                return Ok(());
            }
            Operation::ReorderTasks { tasks } => {
                let mut checkpoint=checkpoint_object(entry);
                if checkpoint["key_migration"] != true {
                    match self.command(entry,"item_reorder",json!({"items":tasks})) {
                        Ok(())=>{store.set_meta("ordering_dirty","true")?;store.finish_outbox(entry.seq)?;return Ok(());},
                        Err(error) if permanent_rejection(&error)=>{},Err(error)=>return Err(error),
                    }
                }
                self.ensure_order_metadata(store)?;
                let plans=store.legacy_order_plans(tasks.clone(),false)?;
                self.push_order_plans(store,entry,&plans,&mut checkpoint)?;
                return Ok(());
            }
            Operation::OrderResources {plans}=>{
                let mut checkpoint=checkpoint_object(entry);
                self.push_order_plans(store,entry,plans,&mut checkpoint)?;
                return Ok(());
            }
        }
        if matches!(
            entry.operation,
            Operation::QuickEdit { .. }
                | Operation::AddProject { .. }
                | Operation::DeleteTask
                | Operation::CompleteTask
                | Operation::UncompleteTask
                | Operation::RestoreCompletion { .. }
                | Operation::RestoreRecurring { .. }
        ) {
            store.set_meta("ordering_dirty", "true")?;
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
        checkpoint["parsed"] = value
            .get("task")
            .or_else(|| value.get("item"))
            .unwrap_or(&value)
            .clone();
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
            let mut updated = merge_quick_edit(
                &original,
                &parsed,
                expression,
                description,
                parsed_project_selected,
            );
            let section = checkpoint["parsed"]
                .get("section_id")
                .filter(|value| !value.is_null())
                .map(id_value)
                .transpose()?;
            if parsed_project_selected
                && (updated.project_id != original.project_id || section.is_some())
            {
                updated.section_id = section;
            }
            checkpoint["updated"] = serde_json::to_value(&updated)?;
            store.save_checkpoint(entry.seq, &checkpoint)?;
            updated
        };
        let move_project = (updated.project_id != original.project_id
            || updated.section_id != original.section_id)
            .then(|| updated.project_id.clone());

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
            store.set_meta("ordering_dirty", "true")?;
            self.command_with_uuid(
                move_uuid,
                "item_move",
                if let Some(section) = &updated.section_id {
                    json!({"id":entry.entity_id,"section_id":section})
                } else {
                    json!({"id": entry.entity_id, "project_id": project_id})
                },
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

    fn ensure_order_metadata(&self, store: &Store) -> Result<()> {
        if store.ordering_needs_pull()? {
            self.pull(store)?;
        }
        Ok(())
    }

    fn ensure_workspace_folders(&self, store: &Store, workspace: &str) -> Result<()> {
        let dirty = store.meta(&format!("folder_dirty:{workspace}"))?.as_deref() == Some("true");
        if !dirty && !store.folders_need_refresh(workspace)? {
            return Ok(());
        }
        let mut folders = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = self
                .auth(self.http.get(format!("{}/folders", self.base)))
                .query(&[("workspace_id", workspace), ("limit", "200")]);
            if let Some(cursor) = &cursor {
                request = request.query(&[("cursor", cursor)]);
            }
            let page: Page<Value> = Self::check(request.send()?)?.json()?;
            folders.extend(page.results);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        store.replace_workspace_folders(workspace, &folders)?;
        store.set_meta(&format!("folder_dirty:{workspace}"), "false")
    }

    fn push_order_plans(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        plans: &[crate::ordering::OrderPlan],
        checkpoint: &mut Value,
    ) -> Result<()> {
        if checkpoint.get("keys_writes").is_none() {
            self.ensure_order_metadata(store)?;
            for workspace in store.order_workspaces(plans)? {
                self.ensure_workspace_folders(store, &workspace)?;
            }
            store.set_meta("ordering_dirty", "true")?;
            checkpoint["keys_writes"] = serde_json::to_value(store.prepare_key_writes(plans)?)?;
            store.save_checkpoint(entry.seq, checkpoint)?;
        }
        self.send_key_writes(store, entry, checkpoint, "keys")?;
        for workspace in store.order_workspaces(plans)? {
            if store.meta(&format!("folder_dirty:{workspace}"))?.as_deref() == Some("true") {
                self.ensure_workspace_folders(store, &workspace)?;
            }
        }
        store.finish_order_outbox(entry, plans)
    }

    fn send_key_writes(
        &self,
        store: &Store,
        entry: &OutboxEntry,
        checkpoint: &mut Value,
        prefix: &str,
    ) -> Result<()> {
        let writes: Vec<crate::ordering::KeyWrite> =
            serde_json::from_value(checkpoint[format!("{prefix}_writes")].clone())?;
        let done_field = format!("{prefix}_done");
        let mut done: Vec<bool> = checkpoint
            .get(&done_field)
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_else(|| vec![false; writes.len()]);
        anyhow::ensure!(
            done.len() == writes.len(),
            "Ordering checkpoint has an inconsistent command count"
        );
        let pending = (0..writes.len())
            .filter(|index| !done[*index])
            .collect::<Vec<_>>();
        for indices in pending.chunks(100) {
            let commands=indices.iter().map(|index|{
                let write=&writes[*index];json!({"type":write.command,"uuid":format!("{}-{prefix}-{index}",entry.uuid),"args":{"id":write.id,&write.field:write.key}})
            }).collect::<Vec<_>>();
            store.set_meta("ordering_dirty", "true")?;
            for write in &writes {
                if write.command == "folder_update"
                    && let Some(workspace) = store.folder_workspace(&write.id)?
                {
                    store.set_meta(&format!("folder_dirty:{workspace}"), "true")?;
                }
            }
            let response = Self::check(
                self.auth(self.http.post(format!("{}/sync", self.base)))
                    .form(&[("commands", serde_json::to_string(&commands)?)])
                    .send()?,
            )?;
            let value: Value = response
                .json()
                .context("decode ordering command response")?;
            let statuses = value
                .get("sync_status")
                .context("Todoist ordering response omitted statuses")?;
            let mut failure = None;
            for index in indices {
                let uuid = format!("{}-{prefix}-{index}", entry.uuid);
                match statuses.get(&uuid) {
                    Some(status) if status == "ok" => {
                        done[*index] = true;
                        store.confirm_order_key(&writes[*index])?;
                    }
                    Some(status)
                        if status.get("http_code") == Some(&json!(404))
                            || status.to_string().contains("ITEM_NOT_FOUND") =>
                    {
                        done[*index] = true;
                        store.mark_order_resource_missing(&writes[*index])?;
                    }
                    Some(status) => {
                        if failure.is_none() {
                            failure = Some(command_failure(status));
                        }
                    }
                    None => {
                        if failure.is_none() {
                            failure = Some(anyhow::anyhow!(
                                "Todoist ordering response omitted a command status"
                            ));
                        }
                    }
                }
            }
            checkpoint[&done_field] = serde_json::to_value(&done)?;
            store.save_checkpoint(entry.seq, checkpoint)?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        Ok(())
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
                serde_json::from_value(value.clone()).context("decode sync response")?;
            store.merge_order_metadata(&value, sync.full_sync)?;
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
        store.finish_order_pull()?;
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
            let page: Page<Value> = Self::check(request.send()?)?.json()?;
            for item in page.results {
                store.record_completed_order(&item)?;
                all.push(serde_json::from_value(item)?);
            }
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
            let page: Page<Value> = Self::check(request.send()?)?.json()?;
            for item in page.results {
                store.record_created_order(&item)?;
                all.push(serde_json::from_value(item)?);
            }
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
    let unsupported_workspace_move = status
        .pointer("/error_extra/explanation")
        .and_then(Value::as_str)
        .is_some_and(|explanation| {
            explanation.eq_ignore_ascii_case("Moving from a workspace is not implemented.")
        });
    ApiFailure {
        status: status
            .get("http_code")
            .and_then(Value::as_u64)
            .unwrap_or(400) as u16,
        retry_after: status
            .pointer("/error_extra/retry_after")
            .and_then(Value::as_u64)
            .map(Duration::from_secs),
        message: if unsupported_workspace_move {
            format!("Todoist does not support moving individual tasks out of a team workspace. Undo the rejected move, or use v to return the task to a project in its original team. Original rejection: {status}")
        } else {format!("Todoist rejected operation: {status}")},
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
#[path = "todoist_live_tests.rs"]
mod live_tests;

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
                let result = worker.join();
                if !thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }

    fn ordering_fixture(value: Value) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        store.merge_order_metadata(&value, false).unwrap();
        store
            .merge_remote_snapshot(
                &serde_json::from_value::<Vec<RemoteProject>>(
                    value.get("projects").cloned().unwrap_or(json!([])),
                )
                .unwrap(),
                &serde_json::from_value::<Vec<RemoteTask>>(
                    value.get("items").cloned().unwrap_or(json!([])),
                )
                .unwrap(),
                &[],
                &[],
                false,
                Some("cached-token"),
            )
            .unwrap();
        store.finish_order_pull().unwrap();
        (dir, store)
    }

    fn ordering_tasks() -> Value {
        json!({"projects":[{"id":"p","name":"Project"}],"items":[
            {"id":"a","project_id":"p","content":"A","order_key":"a0"},
            {"id":"b","project_id":"p","content":"B","order_key":"a1"},
            {"id":"c","project_id":"p","content":"C","order_key":"a2"},
            {"id":"d","project_id":"p","content":"D","order_key":"a3"}
        ]})
    }

    fn request_commands(request: &str) -> Vec<Value> {
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let url = reqwest::Url::parse(&format!("http://mock/?{body}")).unwrap();
        let commands = url
            .query_pairs()
            .find(|(key, _)| key == "commands")
            .unwrap()
            .1;
        serde_json::from_str(&commands).unwrap()
    }

    #[test]
    fn key_batches_resume_only_failed_commands_with_identical_arguments_after_restart() {
        use crate::{
            model::TaskOrder,
            ordering::{OrderPlan, OrderScope},
        };
        let api = MockApi::new();
        let (dir, store) = ordering_fixture(ordering_tasks());
        // Reverse a whole sibling scope: three key changes are needed.
        let operation = Operation::OrderResources {
            plans: vec![OrderPlan {
                scope: OrderScope::Tasks {
                    project_id: "p".into(),
                    parent_id: None,
                    section_id: None,
                },
                positions: ["d", "c", "b", "a"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| TaskOrder {
                        id: id.into(),
                        child_order: index as i64,
                    })
                    .collect(),
                full: true,
                resolve_scopes: false,
            }],
        };
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
        conn.execute(
            "INSERT INTO outbox(uuid,entity_id,payload) VALUES('batch','a',?1)",
            [serde_json::to_string(&operation).unwrap()],
        )
        .unwrap();
        drop(conn);
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(200,json!({"sync_status":{
            "batch-keys-0":"ok","batch-keys-1":{"http_code":500,"error":"temporary"},"batch-keys-2":"ok"
        }}));
        assert!(api.client.push(&store, &entry).is_err());
        let original = request_commands(&api.request());
        assert_eq!(original.len(), 3);
        assert!(
            original
                .iter()
                .all(|command| command["type"] == "item_update"
                    && command["args"].get("order_key").is_some())
        );
        drop(store);
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let resumed = store.claim_next_outbox().unwrap().unwrap();
        assert_eq!(resumed.checkpoint["keys_done"], json!([true, false, true]));
        // Even if the remote keys changed after a partial success, a possibly
        // executed command must retain its checkpointed UUID and arguments.
        let mut remote = ordering_tasks();
        remote["items"][0]["order_key"] = json!("a9");
        store.merge_order_metadata(&remote, false).unwrap();
        api.command("batch-keys-1");
        api.client.push(&store, &resumed).unwrap();
        assert_eq!(request_commands(&api.request()), vec![original[1].clone()]);
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn legacy_queues_translate_only_unsent_or_confirmed_rejected_commands() {
        for (claimed, rejected) in [(false, false), (true, false), (true, true)] {
            let api = MockApi::new();
            let (dir, store) = ordering_fixture(ordering_tasks());
            let operation = Operation::ReorderTasks {
                tasks: vec![
                    crate::model::TaskOrder {
                        id: "b".into(),
                        child_order: 0,
                    },
                    crate::model::TaskOrder {
                        id: "a".into(),
                        child_order: 1,
                    },
                ],
            };
            let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
            conn.execute(
                "INSERT INTO outbox(uuid,entity_id,payload,claimed) VALUES('legacy','a',?1,?2)",
                rusqlite::params![serde_json::to_string(&operation).unwrap(), claimed],
            )
            .unwrap();
            drop(conn);
            let entry = store.claim_next_outbox().unwrap().unwrap();
            if claimed {
                if rejected {
                    api.respond(200,json!({"sync_status":{"legacy":{"http_code":400,"error":"deprecated command"}}}));
                } else {
                    api.command("legacy");
                }
            }
            if !claimed || rejected {
                api.command("legacy-keys-0");
            }
            api.client.push(&store, &entry).unwrap();
            if claimed {
                assert_eq!(
                    request_commands(&api.request()),
                    vec![
                        json!({"type":"item_reorder","uuid":"legacy","args":{"items":[{"id":"b","child_order":0},{"id":"a","child_order":1}]}})
                    ]
                );
            }
            if !claimed || rejected {
                let commands = request_commands(&api.request());
                assert_eq!(commands.len(), 1);
                assert_eq!(commands[0]["type"], "item_update");
                assert_eq!(commands[0]["uuid"], "legacy-keys-0");
                assert_eq!(commands[0]["args"]["id"], "a");
                assert!(commands[0]["args"]["order_key"].as_str().unwrap() > "a1");
            }
            assert_eq!(store.outbox_len().unwrap(), 0);
            assert!(api.requests.try_recv().is_err());
        }
    }

    #[test]
    fn workspace_reorder_paginates_folder_anchors_and_uses_default_keys() {
        let api = MockApi::new();
        let (_dir, store) = ordering_fixture(json!({"projects":[
            {"id":"a","name":"A","workspace_id":"w","child_order":0,"default_order_key":"a0"},
            {"id":"b","name":"B","workspace_id":"w","child_order":2,"default_order_key":"a2"}
        ]}));
        store.move_project("a", true).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(200, json!({"results":[],"next_cursor":"page-two"}));
        api.respond(
            200,
            json!({"results":[{"id":"f","workspace_id":"w","default_order":1}],"next_cursor":null}),
        );
        api.respond(200,json!({"sync_status":{
            format!("{}-keys-0",entry.uuid):"ok",format!("{}-keys-1",entry.uuid):"ok",format!("{}-keys-2",entry.uuid):"ok"
        }}));
        api.respond(200,json!({"results":[{"id":"f","workspace_id":"w","default_order_key":"a1"}],"next_cursor":null}));
        api.client.push(&store, &entry).unwrap();
        let first = api.request();
        assert!(first.starts_with("GET /folders?") && first.contains("workspace_id=w"));
        assert!(api.request().contains("cursor=page-two"));
        let commands = request_commands(&api.request());
        assert_eq!(commands.len(), 3);
        assert!(
            commands
                .iter()
                .all(|command| command["args"].get("default_order_key").is_some()
                    && command["args"].get("order_key").is_none())
        );
        assert!(
            commands
                .iter()
                .any(|command| command["type"] == "folder_update" && command["args"]["id"] == "f")
        );
        assert!(
            commands
                .iter()
                .filter(|command| command["type"] == "project_update")
                .count()
                == 2
        );
        assert!(api.request().starts_with("GET /folders?"));
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn a_pull_reconciles_server_adjusted_keys_and_missing_reorder_targets() {
        let api = MockApi::new();
        let (dir, store) = ordering_fixture(ordering_tasks());
        store.reorder_task("a", "b").unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.command(&format!("{}-keys-0", entry.uuid));
        api.client.push(&store, &entry).unwrap();
        let commands = request_commands(&api.request());
        let requested = commands[0]["args"]["order_key"].as_str().unwrap();
        assert!(requested > "a1" && requested < "a2");
        let mut refreshed = ordering_tasks();
        refreshed["sync_token"] = json!("after-collision");
        refreshed["items"][0]["order_key"] = json!("a1l");
        api.respond(200, refreshed);
        api.client.pull(&store).unwrap();
        assert!(api.request().contains("resource_types="));
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT key FROM order_meta WHERE resource='task' AND id='a'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "a1l"
        );
        store.reorder_task("a", "c").unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(200,json!({"sync_status":{format!("{}-keys-0",entry.uuid):{"http_code":404,"error":"ITEM_NOT_FOUND"}}}));
        api.client.push(&store, &entry).unwrap();
        assert_eq!(request_commands(&api.request())[0]["args"]["id"], "a");
        assert!(store.task("a").unwrap().is_none());
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn personal_project_reordering_uses_project_update_order_key() {
        let api = MockApi::new();
        let (_dir, store) = ordering_fixture(json!({"projects":[
            {"id":"a","name":"A","order_key":"a0"},
            {"id":"b","name":"B","order_key":"a1"}
        ]}));
        store.move_project("a", true).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.command(&format!("{}-keys-0", entry.uuid));
        api.client.push(&store, &entry).unwrap();
        let commands = request_commands(&api.request());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["type"], "project_update");
        assert_eq!(commands[0]["args"]["id"], "a");
        assert!(commands[0]["args"]["order_key"].as_str().unwrap() > "a1");
        assert!(commands[0]["args"].get("default_order_key").is_none());
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn personal_to_team_task_moves_retry_durably_and_keep_later_edits_and_subtasks() {
        let (source, destination) = ("personal", "team");
        let api = MockApi::new();
        let mut data = json!({"projects":[
            {"id":"personal","name":"Personal","order_key":"a0"},
            {"id":"team","name":"Team","workspace_id":"workspace","default_order_key":"a0"}
        ],"items":[
            {"id":"root","project_id":source,"section_id":"source-section","content":"Root","order_key":"a0"},
            {"id":"child","project_id":source,"section_id":"source-section","parent_id":"root","content":"Child","order_key":"a0"},
            {"id":"grandchild","project_id":source,"section_id":"source-section","parent_id":"child","content":"Grandchild","order_key":"a0"},
            {"id":"neighbor","project_id":destination,"content":"Neighbor","order_key":"a0"}
        ]});
        let (dir, store) = ordering_fixture(data.clone());
        store.move_task("root", destination).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        assert!(matches!(entry.operation, Operation::MoveTask { .. }));
        api.respond(500, json!({"error":"temporary transport failure"}));
        assert!(api.client.push(&store, &entry).is_err());
        let first = request_commands(&api.request());
        assert_eq!(
            first,
            vec![
                json!({"type":"item_move","uuid":entry.uuid,"args":{"id":"root","project_id":destination}})
            ]
        );
        store.update_labels("root", &["local".into()]).unwrap();
        drop(store);
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        let resumed = store.claim_next_outbox().unwrap().unwrap();
        assert_eq!(resumed.uuid, entry.uuid);
        api.command(&resumed.uuid);
        api.client.push(&store, &resumed).unwrap();
        assert_eq!(request_commands(&api.request()), first);
        for task in data["items"].as_array_mut().unwrap().iter_mut().take(3) {
            task["project_id"] = json!(destination);
            task["section_id"] = Value::Null;
        }
        data["items"][0]["order_key"] = json!("a1");
        data["sync_token"] = json!("after-cross-workspace-move");
        api.respond(200, data);
        let order = store.claim_next_outbox().unwrap().unwrap();
        assert!(matches!(order.operation, Operation::OrderResources { .. }));
        api.client.push(&store, &order).unwrap();
        let pull = api.request();
        assert!(pull.contains("sync_token=") && !pull.contains("commands="));
        for id in ["root", "child", "grandchild"] {
            let task = store.task(id).unwrap().unwrap();
            assert_eq!(task.project_id, destination);
            assert!(task.section_id.is_none());
        }
        assert_eq!(
            store.task("child").unwrap().unwrap().parent_id.as_deref(),
            Some("root")
        );
        assert_eq!(
            store
                .task("grandchild")
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some("child")
        );
        assert_eq!(store.task("root").unwrap().unwrap().labels, ["local"]);
        let labels = store.claim_next_outbox().unwrap().unwrap();
        api.command(&labels.uuid);
        api.client.push(&store, &labels).unwrap();
        assert_eq!(
            request_commands(&api.request())[0]["args"],
            json!({"id":"root","labels":["local"]})
        );
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn actual_workspace_rejection_survives_restart_and_returning_to_the_team_recovers() {
        let api = MockApi::new();
        let data = json!({"projects":[{"id":"team","name":"Team","workspace_id":"workspace","default_order_key":"a0"},{"id":"personal","name":"Personal","order_key":"a0"}],"items":[{"id":"root","project_id":"team","content":"Root","order_key":"a0"},{"id":"child","project_id":"team","parent_id":"root","content":"Child","order_key":"a0"}]});
        let (dir, store) = ordering_fixture(data.clone());
        store.move_task("root", "personal").unwrap();
        let failed = store.claim_next_outbox().unwrap().unwrap();
        let rejection = json!({"error":"Forbidden","error_code":1,"error_extra":{"explanation":"Moving from a workspace is not implemented."},"error_tag":"FORBIDDEN","http_code":403});
        api.respond(200, json!({"sync_status":{&failed.uuid:rejection}}));
        let error = api.client.push(&store, &failed).unwrap_err();
        assert!(permanent_rejection(&error));
        assert!(error.to_string().contains("Undo the rejected move"));
        assert!(
            error
                .to_string()
                .contains("Moving from a workspace is not implemented.")
        );
        store.fail_outbox(failed.seq, &error.to_string()).unwrap();
        assert_eq!(
            request_commands(&api.request())[0]["args"]["project_id"],
            "personal"
        );
        drop(store);
        let store = Store::open(dir.path().join("cache.db")).unwrap();
        assert!(store.outbox_error().unwrap().is_some());
        assert!(store.claim_next_outbox().unwrap().is_none());
        store.move_task("root", "team").unwrap();
        let correction = store.claim_next_outbox().unwrap().unwrap();
        api.command(&correction.uuid);
        api.client.push(&store, &correction).unwrap();
        assert_eq!(
            request_commands(&api.request())[0]["args"],
            json!({"id":"root","project_id":"team"})
        );
        let mut refreshed = data;
        refreshed["sync_token"] = json!("recovered");
        api.respond(200, refreshed);
        let placement = store.claim_next_outbox().unwrap().unwrap();
        api.client.push(&store, &placement).unwrap();
        assert!(api.request().contains("sync_token="));
        assert!(store.outbox_error().unwrap().is_none());
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert_eq!(store.task("root").unwrap().unwrap().project_id, "team");
        assert_eq!(store.task("child").unwrap().unwrap().project_id, "team");
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn claimed_legacy_quick_add_retries_its_original_placement_before_new_key_writes() {
        let api = MockApi::new();
        let (dir, store) = ordering_fixture(ordering_tasks());
        let project = store.project("p").unwrap().unwrap();
        let id = store
            .add_task_at_top_with_description("Legacy task", "", &project)
            .unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
        // Emulate a pre-migration creation whose parser response was saved,
        // but whose integer placement may already have reached Todoist.
        let parsed =
            json!({"id":"created","project_id":"p","content":"Legacy task","order_key":"a9"});
        conn.execute("UPDATE outbox SET uuid='old-add',claimed=1,payload=json_remove(payload,'$.placement'),checkpoint=?1 WHERE entity_id=?2",rusqlite::params![json!({"parsed":parsed}).to_string(),id]).unwrap();
        drop(conn);
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.command("old-add-order");
        api.client.push(&store, &entry).unwrap();
        assert_eq!(
            request_commands(&api.request()),
            vec![json!({
                "type":"item_reorder","uuid":"old-add-order","args":{"items":[{"id":"created","child_order":0}]}
            })]
        );
        assert!(store.ordering_needs_pull().unwrap());
        assert_eq!(store.task(&id).unwrap().unwrap().id, "created");
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert!(api.requests.try_recv().is_err());
    }

    #[test]
    fn quick_add_resumes_from_durable_checkpoint_without_recreating_or_reordering() {
        let api = MockApi::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let store = Store::open(&path).unwrap();
        let neighbor: RemoteTask = serde_json::from_value(
            json!({"id":"neighbor","project_id":"inbox","content":"Existing"}),
        )
        .unwrap();
        store.merge_remote(&[], &[neighbor], &[], &[]).unwrap();
        store
            .record_created_order(&json!({"id":"neighbor","project_id":"inbox","order_key":"a0"}))
            .unwrap();
        store.finish_order_pull().unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        let id = store
            .add_task_at_top_with_description("Buy milk", "Description", &inbox)
            .unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(
            200,
            json!({"id":"remote-task", "project_id":"inbox", "content":"Buy milk","order_key":"a1"}),
        );
        api.command(&format!("{}-order-keys-0", entry.uuid));
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
        let ordered = api.request();
        assert!(ordered.contains("item_update") && ordered.contains("order_key"));
        assert!(!ordered.contains("item_reorder"));
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
        store.finish_order_pull().unwrap();
        let inbox = store.project("inbox").unwrap().unwrap();
        store.add_task("Captured", &inbox).unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        api.respond(500, json!({"error":"response lost after server commit"}));
        assert!(api.client.push(&store, &entry).is_err());
        drop(store);
        let store = Store::open(path).unwrap();
        api.respond(
            200,
            json!({"id":"only-task","project_id":"inbox","content":"Captured","order_key":"a0"}),
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
        let mut anchored = due.clone();
        anchored.preview_anchor = Some(crate::model::RecurrenceAnchor {
            expression: "every 3 months".into(),
            origin: chrono::NaiveDate::from_ymd_opt(2026, 1, 10).unwrap(),
            expected: chrono::NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
        });
        assert!(
            serde_json::to_value(&anchored)
                .unwrap()
                .get("preview_anchor")
                .is_some()
        );
        assert_eq!(exact_due_value(Some(&anchored)), value);
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
            json!({"id":"created","project_id":"inbox","content":"Task","order_key":"a0"}),
        );
        api.respond(200, json!({"sync_status":{format!("{}-fallback", entry.uuid):{"http_code":400,"error":"Project no longer exists"}}}));
        let error = api.client.push(&store, &entry).unwrap_err();
        assert_eq!(store.task(&id).unwrap().unwrap().id, "created");
        store.fail_outbox(entry.seq, &error.to_string()).unwrap();
        store.move_task(&id, "destination").unwrap();
        api.respond(200,json!({"sync_token":"current","items":[{"id":"created","project_id":"inbox","content":"Task","order_key":"a0"}]}));
        let resumed = store.claim_next_outbox().unwrap().unwrap();
        api.client.push(&store, &resumed).unwrap();
        let moved = store.claim_next_outbox().unwrap().unwrap();
        api.command(&moved.uuid);
        api.client.push(&store, &moved).unwrap();
        api.respond(200,json!({"sync_token":"after-move","items":[{"id":"created","project_id":"destination","content":"Task","order_key":"a0"}]}));
        let ordered = store.claim_next_outbox().unwrap().unwrap();
        api.client.push(&store, &ordered).unwrap();
        assert_eq!(store.outbox_len().unwrap(), 0);
        assert_eq!(store.task(&id).unwrap().unwrap().project_id, "destination");
        assert!(api.request().starts_with("POST /tasks/quick "));
        assert!(api.request().contains("source"));
        assert!(api.request().contains("sync_token"));
        assert!(api.request().contains("destination"));
        assert!(api.request().contains("sync_token"));
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
            section_id: None,
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
