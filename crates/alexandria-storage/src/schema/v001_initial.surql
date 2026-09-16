-- v001: Initial schema (Alexandria v0.1)

-- Tier 0: Raw session logs
DEFINE TABLE OVERWRITE raw SCHEMAFULL;
DEFINE FIELD OVERWRITE content      ON raw TYPE string;
DEFINE FIELD OVERWRITE created_at   ON raw TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE metadata     ON raw TYPE option<object>;
DEFINE FIELD OVERWRITE deleted      ON raw TYPE bool DEFAULT false;

-- Tier 1: Extracted facts
DEFINE TABLE OVERWRITE fact SCHEMAFULL;
DEFINE FIELD OVERWRITE content      ON fact TYPE string;
DEFINE FIELD OVERWRITE confidence   ON fact TYPE float DEFAULT 0.5;
DEFINE FIELD OVERWRITE embedding    ON fact TYPE array<float>;
DEFINE FIELD OVERWRITE tags         ON fact TYPE array<string> DEFAULT [];
DEFINE FIELD OVERWRITE created_at   ON fact TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE metadata     ON fact TYPE option<object>;
DEFINE FIELD OVERWRITE deleted      ON fact TYPE bool DEFAULT false;

-- Provenance
DEFINE TABLE OVERWRITE provenance SCHEMAFULL;
DEFINE FIELD OVERWRITE kind         ON provenance TYPE string;
DEFINE FIELD OVERWRITE session_id   ON provenance TYPE option<string>;
DEFINE FIELD OVERWRITE agent_id     ON provenance TYPE option<string>;
DEFINE FIELD OVERWRITE model        ON provenance TYPE option<string>;
DEFINE FIELD OVERWRITE timestamp    ON provenance TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE metadata     ON provenance TYPE option<object>;

-- Provenance linkage
DEFINE TABLE OVERWRITE has_provenance SCHEMAFULL TYPE RELATION;

-- Heat state (separate table for cache locality)
DEFINE TABLE OVERWRITE heat_state SCHEMAFULL;
DEFINE FIELD OVERWRITE memory       ON heat_state TYPE record;
DEFINE FIELD OVERWRITE heat         ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD OVERWRITE stability    ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD OVERWRITE last_touched ON heat_state TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE access_count ON heat_state TYPE int DEFAULT 0;

-- Clusters
DEFINE TABLE OVERWRITE cluster SCHEMAFULL;
DEFINE FIELD OVERWRITE label        ON cluster TYPE option<string>;
DEFINE FIELD OVERWRITE centroid     ON cluster TYPE array<float>;
DEFINE FIELD OVERWRITE depth        ON cluster TYPE int DEFAULT 0;
DEFINE FIELD OVERWRITE created_at   ON cluster TYPE datetime DEFAULT time::now();

DEFINE TABLE OVERWRITE contains_cluster SCHEMAFULL TYPE RELATION IN cluster OUT cluster;
DEFINE TABLE OVERWRITE contains_memory  SCHEMAFULL TYPE RELATION IN cluster OUT fact;

-- Lineage
DEFINE TABLE OVERWRITE extracted_from SCHEMAFULL TYPE RELATION IN fact OUT raw;

-- Embedding config
DEFINE TABLE OVERWRITE embedding_config SCHEMAFULL;
DEFINE FIELD OVERWRITE model_id     ON embedding_config TYPE string;
DEFINE FIELD OVERWRITE dimensions   ON embedding_config TYPE int;
DEFINE FIELD OVERWRITE updated_at   ON embedding_config TYPE datetime DEFAULT time::now();
