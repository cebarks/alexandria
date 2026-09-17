-- v007: Reminders

DEFINE TABLE OVERWRITE reminder SCHEMAFULL;
-- Non-empty on purpose, not just `TYPE string`: `set_reminder(message: "")` would
-- otherwise store happily, report `status: ok` with a fire preview, and then
-- deliver a bullet with nothing in it. The tool layer rejects the same case; this
-- is the boundary that also covers admin and migration writes, and it trims so a
-- whitespace-only message cannot walk through the same door.
DEFINE FIELD OVERWRITE message          ON reminder TYPE string ASSERT $value != NONE AND string::len(string::trim($value)) > 0;
DEFINE FIELD OVERWRITE target_project   ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE prov_project     ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE prov_session_id  ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE note             ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE schedule_kind    ON reminder TYPE string ASSERT $value IN ['once', 'pattern', 'cron'];
DEFINE FIELD OVERWRITE due_at           ON reminder TYPE option<datetime>;
DEFINE FIELD OVERWRITE freq             ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE time_of_day      ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE weekdays         ON reminder TYPE array<string> DEFAULT [];
DEFINE FIELD OVERWRITE day_of_month     ON reminder TYPE option<int>;
DEFINE FIELD OVERWRITE cron_expr        ON reminder TYPE option<string>;
DEFINE FIELD OVERWRITE next_due_at      ON reminder TYPE option<datetime>;
DEFINE FIELD OVERWRITE status           ON reminder TYPE string DEFAULT 'pending' ASSERT $value IN ['pending', 'delivered', 'cancelled'];
DEFINE FIELD OVERWRITE created_at       ON reminder TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE cancelled_at     ON reminder TYPE option<datetime>;
DEFINE FIELD OVERWRITE last_delivered_at ON reminder TYPE option<datetime>;
DEFINE FIELD OVERWRITE delivered_count  ON reminder TYPE int DEFAULT 0;

DEFINE INDEX OVERWRITE reminder_due_idx ON reminder FIELDS status, next_due_at;
