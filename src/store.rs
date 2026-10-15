use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use chrono::Local;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::model::{
    Due, Filter, INBOX_LOCAL_ID, NavId, NavRow, Operation, OutboxEntry, PROJECT_SPACE_COUNT,
    Project, ProjectOrder, RemoteCollaborator, RemoteFilter, RemoteLabel, RemoteProject,
    RemoteTask, Task,
};

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskViewPreferences {
    pub group_by_label: bool,
    pub sort_alphabetically: bool,
    pub show_completed: bool,
}

impl Default for TaskViewPreferences {
    fn default() -> Self {
        Self {
            group_by_label: true,
            sort_alphabetically: false,
            show_completed: false,
        }
    }
}

impl Store {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("create data directory {}", parent.display()))?;
        }
        let store = Self { path };
        store.initialize()?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("open database {}", self.path.display()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        Ok(conn)
    }

    fn initialize(&self) -> Result<()> {
        let conn = self.connect()?;
        let previous_schema_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // Journal mode persists in the database; setting it once avoids a
        // locking pragma on every short-lived read connection.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS projects (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, parent_id TEXT,
               child_order INTEGER NOT NULL DEFAULT 0, workspace_id TEXT,
               is_inbox INTEGER NOT NULL DEFAULT 0, is_shared INTEGER NOT NULL DEFAULT 0,
               can_assign INTEGER NOT NULL DEFAULT 0, space_override INTEGER,
               is_hidden INTEGER NOT NULL DEFAULT 0,
               deleted INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS tasks (
               id TEXT PRIMARY KEY, project_id TEXT NOT NULL, content TEXT NOT NULL,
               description TEXT NOT NULL DEFAULT '', labels_json TEXT NOT NULL DEFAULT '[]',
               due_json TEXT, priority INTEGER NOT NULL DEFAULT 1,
               child_order INTEGER NOT NULL DEFAULT 0, checked INTEGER NOT NULL DEFAULT 0,
               deleted INTEGER NOT NULL DEFAULT 0, parent_id TEXT,
               responsible_uid TEXT, assignee_name TEXT
             );
             CREATE TABLE IF NOT EXISTS collaborators (
               id TEXT PRIMARY KEY, full_name TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS labels (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, item_order INTEGER NOT NULL DEFAULT 0,
               deleted INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS filters (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, query TEXT NOT NULL,
               item_order INTEGER NOT NULL DEFAULT 0, is_favorite INTEGER NOT NULL DEFAULT 0,
               workspace_id TEXT, is_frozen INTEGER NOT NULL DEFAULT 0,
               deleted INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS filter_tasks (
               filter_id TEXT NOT NULL, task_id TEXT NOT NULL,
               PRIMARY KEY(filter_id, task_id)
             );
             CREATE TABLE IF NOT EXISTS outbox (
               seq INTEGER PRIMARY KEY AUTOINCREMENT, uuid TEXT NOT NULL UNIQUE,
               entity_id TEXT NOT NULL, payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_id_aliases (
               old_id TEXT PRIMARY KEY, new_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS filter_id_aliases (
               old_id TEXT PRIMARY KEY, new_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS project_id_aliases (
               old_id TEXT PRIMARY KEY, new_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS hidden_navigation (
               kind TEXT NOT NULL, id TEXT NOT NULL,
               PRIMARY KEY(kind, id)
             );
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             COMMIT;",
        )?;

        // CREATE TABLE IF NOT EXISTS does not add columns to databases made by
        // earlier versions. Add every current column before creating indexes
        // or running queries that refer to it.
        ensure_column(&conn, "projects", "parent_id", "TEXT")?;
        if ensure_column(
            &conn,
            "projects",
            "child_order",
            "INTEGER NOT NULL DEFAULT 0",
        )? {
            copy_legacy_column(&conn, "projects", "child_order", "position")?;
        }
        ensure_column(&conn, "projects", "workspace_id", "TEXT")?;
        ensure_column(&conn, "projects", "is_inbox", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "projects", "is_shared", "INTEGER NOT NULL DEFAULT 0")?;
        if ensure_column(
            &conn,
            "projects",
            "can_assign",
            "INTEGER NOT NULL DEFAULT 0",
        )? {
            copy_legacy_column(&conn, "projects", "can_assign", "can_assign_tasks")?;
        }
        if ensure_column(&conn, "projects", "space_override", "INTEGER")? {
            migrate_legacy_spaces(&conn)?;
        }
        ensure_column(&conn, "projects", "is_hidden", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "projects", "deleted", "INTEGER NOT NULL DEFAULT 0")?;

        ensure_column(&conn, "tasks", "description", "TEXT NOT NULL DEFAULT ''")?;
        ensure_column(&conn, "tasks", "parent_id", "TEXT")?;
        if ensure_column(&conn, "tasks", "labels_json", "TEXT NOT NULL DEFAULT '[]'")? {
            copy_legacy_column(&conn, "tasks", "labels_json", "labels")?;
        }
        if ensure_column(&conn, "tasks", "due_json", "TEXT")? && has_column(&conn, "tasks", "due")?
        {
            conn.execute(
                "UPDATE tasks SET due_json=json_object(
                   'string',due,'date',NULL,'datetime',NULL,'timezone',NULL,
                   'is_recurring',json('false')
                 ) WHERE due IS NOT NULL AND trim(due)<>''",
                [],
            )?;
        }
        ensure_column(&conn, "tasks", "priority", "INTEGER NOT NULL DEFAULT 1")?;
        if ensure_column(&conn, "tasks", "child_order", "INTEGER NOT NULL DEFAULT 0")? {
            copy_legacy_column(&conn, "tasks", "child_order", "position")?;
        }
        if ensure_column(&conn, "tasks", "checked", "INTEGER NOT NULL DEFAULT 0")? {
            copy_legacy_column(&conn, "tasks", "checked", "completed")?;
        }
        ensure_column(&conn, "tasks", "deleted", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "tasks", "responsible_uid", "TEXT")?;
        ensure_column(&conn, "tasks", "assignee_name", "TEXT")?;

        if ensure_column(&conn, "filters", "item_order", "INTEGER NOT NULL DEFAULT 0")? {
            copy_legacy_column(&conn, "filters", "item_order", "position")?;
        }
        if ensure_column(
            &conn,
            "filters",
            "is_favorite",
            "INTEGER NOT NULL DEFAULT 0",
        )? {
            // The legacy cache contained favorite filters only.
            conn.execute("UPDATE filters SET is_favorite=1", [])?;
        }
        ensure_column(&conn, "filters", "deleted", "INTEGER NOT NULL DEFAULT 0")?;
        ensure_column(&conn, "filters", "workspace_id", "TEXT")?;
        ensure_column(&conn, "filters", "is_frozen", "INTEGER NOT NULL DEFAULT 0")?;

        conn.execute_batch(
            "BEGIN;
             UPDATE projects SET is_inbox=0
             WHERE is_inbox=1 AND id NOT IN (
               SELECT id FROM projects WHERE is_inbox=1
               ORDER BY CASE WHEN lower(name)='inbox' THEN 0 ELSE 1 END, child_order, id LIMIT 1
             );
             CREATE UNIQUE INDEX IF NOT EXISTS one_visible_inbox
               ON projects(is_inbox) WHERE is_inbox = 1 AND deleted = 0;
             CREATE INDEX IF NOT EXISTS tasks_project_order
               ON tasks(project_id, checked, deleted, child_order);
             CREATE INDEX IF NOT EXISTS filter_tasks_filter ON filter_tasks(filter_id);
             CREATE INDEX IF NOT EXISTS outbox_entity ON outbox(entity_id);
             INSERT OR IGNORE INTO projects(id, name, is_inbox) VALUES('inbox', 'Inbox', 1);
             PRAGMA user_version=8;
             COMMIT;",
        )?;
        if previous_schema_version < 8 {
            // Older releases did not request collaborator resources or retain
            // assignment IDs. An existing incremental token cannot backfill
            // that data, so make the next pull a one-time full refresh.
            conn.execute("UPDATE meta SET value='*' WHERE key='sync_token'", [])?;
        }
        compact_pending_due_updates(&conn)?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        projects_from(&conn)
    }

    pub fn favorite_filters(&self) -> Result<Vec<Filter>> {
        let conn = self.connect()?;
        favorite_filters_from(&conn)
    }

    pub fn filters(&self) -> Result<Vec<Filter>> {
        filters_from(&self.connect()?, false)
    }

    pub fn filter(&self, id: &str) -> Result<Option<Filter>> {
        let conn = self.connect()?;
        let resolved = resolve_filter_id_from(&conn, id)?.unwrap_or_else(|| id.to_owned());
        filter_from(&conn, &resolved)
    }

    pub fn labels(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT name FROM labels WHERE deleted=0 ORDER BY item_order, lower(name), id",
        )?;
        let mut labels = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        let mut seen = HashSet::new();
        labels.retain(|label| seen.insert(label.to_lowercase()));

        // Todoist does not return shared labels in the label sync resource;
        // they are represented only by names on active tasks.
        let mut stmt = conn.prepare(
            "SELECT labels_json FROM tasks WHERE checked=0 AND deleted=0 ORDER BY child_order,id",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut shared = Vec::new();
        for row in rows {
            for label in serde_json::from_str::<Vec<String>>(&row?)? {
                if seen.insert(label.to_lowercase()) {
                    shared.push(label);
                }
            }
        }
        shared.sort_by_key(|label| label.to_lowercase());
        labels.extend(shared);
        Ok(labels)
    }

    pub fn nav_rows(&self, show_hidden: bool) -> Result<Vec<NavRow>> {
        let conn = self.connect()?;
        let projects = projects_from(&conn)?;
        let filters = favorite_filters_from(&conn)?;
        let mut project_counts = HashMap::<String, usize>::new();
        {
            let mut stmt = conn.prepare(
                "SELECT project_id,COUNT(*) FROM tasks WHERE checked=0 AND deleted=0 GROUP BY project_id"
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?;
            for row in rows {
                let (id, count) = row?;
                project_counts.insert(id, count);
            }
        }
        let mut filter_counts = HashMap::<String, usize>::new();
        {
            let mut stmt = conn.prepare(
                "SELECT ft.filter_id,COUNT(*) FROM filter_tasks ft JOIN tasks t ON t.id=ft.task_id
                 WHERE t.checked=0 AND t.deleted=0 GROUP BY ft.filter_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?;
            for row in rows {
                let (id, count) = row?;
                filter_counts.insert(id, count);
            }
        }
        let (today_count, upcoming_count) = {
            let today = Local::now().date_naive();
            let mut today_count = 0;
            let mut upcoming_count = 0;
            let mut stmt = conn.prepare(
                "SELECT due_json FROM tasks
                 WHERE checked=0 AND deleted=0 AND due_json IS NOT NULL",
            )?;
            let due_values = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for due_json in due_values {
                let Some(date) = serde_json::from_str::<Due>(&due_json?)
                    .ok()
                    .and_then(|due| due.concrete_date())
                else {
                    continue;
                };
                today_count += usize::from(date <= today);
                upcoming_count +=
                    usize::from(date >= today && date <= today + chrono::Duration::days(6));
            }
            (today_count, upcoming_count)
        };
        let hidden_navigation = {
            let mut stmt = conn.prepare("SELECT kind,id FROM hidden_navigation")?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<HashSet<_>>>()?
        };
        let inbox = projects
            .iter()
            .find(|p| p.is_inbox)
            .cloned()
            .unwrap_or(Project {
                id: INBOX_LOCAL_ID.into(),
                name: "Inbox".into(),
                parent_id: None,
                child_order: 0,
                workspace_id: None,
                is_inbox: true,
                is_shared: false,
                can_assign_tasks: false,
                space_override: None,
                is_hidden: false,
            });
        let mut leading = vec![
            (
                NavRow::Item {
                    id: NavId::Project(inbox.id.clone()),
                    name: "Inbox".into(),
                    count: *project_counts.get(&inbox.id).unwrap_or(&0),
                    is_inbox: true,
                    depth: 0,
                },
                inbox.is_hidden,
            ),
            (
                NavRow::Item {
                    id: NavId::Today,
                    name: "Today".into(),
                    count: today_count,
                    is_inbox: false,
                    depth: 0,
                },
                hidden_navigation.contains(&("view".into(), "today".into())),
            ),
            (
                NavRow::Item {
                    id: NavId::Upcoming,
                    name: "Upcoming".into(),
                    count: upcoming_count,
                    is_inbox: false,
                    depth: 0,
                },
                hidden_navigation.contains(&("view".into(), "upcoming".into())),
            ),
        ];
        leading.extend(filters.into_iter().map(|filter| {
            let is_hidden = hidden_navigation.contains(&("filter".into(), filter.id.clone()));
            (
                NavRow::Item {
                    count: *filter_counts.get(&filter.id).unwrap_or(&0),
                    id: NavId::Filter(filter.id),
                    name: filter.name,
                    is_inbox: false,
                    depth: 0,
                },
                is_hidden,
            )
        }));
        let mut rows = Vec::new();
        let mut hidden_rows = Vec::new();
        for (row, is_hidden) in leading {
            if is_hidden {
                hidden_rows.push(row);
            } else {
                rows.push(row);
            }
        }
        let mut visible_projects = Vec::new();
        let mut hidden_projects = Vec::new();
        for project in &projects {
            if project.is_inbox {
                continue;
            }
            if project.is_hidden {
                hidden_projects.push(project);
            } else {
                visible_projects.push(project);
            }
        }
        let ordered_projects = project_hierarchy_rows(&visible_projects, &project_counts);
        for space in 0..PROJECT_SPACE_COUNT {
            let space_rows = ordered_projects
                .iter()
                .filter(|(root_space, _)| *root_space == space)
                .map(|(_, row)| row.clone())
                .collect::<Vec<_>>();
            if space_rows.is_empty() {
                continue;
            }
            if space > 0 && !rows.is_empty() {
                rows.push(NavRow::Separator);
            }
            rows.extend(space_rows);
        }
        hidden_rows.extend(
            project_hierarchy_rows(&hidden_projects, &project_counts)
                .into_iter()
                .map(|(_, row)| row),
        );
        if show_hidden && !hidden_rows.is_empty() {
            if !rows.is_empty() {
                rows.push(NavRow::Separator);
            }
            rows.extend(hidden_rows);
        }
        Ok(rows)
    }

    #[cfg(test)]
    pub fn tasks_for(&self, nav: &NavId) -> Result<Vec<Task>> {
        self.tasks_for_view(nav, false)
    }

    pub fn tasks_for_view(&self, nav: &NavId, show_completed: bool) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        if matches!(nav, NavId::Today | NavId::Upcoming) {
            let sql = if show_completed {
                "SELECT id,project_id,content,description,labels_json,due_json,priority,child_order,checked,parent_id,assignee_name
                 FROM tasks WHERE deleted=0 ORDER BY checked,child_order,id"
            } else {
                "SELECT id,project_id,content,description,labels_json,due_json,priority,child_order,checked,parent_id,assignee_name
                 FROM tasks WHERE checked=0 AND deleted=0 ORDER BY child_order,id"
            };
            let mut stmt = conn.prepare(sql)?;
            let mut tasks = stmt
                .query_map([], row_to_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let today = Local::now().date_naive();
            tasks.retain(|task| {
                let Some(date) = task.due.as_ref().and_then(Due::concrete_date) else {
                    return false;
                };
                match nav {
                    NavId::Today => date <= today,
                    NavId::Upcoming => date >= today && date <= today + chrono::Duration::days(6),
                    NavId::Project(_) | NavId::Filter(_) => unreachable!(),
                }
            });
            tasks.sort_by_cached_key(|task| match nav {
                NavId::Today => (
                    task.checked,
                    task.due.as_ref().and_then(Due::concrete_date),
                    task.child_order,
                ),
                NavId::Upcoming => (
                    false,
                    task.due.as_ref().and_then(Due::concrete_date),
                    task.child_order,
                ),
                NavId::Project(_) | NavId::Filter(_) => unreachable!(),
            });
            return Ok(tasks);
        }
        let (sql, id) = match (nav, show_completed) {
            (NavId::Project(id), true) => (
                "SELECT id,project_id,content,description,labels_json,due_json,priority,child_order,checked,parent_id,assignee_name
                 FROM tasks WHERE project_id=?1 AND deleted=0 ORDER BY checked,child_order,id", id),
            (NavId::Filter(id), true) => (
                "SELECT t.id,t.project_id,t.content,t.description,t.labels_json,t.due_json,t.priority,t.child_order,t.checked,t.parent_id,t.assignee_name
                 FROM filter_tasks ft JOIN tasks t ON t.id=ft.task_id
                 WHERE ft.filter_id=?1 AND t.deleted=0 ORDER BY t.checked,t.child_order,t.id", id),
            (NavId::Project(id), false) => (
                "SELECT id,project_id,content,description,labels_json,due_json,priority,child_order,checked,parent_id,assignee_name
                 FROM tasks WHERE project_id=?1 AND checked=0 AND deleted=0 ORDER BY child_order,id", id),
            (NavId::Filter(id), false) => (
                "SELECT t.id,t.project_id,t.content,t.description,t.labels_json,t.due_json,t.priority,t.child_order,t.checked,t.parent_id,t.assignee_name
                 FROM filter_tasks ft JOIN tasks t ON t.id=ft.task_id
                 WHERE ft.filter_id=?1 AND t.checked=0 AND t.deleted=0 ORDER BY t.child_order,t.id", id),
            (NavId::Today | NavId::Upcoming, _) => unreachable!(),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([id], row_to_task)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn tasks_for_search(&self) -> Result<Vec<(Task, String)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT t.id,t.project_id,t.content,t.description,t.labels_json,t.due_json,
                    t.priority,t.child_order,t.checked,t.parent_id,t.assignee_name,COALESCE(p.name,'Unknown project')
             FROM tasks t LEFT JOIN projects p ON p.id=t.project_id AND p.deleted=0
             WHERE t.deleted=0
             ORDER BY t.checked,lower(t.content),t.child_order,t.id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row_to_task(row)?, row.get(11)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.connect()?;
        let resolved = resolve_project_id_from(&conn, id)?.unwrap_or_else(|| id.to_owned());
        project_from(&conn, &resolved)
    }

    pub fn project_named(&self, name: &str) -> Result<Option<Project>> {
        let mut matches = self
            .projects()?
            .into_iter()
            .filter(|project| project.name.eq_ignore_ascii_case(name));
        let first = matches.next();
        if first.is_some() && matches.next().is_some() {
            bail!("multiple projects are named '#{name}'");
        }
        Ok(first)
    }

    pub fn add_project(&self, name: &str) -> Result<String> {
        let id = format!("local:{}", Uuid::new_v4());
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let child_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(child_order),-1)+1 FROM projects
             WHERE deleted=0 AND parent_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO projects(id,name,parent_id,child_order,workspace_id,is_inbox,is_shared,
                                   can_assign,space_override,is_hidden,deleted)
             VALUES(?1,?2,NULL,?3,NULL,0,0,0,NULL,0,0)",
            params![id, name, child_order],
        )?;
        queue(
            &tx,
            &id,
            Operation::AddProject {
                name: name.to_owned(),
            },
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<String> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let id = resolve_project_id_from(&tx, id)?.unwrap_or_else(|| id.to_owned());
        let project = project_from(&tx, &id)?.context("project is no longer available")?;
        anyhow::ensure!(!project.is_inbox, "Inbox cannot be renamed");
        tx.execute("UPDATE projects SET name=?2 WHERE id=?1", params![id, name])?;
        queue_project_rename(&tx, &id, name)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn task(&self, id: &str) -> Result<Option<Task>> {
        let conn = self.connect()?;
        let resolved = resolve_task_id_from(&conn, id)?.unwrap_or_else(|| id.to_owned());
        task_from(&conn, &resolved)
    }

    pub fn add_filter(&self, name: &str, query: &str, is_favorite: bool) -> Result<String> {
        let id = format!("local:{}", Uuid::new_v4());
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let item_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(item_order),-1)+1 FROM filters WHERE deleted=0",
            [],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO filters(id,name,query,item_order,is_favorite,workspace_id,is_frozen,deleted)
             VALUES(?1,?2,?3,?4,?5,NULL,0,0)",
            params![id, name, query, item_order, is_favorite],
        )?;
        queue(
            &tx,
            &id,
            Operation::AddFilter {
                name: name.to_owned(),
                query: query.to_owned(),
                is_favorite,
            },
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn update_filter(
        &self,
        id: &str,
        name: &str,
        query: &str,
        is_favorite: bool,
    ) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let id = resolve_filter_id_from(&tx, id)?.unwrap_or_else(|| id.to_owned());
        let filter = filter_from(&tx, &id)?.context("filter is no longer available")?;
        anyhow::ensure!(!filter.is_frozen, "this filter is read-only");
        tx.execute(
            "UPDATE filters SET name=?2,query=?3,is_favorite=?4 WHERE id=?1",
            params![id, name, query, is_favorite],
        )?;
        if !is_favorite {
            tx.execute("DELETE FROM filter_tasks WHERE filter_id=?1", [&id])?;
        }
        queue_filter_change(
            &tx,
            &id,
            name,
            query,
            is_favorite,
            filter.workspace_id.is_some(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_task(&self, expression: &str, project: &Project) -> Result<String> {
        self.add_task_with_parent(expression, project, None)
    }

    pub fn add_child_task(
        &self,
        expression: &str,
        project: &Project,
        parent_id: &str,
    ) -> Result<String> {
        self.add_task_with_parent(expression, project, Some(parent_id))
    }

    fn add_task_with_parent(
        &self,
        expression: &str,
        project: &Project,
        parent_id: Option<&str>,
    ) -> Result<String> {
        let id = format!("local:{}", Uuid::new_v4());
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let project_id =
            resolve_project_id_from(&tx, &project.id)?.unwrap_or_else(|| project.id.clone());
        let project = project_from(&tx, &project_id)?.context("task project no longer exists")?;
        if let Some(parent_id) = parent_id {
            let parent = task_from(&tx, parent_id)?.context("parent task no longer exists")?;
            anyhow::ensure!(!parent.checked, "cannot add a subtask to a completed task");
            anyhow::ensure!(
                parent.project_id == project.id,
                "parent and child tasks must be in the same project"
            );
        }
        let next_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(child_order),-1)+1 FROM tasks
             WHERE project_id=?1 AND parent_id IS ?2",
            params![project.id, parent_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO tasks(id,project_id,parent_id,content,child_order)
             VALUES(?1,?2,?3,?4,?5)",
            params![id, project.id, parent_id, expression.trim(), next_order],
        )?;
        queue(
            &tx,
            &id,
            Operation::QuickAdd {
                expression: expression.trim().to_owned(),
                project_id: project.id.clone(),
                project_name: project.name.clone(),
                parent_id: parent_id.map(str::to_owned),
            },
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn set_task_parent(&self, id: &str, parent_id: Option<&str>) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let task = task_from(&tx, id)?.context("selected task no longer exists")?;
        if let Some(parent_id) = parent_id {
            let mut ancestor_id = Some(parent_id.to_owned());
            let mut visited = HashSet::new();
            while let Some(id) = ancestor_id {
                anyhow::ensure!(id != task.id, "a task cannot become its own descendant");
                anyhow::ensure!(
                    visited.insert(id.clone()),
                    "task hierarchy contains a cycle"
                );
                let ancestor = task_from(&tx, &id)?.context("new parent task no longer exists")?;
                anyhow::ensure!(
                    ancestor.project_id == task.project_id,
                    "parent and child tasks must be in the same project"
                );
                anyhow::ensure!(!ancestor.checked, "a completed task cannot become a parent");
                ancestor_id = ancestor.parent_id;
            }
        }
        let next_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(child_order),-1)+1 FROM tasks
             WHERE project_id=?1 AND parent_id IS ?2 AND id<>?3",
            params![task.project_id, parent_id, id],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE tasks SET parent_id=?2,child_order=?3 WHERE id=?1",
            params![id, parent_id, next_order],
        )?;
        queue(
            &tx,
            id,
            Operation::MoveTaskParent {
                parent_id: parent_id.map(str::to_owned),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn edit_task(&self, id: &str, content: &str, description: &str) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tasks SET content=?2,description=?3 WHERE id=?1",
            params![id, content, description],
        )?;
        queue(
            &tx,
            id,
            Operation::QuickEdit {
                expression: content.into(),
                description: description.into(),
                update_uuid: Uuid::new_v4().to_string(),
                move_uuid: Uuid::new_v4().to_string(),
                delete_uuid: Uuid::new_v4().to_string(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_due(&self, id: &str, due_string: &str) -> Result<()> {
        let due = if due_string.trim().is_empty() {
            None
        } else {
            Some(Due {
                string: Some(due_string.trim().into()),
                ..Due::default()
            })
        };
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tasks SET due_json=?2 WHERE id=?1",
            params![id, due.map(|d| serde_json::to_string(&d)).transpose()?],
        )?;
        remove_pending_due_updates(&tx, id)?;
        queue(
            &tx,
            id,
            Operation::UpdateDue {
                due_string: due_string.trim().into(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_labels(&self, id: &str, labels: &[String]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tasks SET labels_json=?2 WHERE id=?1",
            params![id, serde_json::to_string(labels)?],
        )?;
        queue(
            &tx,
            id,
            Operation::UpdateLabels {
                labels: labels.to_vec(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_task_completed(&self, id: &str, completed: bool) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE tasks SET checked=?2 WHERE id=?1",
            params![id, completed],
        )?;
        anyhow::ensure!(changed == 1, "selected task no longer exists");
        queue(
            &tx,
            id,
            if completed {
                Operation::CompleteTask
            } else {
                Operation::UncompleteTask
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn apply_synced_task_edit(&self, task: &Task, completed_seq: i64) -> Result<()> {
        let conn = self.connect()?;
        let has_newer_text_edit = {
            let mut stmt = conn
                .prepare("SELECT payload FROM outbox WHERE entity_id=?1 AND seq>?2 ORDER BY seq")?;
            let rows = stmt.query_map(params![task.id, completed_seq], |row| {
                row.get::<_, String>(0)
            })?;
            let mut found = false;
            for payload in rows {
                if matches!(
                    serde_json::from_str::<Operation>(&payload?),
                    Ok(Operation::QuickEdit { .. } | Operation::UpdateTask { .. })
                ) {
                    found = true;
                    break;
                }
            }
            found
        };
        if has_newer_text_edit {
            conn.execute(
                "UPDATE tasks SET project_id=?2,labels_json=?3,due_json=?4,priority=?5 WHERE id=?1",
                params![
                    task.id,
                    task.project_id,
                    serde_json::to_string(&task.labels)?,
                    task.due.as_ref().map(serde_json::to_string).transpose()?,
                    task.priority,
                ],
            )?;
        } else {
            conn.execute(
                "UPDATE tasks SET project_id=?2,content=?3,description=?4,labels_json=?5,
                 due_json=?6,priority=?7 WHERE id=?1",
                params![
                    task.id,
                    task.project_id,
                    task.content,
                    task.description,
                    serde_json::to_string(&task.labels)?,
                    task.due.as_ref().map(serde_json::to_string).transpose()?,
                    task.priority,
                ],
            )?;
        }
        Ok(())
    }

    pub fn move_task(&self, id: &str, project_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let project_id =
            resolve_project_id_from(&tx, project_id)?.unwrap_or_else(|| project_id.to_owned());
        let next_order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(child_order),-1)+1 FROM tasks WHERE project_id=?1",
            [&project_id],
            |row| row.get(0),
        )?;
        let changed = tx.execute(
            "UPDATE tasks SET project_id=?2,parent_id=NULL,child_order=?3 WHERE id=?1",
            params![id, project_id, next_order],
        )?;
        anyhow::ensure!(changed == 1, "selected task no longer exists");
        queue(&tx, id, Operation::MoveTask { project_id })?;
        tx.commit()?;
        Ok(())
    }

    pub fn apply_task_snapshot(&self, target: &Task) -> Result<String> {
        let conn = self.connect()?;
        let id = resolve_task_id_from(&conn, &target.id)?.unwrap_or_else(|| target.id.clone());
        let current = task_from(&conn, &id)?.context("task to restore no longer exists")?;
        let target_project_id = resolve_project_id_from(&conn, &target.project_id)?
            .unwrap_or_else(|| target.project_id.clone());
        let target_parent_id = target
            .parent_id
            .as_deref()
            .map(|parent_id| {
                resolve_task_id_from(&conn, parent_id)
                    .map(|resolved| resolved.unwrap_or_else(|| parent_id.to_owned()))
            })
            .transpose()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tasks SET project_id=?2,content=?3,description=?4,labels_json=?5,
             due_json=?6,priority=?7,child_order=?8,checked=?9,parent_id=?10,deleted=0 WHERE id=?1",
            params![
                id,
                target_project_id,
                target.content,
                target.description,
                serde_json::to_string(&target.labels)?,
                target.due.as_ref().map(serde_json::to_string).transpose()?,
                target.priority,
                target.child_order,
                target.checked,
                target_parent_id
            ],
        )?;
        if current.content != target.content || current.description != target.description {
            queue(
                &tx,
                &id,
                Operation::UpdateTask {
                    content: target.content.clone(),
                    description: target.description.clone(),
                },
            )?;
        }
        if current.due != target.due {
            remove_pending_due_updates(&tx, &id)?;
            queue(
                &tx,
                &id,
                Operation::UpdateDue {
                    due_string: target
                        .due
                        .as_ref()
                        .and_then(|due| due.string.as_deref().or_else(|| due.display()))
                        .unwrap_or_default()
                        .to_owned(),
                },
            )?;
        }
        if current.labels != target.labels {
            queue(
                &tx,
                &id,
                Operation::UpdateLabels {
                    labels: target.labels.clone(),
                },
            )?;
        }
        if current.project_id != target_project_id {
            queue(
                &tx,
                &id,
                Operation::MoveTask {
                    project_id: target_project_id,
                },
            )?;
        }
        if current.parent_id != target_parent_id {
            queue(
                &tx,
                &id,
                Operation::MoveTaskParent {
                    parent_id: target_parent_id,
                },
            )?;
        }
        if current.checked != target.checked {
            queue(
                &tx,
                &id,
                if target.checked {
                    Operation::CompleteTask
                } else {
                    Operation::UncompleteTask
                },
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn delete_task(&self, id: &str) -> Result<String> {
        let conn = self.connect()?;
        let id = resolve_task_id_from(&conn, id)?.unwrap_or_else(|| id.to_owned());
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute("UPDATE tasks SET deleted=1 WHERE id=?1", [&id])?;
        anyhow::ensure!(changed == 1, "task to undo no longer exists");
        queue(&tx, &id, Operation::DeleteTask)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn toggle_space(&self, project_id: &str, space: u8) -> Result<(u8, bool)> {
        anyhow::ensure!(
            space < PROJECT_SPACE_COUNT,
            "project space must be between 0 and 9"
        );
        let conn = self.connect()?;
        let project =
            project_from(&conn, project_id)?.context("selected project no longer exists")?;
        anyhow::ensure!(!project.is_inbox, "Inbox cannot be moved");
        let removed = project.space_override == Some(space);
        let override_value = if removed { None } else { Some(space) };
        conn.execute(
            "UPDATE projects SET space_override=?2 WHERE id=?1",
            params![project_id, override_value],
        )?;
        Ok((
            override_value.unwrap_or_else(|| project.default_space()),
            removed,
        ))
    }

    /// Move a project one visible position among projects in the same local
    /// group and with the same Todoist parent. The complete sibling order is
    /// rewritten so projects hidden by another group retain deterministic
    /// positions when they become visible again.
    pub fn move_project(&self, project_id: &str, down: bool) -> Result<bool> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let projects = projects_from(&tx)?;
        let selected = projects
            .iter()
            .find(|project| project.id == project_id)
            .context("selected project no longer exists")?;
        anyhow::ensure!(!selected.is_inbox, "Inbox order is fixed");

        let projects_by_id = projects
            .iter()
            .map(|project| (project.id.as_str(), project))
            .collect::<HashMap<_, _>>();
        let selected_group = project_display_group(selected, &projects_by_id);
        let candidates = projects
            .iter()
            .filter(|project| {
                !project.is_inbox
                    && project.parent_id == selected.parent_id
                    && project_display_group(project, &projects_by_id) == selected_group
            })
            .collect::<Vec<_>>();
        let Some(position) = candidates
            .iter()
            .position(|project| project.id == selected.id)
        else {
            return Ok(false);
        };
        let target_position = if down {
            position
                .checked_add(1)
                .filter(|position| *position < candidates.len())
        } else {
            position.checked_sub(1)
        };
        let Some(target) = target_position.and_then(|position| candidates.get(position)) else {
            return Ok(false);
        };

        let siblings = projects
            .iter()
            .filter(|project| !project.is_inbox && project.parent_id == selected.parent_id)
            .collect::<Vec<_>>();
        let mut ordered_ids = siblings
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();
        let selected_position = ordered_ids
            .iter()
            .position(|id| id == &selected.id)
            .context("selected project is missing from its sibling order")?;
        let target_position = ordered_ids
            .iter()
            .position(|id| id == &target.id)
            .context("target project is missing from its sibling order")?;
        ordered_ids.swap(selected_position, target_position);

        for (child_order, id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE projects SET child_order=?2 WHERE id=?1",
                params![id, child_order as i64],
            )?;
        }
        let order = projects_from(&tx)?
            .into_iter()
            .filter(|project| !project.is_inbox)
            .map(|project| ProjectOrder {
                id: project.id,
                child_order: project.child_order,
            })
            .collect();
        queue_project_reorder(&tx, project_id, order)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn toggle_project_hidden(&self, project_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let project =
            project_from(&conn, project_id)?.context("selected project no longer exists")?;
        let is_hidden = !project.is_hidden;
        conn.execute(
            "UPDATE projects SET is_hidden=?2 WHERE id=?1",
            params![project_id, is_hidden],
        )?;
        Ok(is_hidden)
    }

    pub fn toggle_navigation_hidden(&self, nav: &NavId) -> Result<bool> {
        if let NavId::Project(id) = nav {
            return self.toggle_project_hidden(id);
        }
        let (kind, id) = match nav {
            NavId::Filter(id) => ("filter", id.as_str()),
            NavId::Today => ("view", "today"),
            NavId::Upcoming => ("view", "upcoming"),
            NavId::Project(_) => unreachable!(),
        };
        let conn = self.connect()?;
        let was_hidden = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM hidden_navigation WHERE kind=?1 AND id=?2)",
            params![kind, id],
            |row| row.get::<_, bool>(0),
        )?;
        if was_hidden {
            conn.execute(
                "DELETE FROM hidden_navigation WHERE kind=?1 AND id=?2",
                params![kind, id],
            )?;
        } else {
            conn.execute(
                "INSERT INTO hidden_navigation(kind,id) VALUES(?1,?2)",
                params![kind, id],
            )?;
        }
        Ok(!was_hidden)
    }

    #[cfg(test)]
    pub fn outbox(&self) -> Result<Vec<OutboxEntry>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT seq,uuid,entity_id,payload FROM outbox ORDER BY seq")?;
        let rows = stmt.query_map([], row_to_outbox)?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    pub fn next_outbox(&self) -> Result<Option<OutboxEntry>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT seq,uuid,entity_id,payload FROM outbox ORDER BY seq LIMIT 1",
                [],
                row_to_outbox,
            )
            .optional()?)
    }

    pub fn outbox_len(&self) -> Result<usize> {
        let count: i64 = self
            .connect()?
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn finish_outbox(&self, seq: i64) -> Result<()> {
        self.connect()?
            .execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
        Ok(())
    }

    pub fn replace_task_id(&self, old_id: &str, task: &RemoteTask) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let assignee_name = if let Some(id) = task.responsible_uid.as_ref() {
            tx.query_row(
                "SELECT full_name FROM collaborators WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        } else {
            None
        };
        let locally_deleted: bool =
            tx.query_row("SELECT deleted FROM tasks WHERE id=?1", [old_id], |row| {
                row.get(0)
            })?;
        tx.execute(
            "UPDATE OR REPLACE tasks SET id=?2,project_id=?3,content=?4,description=?5,labels_json=?6,
             due_json=?7,priority=?8,child_order=?9,checked=?10,deleted=?11,parent_id=?12,
             responsible_uid=?13,assignee_name=?14 WHERE id=?1",
            params![old_id,task.id,task.project_id,task.content,task.description,
                serde_json::to_string(&task.labels)?,task.due.as_ref().map(serde_json::to_string).transpose()?,
                    task.priority,task.child_order,
                    task.checked || task.is_completed || task.completed_at.is_some(),locally_deleted,task.parent_id,
                    task.responsible_uid,assignee_name],
        )?;
        tx.execute(
            "UPDATE outbox SET entity_id=?2 WHERE entity_id=?1",
            params![old_id, task.id],
        )?;
        tx.execute(
            "UPDATE tasks SET parent_id=?2 WHERE parent_id=?1",
            params![old_id, task.id],
        )?;
        tx.execute(
            "UPDATE filter_tasks SET task_id=?2 WHERE task_id=?1",
            params![old_id, task.id],
        )?;
        tx.execute(
            "INSERT INTO task_id_aliases(old_id,new_id) VALUES(?1,?2)
             ON CONFLICT(old_id) DO UPDATE SET new_id=excluded.new_id",
            params![old_id, task.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replace_filter_id(&self, old_id: &str, new_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE filters SET id=?2 WHERE id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE filter_tasks SET filter_id=?2 WHERE filter_id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE hidden_navigation SET id=?2 WHERE kind='filter' AND id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE outbox SET entity_id=?2 WHERE entity_id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "INSERT INTO filter_id_aliases(old_id,new_id) VALUES(?1,?2)
             ON CONFLICT(old_id) DO UPDATE SET new_id=excluded.new_id",
            params![old_id, new_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replace_project_id(&self, old_id: &str, new_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE projects SET id=?2 WHERE id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE projects SET parent_id=?2 WHERE parent_id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE tasks SET project_id=?2 WHERE project_id=?1",
            params![old_id, new_id],
        )?;
        tx.execute(
            "UPDATE outbox SET entity_id=?2 WHERE entity_id=?1",
            params![old_id, new_id],
        )?;
        let queued = {
            let mut stmt = tx.prepare("SELECT seq,payload FROM outbox ORDER BY seq")?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (seq, payload) in queued {
            let mut operation = serde_json::from_str::<Operation>(&payload)?;
            let changed = match &mut operation {
                Operation::QuickAdd { project_id, .. } | Operation::MoveTask { project_id }
                    if project_id == old_id =>
                {
                    *project_id = new_id.to_owned();
                    true
                }
                Operation::ReorderProjects { projects } => {
                    let mut changed = false;
                    for project in projects {
                        if project.id == old_id {
                            project.id = new_id.to_owned();
                            changed = true;
                        }
                    }
                    changed
                }
                _ => false,
            };
            if changed {
                tx.execute(
                    "UPDATE outbox SET payload=?2 WHERE seq=?1",
                    params![seq, serde_json::to_string(&operation)?],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO project_id_aliases(old_id,new_id) VALUES(?1,?2)
             ON CONFLICT(old_id) DO UPDATE SET new_id=excluded.new_id",
            params![old_id, new_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve_task_id(&self, id: &str) -> Result<Option<String>> {
        resolve_task_id_from(&self.connect()?, id)
    }

    pub fn sync_token(&self) -> Result<String> {
        Ok(self.meta("sync_token")?.unwrap_or_else(|| "*".into()))
    }

    pub fn set_sync_token(&self, token: &str) -> Result<()> {
        self.set_meta("sync_token", token)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        meta_from(&self.connect()?, key)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key,value],
        )?;
        Ok(())
    }

    pub fn task_view_preferences(&self) -> Result<TaskViewPreferences> {
        let conn = self.connect()?;
        let defaults = TaskViewPreferences::default();
        Ok(TaskViewPreferences {
            group_by_label: meta_from(&conn, "task_view_group_by_label")?
                .and_then(|value| parse_bool(&value))
                .unwrap_or(defaults.group_by_label),
            sort_alphabetically: meta_from(&conn, "task_view_sort_alphabetically")?
                .and_then(|value| parse_bool(&value))
                .unwrap_or(defaults.sort_alphabetically),
            show_completed: meta_from(&conn, "task_view_show_completed")?
                .and_then(|value| parse_bool(&value))
                .unwrap_or(defaults.show_completed),
        })
    }

    pub fn set_task_view_preferences(&self, preferences: TaskViewPreferences) -> Result<()> {
        let mut conn = self.connect()?;
        let transaction = conn.transaction()?;
        for (key, value) in [
            ("task_view_group_by_label", preferences.group_by_label),
            (
                "task_view_sort_alphabetically",
                preferences.sort_alphabetically,
            ),
            ("task_view_show_completed", preferences.show_completed),
        ] {
            transaction.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, if value { "true" } else { "false" }],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn merge_collaborators(&self, collaborators: &[RemoteCollaborator]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        for collaborator in collaborators {
            tx.execute(
                "INSERT INTO collaborators(id,full_name) VALUES(?1,?2)
                 ON CONFLICT(id) DO UPDATE SET full_name=excluded.full_name",
                params![collaborator.id, collaborator.full_name],
            )?;
            tx.execute(
                "UPDATE tasks SET assignee_name=?2 WHERE responsible_uid=?1",
                params![collaborator.id, collaborator.full_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn merge_remote(
        &self,
        projects: &[RemoteProject],
        tasks: &[RemoteTask],
        labels: &[RemoteLabel],
        filters: &[RemoteFilter],
    ) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let collaborator_names = collaborator_names(&tx)?;
        let (pending_tasks, pending_filters, pending_projects) = pending_entity_ids(&tx)?;
        let mut transferred_inbox_hidden = None;
        if let Some(remote_inbox) = projects
            .iter()
            .find(|p| p.is_inbox_project && !p.is_deleted && !p.is_archived)
        {
            let old_inbox: Option<(String, bool)> = tx
                .query_row(
                    "SELECT id,is_hidden FROM projects WHERE is_inbox=1 AND deleted=0 LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((old, is_hidden)) = old_inbox.filter(|(old, _)| old != &remote_inbox.id) {
                tx.execute(
                    "UPDATE tasks SET project_id=?2 WHERE project_id=?1",
                    params![old, remote_inbox.id],
                )?;
                tx.execute("DELETE FROM projects WHERE id=?1", [&old])?;
                transferred_inbox_hidden = Some((remote_inbox.id.as_str(), is_hidden));
            }
        }
        for p in projects {
            if pending_projects.contains(&p.id) {
                continue;
            }
            let inbox = p.is_inbox_project;
            tx.execute(
                "INSERT INTO projects(id,name,parent_id,child_order,workspace_id,is_inbox,is_shared,can_assign,deleted)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,parent_id=excluded.parent_id,
                 child_order=excluded.child_order,workspace_id=excluded.workspace_id,is_inbox=excluded.is_inbox,
                 is_shared=excluded.is_shared,can_assign=excluded.can_assign,deleted=excluded.deleted",
                params![p.id,p.name,p.parent_id,p.child_order,p.workspace_id,inbox,p.shared,p.can_assign_tasks,p.is_deleted || p.is_archived],
            )?;
        }
        if let Some((remote_inbox_id, is_hidden)) = transferred_inbox_hidden {
            tx.execute(
                "UPDATE projects SET is_hidden=?2 WHERE id=?1",
                params![remote_inbox_id, is_hidden],
            )?;
        }
        for task in tasks {
            if pending_tasks.contains(&task.id) {
                continue;
            }
            let assignee_name = task
                .responsible_uid
                .as_ref()
                .and_then(|id| collaborator_names.get(id))
                .cloned();
            tx.execute(
                "INSERT INTO tasks(id,project_id,content,description,labels_json,due_json,priority,child_order,checked,deleted,parent_id,responsible_uid,assignee_name)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,content=excluded.content,
                 description=excluded.description,labels_json=excluded.labels_json,due_json=excluded.due_json,
                 priority=excluded.priority,child_order=excluded.child_order,checked=excluded.checked,
                 deleted=excluded.deleted,parent_id=excluded.parent_id,
                 responsible_uid=excluded.responsible_uid,assignee_name=excluded.assignee_name",
                params![task.id,task.project_id,task.content,task.description,serde_json::to_string(&task.labels)?,
                    task.due.as_ref().map(serde_json::to_string).transpose()?,task.priority,task.child_order,
                    task.checked || task.is_completed || task.completed_at.is_some(),task.is_deleted,task.parent_id,
                    task.responsible_uid,assignee_name],
            )?;
        }
        for label in labels {
            tx.execute(
                "INSERT INTO labels(id,name,item_order,deleted) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,item_order=excluded.item_order,deleted=excluded.deleted",
                params![label.id,label.name,label.item_order,label.is_deleted],
            )?;
        }
        for filter in filters {
            if pending_filters.contains(&filter.id) {
                continue;
            }
            tx.execute(
                "INSERT INTO filters(id,name,query,item_order,is_favorite,workspace_id,is_frozen,deleted)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,query=excluded.query,item_order=excluded.item_order,
                 is_favorite=excluded.is_favorite,workspace_id=excluded.workspace_id,
                 is_frozen=excluded.is_frozen,deleted=excluded.deleted",
                params![filter.id,filter.name,filter.query,filter.item_order,filter.is_favorite,
                    filter.workspace_id,filter.is_frozen,filter.is_deleted],
            )?;
            if !filter.is_favorite || filter.is_deleted {
                tx.execute("DELETE FROM filter_tasks WHERE filter_id=?1", [&filter.id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn merge_completed_tasks(&self, tasks: &[RemoteTask]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let collaborator_names = collaborator_names(&tx)?;
        for task in tasks {
            // Completed history can contain an earlier occurrence of a task
            // that is currently active (notably recurring tasks). Never let
            // archival history overwrite an active or locally pending row.
            let assignee_name = task
                .responsible_uid
                .as_ref()
                .and_then(|id| collaborator_names.get(id))
                .cloned();
            tx.execute(
                "INSERT INTO tasks(id,project_id,content,description,labels_json,due_json,priority,child_order,checked,deleted,parent_id,responsible_uid,assignee_name)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,content=excluded.content,
                 description=excluded.description,labels_json=excluded.labels_json,due_json=excluded.due_json,
                 priority=excluded.priority,child_order=excluded.child_order,deleted=excluded.deleted,
                 parent_id=excluded.parent_id,responsible_uid=excluded.responsible_uid,
                 assignee_name=excluded.assignee_name
                 WHERE tasks.checked=1",
                params![task.id,task.project_id,task.content,task.description,
                    serde_json::to_string(&task.labels)?,
                    task.due.as_ref().map(serde_json::to_string).transpose()?,
                    task.priority,task.child_order,task.is_deleted,task.parent_id,
                    task.responsible_uid,assignee_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn replace_filter_membership(&self, filter_id: &str, tasks: &[RemoteTask]) -> Result<()> {
        let conn = self.connect()?;
        let tx = conn.unchecked_transaction()?;
        let collaborator_names = collaborator_names(&tx)?;
        let (pending_tasks, _, _) = pending_entity_ids(&tx)?;
        // Refresh active matches while retaining cached membership for tasks
        // completed from this filter, so they remain available in its history.
        tx.execute(
            "DELETE FROM filter_tasks
             WHERE filter_id=?1 AND task_id NOT IN (SELECT id FROM tasks WHERE checked=1)",
            [filter_id],
        )?;
        for task in tasks {
            if !pending_tasks.contains(&task.id) {
                let assignee_name = task
                    .responsible_uid
                    .as_ref()
                    .and_then(|id| collaborator_names.get(id))
                    .cloned();
                tx.execute(
                    "INSERT INTO tasks(id,project_id,content,description,labels_json,due_json,priority,child_order,checked,deleted,parent_id,responsible_uid,assignee_name)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                     ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id,content=excluded.content,
                     description=excluded.description,labels_json=excluded.labels_json,due_json=excluded.due_json,
                     priority=excluded.priority,child_order=excluded.child_order,checked=excluded.checked,
                     deleted=excluded.deleted,parent_id=excluded.parent_id,
                     responsible_uid=excluded.responsible_uid,assignee_name=excluded.assignee_name",
                    params![task.id,task.project_id,task.content,task.description,serde_json::to_string(&task.labels)?,
                        task.due.as_ref().map(serde_json::to_string).transpose()?,task.priority,task.child_order,
                        task.checked || task.is_completed || task.completed_at.is_some(),task.is_deleted,task.parent_id,
                        task.responsible_uid,assignee_name],
                )?;
            }
            if !task.checked && !task.is_deleted {
                tx.execute(
                    "INSERT OR IGNORE INTO filter_tasks(filter_id,task_id) VALUES(?1,?2)",
                    params![filter_id, task.id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectDisplayGroup {
    Hidden,
    Space(u8),
}

fn project_display_group(
    project: &Project,
    projects_by_id: &HashMap<&str, &Project>,
) -> ProjectDisplayGroup {
    if project.is_hidden {
        return ProjectDisplayGroup::Hidden;
    }
    let mut root = project;
    let mut visited = HashSet::new();
    visited.insert(root.id.as_str());
    while let Some(parent) = root
        .parent_id
        .as_deref()
        .and_then(|parent_id| projects_by_id.get(parent_id).copied())
        .filter(|parent| !parent.is_hidden && visited.insert(parent.id.as_str()))
    {
        root = parent;
    }
    ProjectDisplayGroup::Space(root.space())
}

fn project_hierarchy_rows(
    projects: &[&Project],
    counts: &HashMap<String, usize>,
) -> Vec<(u8, NavRow)> {
    let visible_ids = projects
        .iter()
        .map(|project| project.id.as_str())
        .collect::<HashSet<_>>();
    let mut visited = HashSet::new();
    let mut rows = Vec::with_capacity(projects.len());

    for project in projects.iter().copied().filter(|project| {
        project
            .parent_id
            .as_deref()
            .is_none_or(|parent| parent == project.id || !visible_ids.contains(parent))
    }) {
        append_project_tree(
            project,
            projects,
            counts,
            project.space(),
            0,
            &mut visited,
            &mut rows,
        );
    }
    // Malformed remote data must not hide projects. Treat any cycle or otherwise
    // unreachable project as a root while still preventing duplicate rows.
    for project in projects.iter().copied() {
        if !visited.contains(&project.id) {
            append_project_tree(
                project,
                projects,
                counts,
                project.space(),
                0,
                &mut visited,
                &mut rows,
            );
        }
    }
    rows
}

fn append_project_tree(
    project: &Project,
    projects: &[&Project],
    counts: &HashMap<String, usize>,
    root_space: u8,
    depth: usize,
    visited: &mut HashSet<String>,
    rows: &mut Vec<(u8, NavRow)>,
) {
    if !visited.insert(project.id.clone()) {
        return;
    }
    rows.push((
        root_space,
        NavRow::Item {
            id: NavId::Project(project.id.clone()),
            name: project.name.clone(),
            count: *counts.get(&project.id).unwrap_or(&0),
            is_inbox: false,
            depth,
        },
    ));
    for child in projects
        .iter()
        .copied()
        .filter(|child| child.parent_id.as_deref() == Some(project.id.as_str()))
    {
        append_project_tree(
            child,
            projects,
            counts,
            root_space,
            depth + 1,
            visited,
            rows,
        );
    }
}

fn projects_from(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,parent_id,child_order,workspace_id,is_inbox,is_shared,can_assign,space_override,is_hidden
         FROM projects WHERE deleted=0 ORDER BY child_order,id",
    )?;
    let rows = stmt.query_map([], row_to_project)?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn favorite_filters_from(conn: &Connection) -> Result<Vec<Filter>> {
    filters_from(conn, true)
}

fn filters_from(conn: &Connection, favorites_only: bool) -> Result<Vec<Filter>> {
    let sql = if favorites_only {
        "SELECT id,name,query,item_order,is_favorite,workspace_id,is_frozen FROM filters
         WHERE deleted=0 AND is_favorite=1 ORDER BY item_order,lower(name),id"
    } else {
        "SELECT id,name,query,item_order,is_favorite,workspace_id,is_frozen FROM filters
         WHERE deleted=0 ORDER BY item_order,lower(name),id"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], row_to_filter)?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn filter_from(conn: &Connection, id: &str) -> Result<Option<Filter>> {
    Ok(conn
        .query_row(
            "SELECT id,name,query,item_order,is_favorite,workspace_id,is_frozen
             FROM filters WHERE id=?1 AND deleted=0",
            [id],
            row_to_filter,
        )
        .optional()?)
}

fn row_to_filter(row: &rusqlite::Row<'_>) -> rusqlite::Result<Filter> {
    Ok(Filter {
        id: row.get(0)?,
        name: row.get(1)?,
        query: row.get(2)?,
        item_order: row.get(3)?,
        is_favorite: row.get(4)?,
        workspace_id: row.get(5)?,
        is_frozen: row.get(6)?,
    })
}

fn project_from(conn: &Connection, id: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT id,name,parent_id,child_order,workspace_id,is_inbox,is_shared,can_assign,space_override,is_hidden
             FROM projects WHERE id=?1 AND deleted=0",
            [id],
            row_to_project,
        )
        .optional()?)
}

fn task_from(conn: &Connection, id: &str) -> Result<Option<Task>> {
    Ok(conn
        .query_row(
            "SELECT id,project_id,content,description,labels_json,due_json,priority,child_order,checked,parent_id,assignee_name
             FROM tasks WHERE id=?1 AND deleted=0",
            [id],
            row_to_task,
        )
        .optional()?)
}

fn collaborator_names(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT id,full_name FROM collaborators")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn resolve_task_id_from(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT new_id FROM task_id_aliases WHERE old_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

fn resolve_filter_id_from(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT new_id FROM filter_id_aliases WHERE old_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

fn resolve_project_id_from(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT new_id FROM project_id_aliases WHERE old_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?)
}

fn meta_from(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
}

/// Returns true when the column was added by this call.
fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<bool> {
    if has_column(conn, table, column)? {
        return Ok(false);
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))?;
    Ok(true)
}

fn copy_legacy_column(
    conn: &Connection,
    table: &str,
    destination: &str,
    source: &str,
) -> Result<()> {
    if has_column(conn, table, source)? {
        conn.execute_batch(&format!(
            "UPDATE {table} SET {destination}={source} WHERE {source} IS NOT NULL"
        ))?;
    }
    Ok(())
}

fn migrate_legacy_spaces(conn: &Connection) -> Result<()> {
    if has_column(conn, "projects", "group_override")? {
        conn.execute(
            "UPDATE projects SET space_override=CASE lower(trim(group_override))
               WHEN 'personal' THEN 1 WHEN 'team' THEN 2 WHEN 'third' THEN 3
               WHEN '0' THEN 0 WHEN '1' THEN 1 WHEN '2' THEN 2 WHEN '3' THEN 3
               WHEN '4' THEN 4 WHEN '5' THEN 5 WHEN '6' THEN 6 WHEN '7' THEN 7
               WHEN '8' THEN 8 WHEN '9' THEN 9 ELSE NULL END
             WHERE group_override IS NOT NULL",
            [],
        )?;
    }
    if has_column(conn, "projects", "team_override")? {
        conn.execute(
            "UPDATE projects SET space_override=CASE CAST(team_override AS INTEGER)
               WHEN 0 THEN 1 WHEN 1 THEN 2 WHEN 2 THEN 3 ELSE NULL END
             WHERE team_override IS NOT NULL AND space_override IS NULL",
            [],
        )?;
    }
    if has_column(conn, "projects", "group_label")? {
        conn.execute(
            "UPDATE projects SET space_override=CASE lower(trim(group_label))
               WHEN 'personal' THEN 1 WHEN 'team' THEN 2 WHEN 'third' THEN 3 ELSE 3 END
             WHERE group_label IS NOT NULL AND trim(group_label)<>'' AND space_override IS NULL",
            [],
        )?;
    }
    Ok(())
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let space: Option<u8> = row.get(8)?;
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        parent_id: row.get(2)?,
        child_order: row.get(3)?,
        workspace_id: row.get(4)?,
        is_inbox: row.get(5)?,
        is_shared: row.get(6)?,
        can_assign_tasks: row.get(7)?,
        space_override: space.filter(|space| *space < PROJECT_SPACE_COUNT),
        is_hidden: row.get(9)?,
    })
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let labels_json: String = row.get(4)?;
    let due_json: Option<String> = row.get(5)?;
    Ok(Task {
        id: row.get(0)?,
        project_id: row.get(1)?,
        content: row.get(2)?,
        description: row.get(3)?,
        labels: serde_json::from_str(&labels_json).unwrap_or_default(),
        due: due_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok()),
        priority: row.get(6)?,
        child_order: row.get(7)?,
        checked: row.get(8)?,
        parent_id: row.get(9)?,
        assignee: row.get(10)?,
    })
}

fn row_to_outbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxEntry> {
    let payload: String = row.get(3)?;
    let operation = serde_json::from_str(&payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            payload.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(OutboxEntry {
        seq: row.get(0)?,
        uuid: row.get(1)?,
        entity_id: row.get(2)?,
        operation,
    })
}

fn pending_entity_ids(
    conn: &Connection,
) -> Result<(HashSet<String>, HashSet<String>, HashSet<String>)> {
    let mut tasks = HashSet::new();
    let mut filters = HashSet::new();
    let mut projects = HashSet::new();
    let mut stmt = conn.prepare("SELECT entity_id,payload FROM outbox")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, payload) = row?;
        match serde_json::from_str::<Operation>(&payload)? {
            Operation::AddFilter { .. } | Operation::UpdateFilter { .. } => {
                filters.insert(id);
            }
            Operation::AddProject { .. } | Operation::UpdateProject { .. } => {
                projects.insert(id);
            }
            Operation::ReorderProjects {
                projects: reordered,
            } => projects.extend(reordered.into_iter().map(|project| project.id)),
            _ => {
                tasks.insert(id);
            }
        }
    }
    Ok((tasks, filters, projects))
}

fn queue(tx: &Transaction<'_>, entity_id: &str, operation: Operation) -> Result<()> {
    tx.execute(
        "INSERT INTO outbox(uuid,entity_id,payload) VALUES(?1,?2,?3)",
        params![
            Uuid::new_v4().to_string(),
            entity_id,
            serde_json::to_string(&operation)?
        ],
    )?;
    Ok(())
}

fn queue_filter_change(
    tx: &Transaction<'_>,
    entity_id: &str,
    name: &str,
    query: &str,
    is_favorite: bool,
    is_workspace: bool,
) -> Result<()> {
    let entries = {
        let mut stmt =
            tx.prepare("SELECT seq,payload FROM outbox WHERE entity_id=?1 ORDER BY seq")?;
        stmt.query_map([entity_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut pending_add = None;
    let mut stale_updates = Vec::new();
    for (seq, payload) in entries {
        match serde_json::from_str::<Operation>(&payload) {
            Ok(Operation::AddFilter { .. }) => pending_add = Some(seq),
            Ok(Operation::UpdateFilter { .. }) => stale_updates.push(seq),
            _ => {}
        }
    }
    for seq in stale_updates {
        tx.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
    }
    if let Some(seq) = pending_add {
        tx.execute(
            "UPDATE outbox SET payload=?2 WHERE seq=?1",
            params![
                seq,
                serde_json::to_string(&Operation::AddFilter {
                    name: name.to_owned(),
                    query: query.to_owned(),
                    is_favorite,
                })?
            ],
        )?;
    } else {
        queue(
            tx,
            entity_id,
            Operation::UpdateFilter {
                name: name.to_owned(),
                query: query.to_owned(),
                is_favorite,
                is_workspace,
            },
        )?;
    }
    Ok(())
}

fn queue_project_rename(tx: &Transaction<'_>, entity_id: &str, name: &str) -> Result<()> {
    let entries = {
        let mut stmt =
            tx.prepare("SELECT seq,payload FROM outbox WHERE entity_id=?1 ORDER BY seq")?;
        stmt.query_map([entity_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut pending_add = None;
    let mut stale_updates = Vec::new();
    for (seq, payload) in entries {
        match serde_json::from_str::<Operation>(&payload) {
            Ok(Operation::AddProject { .. }) => pending_add = Some(seq),
            Ok(Operation::UpdateProject { .. }) => stale_updates.push(seq),
            _ => {}
        }
    }
    for seq in stale_updates {
        tx.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
    }
    if let Some(seq) = pending_add {
        tx.execute(
            "UPDATE outbox SET payload=?2 WHERE seq=?1",
            params![
                seq,
                serde_json::to_string(&Operation::AddProject {
                    name: name.to_owned(),
                })?
            ],
        )?;
    } else {
        queue(
            tx,
            entity_id,
            Operation::UpdateProject {
                name: name.to_owned(),
            },
        )?;
    }
    Ok(())
}

fn queue_project_reorder(
    tx: &Transaction<'_>,
    entity_id: &str,
    projects: Vec<ProjectOrder>,
) -> Result<()> {
    let stale = {
        let mut stmt = tx.prepare("SELECT seq,payload FROM outbox ORDER BY seq")?;
        stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|row| match row {
            Ok((seq, payload))
                if matches!(
                    serde_json::from_str::<Operation>(&payload),
                    Ok(Operation::ReorderProjects { .. })
                ) =>
            {
                Some(Ok(seq))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for seq in stale {
        tx.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
    }
    queue(tx, entity_id, Operation::ReorderProjects { projects })
}

fn remove_pending_due_updates(conn: &Connection, entity_id: &str) -> Result<()> {
    let sequences = {
        let mut stmt = conn.prepare("SELECT seq,payload FROM outbox WHERE entity_id=?1")?;
        let rows = stmt.query_map([entity_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut sequences = Vec::new();
        for row in rows {
            let (seq, payload) = row?;
            if matches!(
                serde_json::from_str::<Operation>(&payload),
                Ok(Operation::UpdateDue { .. })
            ) {
                sequences.push(seq);
            }
        }
        sequences
    };
    for seq in sequences {
        conn.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
    }
    Ok(())
}

fn compact_pending_due_updates(conn: &Connection) -> Result<()> {
    let stale_sequences = {
        let mut stmt = conn.prepare("SELECT seq,entity_id,payload FROM outbox ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut latest_by_entity = HashMap::<String, i64>::new();
        let mut stale = Vec::new();
        for row in rows {
            let (seq, entity_id, payload) = row?;
            if matches!(
                serde_json::from_str::<Operation>(&payload),
                Ok(Operation::UpdateDue { .. })
            ) && let Some(previous) = latest_by_entity.insert(entity_id, seq)
            {
                stale.push(previous);
            }
        }
        stale
    };
    if stale_sequences.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for seq in stale_sequences {
        tx.execute("DELETE FROM outbox WHERE seq=?1", [seq])?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> Store {
        let dir = tempdir().unwrap().keep();
        Store::open(dir.join("test.db")).unwrap()
    }

    fn remote_filter(id: &str, name: &str, query: &str, favorite: bool) -> RemoteFilter {
        RemoteFilter {
            id: id.into(),
            name: name.into(),
            query: query.into(),
            workspace_id: None,
            item_order: 0,
            is_favorite: favorite,
            is_frozen: false,
            is_deleted: false,
        }
    }

    fn remote_project(id: &str, name: &str) -> RemoteProject {
        RemoteProject {
            id: id.into(),
            name: name.into(),
            parent_id: None,
            child_order: 1,
            workspace_id: None,
            is_inbox_project: false,
            shared: false,
            can_assign_tasks: false,
            is_deleted: false,
            is_archived: false,
        }
    }

    #[test]
    fn local_project_add_and_rename_share_one_pending_operation() {
        let store = store();
        let id = store.add_project("Initial").unwrap();
        store.rename_project(&id, "Corrected").unwrap();

        assert_eq!(store.project(&id).unwrap().unwrap().name, "Corrected");
        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(matches!(
            &outbox[0].operation,
            Operation::AddProject { name } if name == "Corrected"
        ));

        let project = store.project(&id).unwrap().unwrap();
        let task_id = store.add_task("Task", &project).unwrap();
        store.replace_project_id(&id, "remote-project").unwrap();
        assert_eq!(store.project(&id).unwrap().unwrap().id, "remote-project");
        assert_eq!(
            store.task(&task_id).unwrap().unwrap().project_id,
            "remote-project"
        );
        assert!(store.outbox().unwrap().iter().any(|entry| matches!(
            &entry.operation,
            Operation::QuickAdd { project_id, .. } if project_id == "remote-project"
        )));
    }

    #[test]
    fn pending_project_rename_survives_remote_refresh_and_is_coalesced() {
        let store = store();
        store
            .merge_remote(&[remote_project("p", "Remote")], &[], &[], &[])
            .unwrap();
        store.rename_project("p", "Local").unwrap();
        store.rename_project("p", "Newest").unwrap();
        store
            .merge_remote(&[remote_project("p", "Stale")], &[], &[], &[])
            .unwrap();

        assert_eq!(store.project("p").unwrap().unwrap().name, "Newest");
        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(matches!(
            outbox[0].operation,
            Operation::UpdateProject { .. }
        ));
    }

    #[test]
    fn local_filter_add_and_correction_share_one_pending_operation() {
        let store = store();
        let id = store.add_filter("Initial", "invalid", true).unwrap();
        store
            .update_filter(&id, "Corrected", "today & p1", false)
            .unwrap();

        let filters = store.filters().unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].name, "Corrected");
        assert!(!filters[0].is_favorite);
        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(matches!(
            &outbox[0].operation,
            Operation::AddFilter { name, query, is_favorite }
                if name == "Corrected" && query == "today & p1" && !is_favorite
        ));

        store.replace_filter_id(&id, "remote-filter").unwrap();
        assert_eq!(store.filter(&id).unwrap().unwrap().id, "remote-filter");
        assert_eq!(store.outbox().unwrap()[0].entity_id, "remote-filter");
    }

    #[test]
    fn pending_filter_edit_survives_remote_refresh_and_is_coalesced() {
        let store = store();
        store
            .merge_remote(
                &[],
                &[],
                &[],
                &[remote_filter("f", "Remote", "today", true)],
            )
            .unwrap();
        store.update_filter("f", "Local", "overdue", false).unwrap();
        store
            .update_filter("f", "Newest", "tomorrow", true)
            .unwrap();
        store
            .merge_remote(
                &[],
                &[],
                &[],
                &[remote_filter("f", "Stale", "today", false)],
            )
            .unwrap();

        let filter = store.filter("f").unwrap().unwrap();
        assert_eq!(filter.name, "Newest");
        assert_eq!(filter.query, "tomorrow");
        assert!(filter.is_favorite);
        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(matches!(
            outbox[0].operation,
            Operation::UpdateFilter { .. }
        ));
    }

    #[test]
    fn migrates_legacy_database_without_losing_cached_data() {
        let dir = tempdir().unwrap().keep();
        let path = dir.join("legacy.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, position INTEGER NOT NULL,
               is_inbox INTEGER NOT NULL, is_shared INTEGER NOT NULL, workspace_id TEXT,
               can_assign_tasks INTEGER NOT NULL, team_override INTEGER, group_label TEXT NOT NULL,
               group_override TEXT
             );
             CREATE TABLE tasks (
               id TEXT PRIMARY KEY, project_id TEXT NOT NULL, content TEXT NOT NULL,
               description TEXT NOT NULL, due TEXT, priority INTEGER NOT NULL,
               position INTEGER NOT NULL, completed INTEGER NOT NULL, labels TEXT NOT NULL
             );
             CREATE TABLE filters (id TEXT PRIMARY KEY, name TEXT NOT NULL, query TEXT NOT NULL, position INTEGER NOT NULL);
             CREATE TABLE filter_tasks (filter_id TEXT NOT NULL, task_id TEXT NOT NULL, PRIMARY KEY(filter_id,task_id));
             CREATE TABLE operations (id INTEGER PRIMARY KEY AUTOINCREMENT, action TEXT NOT NULL, entity_id TEXT NOT NULL, payload TEXT NOT NULL);
             INSERT INTO projects VALUES('old-inbox','Inbox',0,1,0,NULL,0,NULL,'',NULL);
             INSERT INTO projects VALUES('p','Shared',1,0,1,'w',1,2,'','third');
             INSERT INTO tasks VALUES('t','p','Legacy task','description','tomorrow',4,7,0,'[\"Work\"]');
             INSERT INTO filters VALUES('f','Important','p1',3);
             INSERT INTO filter_tasks VALUES('f','t');"
        ).unwrap();
        drop(conn);

        let store = Store::open(path).unwrap();
        assert_eq!(
            store
                .projects()
                .unwrap()
                .iter()
                .filter(|p| p.is_inbox)
                .count(),
            1
        );
        assert_eq!(store.project("p").unwrap().unwrap().space(), 3);
        let task = store
            .tasks_for(&NavId::Project("p".into()))
            .unwrap()
            .remove(0);
        assert_eq!(task.labels, ["Work"]);
        assert_eq!(task.due.unwrap().display(), Some("tomorrow"));
        assert_eq!(store.favorite_filters().unwrap()[0].name, "Important");
    }

    #[test]
    fn assignee_migration_requests_one_full_refresh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("todoist-tui.db");
        let store = Store::open(path.clone()).unwrap();
        store.set_sync_token("incremental-token").unwrap();
        drop(store);

        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
        drop(conn);

        let migrated = Store::open(path).unwrap();
        assert_eq!(migrated.sync_token().unwrap(), "*");
    }

    #[test]
    fn starts_with_exactly_one_inbox() {
        let store = store();
        let rows = store.nav_rows(false).unwrap();
        assert!(matches!(&rows[0], NavRow::Item { name, is_inbox: true, .. } if name == "Inbox"));
        assert!(matches!(&rows[1], NavRow::Item { id: NavId::Today, name, .. } if name == "Today"));
        assert!(
            matches!(&rows[2], NavRow::Item { id: NavId::Upcoming, name, .. } if name == "Upcoming")
        );
        assert_eq!(
            store
                .projects()
                .unwrap()
                .iter()
                .filter(|p| p.is_inbox)
                .count(),
            1
        );
    }

    #[test]
    fn global_task_search_source_includes_active_and_completed_tasks() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let active_id = store.add_task("Active task", &inbox).unwrap();
        let completed_id = store.add_task("Completed task", &inbox).unwrap();
        store.set_task_completed(&completed_id, true).unwrap();

        let tasks = store.tasks_for_search().unwrap();
        assert!(tasks.iter().any(|(task, project)| {
            task.id == active_id && !task.checked && project == "Inbox"
        }));
        assert!(tasks.iter().any(|(task, project)| {
            task.id == completed_id && task.checked && project == "Inbox"
        }));
    }

    #[test]
    fn today_and_upcoming_are_offline_due_date_views() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let today = Local::now().date_naive();
        let overdue_id = store.add_task("Overdue", &inbox).unwrap();
        let today_id = store.add_task("Today", &inbox).unwrap();
        let upcoming_id = store.add_task("Upcoming", &inbox).unwrap();
        let last_visible_id = store.add_task("Last visible day", &inbox).unwrap();
        let outside_window_id = store.add_task("Outside window", &inbox).unwrap();
        store.add_task("No date", &inbox).unwrap();
        store
            .update_due(
                &overdue_id,
                &(today - chrono::Duration::days(1)).format("%F").to_string(),
            )
            .unwrap();
        store
            .update_due(&today_id, &today.format("%F").to_string())
            .unwrap();
        store
            .update_due(
                &upcoming_id,
                &(today + chrono::Duration::days(1)).format("%F").to_string(),
            )
            .unwrap();
        store
            .update_due(
                &last_visible_id,
                &(today + chrono::Duration::days(6)).format("%F").to_string(),
            )
            .unwrap();
        store
            .update_due(
                &outside_window_id,
                &(today + chrono::Duration::days(7)).format("%F").to_string(),
            )
            .unwrap();

        let today_tasks = store.tasks_for(&NavId::Today).unwrap();
        assert_eq!(
            today_tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            [overdue_id.as_str(), today_id.as_str()]
        );
        let upcoming_tasks = store.tasks_for(&NavId::Upcoming).unwrap();
        assert_eq!(
            upcoming_tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            [
                today_id.as_str(),
                upcoming_id.as_str(),
                last_visible_id.as_str()
            ]
        );

        let rows = store.nav_rows(false).unwrap();
        assert!(matches!(
            &rows[1],
            NavRow::Item {
                id: NavId::Today,
                count: 2,
                ..
            }
        ));
        assert!(matches!(
            &rows[2],
            NavRow::Item {
                id: NavId::Upcoming,
                count: 3,
                ..
            }
        ));
    }

    #[test]
    fn local_edit_and_outbox_commit_together() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Buy milk tomorrow", &inbox).unwrap();
        store.update_labels(&id, &["Errands".into()]).unwrap();
        assert_eq!(
            store.tasks_for(&NavId::Project(inbox.id)).unwrap()[0].labels,
            ["Errands"]
        );
        assert_eq!(store.outbox().unwrap().len(), 2);
        assert_eq!(store.outbox_len().unwrap(), 2);
        assert!(matches!(
            store.next_outbox().unwrap().unwrap().operation,
            Operation::QuickAdd { .. }
        ));
    }

    #[test]
    fn subtasks_and_hierarchy_moves_are_applied_locally_and_queued() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let parent_id = store.add_task("Parent", &inbox).unwrap();
        let child_id = store.add_child_task("Child", &inbox, &parent_id).unwrap();
        let sibling_id = store.add_task("Sibling", &inbox).unwrap();

        assert_eq!(
            store.task(&child_id).unwrap().unwrap().parent_id.as_deref(),
            Some(parent_id.as_str())
        );
        assert!(matches!(
            store.outbox().unwrap()[1].operation,
            Operation::QuickAdd {
                parent_id: Some(ref id),
                ..
            } if id == &parent_id
        ));

        store.set_task_parent(&sibling_id, Some(&child_id)).unwrap();
        assert_eq!(
            store
                .task(&sibling_id)
                .unwrap()
                .unwrap()
                .parent_id
                .as_deref(),
            Some(child_id.as_str())
        );
        assert!(matches!(
            store.outbox().unwrap().last().unwrap().operation,
            Operation::MoveTaskParent {
                parent_id: Some(ref id)
            } if id == &child_id
        ));
        assert!(
            store
                .set_task_parent(&parent_id, Some(&sibling_id))
                .unwrap_err()
                .to_string()
                .contains("own descendant")
        );

        store.set_task_parent(&sibling_id, None).unwrap();
        assert_eq!(store.task(&sibling_id).unwrap().unwrap().parent_id, None);
        assert!(matches!(
            store.outbox().unwrap().last().unwrap().operation,
            Operation::MoveTaskParent { parent_id: None }
        ));
    }

    #[test]
    fn corrected_due_date_replaces_the_pending_due_operation() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Dated task", &inbox).unwrap();

        store.update_due(&id, "not a real date").unwrap();
        store.update_due(&id, "tomorrow at 9").unwrap();

        let due_updates = store
            .outbox()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.operation {
                Operation::UpdateDue { due_string } => Some(due_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(due_updates, ["tomorrow at 9"]);
    }

    #[test]
    fn task_edits_queue_the_complete_quick_add_expression() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Original", &inbox).unwrap();

        store
            .edit_task(&id, "Buy milk tomorrow @errands p1", "Two bottles")
            .unwrap();

        let edit = store
            .outbox()
            .unwrap()
            .into_iter()
            .find_map(|entry| match entry.operation {
                Operation::QuickEdit {
                    expression,
                    description,
                    update_uuid,
                    move_uuid,
                    delete_uuid,
                } => Some((expression, description, update_uuid, move_uuid, delete_uuid)),
                _ => None,
            })
            .unwrap();
        assert_eq!(edit.0, "Buy milk tomorrow @errands p1");
        assert_eq!(edit.1, "Two bottles");
        assert!(!edit.2.is_empty());
        assert!(!edit.3.is_empty());
        assert!(!edit.4.is_empty());
    }

    #[test]
    fn an_older_synced_edit_does_not_overwrite_newer_local_text() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Original", &inbox).unwrap();
        store
            .edit_task(&id, "First edit tomorrow", "First")
            .unwrap();
        store
            .edit_task(&id, "Second edit next week", "Second")
            .unwrap();
        let first_edit_seq = store
            .outbox()
            .unwrap()
            .into_iter()
            .find(|entry| matches!(entry.operation, Operation::QuickEdit { .. }))
            .unwrap()
            .seq;

        let mut parsed_first = store.task(&id).unwrap().unwrap();
        parsed_first.content = "First edit".into();
        parsed_first.description = "First".into();
        parsed_first.due = Some(Due {
            string: Some("tomorrow".into()),
            ..Due::default()
        });
        store
            .apply_synced_task_edit(&parsed_first, first_edit_seq)
            .unwrap();

        let current = store.task(&id).unwrap().unwrap();
        assert_eq!(current.content, "Second edit next week");
        assert_eq!(current.description, "Second");
        assert_eq!(current.due.unwrap().string.as_deref(), Some("tomorrow"));
    }

    #[test]
    fn reopening_store_discards_obsolete_pending_due_operations() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Dated task", &inbox).unwrap();
        let conn = store.connect().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        queue(
            &tx,
            &id,
            Operation::UpdateDue {
                due_string: "invalid date".into(),
            },
        )
        .unwrap();
        queue(
            &tx,
            &id,
            Operation::UpdateDue {
                due_string: "tomorrow".into(),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);
        drop(store);

        let reopened = Store::open(path).unwrap();
        let due_updates = reopened
            .outbox()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.operation {
                Operation::UpdateDue { due_string } => Some(due_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(due_updates, ["tomorrow"]);
    }

    #[test]
    fn completed_tasks_can_be_shown_and_reopened_offline() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Finish release", &inbox).unwrap();

        store.set_task_completed(&id, true).unwrap();
        assert!(
            store
                .tasks_for(&NavId::Project(inbox.id.clone()))
                .unwrap()
                .is_empty()
        );
        let completed = store
            .tasks_for_view(&NavId::Project(inbox.id.clone()), true)
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert!(completed[0].checked);

        store.set_task_completed(&id, false).unwrap();
        assert!(!store.tasks_for(&NavId::Project(inbox.id)).unwrap()[0].checked);
        let operations = store.outbox().unwrap();
        assert!(matches!(operations[1].operation, Operation::CompleteTask));
        assert!(matches!(operations[2].operation, Operation::UncompleteTask));
    }

    #[test]
    fn filter_refresh_retains_cached_completed_membership() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let id = store.add_task("Filtered task", &inbox).unwrap();
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO filters(id,name,query,is_favorite) VALUES('f','Favorite','today',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO filter_tasks(filter_id,task_id) VALUES('f',?1)",
            [&id],
        )
        .unwrap();
        drop(conn);

        store.set_task_completed(&id, true).unwrap();
        store.replace_filter_membership("f", &[]).unwrap();
        let tasks = store
            .tasks_for_view(&NavId::Filter("f".into()), true)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].checked);
    }

    #[test]
    fn moving_task_updates_local_project_and_queues_remote_move() {
        let store = store();
        store
            .merge_remote(
                &[RemoteProject {
                    id: "destination".into(),
                    name: "Destination".into(),
                    parent_id: None,
                    child_order: 1,
                    workspace_id: None,
                    is_inbox_project: false,
                    shared: false,
                    can_assign_tasks: false,
                    is_deleted: false,
                    is_archived: false,
                }],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let inbox = store
            .projects()
            .unwrap()
            .into_iter()
            .find(|p| p.is_inbox)
            .unwrap();
        let task_id = store.add_task("Move me", &inbox).unwrap();
        store.move_task(&task_id, "destination").unwrap();
        assert!(
            store
                .tasks_for(&NavId::Project(inbox.id))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .tasks_for(&NavId::Project("destination".into()))
                .unwrap()[0]
                .id,
            task_id
        );
        assert!(matches!(
            store.outbox().unwrap().last().unwrap().operation,
            Operation::MoveTask { ref project_id } if project_id == "destination"
        ));
    }

    #[test]
    fn override_survives_remote_project_merge() {
        let store = store();
        store
            .merge_remote(
                &[RemoteProject {
                    id: "p".into(),
                    name: "P".into(),
                    parent_id: None,
                    child_order: 1,
                    workspace_id: None,
                    is_inbox_project: false,
                    shared: false,
                    can_assign_tasks: false,
                    is_deleted: false,
                    is_archived: false,
                }],
                &[],
                &[],
                &[],
            )
            .unwrap();
        store.toggle_space("p", 7).unwrap();
        store
            .merge_remote(
                &[RemoteProject {
                    id: "p".into(),
                    name: "P2".into(),
                    parent_id: None,
                    child_order: 2,
                    workspace_id: None,
                    is_inbox_project: false,
                    shared: false,
                    can_assign_tasks: false,
                    is_deleted: false,
                    is_archived: false,
                }],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(store.project("p").unwrap().unwrap().space(), 7);
        assert_eq!(store.toggle_space("p", 7).unwrap(), (1, true));
        assert_eq!(store.project("p").unwrap().unwrap().space_override, None);
    }

    #[test]
    fn hidden_project_membership_persists_and_controls_navigation() {
        let store = store();
        let project = RemoteProject {
            id: "later".into(),
            name: "Later".into(),
            parent_id: None,
            child_order: 1,
            workspace_id: None,
            is_inbox_project: false,
            shared: false,
            can_assign_tasks: false,
            is_deleted: false,
            is_archived: false,
        };
        store
            .merge_remote(std::slice::from_ref(&project), &[], &[], &[])
            .unwrap();
        assert!(store.toggle_project_hidden("later").unwrap());
        store.merge_remote(&[project], &[], &[], &[]).unwrap();
        assert!(store.project("later").unwrap().unwrap().is_hidden);

        let collapsed = store.nav_rows(false).unwrap();
        assert!(
            !collapsed
                .iter()
                .any(|row| matches!(row, NavRow::Item { name, .. } if name == "Later"))
        );
        let expanded = store.nav_rows(true).unwrap();
        assert!(
            expanded
                .iter()
                .any(|row| matches!(row, NavRow::Item { name, .. } if name == "Later"))
        );

        assert!(!store.toggle_project_hidden("later").unwrap());
        assert!(!store.project("later").unwrap().unwrap().is_hidden);
    }

    #[test]
    fn project_reordering_preserves_hidden_and_other_group_positions() {
        let store = store();
        let make_project = |id: &str, name: &str, child_order| RemoteProject {
            id: id.into(),
            name: name.into(),
            parent_id: None,
            child_order,
            workspace_id: None,
            is_inbox_project: false,
            shared: false,
            can_assign_tasks: false,
            is_deleted: false,
            is_archived: false,
        };
        let remote = vec![
            make_project("a", "A", 1),
            make_project("hidden", "Hidden", 2),
            make_project("b", "B", 3),
            make_project("other", "Other group", 4),
        ];
        store.merge_remote(&remote, &[], &[], &[]).unwrap();
        assert!(store.toggle_project_hidden("hidden").unwrap());
        assert_eq!(store.toggle_space("other", 2).unwrap(), (2, false));

        assert!(store.move_project("a", true).unwrap());
        let ordered = store
            .projects()
            .unwrap()
            .into_iter()
            .filter(|project| !project.is_inbox)
            .map(|project| (project.id, project.child_order))
            .collect::<Vec<_>>();
        assert_eq!(
            ordered,
            [("b", 0), ("hidden", 1), ("a", 2), ("other", 3)]
                .map(|(id, order)| (id.to_owned(), order))
        );

        let outbox = store.outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(matches!(
            &outbox[0].operation,
            Operation::ReorderProjects { projects }
                if projects.iter().map(|project| project.id.as_str()).collect::<Vec<_>>()
                    == ["b", "hidden", "a", "other"]
        ));

        // A stale pull cannot overwrite any part of an order waiting to sync.
        store.merge_remote(&remote, &[], &[], &[]).unwrap();
        let retained = store
            .projects()
            .unwrap()
            .into_iter()
            .filter(|project| !project.is_inbox)
            .map(|project| project.id)
            .collect::<Vec<_>>();
        assert_eq!(retained, ["b", "hidden", "a", "other"]);
        assert!(!store.move_project("a", true).unwrap());
    }

    #[test]
    fn inbox_views_and_filters_can_share_the_persistent_hidden_group() {
        let dir = tempdir().unwrap().keep();
        let path = dir.join("hidden-navigation.db");
        let store = Store::open(&path).unwrap();
        let inbox_id = store
            .projects()
            .unwrap()
            .into_iter()
            .find(|project| project.is_inbox)
            .unwrap()
            .id;
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO filters(id,name,query,is_favorite) VALUES('filter','Favorite','p1',1)",
                [],
            )
            .unwrap();

        assert!(
            store
                .toggle_navigation_hidden(&NavId::Project(inbox_id.clone()))
                .unwrap()
        );
        assert!(store.toggle_navigation_hidden(&NavId::Today).unwrap());
        assert!(store.toggle_navigation_hidden(&NavId::Upcoming).unwrap());
        assert!(
            store
                .toggle_navigation_hidden(&NavId::Filter("filter".into()))
                .unwrap()
        );
        assert!(store.nav_rows(false).unwrap().is_empty());

        drop(store);
        let reopened = Store::open(path).unwrap();
        let names = reopened
            .nav_rows(true)
            .unwrap()
            .into_iter()
            .filter_map(|row| match row {
                NavRow::Item { name, .. } => Some(name),
                NavRow::Separator => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["Inbox", "Today", "Upcoming", "Favorite"]);

        assert!(!reopened.toggle_navigation_hidden(&NavId::Today).unwrap());
        assert!(matches!(
            reopened.nav_rows(false).unwrap().as_slice(),
            [NavRow::Item {
                id: NavId::Today,
                ..
            }]
        ));
    }

    #[test]
    fn navigation_orders_non_empty_spaces_numerically() {
        let store = store();
        let projects = [
            RemoteProject {
                id: "personal".into(),
                name: "Personal".into(),
                parent_id: None,
                child_order: 1,
                workspace_id: None,
                is_inbox_project: false,
                shared: false,
                can_assign_tasks: false,
                is_deleted: false,
                is_archived: false,
            },
            RemoteProject {
                id: "team".into(),
                name: "Team".into(),
                parent_id: None,
                child_order: 2,
                workspace_id: Some("workspace".into()),
                is_inbox_project: false,
                shared: true,
                can_assign_tasks: true,
                is_deleted: false,
                is_archived: false,
            },
            RemoteProject {
                id: "zero".into(),
                name: "Zero".into(),
                parent_id: None,
                child_order: 3,
                workspace_id: None,
                is_inbox_project: false,
                shared: false,
                can_assign_tasks: false,
                is_deleted: false,
                is_archived: false,
            },
            RemoteProject {
                id: "nine".into(),
                name: "Nine".into(),
                parent_id: None,
                child_order: 4,
                workspace_id: None,
                is_inbox_project: false,
                shared: false,
                can_assign_tasks: false,
                is_deleted: false,
                is_archived: false,
            },
        ];
        store.merge_remote(&projects, &[], &[], &[]).unwrap();
        store.toggle_space("zero", 0).unwrap();
        store.toggle_space("nine", 9).unwrap();

        let names = store
            .nav_rows(false)
            .unwrap()
            .into_iter()
            .filter_map(|row| match row {
                NavRow::Item { name, .. } => Some(name),
                NavRow::Separator => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Inbox", "Today", "Upcoming", "Zero", "Personal", "Team", "Nine"
            ]
        );
    }

    #[test]
    fn navigation_keeps_child_projects_directly_below_their_parent() {
        let store = store();
        let project = |id: &str, name: &str, parent_id: Option<&str>, child_order| RemoteProject {
            id: id.into(),
            name: name.into(),
            parent_id: parent_id.map(str::to_owned),
            child_order,
            workspace_id: None,
            is_inbox_project: false,
            shared: false,
            can_assign_tasks: false,
            is_deleted: false,
            is_archived: false,
        };
        store
            .merge_remote(
                &[
                    project("parent", "Parent", None, 1),
                    project("other", "Other", None, 2),
                    project("child", "Child", Some("parent"), 99),
                ],
                &[],
                &[],
                &[],
            )
            .unwrap();

        let projects = store
            .nav_rows(false)
            .unwrap()
            .into_iter()
            .filter_map(|row| match row {
                NavRow::Item {
                    id: NavId::Project(_),
                    name,
                    depth,
                    is_inbox: false,
                    ..
                } => Some((name, depth)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            projects,
            [
                ("Parent".into(), 0),
                ("Child".into(), 1),
                ("Other".into(), 0)
            ]
        );
    }

    #[test]
    fn task_parent_relationship_survives_remote_cache_round_trip() {
        let store = store();
        let remote = RemoteTask {
            id: "child".into(),
            project_id: "inbox".into(),
            parent_id: Some("parent".into()),
            responsible_uid: None,
            content: "Child".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };

        store.merge_remote(&[], &[remote], &[], &[]).unwrap();

        assert_eq!(
            store.task("child").unwrap().unwrap().parent_id.as_deref(),
            Some("parent")
        );
    }

    #[test]
    fn collaborator_names_are_cached_and_refreshed_for_assigned_tasks() {
        let store = store();
        store
            .merge_collaborators(&[RemoteCollaborator {
                id: "person-1".into(),
                full_name: "Ada Lovelace".into(),
            }])
            .unwrap();
        let remote = RemoteTask {
            id: "assigned".into(),
            project_id: "inbox".into(),
            parent_id: None,
            responsible_uid: Some("person-1".into()),
            content: "Review".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };
        store.merge_remote(&[], &[remote], &[], &[]).unwrap();
        assert_eq!(
            store.task("assigned").unwrap().unwrap().assignee.as_deref(),
            Some("Ada Lovelace")
        );

        store
            .merge_collaborators(&[RemoteCollaborator {
                id: "person-1".into(),
                full_name: "Ada Byron".into(),
            }])
            .unwrap();
        assert_eq!(
            store.task("assigned").unwrap().unwrap().assignee.as_deref(),
            Some("Ada Byron")
        );
    }

    #[test]
    fn canonical_remote_inbox_absorbs_local_inbox() {
        let store = store();
        let local = store.projects().unwrap().remove(0);
        store.add_task("offline", &local).unwrap();
        store
            .toggle_navigation_hidden(&NavId::Project(local.id.clone()))
            .unwrap();
        store
            .merge_remote(
                &[RemoteProject {
                    id: "remote-inbox".into(),
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
            )
            .unwrap();
        let projects = store.projects().unwrap();
        assert_eq!(projects.iter().filter(|p| p.is_inbox).count(), 1);
        assert_eq!(
            projects.iter().find(|p| p.is_inbox).unwrap().id,
            "remote-inbox"
        );
        assert!(projects.iter().find(|p| p.is_inbox).unwrap().is_hidden);
        assert_eq!(
            store
                .tasks_for(&NavId::Project("remote-inbox".into()))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pending_local_task_is_not_overwritten_by_pull() {
        let store = store();
        let remote = RemoteTask {
            id: "t".into(),
            project_id: "inbox".into(),
            parent_id: None,
            responsible_uid: None,
            content: "Remote".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };
        store
            .merge_remote(&[], std::slice::from_ref(&remote), &[], &[])
            .unwrap();
        store.edit_task("t", "Local", "").unwrap();
        store.merge_remote(&[], &[remote], &[], &[]).unwrap();
        assert_eq!(
            store.tasks_for(&NavId::Project("inbox".into())).unwrap()[0].content,
            "Local"
        );
    }

    #[test]
    fn completed_history_response_is_stored_as_completed() {
        let store = store();
        let remote = RemoteTask {
            id: "completed-task".into(),
            project_id: "inbox".into(),
            parent_id: None,
            responsible_uid: None,
            content: "Already done".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: Some("2026-10-12T10:00:00Z".into()),
            is_deleted: false,
        };

        store.merge_completed_tasks(&[remote]).unwrap();
        assert!(
            store
                .tasks_for(&NavId::Project("inbox".into()))
                .unwrap()
                .is_empty()
        );
        let tasks = store
            .tasks_for_view(&NavId::Project("inbox".into()), true)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].checked);
    }

    #[test]
    fn completed_history_does_not_hide_an_active_recurring_task() {
        let store = store();
        let mut task = RemoteTask {
            id: "recurring-task".into(),
            project_id: "inbox".into(),
            parent_id: None,
            responsible_uid: None,
            content: "Daily task".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };
        store
            .merge_remote(&[], std::slice::from_ref(&task), &[], &[])
            .unwrap();
        task.completed_at = Some("2026-10-12T10:00:00Z".into());
        store.merge_completed_tasks(&[task]).unwrap();

        let tasks = store.tasks_for(&NavId::Project("inbox".into())).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].checked);
    }

    #[test]
    fn label_picker_source_includes_personal_and_shared_task_labels() {
        let store = store();
        let task = RemoteTask {
            id: "t".into(),
            project_id: "inbox".into(),
            parent_id: None,
            responsible_uid: None,
            content: "Task".into(),
            description: String::new(),
            labels: vec!["Shared".into(), "personal".into()],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };
        let personal = RemoteLabel {
            id: "label-1".into(),
            name: "Personal".into(),
            item_order: 0,
            is_deleted: false,
        };
        store.merge_remote(&[], &[task], &[personal], &[]).unwrap();

        assert_eq!(store.labels().unwrap(), ["Personal", "Shared"]);
    }

    #[test]
    fn temporary_task_id_mapping_is_durable() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let local_id = store.add_task("Milk", &inbox).unwrap();
        store.edit_task(&local_id, "Oat milk", "").unwrap();
        let remote = RemoteTask {
            id: "remote-task".into(),
            project_id: inbox.id,
            parent_id: None,
            responsible_uid: None,
            content: "Milk".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };
        store.replace_task_id(&local_id, &remote).unwrap();
        assert_eq!(
            store.resolve_task_id(&local_id).unwrap().as_deref(),
            Some("remote-task")
        );
        assert!(
            store
                .outbox()
                .unwrap()
                .iter()
                .all(|entry| entry.entity_id == "remote-task")
        );
    }

    #[test]
    fn replacing_a_local_parent_id_updates_cached_children() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let parent_id = store.add_task("Parent", &inbox).unwrap();
        let child_id = store.add_child_task("Child", &inbox, &parent_id).unwrap();
        let remote = RemoteTask {
            id: "remote-parent".into(),
            project_id: inbox.id,
            parent_id: None,
            responsible_uid: None,
            content: "Parent".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };

        store.replace_task_id(&parent_id, &remote).unwrap();

        assert_eq!(
            store.task(&child_id).unwrap().unwrap().parent_id.as_deref(),
            Some("remote-parent")
        );
    }

    #[test]
    fn id_mapping_does_not_resurrect_an_undone_addition() {
        let store = store();
        let inbox = store.projects().unwrap().remove(0);
        let local_id = store.add_task("Temporary", &inbox).unwrap();
        store.delete_task(&local_id).unwrap();
        let remote = RemoteTask {
            id: "remote-deleted-task".into(),
            project_id: inbox.id,
            parent_id: None,
            responsible_uid: None,
            content: "Temporary".into(),
            description: String::new(),
            labels: vec![],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            is_completed: false,
            completed_at: None,
            is_deleted: false,
        };

        store.replace_task_id(&local_id, &remote).unwrap();
        assert!(
            store
                .tasks_for_view(&NavId::Project(remote.project_id), true)
                .unwrap()
                .is_empty()
        );
        assert!(store.outbox().unwrap().iter().any(|entry| {
            entry.entity_id == "remote-deleted-task"
                && matches!(entry.operation, Operation::DeleteTask)
        }));
    }
}
