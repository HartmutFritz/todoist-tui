//! Conservative previews for documented recurrence rules. No server parsing or I/O.
use crate::model::{Due, RecurrenceAnchor};
use chrono::{DateTime, Datelike, Duration, FixedOffset, Months, NaiveDate, SecondsFormat, Utc};

fn normalized(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Clone, Copy)]
enum Interval {
    Days(i64),
    Months(u32),
    Elapsed(i64),
}

struct Rule<'a> {
    schedule: &'a str,
    relative: bool,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
}

fn iso_date(value: &str) -> Option<NaiveDate> {
    (value.len() == 10)
        .then(|| NaiveDate::parse_from_str(value, "%F").ok())
        .flatten()
}

fn rule(expression: &str) -> Option<Rule<'_>> {
    let mut body = expression;
    let mut start = None;
    let mut end = None;
    // Only explicit, unambiguous ISO bounds are interpreted locally. Natural
    // language bounds remain Todoist's responsibility, including relative ends.
    for delimiter in [" until ", " ending "] {
        if let Some((before, date)) = body.split_once(delimiter) {
            end = Some(iso_date(date)?);
            body = before;
            break;
        }
    }
    for delimiter in [" starting on ", " starting from ", " starting ", " from "] {
        if let Some((before, date)) = body.split_once(delimiter) {
            start = Some(iso_date(date)?);
            body = before;
            break;
        }
    }
    if start.zip(end).is_some_and(|(start, end)| start > end) {
        return None;
    }
    let (schedule, relative) = if let Some(schedule) = body
        .strip_prefix("every! ")
        .or_else(|| body.strip_prefix("ev! "))
        .or_else(|| body.strip_prefix("after "))
    {
        (schedule, true)
    } else {
        (
            body.strip_prefix("every ")
                .or_else(|| body.strip_prefix("ev "))
                .unwrap_or(body),
            false,
        )
    };
    Some(Rule {
        schedule,
        relative,
        start,
        end,
    })
}

fn interval(schedule: &str) -> Option<Interval> {
    let (amount, unit) = match schedule.split_once(' ') {
        Some(("other", unit)) => (2, unit),
        Some((amount, unit)) => (amount.parse::<i64>().ok()?, unit),
        None => (1, schedule),
    };
    if amount <= 0 {
        return None;
    }
    match unit {
        "day" | "days" | "daily" => Some(Interval::Days(amount)),
        "week" | "weeks" | "weekly" => Some(Interval::Days(amount.checked_mul(7)?)),
        "month" | "months" | "monthly" => Some(Interval::Months(amount.try_into().ok()?)),
        "quarter" | "quarters" | "quarterly" => {
            Some(Interval::Months(amount.checked_mul(3)?.try_into().ok()?))
        }
        "year" | "years" | "yearly" => {
            Some(Interval::Months(amount.checked_mul(12)?.try_into().ok()?))
        }
        "minute" | "minutes" => Some(Interval::Elapsed(amount.checked_mul(60)?)),
        "hour" | "hours" => Some(Interval::Elapsed(amount.checked_mul(3600)?)),
        _ => None,
    }
}

pub(crate) fn next(due: &Due, now: DateTime<FixedOffset>) -> Due {
    advance(due, now).unwrap_or_else(|| due.clone())
}

