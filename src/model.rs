use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INBOX_LOCAL_ID: &str = "inbox";
pub const PROJECT_SPACE_COUNT: u8 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub child_order: i64,
    pub workspace_id: Option<String>,
    pub is_inbox: bool,
    pub is_shared: bool,
    pub can_assign_tasks: bool,
    pub space_override: Option<u8>,
    pub is_hidden: bool,
    pub folder_id: Option<String>,
}

impl Project {
    pub fn default_space(&self) -> u8 {
        if self.workspace_id.is_some() || self.is_shared || self.can_assign_tasks {
            2
        } else {
            1
        }
    }

    pub fn space(&self) -> u8 {
        self.space_override.unwrap_or_else(|| self.default_space())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub id: String,
    pub name: String,
    pub query: String,
    pub item_order: i64,
    pub is_favorite: bool,
    pub workspace_id: Option<String>,
    pub is_frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Due {
    pub string: Option<String>,
    pub date: Option<String>,
    pub datetime: Option<String>,
    pub timezone: Option<String>,
    pub is_recurring: bool,
    /// Local-only calendar phase, retained in snapshots but never sent to Todoist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_anchor: Option<RecurrenceAnchor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceAnchor {
    pub expression: String,
    pub origin: NaiveDate,
    pub expected: NaiveDate,
}

impl Due {
    pub fn display(&self) -> Option<&str> {
        // Once Todoist has parsed a natural-language expression, show its
        // concrete result. Keep `string` as the fallback for pending/offline
        // changes and as the editable source expression.
        self.datetime
            .as_deref()
            .or(self.date.as_deref())
            .or(self.string.as_deref())
    }

    pub(crate) fn list_display_on(&self, today: NaiveDate) -> Option<String> {
        let value = self.display()?;
        if value.trim().eq_ignore_ascii_case("tod") || value.trim().eq_ignore_ascii_case("today") {
            return Some("Tod".to_owned());
        }
        let Some(date) = self.concrete_date() else {
            return Some(value.to_owned());
        };
        let days_from_today = date.signed_duration_since(today).num_days();
        let mut formatted = if days_from_today == 0 {
            "Tod".to_owned()
        } else if date.year() != today.year() {
            date.format("%b %-d, %Y").to_string()
        } else if (1..=6).contains(&days_from_today) {
            date.format("%a").to_string()
        } else {
            date.format("%b %-d").to_string()
        };
        let local = self
            .local_datetime()
            .map(|date| date.format("%H:%M").to_string());
        if let Some(time) = local.as_deref().or_else(|| value.get(11..16)) {
            formatted.push(' ');
            formatted.push_str(time);
        }
        Some(formatted)
    }

    pub(crate) fn concrete_date(&self) -> Option<NaiveDate> {
        if let Some(date) = self.local_datetime() {
            return Some(date.date_naive());
        }
        let value = self.display()?;
        NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()
    }

    fn local_datetime(&self) -> Option<DateTime<Local>> {
        DateTime::parse_from_rfc3339(self.display()?)
            .ok()
            .map(|date| date.with_timezone(&Local))
    }

    pub(crate) fn readable_display(&self) -> Option<String> {
        self.local_datetime()
            .map(|date| date.format("%Y-%m-%d %H:%M %:z").to_string())
            .or_else(|| self.display().map(str::to_owned))
    }

    pub(crate) fn next_occurrence(&self) -> Self {
        crate::recurrence::next(self, Local::now().fixed_offset())
    }

    #[cfg(test)]
    fn next_occurrence_on(&self, today: NaiveDate) -> Self {
        use chrono::TimeZone;
        let now = chrono::Utc
            .from_utc_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
            .fixed_offset();
        crate::recurrence::next(self, now)
    }

    pub(crate) fn preserve_preview_anchor(&mut self, previous: Option<&Due>) {
        self.preview_anchor = previous
            .filter(|old| {
                old.is_recurring
                    && self.is_recurring
                    && old.string == self.string
                    && old.timezone == self.timezone
            })
            .and_then(|old| old.preview_anchor.as_ref())
            .filter(|anchor| {
                self.datetime.is_none()
                    && self
                        .date
                        .as_deref()
                        .and_then(|date| NaiveDate::parse_from_str(date, "%F").ok())
                        == Some(anchor.expected)
            })
            .cloned();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub parent_id: Option<String>,
    pub content: String,
    pub description: String,
    pub labels: Vec<String>,
    pub due: Option<Due>,
    pub priority: u8,
    pub child_order: i64,
    pub checked: bool,
    pub assignee: Option<String>,
    #[serde(default)]
    pub section_id: Option<String>,
}

impl Task {
    pub fn display_group(&self) -> Option<&str> {
        self.labels
            .iter()
            .min_by_key(|label| label.to_lowercase())
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NavId {
    Project(String),
    Filter(String),
    Today,
    Upcoming,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    Item {
        id: NavId,
        name: String,
        count: usize,
        is_inbox: bool,
        depth: usize,
    },
    Separator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    QuickAdd {
        expression: String,
        #[serde(default)]
        description: String,
        project_id: String,
        project_name: String,
        #[serde(default)]
        parent_id: Option<String>,
        #[serde(default)]
        target_order: Option<i64>,
        #[serde(default)]
        placement: Option<crate::ordering::Placement>,
    },
    UpdateTask {
        content: String,
        description: String,
    },
    QuickEdit {
        expression: String,
        description: String,
        update_uuid: String,
        move_uuid: String,
        delete_uuid: String,
    },
    UpdateDue {
        due_string: String,
    },
    RestoreDue {
        due: Option<Due>,
    },
    RestoreRecurring {
        due: Due,
    },
    RestoreCompletion {
        completed: bool,
    },
    UpdateAssignee {
        responsible_uid: Option<String>,
    },
    UpdateLabels {
        labels: Vec<String>,
    },
    UpdatePriority {
        priority: u8,
    },
    MoveTask {
        project_id: String,
    },
    MoveTaskParent {
        parent_id: Option<String>,
        #[serde(default)]
        project_id: String,
        #[serde(default)]
        section_id: Option<String>,
    },
    CompleteTask,
    UncompleteTask,
    DeleteTask,
    AddFilter {
        name: String,
        query: String,
        is_favorite: bool,
    },
    UpdateFilter {
        name: String,
        query: String,
        is_favorite: bool,
        is_workspace: bool,
    },
    AddProject {
        name: String,
    },
    UpdateProject {
        name: String,
    },
    ReorderProjects {
        projects: Vec<ProjectOrder>,
    },
    ReorderTasks {
        tasks: Vec<TaskOrder>,
    },
    OrderResources {
        plans: Vec<crate::ordering::OrderPlan>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectOrder {
    pub id: String,
    pub child_order: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOrder {
    pub id: String,
    pub child_order: i64,
}

#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub seq: i64,
    pub uuid: String,
    pub entity_id: String,
    pub operation: Operation,
    pub checkpoint: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteProject {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "optional_string_id")]
    pub parent_id: Option<String>,
    #[serde(default, alias = "order")]
    pub child_order: i64,
    #[serde(default, deserialize_with = "optional_string_id")]
    pub workspace_id: Option<String>,
    #[serde(default, alias = "inbox_project")]
    pub is_inbox_project: bool,
    #[serde(default, alias = "is_shared")]
    pub shared: bool,
    #[serde(default)]
    pub can_assign_tasks: bool,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub is_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTask {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    #[serde(deserialize_with = "string_id")]
    pub project_id: String,
    #[serde(default, deserialize_with = "optional_string_id")]
    pub parent_id: Option<String>,
    #[serde(
        default,
        alias = "assignee_id",
        deserialize_with = "optional_string_id"
    )]
    pub responsible_uid: Option<String>,
    pub content: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub due: Option<Due>,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default, alias = "order")]
    pub child_order: i64,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub is_completed: bool,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub is_deleted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCollaborator {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    pub full_name: String,
}

fn default_priority() -> u8 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFilter {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    pub name: String,
    pub query: String,
    #[serde(default, deserialize_with = "optional_string_id")]
    pub workspace_id: Option<String>,
    #[serde(default, alias = "order")]
    pub item_order: i64,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub is_frozen: bool,
    #[serde(default)]
    pub is_deleted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteLabel {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    pub name: String,
    #[serde(default, alias = "order")]
    pub item_order: i64,
    #[serde(default)]
    pub is_deleted: bool,
}

pub fn string_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(serde::de::Error::custom("expected string or numeric id")),
    }
}

pub fn optional_string_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .map(|v| match v {
            Value::String(s) => Ok(s),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err(serde::de::Error::custom("expected string or numeric id")),
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_timezone_dates_use_the_same_local_day_and_time_for_the_same_instant() {
        let first = Due {
            date: Some("2026-10-17T00:30:00+14:00".into()),
            ..Due::default()
        };
        let second = Due {
            date: Some("2026-10-16T10:30:00Z".into()),
            ..Due::default()
        };
        let expected = DateTime::parse_from_rfc3339("2026-10-16T10:30:00Z")
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(first.concrete_date(), Some(expected.date_naive()));
        assert_eq!(second.concrete_date(), Some(expected.date_naive()));
        let expected_suffix = format!("Tod {}", expected.format("%H:%M"));
        assert_eq!(
            first.list_display_on(expected.date_naive()).as_deref(),
            Some(expected_suffix.as_str())
        );
        assert_eq!(
            second.list_display_on(expected.date_naive()),
            first.list_display_on(expected.date_naive())
        );
    }

    #[test]
    fn recurrence_previews_follow_documented_dynamic_weekly_rules_and_leave_timed_rules_to_todoist()
    {
        let today = Local::now().date_naive();
        let anchor = today - chrono::Duration::days(10);
        let weekly = Due {
            date: Some(anchor.format("%F").to_string()),
            string: Some("every week".into()),
            is_recurring: true,
            ..Due::default()
        };
        let next = weekly.next_occurrence().concrete_date().unwrap();
        assert!(next > today);
        assert_eq!(next, today + chrono::Duration::days(7));
        let timed = Due {
            date: Some("2026-10-24T08:00:00Z".into()),
            string: Some("every day".into()),
            timezone: Some("Europe/London".into()),
            is_recurring: true,
            ..Due::default()
        };
        assert_eq!(timed.next_occurrence(), timed);
    }

    #[test]
    fn recurring_day_and_week_intervals_skip_elapsed_occurrences_without_losing_the_anchor() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        for (expression, anchor, expected) in [
            ("every 3 days", "2026-10-10", "2026-10-22"),
            ("every other day", "2026-10-10", "2026-10-20"),
            ("every 2 weeks", "2026-10-01", "2026-10-29"),
            ("ev other week", "2026-10-01", "2026-10-29"),
            ("every 1 day", "2026-10-25", "2026-10-20"),
            ("weekly", "2026-10-25", "2026-10-26"),
            ("every 1 week", "2026-10-01", "2026-10-26"),
        ] {
            let due = Due {
                date: Some(anchor.into()),
                string: Some(expression.into()),
                is_recurring: true,
                ..Due::default()
            };
            let next = due.next_occurrence_on(today);
            assert_eq!(next.date.as_deref(), Some(expected), "{expression}");
            assert_eq!(next.string, due.string);
            assert!(next.is_recurring && next.datetime.is_none());
        }
    }

    #[test]
    fn recurring_weekday_lists_support_aliases_and_preserve_unsupported_schedules() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap(); // Friday
        for (expression, expected) in [
            ("every mon, fri", "2026-10-26"),
            (" EVERY   MONDAY, friday ", "2026-10-26"),
            ("every tues, wed, and fri", "2026-10-27"),
            ("every workday", "2026-10-26"),
            ("ev sat, sun", "2026-10-24"),
        ] {
            let due = Due {
                date: Some("2026-10-16".into()),
                string: Some(expression.into()),
                is_recurring: true,
                ..Due::default()
            };
            assert_eq!(
                due.next_occurrence_on(today).date.as_deref(),
                Some(expected),
                "{expression}"
            );
        }
        for expression in [
            "every 0 days",
            "every 999999999999999999 days",
            "every 3 days until Dec 1",
            "every mon, nonsense",
            "every fri at 9",
        ] {
            let due = Due {
                date: Some("2026-10-16".into()),
                string: Some(expression.into()),
                is_recurring: true,
                ..Due::default()
            };
            assert_eq!(due.next_occurrence_on(today), due, "{expression}");
        }
    }

    #[test]
    fn task_uses_alphabetically_first_label_for_group() {
        let task = Task {
            id: "1".into(),
            project_id: "p".into(),
            parent_id: None,
            content: String::new(),
            description: String::new(),
            labels: vec!["Zoo".into(), "alpha".into()],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
            assignee: None,
            section_id: None,
        };
        assert_eq!(task.display_group(), Some("alpha"));
    }

    #[test]
    fn legacy_quick_add_operations_default_to_no_parent() {
        let operation: Operation = serde_json::from_str(
            r##"{"kind":"quick_add","expression":"Task","project_id":"inbox","project_name":"Inbox"}"##,
        )
        .unwrap();
        assert!(matches!(
            operation,
            Operation::QuickAdd {
                parent_id: None,
                target_order: None,
                ref description,
                ..
            } if description.is_empty()
        ));
    }

    #[test]
    fn remote_task_accepts_sync_and_rest_assignee_fields() {
        let sync: RemoteTask = serde_json::from_str(
            r#"{"id":"1","project_id":"p","content":"Task","responsible_uid":42}"#,
        )
        .unwrap();
        let rest: RemoteTask = serde_json::from_str(
            r#"{"id":"2","project_id":"p","content":"Task","assignee_id":"84"}"#,
        )
        .unwrap();

        assert_eq!(sync.responsible_uid.as_deref(), Some("42"));
        assert_eq!(rest.responsible_uid.as_deref(), Some("84"));
    }

    #[test]
    fn due_display_prefers_todoists_parsed_value() {
        let due = Due {
            string: Some("tod".into()),
            date: Some("2026-10-12".into()),
            ..Due::default()
        };
        assert_eq!(due.display(), Some("2026-10-12"));

        let pending = Due {
            string: Some("tod".into()),
            ..Due::default()
        };
        assert_eq!(pending.display(), Some("tod"));
    }

    #[test]
    fn due_list_display_uses_today_weekdays_then_compact_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let today_due = Due {
            date: Some("2026-10-12".into()),
            ..Due::default()
        };
        assert_eq!(today_due.list_display_on(today), Some("Tod".into()));

        let today_with_time = Due {
            datetime: Some("2026-10-12T09:30:00".into()),
            ..Due::default()
        };
        assert_eq!(
            today_with_time.list_display_on(today),
            Some("Tod 09:30".into())
        );

        let pending_today = Due {
            string: Some("tod".into()),
            ..Due::default()
        };
        assert_eq!(pending_today.list_display_on(today), Some("Tod".into()));

        let six_days_away = Due {
            date: Some("2026-10-18".into()),
            ..Due::default()
        };
        assert_eq!(six_days_away.list_display_on(today), Some("Sun".into()));

        let seven_days_away = Due {
            date: Some("2026-10-19".into()),
            ..Due::default()
        };
        assert_eq!(
            seven_days_away.list_display_on(today),
            Some("Oct 19".into())
        );

        let past_date = Due {
            date: Some("2026-10-11".into()),
            ..Due::default()
        };
        assert_eq!(past_date.list_display_on(today), Some("Oct 11".into()));

        let current_year_with_time = Due {
            datetime: Some("2026-10-13T09:30:00".into()),
            ..Due::default()
        };
        assert_eq!(
            current_year_with_time.list_display_on(today),
            Some("Tue 09:30".into())
        );

        let current_year_later = Due {
            date: Some("2026-10-13".into()),
            ..Due::default()
        };
        assert_eq!(
            current_year_later.list_display_on(today),
            Some("Tue".into())
        );

        let another_year = Due {
            date: Some("2027-10-13".into()),
            ..Due::default()
        };
        assert_eq!(
            another_year.list_display_on(today),
            Some("Oct 13, 2027".into())
        );

        let next_year_within_six_calendar_days = Due {
            date: Some("2027-01-02".into()),
            ..Due::default()
        };
        assert_eq!(
            next_year_within_six_calendar_days
                .list_display_on(NaiveDate::from_ymd_opt(2026, 12, 30).unwrap()),
            Some("Jan 2, 2027".into())
        );
    }
}
