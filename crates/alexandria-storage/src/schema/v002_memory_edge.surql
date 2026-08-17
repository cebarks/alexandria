-- v002: Graph edges between memories (Alexandria v0.2)

DEFINE TABLE memory_edge SCHEMAFULL TYPE RELATION FROM fact TO fact;
DEFINE FIELD edge_type   ON memory_edge TYPE string;
DEFINE FIELD strength    ON memory_edge TYPE float DEFAULT 1.0;
DEFINE FIELD created_at  ON memory_edge TYPE datetime DEFAULT time::now();
DEFINE INDEX idx_memory_edge_type ON memory_edge FIELDS edge_type;