fn advance(due: &Due, now: DateTime<FixedOffset>) -> Option<Due> {
    if !due.is_recurring {
        return None;
    }
    let expression = normalized(due.string.as_deref()?);
    let rule = rule(&expression)?;
    let mut next = due.clone();
    let scheduled = due.display()?;
    if scheduled.len() > 10 {
        // An explicitly completion-relative minute/hour interval is an elapsed
        // duration, not civil-time arithmetic. RFC3339 timestamps are absolute;
        // floating times and fixed local-time schedules remain untouched.
        DateTime::parse_from_rfc3339(scheduled).ok()?;
        let Interval::Elapsed(seconds) = interval(rule.schedule)? else {
            return None;
        };
        if !rule.relative || rule.start.is_some() || rule.end.is_some() {
            return None;
        }
        let instant = now
            .with_timezone(&Utc)
            .checked_add_signed(Duration::try_seconds(seconds)?)?;
        let value = instant.to_rfc3339_opts(SecondsFormat::Secs, true);
        if due.datetime.is_some() {
            next.datetime = Some(value);
            // Do not invent a named-zone calendar date for an unused secondary
            // date field. The absolute datetime is authoritative throughout UI
            // membership, rendering, and exact-state synchronization.
            next.date = None;
        } else {
            next.date = Some(value);
        }
        next.preview_anchor = None;
        return Some(next);
    }
    let scheduled = iso_date(scheduled)?;
    if rule.start.is_some_and(|start| scheduled < start) {
        return None;
    }
    let today = now.date_naive();
    let candidate = match interval(rule.schedule) {
        Some(Interval::Days(days)) => {
            let dynamic = rule.relative
                || matches!(
                    rule.schedule,
                    "day" | "daily" | "1 day" | "1 days" | "week" | "weekly" | "1 week" | "1 weeks"
                );
            let base = if dynamic { today } else { scheduled };
            let count = if dynamic {
                1
            } else {
                today.signed_duration_since(base).num_days().max(0) / days + 1
            };
            next.preview_anchor = None;
            base.checked_add_signed(Duration::try_days(days.checked_mul(count)?)?)?
        }
        Some(Interval::Months(months)) => {
            let origin = if rule.relative {
                today
            } else {
                due.preview_anchor
                    .as_ref()
                    .filter(|anchor| {
                        anchor.expression == expression && anchor.expected == scheduled
                    })
                    .map_or(scheduled, |anchor| anchor.origin)
            };
            // The public contract does not specify how clipped dates carry
            // their anchor through short months/leap years. Do not infer it.
            if (months % 12 != 0 && origin.day() > 28)
                || (origin.month() == 2 && origin.day() == 29)
            {
                return None;
            }
            let candidate = calendar_next(
                origin,
                if rule.relative {
                    origin
                } else {
                    today.max(scheduled)
                },
                months,
            )?;
            next.preview_anchor = (!rule.relative).then(|| RecurrenceAnchor {
                expression: expression.clone(),
                origin,
                expected: candidate,
            });
            candidate
        }
        Some(Interval::Elapsed(_)) => return None,
        None if !rule.relative => {
            let weekdays = weekdays(rule.schedule)?;
            let from = today.max(scheduled);
            let candidate = (1..=7)
                .filter_map(|days| from.checked_add_signed(Duration::days(days)))
                .find(|date| weekdays[date.weekday().num_days_from_monday() as usize])?;
            next.preview_anchor = None;
            candidate
        }
        None => return None,
    };
    // Crossing an end date is not a request to complete the whole task/tree.
    // Retain the cached value and let Todoist perform final completion.
    if rule.start.is_some_and(|start| candidate < start)
        || rule.end.is_some_and(|end| candidate > end)
    {
        return None;
    }
    next.date = Some(candidate.format("%F").to_string());
    Some(next)
}

fn calendar_next(origin: NaiveDate, after: NaiveDate, months: u32) -> Option<NaiveDate> {
    let elapsed = (i64::from(after.year()) - i64::from(origin.year())) * 12
        + i64::from(after.month())
        - i64::from(origin.month());
    let mut steps = (elapsed.max(0) as u64 / u64::from(months)).max(1);
    loop {
        let total = u32::try_from(steps.checked_mul(u64::from(months))?).ok()?;
        let candidate = origin.checked_add_months(Months::new(total))?;
        if candidate > after {
            return Some(candidate);
        }
        steps = steps.checked_add(1)?;
    }
}

