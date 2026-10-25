//! Opt-in real-API ordering checks, isolated to a new disposable project/database.
use super::*;
use crate::model::NavId;

struct Fixture {
    client: TodoistClient,
    project: Option<String>,
}

impl Fixture {
    fn remove(&mut self) -> Result<()> {
        if let Some(id) = &self.project {
            TodoistClient::check(
                self.client
                    .auth(
                        self.client
                            .http
                            .delete(format!("{}/projects/{id}", self.client.base)),
                    )
                    .send()?,
            )?;
            self.project = None;
        }
        Ok(())
    }

    fn tasks(&self) -> Result<Vec<Value>> {
        let mut tasks = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = self
                .client
                .auth(self.client.http.get(format!("{}/tasks", self.client.base)))
                .query(&[
                    ("project_id", self.project.as_deref().unwrap()),
                    ("limit", "200"),
                ]);
            if let Some(cursor) = &cursor {
                request = request.query(&[("cursor", cursor)]);
            }
            let page: Page<Value> = TodoistClient::check(request.send()?)?.json()?;
            tasks.extend(page.results);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(tasks)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.remove().is_err() {
            eprintln!(
                "Disposable live-test project cleanup failed; remove project {} in Todoist.",
                self.project.as_deref().unwrap_or("(unknown)")
            );
        }
    }
}

fn flush(client: &TodoistClient, store: &Store) -> Result<()> {
    let mut operations = 0;
    while let Some(entry) = store.claim_next_outbox()? {
        operations += 1;
        anyhow::ensure!(
            operations <= 200,
            "live fixture exceeded its expected operation count"
        );
        client.push(store, &entry)?;
    }
    anyhow::ensure!(
        store.outbox_len()? == 0,
        "live fixture has unresolved dependencies"
    );
    client.pull(store)
}

fn root_ids(store: &Store, project: &str) -> Result<Vec<String>> {
    Ok(store
        .tasks_for(&NavId::Project(project.into()))?
        .into_iter()
        .filter(|task| task.parent_id.is_none() && task.section_id.is_none())
        .map(|task| task.id)
        .collect())
}

#[test]
#[ignore = "real Todoist API: requires TODOIST_TEST_API_TOKEN for an isolated test account"]
fn live_ordering_round_trip() -> Result<()> {
    let token = std::env::var("TODOIST_TEST_API_TOKEN")
        .context("set TODOIST_TEST_API_TOKEN for an isolated Todoist test account")?;
    anyhow::ensure!(!token.trim().is_empty(), "TODOIST_TEST_API_TOKEN is empty");
    let client = TodoistClient::new(token.trim().into())?;
    let second_client = TodoistClient::new(token.trim().into())?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("live-ordering.db");
    let store = Store::open(&path)?;
    client.verify_account(&store)?;
    let name = format!("todoist-tui-ordering-{}", uuid::Uuid::new_v4());
    let value: Value = TodoistClient::check(
        client
            .auth(client.http.post(format!("{}/projects", client.base)))
            .json(&json!({"name":name}))
            .send()?,
    )?
    .json()?;
    let project_id = id_value(value.get("id").context("project creation omitted ID")?)?;
    let mut fixture = Fixture {
        client: second_client,
        project: Some(project_id.clone()),
    };
    client.pull(&store)?;
    let project = store
        .project(&project_id)?
        .context("created project not returned by sync")?;
    let a = store.add_task_at_top_with_description("Order fixture A", "", &project)?;
    let b = store.add_task("Order fixture B", &project)?;
    flush(&client, &store)?;
    let a = store.task(&a)?.context("fixture A missing")?.id;
    let b = store.task(&b)?.context("fixture B missing")?.id;
    assert_eq!(root_ids(&store, &project_id)?, [a.clone(), b.clone()]);

    store.reorder_task(&a, &b)?;
    drop(store);
    let store = Store::open(&path)?;
    flush(&client, &store)?;
    assert_eq!(root_ids(&store, &project_id)?, [b.clone(), a.clone()]);
    let remote = fixture.tasks()?;
    let key = |id: &str| {
        remote
            .iter()
            .find(|task| task["id"] == id)
            .and_then(|task| task["order_key"].as_str())
            .context("server omitted a task ordering key")
    };
    anyhow::ensure!(
        key(&b)? < key(&a)?,
        "REST client did not see synchronized sibling order"
    );

    let section: Value = TodoistClient::check(
        client
            .auth(client.http.post(format!("{}/sections", client.base)))
            .json(&json!({"project_id":project_id,"name":"Order fixture section"}))
            .send()?,
    )?
    .json()?;
    let section = id_value(section.get("id").context("section creation omitted ID")?)?;
    client.pull(&store)?;
    let original = store
        .task(&a)?
        .context("fixture A missing before section move")?;
    let mut moved = original.clone();
    moved.section_id = Some(section.clone());
    store.apply_task_snapshot(&moved)?;
    flush(&client, &store)?;
    assert!(
        fixture
            .tasks()?
            .iter()
            .any(|task| task["id"] == a && task["section_id"] == section)
    );
    store.apply_task_snapshot(&original)?;
    flush(&client, &store)?;
    assert_eq!(root_ids(&store, &project_id)?, [b.clone(), a.clone()]);

    let parent = store.task(&a)?.context("fixture parent missing")?;
    let child = store.add_child_task("Order fixture child", &project, &parent.id)?;
    flush(&client, &store)?;
    let child = store.task(&child)?.context("fixture child missing")?.id;
    store.set_task_parent(&child, None)?;
    flush(&client, &store)?;
    assert!(
        fixture
            .tasks()?
            .iter()
            .any(|task| task["id"] == child && task["parent_id"].is_null())
    );
    store.set_task_parent(&child, Some(&a))?;
    flush(&client, &store)?;
    assert!(
        fixture
            .tasks()?
            .iter()
            .any(|task| task["id"] == child && task["parent_id"] == a)
    );

    let deleted = store.delete_task_tree(&a)?;
    flush(&client, &store)?;
    assert!(
        fixture
            .tasks()?
            .iter()
            .all(|task| task["id"] != a && task["id"] != child)
    );
    store.restore_deleted_tasks(&deleted)?;
    flush(&client, &store)?;
    let restored = store.task(&a)?.context("restored parent missing")?.id;
    let restored_child = store.task(&child)?.context("restored child missing")?.id;
    assert_eq!(
        root_ids(&store, &project_id)?,
        [b.clone(), restored.clone()]
    );
    assert!(
        fixture
            .tasks()?
            .iter()
            .any(|task| task["id"] == restored_child && task["parent_id"] == restored)
    );

    // A separate HTTP client changes the shared server order. Verify the TUI's
    // subsequent pull observes it without recreating or duplicating tasks.
    let remote = fixture.tasks()?;
    let key = remote
        .iter()
        .find(|task| task["id"] == b)
        .and_then(|task| task["order_key"].as_str())
        .context("neighbor key missing")?;
    let before = crate::ordering::between(None, Some(key))?;
    fixture.client.command_with_uuid(
        &uuid::Uuid::new_v4().to_string(),
        "item_update",
        json!({"id":restored,"order_key":before}),
    )?;
    client.pull(&store)?;
    assert_eq!(root_ids(&store, &project_id)?, [restored, b]);
    assert_eq!(fixture.tasks()?.len(), 3);
    fixture.remove()?;
    Ok(())
}
