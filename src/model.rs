use chrono::{Datelike, NaiveDate};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Due {
    pub string: Option<String>,
    pub date: Option<String>,
    pub datetime: Option<String>,
    pub timezone: Option<String>,
    pub is_recurring: bool,
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
        let Some(date_text) = value.get(..10) else {
            return Some(value.to_owned());
        };
        let Ok(date) = NaiveDate::parse_from_str(date_text, "%Y-%m-%d") else {
            return Some(value.to_owned());
        };
        let days_from_today = date.signed_duration_since(today).num_days();
        let mut formatted = if date.year() != today.year() {
            date.format("%b %-d, %Y").to_string()
        } else if (0..=6).contains(&days_from_today) {
            date.format("%a").to_string()
        } else {
            date.format("%b %-d").to_string()
        };
        if let Some(time) = value.get(11..16) {
            formatted.push(' ');
            formatted.push_str(time);
        }
        Some(formatted)
    }

    pub(crate) fn concrete_date(&self) -> Option<NaiveDate> {
        let value = self.display()?;
        NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub content: String,
    pub description: String,
    pub labels: Vec<String>,
    pub due: Option<Due>,
    pub priority: u8,
    pub child_order: i64,
    pub checked: bool,
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
    },
    Separator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    QuickAdd {
        expression: String,
        project_id: String,
        project_name: String,
    },
    UpdateTask {
        content: String,
        description: String,
    },
    UpdateDue {
        due_string: String,
    },
    UpdateLabels {
        labels: Vec<String>,
    },
    MoveTask {
        project_id: String,
    },
    CompleteTask,
    UncompleteTask,
    DeleteTask,
}

#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub seq: i64,
    pub uuid: String,
    pub entity_id: String,
    pub operation: Operation,
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

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteTask {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    #[serde(deserialize_with = "string_id")]
    pub project_id: String,
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

fn default_priority() -> u8 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFilter {
    #[serde(deserialize_with = "string_id")]
    pub id: String,
    pub name: String,
    pub query: String,
    #[serde(default, alias = "order")]
    pub item_order: i64,
    #[serde(default)]
    pub is_favorite: bool,
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
    fn task_uses_alphabetically_first_label_for_group() {
        let task = Task {
            id: "1".into(),
            project_id: "p".into(),
            content: String::new(),
            description: String::new(),
            labels: vec!["Zoo".into(), "alpha".into()],
            due: None,
            priority: 1,
            child_order: 0,
            checked: false,
        };
        assert_eq!(task.display_group(), Some("alpha"));
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
    fn due_list_display_uses_weekdays_then_compact_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let today_due = Due {
            date: Some("2026-10-12".into()),
            ..Due::default()
        };
        assert_eq!(today_due.list_display_on(today), Some("Mon".into()));

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