fn weekdays(rule: &str) -> Option<[bool; 7]> {
    if matches!(rule, "weekday" | "workday") {
        return Some([true, true, true, true, true, false, false]);
    }
    let mut days = [false; 7];
    for name in rule.split(',') {
        let name = name.trim().strip_prefix("and ").unwrap_or(name.trim());
        let index = match name {
            "mon" | "monday" => 0,
            "tue" | "tues" | "tuesday" => 1,
            "wed" | "wednesday" => 2,
            "thu" | "thurs" | "thursday" => 3,
            "fri" | "friday" => 4,
            "sat" | "saturday" => 5,
            "sun" | "sunday" => 6,
            _ => return None,
        };
        days[index] = true;
    }
    Some(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due(expression: &str, date: &str) -> Due {
        Due {
            string: Some(expression.into()),
            date: Some(date.into()),
            is_recurring: true,
            ..Due::default()
        }
    }
    fn at(value: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(value).unwrap()
    }

    #[test]
    fn completion_relative_dates_use_completion_day_and_calendar_intervals_keep_their_phase() {
        let now = at("2026-10-19T23:30:00-04:00");
        for (expression, expected) in [
            ("every! 3 days", "2026-10-22"),
            ("after 10 days", "2026-10-29"),
            ("every! 2 weeks", "2026-11-02"),
            ("every! 3 months", "2027-01-19"),
            ("every! year", "2027-10-19"),
            ("every 3 months", "2027-01-10"),
            ("quarterly", "2027-01-10"),
            ("every other month", "2026-11-10"),
            ("every 2 years", "2027-01-10"),
        ] {
            let original = due(expression, "2023-01-10");
            let result = next(&original, now);
            assert_eq!(result.date.as_deref(), Some(expected), "{expression}");
            assert_eq!(result.string, original.string);
        }
    }

    #[test]
    fn calendar_phase_survives_serialization_and_agreeing_server_refreshes_but_not_rescheduling() {
        let mut preview = next(
            &due("every 3 months", "2026-01-10"),
            at("2026-01-15T12:00:00Z"),
        );
        assert_eq!(preview.date.as_deref(), Some("2026-04-10"));
        let persisted: Due =
            serde_json::from_str(&serde_json::to_string(&preview).unwrap()).unwrap();
        let mut server = due("every 3 months", "2026-04-10");
        server.preserve_preview_anchor(Some(&persisted));
        assert_eq!(server.preview_anchor, persisted.preview_anchor);
        preview = next(&server, at("2026-08-01T12:00:00Z"));
        assert_eq!(preview.date.as_deref(), Some("2026-10-10"));
        assert_eq!(
            preview.preview_anchor.as_ref().unwrap().origin,
            iso_date("2026-01-10").unwrap()
        );
        server.date = Some("2026-05-20".into());
        server.preserve_preview_anchor(Some(&preview));
        assert!(server.preview_anchor.is_none());
        assert_eq!(
            next(&server, at("2026-05-20T12:00:00Z")).date.as_deref(),
            Some("2026-08-20")
        );
        let mut changed_rule = due("every 2 months", "2026-04-10");
        changed_rule.preserve_preview_anchor(Some(&persisted));
        assert!(changed_rule.preview_anchor.is_none());
        let mut changed_zone = due("every 3 months", "2026-04-10");
        changed_zone.timezone = Some("Europe/London".into());
        changed_zone.preserve_preview_anchor(Some(&persisted));
        assert!(changed_zone.preview_anchor.is_none());
    }

    #[test]
    fn explicit_iso_bounds_are_inclusive_and_exhausted_schedules_wait_for_server_completion() {
        let original = due(
            "every 3 days starting 2026-10-10 until 2026-10-22",
            "2026-10-10",
        );
        let result = next(&original, at("2026-10-19T12:00:00Z"));
        assert_eq!(result.date.as_deref(), Some("2026-10-22"));
        assert_eq!(next(&result, at("2026-10-22T12:00:00Z")), result);
        let early = due("every day from 2026-11-01 ending 2026-11-20", "2026-11-01");
        assert_eq!(next(&early, at("2026-10-19T12:00:00Z")), early);
        assert_eq!(
            next(&early, at("2026-11-10T12:00:00Z")).date.as_deref(),
            Some("2026-11-11")
        );
        for expression in [
            "every day from tomorrow",
            "every day until Jan 20",
            "every day for 3 weeks",
            "every day from 2027-01-01 until 2026-01-01",
        ] {
            let due = due(expression, "2026-10-19");
            assert_eq!(next(&due, at("2026-10-19T12:00:00Z")), due);
        }
    }

    #[test]
    fn elapsed_completion_intervals_cross_dst_boundaries_as_absolute_instants() {
        for (now, expected) in [
            ("2026-03-08T01:30:00-05:00", "2026-03-08T09:30:00Z"),
            ("2026-11-01T01:30:00-04:00", "2026-11-01T08:30:00Z"),
        ] {
            let mut due = due("every! 3 hours", "2026-01-01T09:00:00Z");
            due.timezone = Some("America/New_York".into());
            let preview = next(&due, at(now));
            assert_eq!(preview.date.as_deref(), Some(expected));
            assert_eq!(preview.timezone, due.timezone);
            assert_eq!(
                DateTime::parse_from_rfc3339(preview.display().unwrap()).unwrap() - at(now),
                Duration::hours(3)
            );
        }
        let mut timed = due("after 90 minutes", "2026-10-19");
        timed.datetime = Some("2026-10-19T12:00:00Z".into());
        let next = next(&timed, at("2026-10-19T15:45:00+02:00"));
        assert_eq!(next.datetime.as_deref(), Some("2026-10-19T15:15:00Z"));
        assert!(next.date.is_none());
    }

    #[test]
    fn ambiguous_dates_floating_times_and_overflows_remain_unchanged() {
        for (expression, date) in [
            ("every month", "2026-01-31"),
            ("every year", "2024-02-29"),
            ("every! 2 months", "2026-01-10"), // completion on January 31 is ambiguous
            ("every day", "2026-01-10T09:00:00Z"),
            ("every! 3 hours", "2026-01-10T09:00:00"),
            ("every! 3 hours until 2026-11-01", "2026-01-10T09:00:00Z"),
            ("every 999999999999999 months", "2026-01-10"),
            ("every! 9223372036854775807 hours", "2026-01-10T09:00:00Z"),
            ("every 0 years", "2026-01-10"),
        ] {
            let due = due(expression, date);
            assert_eq!(next(&due, at("2026-01-31T12:00:00Z")), due, "{expression}");
        }
    }
}
