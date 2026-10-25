# Offline recurrence previews

Todoist remains authoritative. Local completion previews only recognized,
unambiguous schedules; unsupported rules keep the cached due value and remain
active until the server confirms the next occurrence. Parsing and date arithmetic
are in `src/recurrence.rs`; keystrokes never request a server-side preview.

The [official recurring-date guide](https://www.todoist.com/help/articles/introduction-to-recurring-dates-YUYVJJAV)
was reviewed on 2026-10-19 (article updated 2026-10-14). It explicitly distinguishes
dynamic daily/weekly schedules from original-date interval schedules and lists
weekday aliases, comma-separated weekday sets, and `every other` intervals.
The [completion guide](https://www.todoist.com/help/articles/complete-a-task-with-a-recurring-date-dmI6SVqdP)
and [date/time guide](https://www.todoist.com/help/articles/schedule-a-date-and-time-for-your-todoist-tasks-q7VobO)
were also reviewed. They do not fully specify clipped calendar anchors or
civil-time recurrence behavior at DST gaps/folds; those cases remain deferred.

| Recognized rule | Local preview |
| --- | --- |
| `every day`, `daily`, `every 1 day` | Tomorrow relative to completion day |
| `every week`, `weekly`, `every 1 week` | Seven days after completion day |
| `every N days/weeks`, `every other day/week` | First scheduled interval after today, preserving the cached scheduled-date anchor |
| `every weekday`, `every workday` | Next eligible Monday–Friday |
| `every Monday`, `every mon, fri`, `every tues, wed, and fri` | Next matching weekday after the later of today and the cached due date |
| `every N months/years`, `monthly`, `yearly`, `quarterly`, `every other month/year` | First future calendar interval, preserving the known calendar phase |
| `every! N days/weeks/months/years`, `after N days/weeks/months/years` | One interval from the completion day, subject to the calendar restrictions below |
| Supported all-day rules with `starting [on/from] YYYY-MM-DD` / `from YYYY-MM-DD` and `until YYYY-MM-DD` / `ending YYYY-MM-DD` | Respect the already-parsed start and inclusive end bounds |
| `every! N minutes/hours`, `after N minutes/hours` with an absolute RFC3339 due timestamp | Add the elapsed duration to the completion instant; retain timezone metadata and store the new instant in UTC |

Rules are case-insensitive and accept surrounding/repeated whitespace. `ev`
is accepted as the documented abbreviation for `every`. Previewing does not
rewrite the natural-language expression or recurrence metadata.

Month-based intervals that can land in different months preview only when their
origin day is 1–28. Whole-year intervals also support other dates except February
29. For completion-relative rules, the origin is the completion day. This avoids
guessing how Todoist clips or restores a month-end/leap-day anchor.

Scheduled calendar previews store their origin and expected next date in local
due metadata. It survives restarts and undo/redo snapshots and is never uploaded
as an API field. An agreeing sync/filter refresh retains it; a changed recurrence,
timezone, or unexpected server date resets it. Existing due JSON needs no schema
migration; omitted metadata defaults to absent.

When a preview would exceed its end bound, retain the cached value and let Todoist
perform final completion. Do not guess task-tree completion locally. Natural-
language/relative bounds (`until Friday`, `for 3 weeks`), floating timestamps,
fixed local-time schedules, timed day/calendar intervals, localized or unsupported
grammar, and invalid/overflowing intervals retain the cached value. Elapsed-hour
tests across DST transition instants verify duration arithmetic, not Todoist's
unspecified civil-clock DST policy.

Tests use fixed completion dates for overdue intervals, completion-relative
rules, calendar phase, bounds, leap/month-end fallback, and DST-adjacent absolute
instants. Storage tests cover restarts and sync/filter confirmation; completion
undo/redo tests prevent a second remote recurrence advance. No live account was
used to validate recurrence behavior; the documented arithmetic is distinguished
from unresolved server policies above.
