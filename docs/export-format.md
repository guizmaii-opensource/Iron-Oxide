# Account export format (#22)

`export_account_data()` (`POST /api/account/export`) returns everything the signed-in user owns as
one JSON document. `import_account_data(document)` (`POST /api/account/import`) takes the same
document back, as the JSON text of the file. The types are `ExportDocument` and its parts in
`crates/iron-oxide-app/src/api/account.rs`. This page is their reference.

The export holds the user's own data only: never another user's, never a sign-in session, never
passkey key material.

## Example

```json
{
  "format": "iron-oxide-export",
  "format_version": 2,
  "exported_at": 1790000600000,
  "account": {
    "user_id": "0199a3c4-…",
    "created_at": 1789000000000,
    "plan": "free",
    "display_name": "Jules"
  },
  "sign_in": {
    "passkeys": [
      { "nickname": "iPhone", "created_at": 1789000000000, "last_used_at": 1790000000000, "backed_up": true }
    ],
    "linked_accounts": [
      { "provider": "google", "created_at": 1789000100000, "last_used_at": null }
    ]
  },
  "settings": {
    "unit": "kg", "bar_weight": 20.0,
    "plate_inventory": [{ "plate": 20.0, "pairs": 4 }, { "plate": 2.5, "pairs": 2 }],
    "default_rest": 120, "sound_enabled": true,
    "updated_at": 1789500000000
  },
  "training_maxes": [
    { "exercise_id": "bench-press", "weight": 82.5, "set_at": 1789600000000 }
  ],
  "programs": [
    {
      "creation_id": "0199a3c5-…",
      "name": "Full body",
      "source_builtin_id": "full-body-3day",
      "archived": false,
      "created_at": 1789000200000,
      "versions": [
        { "version": 1, "created_at": 1789000200000, "document": { "schema_version": 1, "name": "Full body", "…": "…" } }
      ]
    }
  ],
  "active_program": "0199a3c5-…",
  "sessions": [
    {
      "id": "0199a3c6-…",
      "program": "0199a3c5-…",
      "version": 1,
      "day": "a",
      "status": "completed",
      "started_at": 1790000000000,
      "finished_at": 1790003600000,
      "sets": [
        { "id": "0199a3c7-…", "exercise": "back-squat", "set_index": 0, "reps": 5, "weight": 100.0,
          "duration": null, "warm_up": false, "completed_at": 1790000300000,
          "target": { "weight": 100.0, "goal": { "reps": { "reps": 5, "range": null } } } }
      ]
    }
  ]
}
```

## Fields

The values use the API's types (`docs/api.md`):

- **Times** are milliseconds since the Unix epoch, UTC. Server-side times (`created_at`, `set_at`,
  `updated_at`) are stored in microseconds, and their sub-millisecond digits are dropped.
- **Weights** are kg numbers (the domain `Weight`).
- `default_rest` and `duration` are seconds.
- **Ids** are UUIDs and slugs.

