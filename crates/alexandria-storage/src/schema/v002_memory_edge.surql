-- v002: Graph edges between memories (Alexandria v0.2)

DEFINE TABLE OVERWRITE memory_edge SCHEMAFULL TYPE RELATION FROM fact TO fact;
DEFINE FIELD OVERWRITE edge_type   ON memory_edge TYPE string;
DEFINE FIELD OVERWRITE strength    ON memory_edge TYPE float DEFAULT 1.0;
DEFINE FIELD OVERWRITE created_at  ON memory_edge TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_memory_edge_type ON memory_edge FIELDS edge_type;
