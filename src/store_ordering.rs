//! Server ordering metadata and durable local ordering intents.
use super::*;
use crate::ordering::{self, KeyWrite, OrderPlan, OrderScope, Placement};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone)]
struct Node {
    resource: String,
    id: String,
    scope: OrderScope,
    key: Option<String>,
    legacy: i64,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Fields {
    #[serde(deserialize_with = "crate::model::string_id")]
    id: String,
    #[serde(deserialize_with = "crate::model::optional_string_id")]
    project_id: Option<String>,
    #[serde(deserialize_with = "crate::model::optional_string_id")]
    parent_id: Option<String>,
    #[serde(deserialize_with = "crate::model::optional_string_id")]
    section_id: Option<String>,
    #[serde(deserialize_with = "crate::model::optional_string_id")]
    workspace_id: Option<String>,
    #[serde(deserialize_with = "crate::model::optional_string_id")]
    folder_id: Option<String>,
    order_key: Option<String>,
    default_order_key: Option<String>,
    child_order: i64,
    item_order: i64,
    section_order: i64,
    default_order: Option<i64>,
    checked: bool,
    is_completed: bool,
    is_deleted: bool,
    is_archived: bool,
    #[serde(alias = "inbox_project")]
    is_inbox_project: bool,
}

pub(super) fn initialize(conn: &Connection, previous: i64) -> Result<()> {
    ensure_column(conn, "tasks", "section_id", "TEXT")?;
    ensure_column(conn, "projects", "folder_id", "TEXT")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS order_meta (
        resource TEXT NOT NULL,id TEXT NOT NULL,scope TEXT NOT NULL,domain TEXT NOT NULL,
        key TEXT,legacy INTEGER NOT NULL DEFAULT 0,active INTEGER NOT NULL DEFAULT 1,
        rank INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(resource,id));
        CREATE INDEX IF NOT EXISTS order_meta_scope ON order_meta(domain,active);",
    )?;
    if previous < 10 {
        for project in projects_from(conn)? {
            let scope = project_scope(&project);
            if !has_metadata(conn, "project", &project.id)? {
                record(
                    conn,
                    "project",
                    &project.id,
                    &scope,
                    None,
                    project.child_order,
                    !project.is_inbox,
                )?;
            }
        }
        let tasks = conn
            .prepare(
                "SELECT id,project_id,parent_id,section_id,child_order,checked,deleted FROM tasks",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, project_id, parent_id, section_id, legacy, checked, deleted) in tasks {
            if !has_metadata(conn, "task", &id)? {
                record(
                    conn,
                    "task",
                    &id,
                    &OrderScope::Tasks {
                        project_id,
                        parent_id,
                        section_id,
                    },
                    None,
                    legacy,
                    !checked && !deleted,
                )?;
            }
        }
        // Normalize local ordinals without changing the last-known legacy server
        // values. Old outbox payloads and their UUIDs remain untouched.
        project_ordinals_from_cache(conn)?;
        conn.execute("INSERT INTO meta(key,value) VALUES('ordering_backfill','true') ON CONFLICT(key) DO UPDATE SET value=excluded.value", [])?;
        conn.execute("UPDATE meta SET value='*' WHERE key='sync_token'", [])?;
    }
    Ok(())
}

fn has_metadata(conn: &Connection, resource: &str, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM order_meta WHERE resource=?1 AND id=?2)",
        params![resource, id],
        |row| row.get(0),
    )?)
}

pub(super) fn sort_tasks(conn: &Connection, tasks: &mut [Task]) -> Result<()> {
    let ranks = conn
        .prepare("SELECT id,rank FROM order_meta WHERE resource='section' AND active=1")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    tasks.sort_by_cached_key(|task| {
        (
            task.checked,
            task.section_id
                .as_ref()
                .and_then(|id| ranks.get(id))
                .copied()
                .unwrap_or(-1),
            task.child_order,
            task.id.clone(),
        )
    });
    Ok(())
}

fn project_ordinals_from_cache(conn: &Connection) -> Result<()> {
    let mut groups: HashMap<String, Vec<Node>> = HashMap::new();
    let nodes = all_nodes(conn)?;
    for node in nodes {
        groups
            .entry(serde_json::to_string(&node.scope.allocation_scope())?)
            .or_default()
            .push(node);
    }
    for (_, mut nodes) in groups {
        nodes.sort_by(|a, b| a.legacy.cmp(&b.legacy).then(a.id.cmp(&b.id)));
        for (rank, node) in nodes.iter().enumerate() {
            let table = if node.resource == "task" {
                "tasks"
            } else {
                "projects"
            };
            conn.execute(
                &format!("UPDATE {table} SET child_order=?2 WHERE id=?1"),
                params![node.id, rank as i64],
            )?;
        }
    }
    Ok(())
}

fn record(
    conn: &Connection,
    resource: &str,
    id: &str,
    scope: &OrderScope,
    key: Option<&str>,
    legacy: i64,
    active: bool,
) -> Result<()> {
    conn.execute("INSERT INTO order_meta(resource,id,scope,domain,key,legacy,active) VALUES(?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(resource,id) DO UPDATE SET scope=excluded.scope,domain=excluded.domain,key=excluded.key,legacy=excluded.legacy,active=excluded.active",
        params![resource,id,serde_json::to_string(scope)?,serde_json::to_string(&scope.allocation_scope())?,key,legacy,active])?;
    Ok(())
}

fn row_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let scope: String = row.get(2)?;
    Ok(Node {
        resource: row.get(0)?,
        id: row.get(1)?,
        scope: serde_json::from_str(&scope).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        key: row.get(3)?,
        legacy: row.get(4)?,
    })
}