| Field | What | On import |
|---|---|---|
| `format` | Always `iron-oxide-export` | Checked first: anything else is `422 This file is not an Iron Oxide export.` |
| `format_version` | `2` (see [Versioning](#versioning)) | Checked before the rest is parsed: versions 1 and 2 are read, another is a `422` that names it |
| `exported_at` | When the export was made | Ignored |
| `account` | User id, creation time, plan, display name | **Ignored.** An import never changes the account, its plan or its name. |
| `sign_in.passkeys` | Each passkey's nickname, dates and whether it is synced (`backed_up`). No public key, credential id or user handle. | Ignored: a passkey cannot be restored from a file |
| `sign_in.linked_accounts` | Linked providers (`google`) and dates. No provider subject. | Ignored |
| `settings` | The saved settings (as `get_settings`) and `updated_at`; `null` if never saved | Validated like `update_settings`; added only if the account has no settings |
| `training_maxes` | One per exercise | Weight more than zero, one per exercise; added for exercises without one |
| `programs` | The user's programs, archived ones too, oldest first, each with **all** its versions | See below |
| `programs[].creation_id` | The program's key (the client idempotency key it was created with, unique per user) | Matches an existing program |
| `programs[].versions[].document` | The `program.json` as stored | Must pass the domain's `Program::from_json`. Otherwise `422`: the message names the program, the version and the first broken rule, and the details list every problem, with paths starting at `programs[i].versions[j].document` |
| `active_program` | The `creation_id` of the active program, or `null` | Must be an unarchived program of the export; set only if the account has no active program |
| `sessions` | Every workout session, oldest first | See below |
| `sessions[].program`, `.version` | The session's program (`creation_id`) and version number | Must be a version in the export, and `day` one of its days |
| `sessions[].sets` | The session's sets, in the order they were completed (the domain `LoggedSet`) | Set ids unique in the file |
| `sessions[].sets[].target` | What the app prescribed for the set when it was logged (#60): the `SetTarget` shown (`weight`, and `goal`: reps with the program's range, a hold or intervals). Left out when none was recorded (sets logged before #60, extras) | Kept as is; a set without it is judged with the legacy tolerance |

Server-generated ids (program and version ids) are not in the export: they are global, so another
account could not reuse them. Programs are keyed by `creation_id`, versions by their number, and
sessions point at both. Session and set ids are kept, because they are only unique per user.

Not exported:

- sign-in sessions (`sessions`), the WebAuthn user handle and in-flight sign-in ceremonies;
- built-in programs (only the user's copies);
- other users' data, by construction: every query is scoped to the signed-in user.

`every_user_table_is_exported_or_deliberately_left_out` lists every table with a `user_id` column
and where it goes. A new table fails the test until it is added there and here.

## Import rules

1. **Size.** The request body is capped at `IMPORT_BODY_LIMIT` (2 × `MAX_EXPORT_BYTES` + 64 KiB,
   since the file travels as a JSON string). The session is checked before the body is read, so a
   signed-out request gets `401` without uploading anything. The document itself is capped at
   `MAX_EXPORT_BYTES` (8 MiB). Both give `413`.
   - The route reads its own body (`limits::OWN_BODY_LIMIT`, `limits::read_body`), so the 64 KiB
     default cap does not apply, and it has its own read timeout: **60 s**
     (`IMPORT_BODY_READ_TIMEOUT_SECS`, against 10 s for other requests). A real export of 8 MiB
     travels as about 9.4 MB, which takes 60 s at 1.25 Mbit/s. A slower body gets `408` and frees
     its import slot.
   - **At most 2 account operations at once per server process** (`MAX_ACCOUNT_OPERATIONS`,
     imports and deletions together). Each runs on a dedicated connection with 60 s deadlines, and
     an import holds its body, the decoded text and the parsed document, about 80 MB at the
     largest, on a 512 MB machine. One more gets `503` with `Retry-After: 5` (and
     `retry_after_secs` in the details); an import gets it after the session check and before its
     body is read. It is retryable, like every `503`.
   - **The slot is held by the work, not the request.** The import's middleware takes it and
     hands it to the server function (an `AccountSlot` in the request's extensions); Dioxus runs
     the function in a task of its own that goes on after the client disconnects, and the slot
     goes with it. Abandoned imports therefore never hold more dedicated connections than the
     slots (`abandoned_imports_never_hold_more_dedicated_connections_than_the_slots` counts them in
     `pg_stat_activity`, where they are `application_name = 'iron-oxide-account'`).
2. **Version, then content.** All of the document is validated before anything is written:
   - the domain types: slugs, weights, reps;
   - the domain validators: program documents, settings;
   - the references inside the file: a session's version and day, the active program;
   - uniqueness: creation ids, version numbers, session and set ids, at most one session in
     progress;
   - the session rules: an end time if and only if the session ended, and not before its start.

   Any failure is `422` and nothing is written. Sets are **not** checked against their session's
   time span: stored sets may fall outside it (see `docs/api.md`), and every export must import
   back.

   **Stored versions always pass today's rules.** A change that tightens a program rule ships a
   migration that fixes the stored documents (decision of 2026-10-03 on #41), so every export
   imports back. A version refused on import therefore means a hand-edited file or a missing
   migration, and the `422` message says which version and which rule, for support.
3. **One transaction, with longer database deadlines.** The pool's connections abort a statement,
   an idle transaction or a whole transaction after 5 s (`db::STATEMENT_DEADLINE`). An import and
   an account deletion get **60 s** instead (`db::account::ACCOUNT_DEADLINE`): the largest import
   measured, 40,000 sets, took up to 10.8 s on a debug build. `transaction_timeout` is armed when a
   transaction starts, so `SET LOCAL` inside it cannot raise it. The three deadlines are set on a
   connection taken out of the pool before `BEGIN` (`db::account::long_connection`), and that
   connection is closed afterwards, never returned to the pool with them.

   The transaction starts by locking the user's row, like every quota write
   (`docs/billing.md`), so concurrent imports of one user run one after the other. Then the
   account's programs the export names are locked (`FOR UPDATE`, in id order), and every program
   the import adds versions to (a companion) is locked before its versions are read, as
   `add_version` does: an upload of a version at the same moment waits instead of taking a number
   the import has read (`an_import_and_a_concurrent_version_upload_both_succeed`).
4. **Quota: refused, never trimmed.**
   - New unarchived programs count toward the plan's `CustomPrograms` limit. Programs the account
     already has, and archived ones, take no slot.
   - If the new unarchived programs do not all fit, the import is refused with `403` ("Your plan
     keeps up to 10 programs. Archive one, or upgrade to Pro.") before anything is written.
   - Each new unarchived program still goes through `reserve_quota`.
   - Re-importing an export into an account at its limit adds nothing, so it is never refused.
5. **Conflicts: what the account already has wins.** An import only inserts rows the account does
   not have. It never updates or deletes one:

   | Row | Matched by | If the account has it |
   |---|---|---|
   | Settings | the user | kept |
   | Training max | exercise id | kept |
   | Program | `creation_id` | kept as it is: name, archived flag, versions, **current version**, active program |
   | Program version | content (`jsonb` equality), **never the number alone** | an identical document is the account's version. The export's other versions of that program go to its **archived companion** (below). |
   | Active program | the user | kept |
   | Session | session id | kept, with its own sets (the export's sets of that session are not added) |
   | Session in progress | at most one per user | if the account already has another one in progress, the export's one is skipped with its sets |
   | Set | set id | kept |

   Programs and versions get new ids. Every other id is kept.

   **The archived companion.** A program's current version is its highest-numbered one, so a
   version added to an existing program would become what the user trains with. Instead, the
   export's versions that the account's program does not have go to a companion program:
   - named `"<name> (imported)"` after the export's program (shortened to fit 100 characters);
   - **archived**, so it takes no quota slot and is never the active program;
   - with a `creation_id` derived from the original's (UUIDv5 in a fixed namespace,
     `companion_creation_id`), so importing the same export again finds the same companion;
   - holding each version once (matched by content), under its own number if free, else the next
     free one.

   Every program of an import is resolved by `creation_id` through one map: the account's
   programs, and those the import creates (the export's, and the companions). An export that holds
   a program and its companion, whose companion the same import has just created, therefore never
   inserts that `creation_id` twice (`diverging_copies_import_each_others_exports_and_then_nothing`).

   The export's sessions of those versions point at the companion's, so their day always exists
   in their version and their plan loads. **Tradeoff:** those sessions belong to the companion,
   not the original program, so they do not count toward its rotation (the next day) or its
   progression; the history and the personal records, which span every program, include them.
   Only a program the import creates takes the export's versions, and so its current version.
6. **Idempotent.** Importing the same export a second time finds every row already there and adds
   nothing (`ImportSummary` all zero).

The answer is an `ImportSummary`: whether settings and the active program were set, and how many
training maxes, programs, versions, sessions and sets were added.

## Export size

An export larger than `MAX_EXPORT_BYTES` (8 MiB of compact JSON, about 40,000 logged sets) is
refused with `413` and a message to contact support. That keeps every export importable. At three
sessions a week it takes years to reach.

## Versioning

`format_version` changes when a reader of the previous version would misread a document:
- a field is removed or renamed, or its meaning or unit changes;
- a field is added that an import must not ignore.

A new optional field that an older reader can safely ignore does not need a new version. Unknown
fields are ignored on import.

When the version changes, the server keeps reading the versions it can still convert, and this page
documents each one.

| Version | Since | What changed | Imported |
|---|---|---|---|
| 1 | #22 | The first format | yes: its sets have no `target`, as if logged before #60 |
| 2 | #60 | Sets may carry their `target` | yes |

Version 2 only adds an optional field, which an older reader would ignore. It still gets a new
version: a version 1 reader importing it would drop the targets, and the training max sessions it
restores would then be judged with the legacy tolerance instead of exactly, which can change their
verdicts (for a step above 2.5 kg). A version 2 reader reads version 1 as is.

## Account deletion

`delete_account()` (`POST /api/account/delete`) deletes the account and everything above:

- **A fresh sign-in is the confirmation.** The session must have signed in within the last 10
  minutes (`DELETE_REAUTH_WINDOW_SECS`), otherwise `403 To delete your account, sign in again
  first.` The UI then asks for a passkey or Google sign-in and retries.
  - The time is the session's sign-in (`auth.signed_in_at`). Only a sign-in writes it: a passkey
    or Google sign-in (a credential the account had before this session started) or the sign-up.
  - **Adding a sign-in method needs the same step-up** (`AuthContext::require_recent_sign_in`):
    starting and finishing a passkey registration, starting a Google link and completing it in the
    callback. Otherwise a stale session (a stolen cookie, a forgotten tab) could add its own
    passkey or Google account, sign in afresh with it and delete the account.
  - Adding a method never signs in again, so a credential added in a session never makes that
    session fresh.
  - A stolen session cookie, or a page left open, cannot delete the account.
  - The CSRF check refuses cross-site requests.
- **Billing first.** `server::billing::cancel_before_account_deletion` runs before anything is
  deleted, and its failure stops the deletion.
  - Today it is a no-op stub: Stripe is not implemented yet. `docs/billing.md` describes what it
    must do (cancel the subscription immediately).
- **One statement.** `DELETE FROM users` runs in one transaction and cascades to every user-owned
  table. Every sign-in session goes too, so every device is signed out at once.
  - Programs and versions refuse any direct `DELETE` (`forbid_direct_delete`), so this cascade is
    the only way they are ever deleted.
- **Signed out.** The answer clears the cookie. A replayed request has no session left and gets
  `401`.
