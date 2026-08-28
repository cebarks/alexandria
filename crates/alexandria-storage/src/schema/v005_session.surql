-- v005: Session memory

DEFINE TABLE session SCHEMAFULL;
DEFINE FIELD external_id   ON session TYPE string;
DEFINE FIELD agent_id      ON session TYPE option<string>;
DEFINE FIELD model         ON session TYPE option<string>;
DEFINE FIELD started_at    ON session TYPE datetime DEFAULT time::now();
DEFINE FIELD ended_at      ON session TYPE option<datetime>;
DEFINE FIELD summary       ON session TYPE option<string>;
DEFINE FIELD memory_count  ON session TYPE int DEFAULT 0;
DEFINE FIELD tags          ON session TYPE array<string> DEFAULT [];

DEFINE INDEX session_external_id_idx ON session FIELDS external_id UNIQUE;

DEFINE TABLE contains_session_memory SCHEMAFULL TYPE RELATION IN session OUT fact;