fn all_nodes(conn: &Connection) -> Result<Vec<Node>> {
    Ok(conn
        .prepare(
            "SELECT resource,id,scope,key,legacy FROM order_meta WHERE active=1 ORDER BY legacy,id",
        )?
        .query_map([], row_node)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn task_scope(task: &Task) -> OrderScope {
    OrderScope::Tasks {
        project_id: task.project_id.clone(),
        parent_id: task.parent_id.clone(),
        section_id: task.section_id.clone(),
    }
}
pub(super) fn project_scope(project: &Project) -> OrderScope {
    OrderScope::Projects {
        parent_id: project.parent_id.clone(),
        workspace_id: project.workspace_id.clone(),
        folder_id: project.folder_id.clone(),
    }
}

pub(super) fn remap(conn: &Connection, old: &str, new: &str, project: bool) -> Result<()> {
    conn.execute(
        "UPDATE OR REPLACE order_meta SET id=?3 WHERE resource=?1 AND id=?2",
        params![if project { "project" } else { "task" }, old, new],
    )?;
    let nodes = conn
        .prepare("SELECT resource,id,scope,key,legacy FROM order_meta")?
        .query_map([], row_node)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for mut node in nodes {
        match &mut node.scope {
            OrderScope::Tasks { project_id, .. } | OrderScope::Sections { project_id }
                if project && project_id == old =>
            {
                *project_id = new.into()
            }
            OrderScope::Projects { parent_id, .. }
                if project && parent_id.as_deref() == Some(old) =>
            {
                *parent_id = Some(new.into())
            }
            OrderScope::Tasks { parent_id, .. }
                if !project && parent_id.as_deref() == Some(old) =>
            {
                *parent_id = Some(new.into())
            }
            _ => continue,
        }
        conn.execute(
            "UPDATE order_meta SET scope=?3,domain=?4 WHERE resource=?1 AND id=?2",
            params![
                node.resource,
                node.id,
                serde_json::to_string(&node.scope)?,
                serde_json::to_string(&node.scope.allocation_scope())?
            ],
        )?;
    }
    Ok(())
}

/// Keyed nodes keep server order; unkeyed runs keep their legacy predecessor.
/// This is a total ordering even when the two representations disagree.
fn ordered(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by(|a, b| a.legacy.cmp(&b.legacy).then(a.id.cmp(&b.id)));
    let mut before = Vec::new();
    let mut after: HashMap<String, Vec<Node>> = HashMap::new();
    let mut keyed = Vec::new();
    let mut anchor = None;
    for node in nodes {
        if node.key.is_some() {
            anchor = Some(node.id.clone());
            keyed.push(node);
        } else if let Some(id) = &anchor {
            after.entry(id.clone()).or_default().push(node);
        } else {
            before.push(node);
        }
    }
    keyed.sort_by(|a, b| a.key.cmp(&b.key).then(a.id.cmp(&b.id)));
    for node in keyed {
        let id = node.id.clone();
        before.push(node);
        before.extend(after.remove(&id).unwrap_or_default());
    }
    before
}

pub(super) fn reconcile(conn: &Connection) -> Result<()> {
    let blocked = protected_ordinals(conn)?;
    let protected = pending_scope_changes(conn)?;
    let mut groups: HashMap<OrderScope, Vec<Node>> = HashMap::new();
    for node in all_nodes(conn)? {
        groups
            .entry(node.scope.allocation_scope())
            .or_default()
            .push(node);
    }
    for (_, nodes) in groups {
        let pending = nodes
            .iter()
            .any(|node| blocked.contains(&node.id) || protected.contains(&node.id));
        for (rank, node) in ordered(nodes).iter().enumerate() {
            conn.execute(
                "UPDATE order_meta SET rank=?3 WHERE resource=?1 AND id=?2",
                params![node.resource, node.id, rank as i64],
            )?;
            if pending {
                continue;
            }
            let (table, column) = match node.resource.as_str() {
                "task" => ("tasks", "child_order"),
                "project" => ("projects", "child_order"),
                "filter" => ("filters", "item_order"),
                _ => continue,
            };
            // Non-order mutations may still protect an optimistic ordinal/section.
            if !protected.contains(&node.id) {
                conn.execute(
                    &format!("UPDATE {table} SET {column}=?2 WHERE id=?1"),
                    params![node.id, rank as i64],
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn local_position(
    conn: &Connection,
    id: Option<&str>,
    scope: &OrderScope,
    index: i64,
) -> Result<()> {
    let OrderScope::Tasks {
        project_id,
        parent_id,
        section_id,
    } = scope
    else {
        return Ok(());
    };
    let mut ids=conn.prepare("SELECT id FROM tasks WHERE project_id=?1 AND parent_id IS ?2 AND section_id IS ?3 AND checked=0 AND deleted=0 ORDER BY child_order,id")?
        .query_map(params![project_id,parent_id,section_id],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(id) = id {
        ids.retain(|other| other != id);
        ids.insert((index.max(0) as usize).min(ids.len()), id.into());
    }
    for (rank, id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE tasks SET child_order=?2 WHERE id=?1",
            params![id, rank as i64],
        )?;
    }
    Ok(())
}

pub(super) fn protected_ordinals(conn: &Connection) -> Result<HashSet<String>> {
    let payloads = conn
        .prepare("SELECT entity_id,payload FROM outbox")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut ids = HashSet::new();
    let mut domains = HashSet::new();
    for (id, payload) in payloads {
        match serde_json::from_str::<Operation>(&payload)? {
            Operation::OrderResources { plans } => {
                for plan in plans {
                    domains.insert(plan.scope.allocation_scope());
                    ids.extend(plan.positions.into_iter().map(|position| position.id));
                }
            }
            Operation::ReorderTasks { tasks } => {
                ids.extend(tasks.into_iter().map(|position| position.id))
            }
            Operation::ReorderProjects { projects } => {
                ids.extend(projects.into_iter().map(|position| position.id))
            }
            Operation::QuickAdd { .. }
            | Operation::MoveTask { .. }
            | Operation::MoveTaskParent { .. }
            | Operation::AddProject { .. } => {
                ids.insert(id);
            }
            _ => {}
        }
    }
    let nodes = all_nodes(conn)?;
    for node in &nodes {
        if ids.contains(&node.id) {
            domains.insert(node.scope.allocation_scope());
        }
    }
    for id in ids.clone() {
        if let Some(task) = task_from_including_deleted(conn, &id)? {
            domains.insert(task_scope(&task).allocation_scope());
        }
        if let Some(project) = project_from(conn, &id)? {
            domains.insert(project_scope(&project).allocation_scope());
        }
    }
    for node in nodes {
        if domains.contains(&node.scope.allocation_scope()) {
            ids.insert(node.id);
        }
    }
    Ok(ids)
}

fn pending_scope_changes(conn: &Connection) -> Result<HashSet<String>> {
    Ok(conn.prepare("SELECT entity_id FROM outbox WHERE json_extract(payload,'$.kind') IN ('quick_add','move_task','move_task_parent')")?
        .query_map([],|row|row.get(0))?.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn hydrate(conn: &Connection) -> Result<()> {
    let protected = pending_scope_changes(conn)?;
    let nodes = conn
        .prepare("SELECT resource,id,scope,key,legacy FROM order_meta")?
        .query_map([], row_node)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for node in nodes {
        match node.scope {
            OrderScope::Tasks {
                project_id,
                parent_id,
                section_id,
            } if !protected.contains(&node.id) => {
                conn.execute("UPDATE tasks SET section_id=?2 WHERE id=?1 AND project_id=?3 AND parent_id IS ?4",params![node.id,section_id,project_id,parent_id])?;
            }
            OrderScope::Projects { folder_id, .. } if node.resource == "project" => {
                conn.execute(
                    "UPDATE projects SET folder_id=?2 WHERE id=?1",
                    params![node.id, folder_id],
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn queue_plan(
    tx: &Transaction<'_>,
    id: &str,
    scope: OrderScope,
    positions: Vec<TaskOrder>,
    full: bool,
) -> Result<()> {
    let unresolved = meta_from(tx, "ordering_backfill")?.as_deref() == Some("true");
    let plan = OrderPlan {
        scope,
        positions,
        full,
        resolve_scopes: unresolved,
    };
    // Coalesce only full, never-sent plans of the same scope. Claimed operations
    // have checkpointed commands and must remain immutable.
    if full {
        let entries = tx
            .prepare("SELECT seq,payload FROM outbox WHERE claimed=0")?
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (seq, payload) in entries {
            if matches!(serde_json::from_str::<Operation>(&payload)?, Operation::OrderResources { plans } if plans.len()==1 && plans[0].full && plans[0].scope == plan.scope)
            {
                tx.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
            }
        }
    }
    queue(tx, id, Operation::OrderResources { plans: vec![plan] })
}

impl Store {
    pub fn merge_order_metadata(&self, value: &Value, full: bool) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        for (field, resource) in [
            ("projects", "project"),
            ("items", "task"),
            ("filters", "filter"),
            ("workspace_filters", "filter"),
            ("sections", "section"),
        ] {
            if full && field != "workspace_filters" {
                tx.execute(
                    "UPDATE order_meta SET active=0 WHERE resource=?1 AND resource<>'folder'",
                    [resource],
                )?;
            }
            if let Some(items) = value.get(field).and_then(Value::as_array) {
                for item in items {
                    merge_fields(&tx, resource, item)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_created_order(&self, value: &Value) -> Result<()> {
        let conn = self.connect()?;
        merge_fields(&conn, "task", value)
    }

    pub fn ordering_needs_pull(&self) -> Result<bool> {
        Ok(self.meta("ordering_backfill")?.as_deref() == Some("true")
            || self.meta("ordering_dirty")?.as_deref() == Some("true"))
    }

    pub fn finish_order_pull(&self) -> Result<()> {
        self.set_meta("ordering_backfill", "false")?;
        self.set_meta("ordering_dirty", "false")
    }

    pub fn workspace_for_order_scope(scope: &OrderScope) -> Option<&str> {
        match scope {
            OrderScope::Projects {
                workspace_id: Some(id),
                ..
            } => Some(id),
            _ => None,
        }
    }

    pub fn folders_need_refresh(&self, workspace: &str) -> Result<bool> {
        Ok(self
            .meta(&format!("folder_refresh:{workspace}"))?
            .and_then(|value| value.parse::<i64>().ok())
            .is_none_or(|last| chrono::Utc::now().timestamp() - last >= 60))
    }

    pub fn replace_workspace_folders(&self, workspace: &str, folders: &[Value]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let domain = serde_json::to_string(&OrderScope::Projects {
            parent_id: None,
            workspace_id: Some(workspace.into()),
            folder_id: None,
        })?;
        tx.execute(
            "UPDATE order_meta SET active=0 WHERE resource='folder' AND domain=?1",
            [domain],
        )?;
        for folder in folders {
            merge_fields(&tx, "folder", folder)?;
        }
        tx.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("folder_refresh:{workspace}"),chrono::Utc::now().timestamp().to_string()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn legacy_order_plans(
        &self,
        positions: Vec<TaskOrder>,
        projects: bool,
    ) -> Result<Vec<OrderPlan>> {
        let full = projects || positions.len() > 1;
        self.resolve_order_plans(positions, projects, full, true)
    }

    fn resolve_order_plans(
        &self,
        positions: Vec<TaskOrder>,
        projects: bool,
        full: bool,
        legacy_index: bool,
    ) -> Result<Vec<OrderPlan>> {
        let conn = self.connect()?;
        let resource = if projects { "project" } else { "task" };
        let mut groups: HashMap<OrderScope, Vec<TaskOrder>> = HashMap::new();
        for position in positions {
            let node = conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource=?1 AND id=?2", params![resource,position.id],row_node).optional()?;
            if let Some(node) = node {
                groups.entry(node.scope).or_default().push(position);
            }
        }
        let mut plans = Vec::new();
        for (scope, mut positions) in groups {
            positions.sort_by(|a, b| a.child_order.cmp(&b.child_order).then(a.id.cmp(&b.id)));
            if !full && legacy_index {
                let domain = serde_json::to_string(&scope.allocation_scope())?;
                positions[0].child_order = conn.query_row("SELECT COUNT(*) FROM order_meta WHERE resource='task' AND domain=?1 AND active=1 AND legacy<?2 AND id<>?3", params![domain,positions[0].child_order,positions[0].id], |row| row.get(0))?;
            }
            plans.push(OrderPlan {
                scope,
                positions,
                full,
                resolve_scopes: false,
            });
        }
        plans.sort_by_key(|plan| serde_json::to_string(&plan.scope).unwrap_or_default());
        Ok(plans)
    }

    pub fn placement_plan(&self, id: &str, placement: Placement) -> Result<OrderPlan> {
        let conn = self.connect()?;
        let node = conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource='task' AND id=?1", [id], row_node).optional()?.context("new task ordering metadata is unavailable")?;
        let domain = serde_json::to_string(&node.scope.allocation_scope())?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM order_meta WHERE resource='task' AND domain=?1 AND active=1 AND id<>?2", params![domain,id], |row| row.get(0))?;
        let index = match placement { Placement::Top => 0, Placement::Bottom => count, Placement::Index(index) => index,
            Placement::Legacy(value) => conn.query_row("SELECT COUNT(*) FROM order_meta WHERE resource='task' AND domain=?1 AND active=1 AND legacy<?2 AND id<>?3", params![domain,value,id], |row| row.get(0))? };
        Ok(OrderPlan {
            scope: node.scope,
            positions: vec![TaskOrder {
                id: id.into(),
                child_order: index,
            }],
            full: false,
            resolve_scopes: false,
        })
    }

    pub fn prepare_key_writes(&self, plans: &[OrderPlan]) -> Result<Vec<KeyWrite>> {
        let conn = self.connect()?;
        let mut writes = Vec::new();
        for plan in plans {
            let resolved = self.resolve_order_plans(
                plan.positions.clone(),
                matches!(plan.scope, OrderScope::Projects { .. }),
                plan.full,
                false,
            )?;
            if plan.resolve_scopes || resolved.iter().any(|actual| actual.scope != plan.scope) {
                writes.extend(self.prepare_key_writes(&resolved)?);
                continue;
            }
            let domain = serde_json::to_string(&plan.scope.allocation_scope())?;
            let nodes = conn.prepare("SELECT resource,id,scope,key,legacy FROM order_meta WHERE domain=?1 AND active=1 ORDER BY legacy,id")?
                .query_map([domain],row_node)?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut original = ordered(nodes);
            let resource = if matches!(plan.scope, OrderScope::Tasks { .. }) {
                "task"
            } else {
                "project"
            };
            let mut positions = plan.positions.clone();
            positions.sort_by_key(|position| position.child_order);
            let ids = positions
                .iter()
                .map(|position| position.id.as_str())
                .collect::<HashSet<_>>();
            // Include an explicitly restored archived task, but never revive a
            // remotely absent resource merely to satisfy an ordering intent.
            if !plan.full
                && let Some(position) = positions.first()
                && !original
                    .iter()
                    .any(|node| node.resource == resource && node.id == position.id)
                && let Some(node) = conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource=?1 AND id=?2",params![resource,position.id],row_node).optional()?
                && node.scope.allocation_scope()==plan.scope.allocation_scope()
            {
                original.push(node);
            }
            if original.is_empty() {
                continue;
            }
            // Allocate virtual keys for null/duplicate legacy keys without
            // disturbing already keyed nodes. No virtual key is sent until all
            // final commands have been prepared and checkpointed.
            let mut virtual_keys = HashMap::new();
            let mut occupied = BTreeSet::new();
            for node in &original {
                if let Some(key) = &node.key {
                    ordering::between(Some(key), None)?;
                    occupied.insert(key.clone());
                }
            }
            ensure_unique_anchors(&original)?;
            let mut previous: Option<String> = None;
            for (index, node) in original.iter().enumerate() {
                let upper = original[index + 1..]
                    .iter()
                    .filter_map(|node| node.key.as_deref())
                    .find(|key| previous.as_deref().is_none_or(|lower| *key > lower));
                let key = if let Some(key) = node
                    .key
                    .as_deref()
                    .filter(|key| previous.as_deref().is_none_or(|lower| *key > lower))
                {
                    key.to_owned()
                } else {
                    ordering::between(previous.as_deref(), upper)?
                };
                occupied.insert(key.clone());
                virtual_keys.insert((node.resource.clone(), node.id.clone()), key.clone());
                previous = Some(key);
            }
            let mut desired = original.clone();
            if plan.full {
                let subjects = positions
                    .iter()
                    .filter_map(|position| {
                        original.iter().find(|node| {
                            node.resource == resource
                                && node.id == position.id
                                && node.scope == plan.scope
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let mut subjects = subjects.into_iter();
                for slot in &mut desired {
                    if slot.resource == resource
                        && ids.contains(slot.id.as_str())
                        && slot.scope == plan.scope
                        && let Some(node) = subjects.next()
                    {
                        *slot = node;
                    }
                }
            } else if let Some(position) = positions.first()
                && let Some(index) = desired
                    .iter()
                    .position(|node| node.resource == resource && node.id == position.id)
            {
                let node = desired.remove(index);
                let index = (position.child_order.max(0) as usize).min(desired.len());
                desired.insert(index, node);
            }
            let mut previous: Option<String> = None;
            for (index, node) in desired.iter().enumerate() {
                let own = virtual_keys[&(node.resource.clone(), node.id.clone())].clone();
                let movable = node.resource == resource && ids.contains(node.id.as_str());
                let upper = desired[index + 1..]
                    .iter()
                    .find(|next| next.resource != resource || !ids.contains(next.id.as_str()))
                    .map(|next| virtual_keys[&(next.resource.clone(), next.id.clone())].as_str());
                let key = if previous.as_deref().is_none_or(|lower| own.as_str() > lower)
                    && upper.is_none_or(|upper| own.as_str() < upper)
                {
                    own
                } else if movable {
                    let next = occupied
                        .iter()
                        .map(String::as_str)
                        .find(|key| previous.as_deref().is_none_or(|lower| *key > lower));
                    let high = match (next, upper) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    ordering::between(previous.as_deref(), high)?
                } else {
                    bail!("Server ordering anchors are inconsistent; refresh before reordering");
                };
                occupied.insert(key.clone());
                previous = Some(key.clone());
                if (movable || node.key.is_none()) && node.key.as_deref() != Some(key.as_str()) {
                    writes.push(KeyWrite {
                        command: match node.resource.as_str() {
                            "task" => "item_update",
                            "folder" => "folder_update",
                            _ => "project_update",
                        }
                        .into(),
                        id: node.id.clone(),
                        field: if matches!(
                            node.scope,
                            OrderScope::Projects {
                                workspace_id: Some(_),
                                ..
                            }
                        ) {
                            "default_order_key"
                        } else {
                            "order_key"
                        }
                        .into(),
                        key,
                    });
                }
            }
        }
        Ok(writes)
    }

    pub fn confirm_order_key(&self, write: &KeyWrite) -> Result<()> {
        let resource = match write.command.as_str() {
            "item_update" => "task",
            "folder_update" => "folder",
            _ => "project",
        };
        self.connect()?.execute(
            "UPDATE order_meta SET key=?3 WHERE resource=?1 AND id=?2",
            params![resource, write.id, write.key],
        )?;
        Ok(())
    }

    pub fn mark_order_resource_missing(&self, write: &KeyWrite) -> Result<()> {
        self.connect()?.execute(
            "UPDATE order_meta SET active=0 WHERE resource=?1 AND id=?2",
            params![
                if write.command == "item_update" {
                    "task"
                } else if write.command == "folder_update" {
                    "folder"
                } else {
                    "project"
                },
                write.id
            ],
        )?;
        Ok(())
    }

    pub fn finish_order_outbox(&self, entry: &OutboxEntry, plans: &[OrderPlan]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM outbox WHERE seq=?1", [entry.seq])?;
        let pending = pending_entity_ids(&tx)?;
        for plan in plans {
            for position in &plan.positions {
                let (resource, table, protected) = if matches!(plan.scope, OrderScope::Tasks { .. })
                {
                    ("task", "tasks", &pending.0)
                } else {
                    ("project", "projects", &pending.2)
                };
                if protected.contains(&position.id) {
                    continue;
                }
                let inactive:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM order_meta WHERE resource=?1 AND id=?2 AND active=0)",params![resource,position.id],|row|row.get(0))?;
                if inactive {
                    tx.execute(
                        &format!(
                            "UPDATE {table} SET deleted=1{} WHERE id=?1{}",
                            if resource == "task" { ",checked=1" } else { "" },
                            if resource == "task" {
                                " AND checked=0"
                            } else {
                                ""
                            }
                        ),
                        [&position.id],
                    )?;
                }
            }
        }
        reconcile(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn task_order_scope(&self, id: &str) -> Result<OrderScope> {
        let conn = self.connect()?;
        Ok(conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource='task' AND id=?1",[id],row_node)?.scope)
    }

    pub fn task_order_section(&self, id: &str) -> Result<Option<String>> {
        match self.task_order_scope(id)? {
            OrderScope::Tasks { section_id, .. } => Ok(section_id),
            _ => Ok(None),
        }
    }

    pub fn folder_workspace(&self, id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let node=conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource='folder' AND id=?1",[id],row_node).optional()?;
        Ok(node.and_then(|node| match node.scope {
            OrderScope::Projects { workspace_id, .. } => workspace_id,
            _ => None,
        }))
    }

    pub fn record_completed_order(&self, value: &Value) -> Result<()> {
        let fields: Fields = serde_json::from_value(value.clone())?;
        let conn = self.connect()?;
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM order_meta WHERE resource='task' AND id=?1 AND active=1)",
            [&fields.id],
            |row| row.get(0),
        )?;
        if !active {
            let mut value = value.clone();
            value["checked"] = json!(true);
            merge_fields(&conn, "task", &value)?;
        }
        Ok(())
    }

    pub fn order_workspaces(&self, plans: &[OrderPlan]) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut workspaces = BTreeSet::new();
        for plan in plans {
            if let Some(workspace) = Self::workspace_for_order_scope(&plan.scope) {
                workspaces.insert(workspace.to_owned());
            }
            if matches!(plan.scope, OrderScope::Projects { .. }) {
                for position in &plan.positions {
                    if let Some(node)=conn.query_row("SELECT resource,id,scope,key,legacy FROM order_meta WHERE resource='project' AND id=?1",[&position.id],row_node).optional()?
                    && let Some(workspace)=Self::workspace_for_order_scope(&node.scope){workspaces.insert(workspace.to_owned());}
                }
            }
        }
        Ok(workspaces.into_iter().collect())
    }
}

fn ensure_unique_anchors(nodes: &[Node]) -> Result<()> {
    let mut keys = HashSet::new();
    for node in nodes {
        if let Some(key) = &node.key {
            anyhow::ensure!(
                keys.insert(key),
                "Server ordering keys collide; refresh before reordering"
            );
        }
    }
    Ok(())
}

fn merge_fields(conn: &Connection, resource: &str, value: &Value) -> Result<()> {
    let fields: Fields = serde_json::from_value(value.clone())?;
    if fields.id.is_empty() {
        return Ok(());
    }
    // Incremental deletion/archive records can contain only an ID. Retire the
    // existing anchor before trying to decode a scope that is no longer sent.
    if fields.is_deleted || fields.is_archived || fields.checked || fields.is_completed {
        conn.execute(
            "UPDATE order_meta SET active=0 WHERE resource=?1 AND id=?2",
            params![resource, fields.id],
        )?;
        if fields.project_id.is_none() && matches!(resource, "task" | "section") {
            return Ok(());
        }
        if fields.is_deleted || fields.is_archived {
            return Ok(());
        }
    }
    let (scope, key, legacy) = match resource {
        "task" => {
            let Some(project_id) = fields.project_id else {
                return Ok(());
            };
            (
                OrderScope::Tasks {
                    project_id,
                    parent_id: fields.parent_id,
                    section_id: fields.section_id.clone(),
                },
                fields.order_key,
                fields.child_order,
            )
        }
        "project" => (
            OrderScope::Projects {
                parent_id: fields.parent_id,
                workspace_id: fields.workspace_id.clone(),
                folder_id: fields.folder_id.clone(),
            },
            if fields.workspace_id.is_some() {
                fields.default_order_key
            } else {
                fields.order_key
            },
            fields.default_order.unwrap_or(fields.child_order),
        ),
        "folder" => (
            OrderScope::Projects {
                parent_id: None,
                workspace_id: fields.workspace_id,
                folder_id: None,
            },
            fields.default_order_key,
            fields.default_order.unwrap_or(fields.child_order),
        ),
        "section" => {
            let Some(project_id) = fields.project_id else {
                return Ok(());
            };
            (
                OrderScope::Sections { project_id },
                fields.order_key,
                fields.section_order,
            )
        }
        _ => (
            OrderScope::Filters {
                workspace_id: fields.workspace_id,
            },
            fields.order_key,
            fields.item_order,
        ),
    };
    record(
        conn,
        resource,
        &fields.id,
        &scope,
        key.as_deref(),
        legacy,
        !fields.is_deleted
            && !fields.is_archived
            && !fields.checked
            && !fields.is_completed
            && !fields.is_inbox_project,
    )?;
    if resource == "task" {
        let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE entity_id=?1 AND json_extract(payload,'$.kind') IN ('quick_add','quick_edit','move_task','move_task_parent'))",[&fields.id],|row|row.get(0))?;
        if !pending {
            conn.execute(
                "UPDATE tasks SET section_id=?2 WHERE id=?1",
                params![fields.id, fields.section_id],
            )?;
        }
    }
    if resource == "project" {
        conn.execute(
            "UPDATE projects SET folder_id=?2 WHERE id=?1",
            params![fields.id, fields.folder_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge(store: &Store, value: Value) {
        let projects: Vec<RemoteProject> =
            serde_json::from_value(value.get("projects").cloned().unwrap_or(json!([]))).unwrap();
        let tasks: Vec<RemoteTask> =
            serde_json::from_value(value.get("items").cloned().unwrap_or(json!([]))).unwrap();
        let filters: Vec<RemoteFilter> =
            serde_json::from_value(value.get("filters").cloned().unwrap_or(json!([]))).unwrap();
        store.merge_order_metadata(&value, false).unwrap();
        store
            .merge_remote_snapshot(&projects, &tasks, &[], &filters, false, None)
            .unwrap();
    }
    fn fixture(value: Value) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ordering.db")).unwrap();
        merge(&store, value);
        store.finish_order_pull().unwrap();
        (dir, store)
    }
    fn task_ids(store: &Store) -> Vec<String> {
        store
            .tasks_for(&NavId::Project("p".into()))
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect()
    }
    fn task_data() -> Value {
        json!({"projects":[{"id":"p","name":"Project"}],"items":[
            {"id":"a","project_id":"p","content":"A","child_order":30,"order_key":"a0"},
            {"id":"b","project_id":"p","content":"B","child_order":10,"order_key":"a1"}
        ]})
    }

    #[test]
    fn server_keys_control_order_even_when_legacy_integers_disagree_and_survive_restart() {
        let (dir, store) = fixture(task_data());
        assert_eq!(task_ids(&store), ["a", "b"]);
        let reopened = Store::open(dir.path().join("ordering.db")).unwrap();
        assert_eq!(task_ids(&reopened), ["a", "b"]);
        assert!(!reopened.ordering_needs_pull().unwrap());
    }

    #[test]
    fn task_sections_are_ordered_separately_and_cross_section_reorders_are_rejected() {
        let (_dir, store) = fixture(json!({"projects":[{"id":"p","name":"Project"}],
        "sections":[{"id":"first","project_id":"p","order_key":"a0"},{"id":"second","project_id":"p","order_key":"a1"}],
        "items":[
            {"id":"loose","project_id":"p","content":"Loose","order_key":"a9"},
            {"id":"a","project_id":"p","section_id":"first","content":"A","order_key":"a1"},
            {"id":"b","project_id":"p","section_id":"first","content":"B","order_key":"a0"},
            {"id":"c","project_id":"p","section_id":"second","content":"C","order_key":"Zz"}
        ]}));
        assert_eq!(task_ids(&store), ["loose", "b", "a", "c"]);
        assert!(store.reorder_task("b", "c").is_err());
        assert_eq!(store.outbox_len().unwrap(), 0);
        let before = store.task("b").unwrap().unwrap();
        store.set_task_parent("b", Some("a")).unwrap();
        store.set_task_parent("b", None).unwrap();
        assert_eq!(
            store.task("b").unwrap().unwrap().section_id.as_deref(),
            Some("first")
        );
        store.apply_task_snapshot(&before).unwrap();
        assert_eq!(store.task("b").unwrap().unwrap(), before);
    }

    #[test]
    fn pending_order_is_protected_while_independent_remote_fields_are_refreshed() {
        let (_dir, store) = fixture(task_data());
        store.reorder_task("a", "b").unwrap();
        assert_eq!(task_ids(&store), ["b", "a"]);
        let mut changed = task_data();
        changed["items"][0]["content"] = json!("Phone rename");
        merge(&store, changed);
        assert_eq!(task_ids(&store), ["b", "a"]);
        assert_eq!(store.task("a").unwrap().unwrap().content, "Phone rename");
        let entry = store.claim_next_outbox().unwrap().unwrap();
        let Operation::OrderResources { plans } = &entry.operation else {
            panic!()
        };
        let writes = store.prepare_key_writes(plans).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].command, "item_update");
        assert_eq!(writes[0].id, "a");
        assert_eq!(writes[0].field, "order_key");
        assert!(writes[0].key.as_str() > "a1");
        for write in writes {
            store.confirm_order_key(&write).unwrap();
        }
        store.finish_order_outbox(&entry, plans).unwrap();
        assert_eq!(task_ids(&store), ["b", "a"]);
    }

    #[test]
    fn workspace_folder_and_other_workspace_anchors_are_never_reordered_as_personal_projects() {
        let (_dir, store) = fixture(json!({"projects":[
            {"id":"a","name":"A","workspace_id":"w","default_order_key":"a0"},
            {"id":"inside","name":"Inside folder","workspace_id":"w","folder_id":"f","default_order_key":"a1"},
            {"id":"b","name":"B","workspace_id":"w","default_order_key":"a2"},
            {"id":"other","name":"Other workspace","workspace_id":"other-workspace","default_order_key":"a0"}
        ]}));
        store
            .replace_workspace_folders(
                "w",
                &[json!({"id":"f","workspace_id":"w","default_order_key":"a1V"})],
            )
            .unwrap();
        assert!(store.move_project("a", true).unwrap());
        assert!(!store.move_project("b", false).unwrap());
        let entry = store.claim_next_outbox().unwrap().unwrap();
        let Operation::OrderResources { plans } = &entry.operation else {
            panic!()
        };
        assert_eq!(
            plans[0]
                .positions
                .iter()
                .map(|position| position.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        let writes = store.prepare_key_writes(plans).unwrap();
        assert!(
            writes.iter().all(
                |write| write.command == "project_update" && write.field == "default_order_key"
            )
        );
        assert!(
            writes
                .iter()
                .all(|write| write.id == "a" || write.id == "b")
        );
        for write in &writes {
            store.confirm_order_key(write).unwrap();
        }
        let conn = store.connect().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT key FROM order_meta WHERE resource='project' AND id='inside'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "a1"
        );
        assert_eq!(
            conn.query_row(
                "SELECT key FROM order_meta WHERE resource='folder' AND id='f'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "a1V"
        );
    }

    #[test]
    fn mixed_legacy_keys_keep_their_predecessor_and_all_null_scopes_can_be_initialized() {
        let (_dir, store) = fixture(json!({"projects":[{"id":"p","name":"Project"}],"items":[
            {"id":"a","project_id":"p","content":"A","child_order":0,"order_key":"a0"},
            {"id":"legacy","project_id":"p","content":"Legacy","child_order":1},
            {"id":"b","project_id":"p","content":"B","child_order":2,"order_key":"a1"}
        ]}));
        assert_eq!(task_ids(&store), ["a", "legacy", "b"]);
        store.reorder_task("a", "b").unwrap();
        let entry = store.claim_next_outbox().unwrap().unwrap();
        let Operation::OrderResources { plans } = &entry.operation else {
            panic!()
        };
        let writes = store.prepare_key_writes(plans).unwrap();
        assert!(writes.iter().any(|write| write.id == "legacy"));
        for write in &writes {
            store.confirm_order_key(write).unwrap();
        }
        store.finish_order_outbox(&entry, plans).unwrap();
        assert_eq!(task_ids(&store), ["b", "legacy", "a"]);
    }

    #[test]
    fn migration_keeps_original_legacy_payloads_and_cached_desired_order() {
        let (dir, store) = fixture(task_data());
        let path = dir.path().join("ordering.db");
        let payload = r#"{"kind":"reorder_tasks","tasks":[{"id":"b","child_order":10},{"id":"a","child_order":20}]}"#;
        let conn = store.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET child_order=CASE id WHEN 'a' THEN 20 ELSE 10 END",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox(uuid,entity_id,payload) VALUES('legacy-request','a',?1)",
            [payload],
        )
        .unwrap();
        conn.execute_batch("DROP TABLE order_meta; PRAGMA user_version=9; UPDATE meta SET value='old-token' WHERE key='sync_token';").unwrap();
        drop(conn);
        drop(store);
        let reopened = Store::open(path).unwrap();
        assert_eq!(task_ids(&reopened), ["b", "a"]);
        assert_eq!(reopened.sync_token().unwrap(), "*");
        assert!(reopened.ordering_needs_pull().unwrap());
        assert_eq!(
            reopened
                .connect()
                .unwrap()
                .query_row("SELECT payload FROM outbox", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            payload
        );
        let entry = reopened.claim_next_outbox().unwrap().unwrap();
        assert_eq!(entry.uuid, "legacy-request");
        assert_eq!(entry.checkpoint["key_migration"], true);
        assert!(matches!(entry.operation, Operation::ReorderTasks { .. }));
        merge(&reopened, task_data());
        reopened.finish_order_pull().unwrap();
        let Operation::ReorderTasks { tasks } = entry.operation else {
            panic!()
        };
        let plans = reopened.legacy_order_plans(tasks, false).unwrap();
        let writes = reopened.prepare_key_writes(&plans).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].id, "a");
        assert_eq!(writes[0].field, "order_key");
    }

    #[test]
    fn only_unclaimed_plans_coalesce_and_id_aliases_rewrite_positions_and_scopes() {
        let (_dir, store) = fixture(task_data());
        store.reorder_task("a", "b").unwrap();
        store.reorder_task("b", "a").unwrap();
        assert_eq!(store.outbox_len().unwrap(), 1);
        let claimed = store.claim_next_outbox().unwrap().unwrap();
        store.reorder_task("a", "b").unwrap();
        assert_eq!(store.outbox_len().unwrap(), 2);
        assert_eq!(store.next_outbox().unwrap().unwrap().uuid, claimed.uuid);
        let inbox = store.project("inbox").unwrap().unwrap();
        let parent = store.add_task("Parent", &inbox).unwrap();
        let child = store.add_child_task("Child", &inbox, &parent).unwrap();
        store.set_task_parent(&child, None).unwrap();
        store.set_task_parent(&child, Some(&parent)).unwrap();
        store.map_created_task_id(&parent, "parent-server").unwrap();
        store.map_created_task_id(&child, "child-server").unwrap();
        assert!(store.outbox().unwrap().iter().filter_map(|entry|match &entry.operation{Operation::OrderResources{plans}=>Some(plans),_=>None}).flatten().any(|plan|
            matches!(&plan.scope,OrderScope::Tasks{parent_id:Some(id),..} if id=="parent-server") && plan.positions.iter().any(|position|position.id=="child-server")));
    }

    #[test]
    fn incremental_tombstones_retire_anchors_without_requiring_their_old_scope() {
        let (_dir, store) = fixture(task_data());
        store
            .merge_order_metadata(
                &json!({"items":[{"id":"b","is_deleted":true}],
            "projects":[{"id":"p","is_archived":true}]}),
                false,
            )
            .unwrap();
        let conn = store.connect().unwrap();
        for (resource, id) in [("task", "b"), ("project", "p")] {
            let (active, key): (bool, Option<String>) = conn
                .query_row(
                    "SELECT active,key FROM order_meta WHERE resource=?1 AND id=?2",
                    params![resource, id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(!active);
            if resource == "task" {
                assert_eq!(key.as_deref(), Some("a1"));
            }
        }
        let plan = store.placement_plan("a", Placement::Bottom).unwrap();
        assert_eq!(plan.positions[0].child_order, 0);
        assert!(store.prepare_key_writes(&[plan]).unwrap().is_empty());
    }

    #[test]
    fn placements_and_snapshot_restoration_use_the_exact_section_scope() {
        let (_dir, store) = fixture(json!({"projects":[{"id":"p","name":"Project"}],"items":[
            {"id":"a","project_id":"p","section_id":"s","content":"A","order_key":"a0"},
            {"id":"b","project_id":"p","section_id":"s","content":"B","order_key":"a1"},
            {"id":"loose","project_id":"p","content":"Loose","order_key":"a9"}
        ]}));
        for (placement, lower, upper) in [
            (Placement::Top, None, Some("a1")),
            (Placement::Bottom, Some("a1"), None),
        ] {
            let plan = store.placement_plan("a", placement).unwrap();
            let writes = store.prepare_key_writes(&[plan]).unwrap();
            if let Some(write) = writes.first() {
                assert_eq!(write.id, "a");
                assert!(lower.is_none_or(|lower| write.key.as_str() > lower));
                assert!(upper.is_none_or(|upper| write.key.as_str() < upper));
            } else {
                assert!(matches!(placement, Placement::Top));
            }
        }
        let original = store.task("a").unwrap().unwrap();
        store.move_task("a", "inbox").unwrap();
        assert!(store.task("a").unwrap().unwrap().section_id.is_none());
        store.apply_task_snapshot(&original).unwrap();
        assert_eq!(store.task("a").unwrap().unwrap(), original);
        assert!(store.outbox().unwrap().iter().any(|entry| matches!(
            &entry.operation, Operation::MoveTaskParent { section_id:Some(section), .. } if section=="s")));
    }

    #[test]
    fn deletion_undo_restores_local_ordinals_and_places_recreations_after_the_final_move() {
        for synchronized in [false, true] {
            let (_dir, store) = fixture(json!({"projects":[{"id":"p","name":"Project"}],"items":[
                {"id":"a","project_id":"p","section_id":"s","content":"A","order_key":"a0"},
                {"id":"b","project_id":"p","section_id":"s","content":"B","order_key":"a1"},
                {"id":"c","project_id":"p","section_id":"s","content":"C","order_key":"a2"}
            ]}));
            let snapshots = store.delete_task_tree("b").unwrap();
            assert_eq!(task_ids(&store), ["a", "c"]);
            if synchronized {
                let entry = store.claim_next_outbox().unwrap().unwrap();
                store.finish_outbox(entry.seq).unwrap();
            }
            let restored = store.restore_deleted_tasks(&snapshots).unwrap();
            let id = &restored[0].id;
            assert_eq!(task_ids(&store), ["a", id, "c"]);
            assert_eq!(
                store
                    .tasks_for(&NavId::Project("p".into()))
                    .unwrap()
                    .iter()
                    .map(|task| task.child_order)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
            if synchronized {
                let entries = store.outbox().unwrap();
                let project = entries
                    .iter()
                    .position(|entry| matches!(entry.operation, Operation::MoveTask { .. }))
                    .unwrap();
                let section=entries.iter().position(|entry|matches!(&entry.operation,Operation::MoveTaskParent{section_id:Some(section),..} if section=="s")).unwrap();
                let order = entries
                    .iter()
                    .position(|entry| matches!(entry.operation, Operation::OrderResources { .. }))
                    .unwrap();
                assert!(project < section && section < order);
                assert!(matches!(
                    &entries[0].operation,
                    Operation::QuickAdd {
                        target_order: None,
                        placement: None,
                        ..
                    }
                ));
            } else {
                assert_eq!(store.outbox_len().unwrap(), 0);
            }
        }
    }
}
