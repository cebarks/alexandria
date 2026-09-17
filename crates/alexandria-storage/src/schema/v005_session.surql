-- v005: Session memory

DEFINE TABLE OVERWRITE session SCHEMAFULL;
DEFINE FIELD OVERWRITE external_id   ON session TYPE string;
DEFINE FIELD OVERWRITE agent_id      ON session TYPE option<string>;
DEFINE FIELD OVERWRITE model         ON session TYPE option<string>;
DEFINE FIELD OVERWRITE started_at    ON session TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE ended_at      ON session TYPE option<datetime>;
DEFINE FIELD OVERWRITE summary       ON session TYPE option<string>;
DEFINE FIELD OVERWRITE memory_count  ON session TYPE int DEFAULT 0;
DEFINE FIELD OVERWRITE tags          ON session TYPE array<string> DEFAULT [];

DEFINE INDEX OVERWRITE session_external_id_idx ON session FIELDS external_id UNIQUE;

DEFINE TABLE OVERWRITE contains_session_memory SCHEMAFULL TYPE RELATION IN session OUT fact;
