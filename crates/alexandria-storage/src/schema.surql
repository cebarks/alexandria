-- Tier 0: Raw session logs
DEFINE TABLE raw SCHEMAFULL;
DEFINE FIELD content      ON raw TYPE string;
DEFINE FIELD created_at   ON raw TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON raw TYPE option<object>;
DEFINE FIELD deleted      ON raw TYPE bool DEFAULT false;

-- Tier 1: Extracted facts
DEFINE TABLE fact SCHEMAFULL;
DEFINE FIELD content      ON fact TYPE string;
DEFINE FIELD confidence   ON fact TYPE float DEFAULT 0.5;
DEFINE FIELD embedding    ON fact TYPE array<float>;
DEFINE FIELD tags         ON fact TYPE array<string> DEFAULT [];
DEFINE FIELD created_at   ON fact TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON fact TYPE option<object>;
DEFINE FIELD deleted      ON fact TYPE bool DEFAULT false;

-- Provenance
DEFINE TABLE provenance SCHEMAFULL;
DEFINE FIELD kind         ON provenance TYPE string;
DEFINE FIELD session_id   ON provenance TYPE option<string>;
DEFINE FIELD agent_id     ON provenance TYPE option<string>;
DEFINE FIELD model        ON provenance TYPE option<string>;
DEFINE FIELD timestamp    ON provenance TYPE datetime DEFAULT time::now();
DEFINE FIELD metadata     ON provenance TYPE option<object>;

-- Provenance linkage
DEFINE TABLE has_provenance SCHEMAFULL TYPE RELATION;

-- Heat state (separate table)
DEFINE TABLE heat_state SCHEMAFULL;
DEFINE FIELD memory       ON heat_state TYPE record;
DEFINE FIELD heat         ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD stability    ON heat_state TYPE float DEFAULT 1.0;
DEFINE FIELD last_touched ON heat_state TYPE datetime DEFAULT time::now();
DEFINE FIELD access_count ON heat_state TYPE int DEFAULT 0;

-- Clusters
DEFINE TABLE cluster SCHEMAFULL;
DEFINE FIELD label        ON cluster TYPE option<string>;
DEFINE FIELD centroid     ON cluster TYPE array<float>;
DEFINE FIELD depth        ON cluster TYPE int DEFAULT 0;
DEFINE FIELD created_at   ON cluster TYPE datetime DEFAULT time::now();

DEFINE TABLE contains_cluster SCHEMAFULL TYPE RELATION IN cluster OUT cluster;
DEFINE TABLE contains_memory  SCHEMAFULL TYPE RELATION IN cluster OUT fact;

-- Lineage
DEFINE TABLE extracted_from SCHEMAFULL TYPE RELATION IN fact OUT raw;

-- Embedding config
DEFINE TABLE embedding_config SCHEMAFULL;
DEFINE FIELD model_id     ON embedding_config TYPE string;
DEFINE FIELD dimensions   ON embedding_config TYPE int;
DEFINE FIELD updated_at   ON embedding_config TYPE datetime DEFAULT time::now();
