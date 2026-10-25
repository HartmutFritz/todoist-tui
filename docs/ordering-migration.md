# Todoist ordering migration contract

This contributor reference describes the ordering implementation, upgrade
contract, and verification requirements.

## Public API contract

Checked against the [Todoist API v1 documentation](https://developer.todoist.com/api/v1/)
on 2026-10-17:

| Resource / scope | Read field | Write command and argument |
| --- | --- | --- |
| Task siblings in the same project, section, and parent task | `order_key` | `item_update`, `order_key` |
| Sections within a project | `order_key` | Cached for task display; section reordering has no UI action |
| Personal project siblings with the same parent | `order_key` | `project_update`, `order_key` |
| Workspace projects and folders in a shared workspace ordering | `default_order_key` | `project_update` / `folder_update`, `default_order_key` |
| Personal filters | `order_key` | `filter_update`, `order_key` |
| Workspace filters | `order_key` | `workspace_filter_update`, `order_key` |
| A user's personal Today/Upcoming ordering | `user_item_orders[].order_key` | `user_item_order_update`, `item_id`, `scope`, `order_key` |

- Keys use fractional indexing and sort lexicographically within their scope.
  They are not integers or floating-point numbers.
- Keys may be `null` for resources not yet migrated. A workspace project's
  personal-project `order_key` is always `null`; workspace ordering uses
  `default_order_key` instead.
- Omitting an update's ordering key preserves the current key. Do not send
  `null` as a substitute for an absent key: only the documented user-item-order
  clear operation explicitly permits that behavior.
- If a requested key is already used in its scope, Todoist adjusts the position
  to immediately after it and returns the corrected key through synchronization.
- Creation can accept an ordering key; when omitted, Todoist allocates one.
  Workspace filter creation defaults to the top, while the documented task,
  personal project, and personal filter creation defaults are the bottom.
- `item_reorder`, `project_reorder`, and the integer-based filter/day-order bulk
  commands are deprecated. The reviewed notices do not specify a removal date.
- Command UUIDs provide idempotency. Retrying an operation that may already
  have executed must retain its identity and original arguments.

Sources: [task updates](https://developer.todoist.com/api/v1/#tag/Sync/Tasks/Update-a-task),
[project updates](https://developer.todoist.com/api/v1/#tag/Sync/Projects/Update-a-project),
[workspace default ordering](https://developer.todoist.com/api/v1/#tag/Sync/Workspace/Set-default-project-ordering),
[user item orders](https://developer.todoist.com/api/v1/#tag/Sync/User-item-orders),
[command UUIDs](https://developer.todoist.com/api/v1/#tag/Sync/Overview/Command-UUID).

## Implementation

### Task workspace boundaries

The Sync move argument table is not a guarantee that every pair of projects is
supported. Todoist's [team-project guide](https://www.todoist.com/help/todoist/teams/move-a-team-project-to-your-personal-projects-Kw3GqRNlC)
explicitly states that individual tasks cannot move from a team project to a
personal project. A real server rejection reported `FORBIDDEN`, HTTP 403, with
`error_extra.explanation = "Moving from a workspace is not implemented."`.

The `v` command checks known workspace boundaries before applying a new move.
For legacy rejected optimistic moves, the last server ordering scope identifies
the original team even when the cached task is displayed at its intended personal
destination. A new corrective move or undo supersedes the confirmed rejection
and its adjacent, unclaimed single-task placement. In-flight requests and unrelated
edits/errors remain intact. A correction racing with a request is unblocked only
after the old request is conclusively rejected. Recovery tests use the actual
403 response shape rather than assuming all workspace moves return success.

Undo after a personal-to-team move has already synchronized is also subject to
the server's outbound-workspace restriction. The application does not simulate a
native move by copying a task and deleting its original, which would change IDs
and could lose server-only comments or attachments.

### Ordering persistence

- `src/ordering.rs` implements the classic Base-62 fractional-indexing allocator
  using CC0 reference fixtures from
  [rocicorp/fractional-indexing](https://github.com/rocicorp/fractional-indexing).
  Unsupported key formats or inconsistent/colliding anchors produce an error
  before sending guessed replacements. There is no invented length limit or
  client-side rebalance policy.
- Database version 10 stores raw server keys and scope identifiers in
  `order_meta`, alongside local display ordinals. Task sections, project folders,
  section order, and filter order are cached. Workspace folders are fetched with
  pagination in the background when needed for allocation.
- Legacy integers are retained separately. Keyed resources sort lexicographically;
  each unkeyed legacy run follows its nearest preceding keyed resource in legacy
  order (or leads the scope if none precedes it). This defines a total order even
  when integers and keys disagree. Reordering initializes required null-key anchors.
- Upgrades preserve old payloads and request UUIDs, seed metadata from the cache,
  and request a full ordering backfill. Full-snapshot and pending-field protections
  keep optimistic positions while allowing independent remote edits to refresh.
- New `OrderResources` outbox entries record scoped ordering intent. Full plans
  permute eligible sibling slots, preserving hidden/unlisted resources and folder
  anchors. Single-task plans record insertion ordinals for placement and restore.
  Only never-sent full plans in the same scope can coalesce.
- Before the first key command, resolved writes are durably checkpointed. Batches
  contain at most 100 commands with stable per-command UUIDs; successful commands
  have per-command completion checkpoints and are skipped on retry. Possibly executed commands
  retain their exact keys across crashes, remote changes, and restarts.
- Previously claimed legacy bulk requests (including Quick Add placement steps)
  first retry their original command and UUID. A confirmed permanent rejection
  permits key translation; never-sent legacy operations translate directly.
  Subsequent pulls accept Todoist's corrected collision keys.
- Hierarchy/project moves finish before their position is restored. Deletion undo
  restores local ordinals immediately; synchronized deletions recreate their
  hierarchy, move into the final section/project, then restore ordering.

### Preserved boundaries

1. Keep nullable server keys and their scope identifiers separately from local
   display ordinals. Account for cached task sections and workspace folders
   before allocating keys; local display groups are not remote ordering scopes.
2. Preserve legacy integer order as a deterministic fallback for old databases
   and unmigrated resources. Define mixed keyed/unkeyed behavior with fixtures.
   Backfill new fields through a full refresh without losing pending changes.
3. Verify a compatible key-allocation algorithm against the public examples and
   captured test fixtures. The reviewed docs do not define a complete key
   grammar, maximum length, or rebalance algorithm; do not invent those rules.
4. Introduce versioned key-based operations for new writes. Continue decoding
   old queued operations. Never rewrite a claimed operation that may have
   reached Todoist; resolve its result or a confirmed rejection before issuing
   a new compensating operation.
5. Preserve durable top/bottom additions, sibling reordering, hidden siblings,
   hierarchy changes, and undo/redo. Alphabetical and label/date display modes
   must not replace the remote ordering scope.
6. Reconcile server-adjusted keys after synchronization while preserving newer
   local changes. Persist identifiers and progress across partial failures.

## Contract and regression tests

- Lexicographic ordering, insertion before/between/after neighbors, and repeated
  insertions into the same interval using verified key fixtures.
- Legacy and mixed-key caches, full-refresh backfill, null keys, and migration
  of databases containing pending operations.
- Separate task sections/parents, personal project parents, and workspace
  project/folder scopes, including hidden siblings and display-space overrides.
- Top/bottom additions with alphabetical mode on and off; child creation,
  indent/outdent, project moves, deletion restoration, and undo/redo.
- Concurrent remote reordering, server-adjusted collision keys, temporary-ID
  replacement, retries, and restart recovery for possibly executed operations.
- Mock HTTP request/response shapes for each supported write scope, followed by
  a manual Todoist web/phone ordering check when testing the migration.

Automated coverage lives in `ordering::tests`, `store::order_storage::tests`, and
the existing store/app/HTTP suites. It includes reference/randomized allocator
fixtures, migration with pending legacy payloads, scoped anchors, placement,
deletion restoration, independent remote field refreshes, folder pagination,
partial-batch restart recovery, legacy replay/translation, missing resources, and
server-adjusted keys.

### Manual web/phone check

Use a disposable Todoist project/account and a separate local database:

1. Reorder roots and nested subtasks; repeat with hidden siblings and label
   grouping. Confirm the positions in Todoist web and phone clients.
2. Add at top/bottom with alphabetical sorting enabled, then disable it and
   confirm durable placement in each client.
3. Repeat in separate task sections and workspace folders; verify that unrelated
   siblings and folder positions stay in place.
4. Move, indent/promote, delete/undo, and redo task trees. Restart offline between
   mutations and synchronization, then confirm hierarchy, sections, and order.
5. Reorder concurrently in another client, synchronize, and verify a stable
   reconciled result without duplicate tasks or lost local changes.

### Opt-in live API round trip

With `TODOIST_TEST_API_TOKEN` set for a dedicated test account, run:

```sh
cargo test --locked live_ordering_round_trip -- --ignored --nocapture
```

`src/todoist_live_tests.rs` creates a uniquely named disposable personal project
and section and uses a temporary database. It checks top/bottom placement,
reordering after a cache restart, section moves/restoration, child promotion and
reattachment, synchronized deletion/undo with new IDs, and changes made by a
second HTTP client. REST reads independently verify server ordering keys and
hierarchy. The fixture deletes its project on success and attempts cleanup on
failure; if cleanup fails, the test prints the disposable project ID to remove.
It never reads `API_TOKEN`, `TODOIST_API_TOKEN`, or the normal application database.

This test is ignored by the normal suite and requires explicit invocation. It
does not automate Todoist's web or phone UI, nor create workspace/folder fixtures.
Use the manual checklist above for those observations.

### Verification status (2026-10-19)

- Mock HTTP ordering tests and scoped storage regression tests passed, including
  workspace/folder allocation, server-adjusted keys, restart recovery, and legacy
  queues.
- The live API round-trip test compiles but has **not run**: no dedicated
  `TODOIST_TEST_API_TOKEN` is configured in this environment.
- Web/phone visual validation and workspace/folder observations remain **not
  performed**: no authenticated browser or phone session is available here.
