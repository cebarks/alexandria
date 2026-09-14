-- v007: Reminders

DEFINE TABLE reminder SCHEMAFULL;
DEFINE FIELD message          ON reminder TYPE string;
DEFINE FIELD target_project   ON reminder TYPE option<string>;
DEFINE FIELD prov_project     ON reminder TYPE option<string>;
DEFINE FIELD prov_session_id  ON reminder TYPE option<string>;
DEFINE FIELD note             ON reminder TYPE option<string>;
DEFINE FIELD schedule_kind    ON reminder TYPE string ASSERT $value IN ['once', 'pattern', 'cron'];
DEFINE FIELD due_at           ON reminder TYPE option<datetime>;
DEFINE FIELD freq             ON reminder TYPE option<string>;
DEFINE FIELD time_of_day      ON reminder TYPE option<string>;
DEFINE FIELD weekdays         ON reminder TYPE array<string> DEFAULT [];
DEFINE FIELD day_of_month     ON reminder TYPE option<int>;
DEFINE FIELD cron_expr        ON reminder TYPE option<string>;
DEFINE FIELD next_due_at      ON reminder TYPE option<datetime>;
DEFINE FIELD status           ON reminder TYPE string DEFAULT 'pending' ASSERT $value IN ['pending', 'delivered', 'cancelled'];
DEFINE FIELD created_at       ON reminder TYPE datetime DEFAULT time::now();
DEFINE FIELD cancelled_at     ON reminder TYPE option<datetime>;
DEFINE FIELD last_delivered_at ON reminder TYPE option<datetime>;
DEFINE FIELD delivered_count  ON reminder TYPE int DEFAULT 0;

DEFINE INDEX reminder_due_idx ON reminder FIELDS status, next_due_at;
