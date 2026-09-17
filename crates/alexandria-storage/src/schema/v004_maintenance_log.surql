-- v004: Maintenance log for tracking cluster split/merge operations

DEFINE TABLE OVERWRITE maintenance_log SCHEMAFULL;
DEFINE FIELD OVERWRITE action       ON maintenance_log TYPE string;          -- "split" or "merge"
DEFINE FIELD OVERWRITE source_id    ON maintenance_log TYPE string;          -- cluster that was split or merged away
DEFINE FIELD OVERWRITE target_ids   ON maintenance_log TYPE array<string>;   -- resulting cluster(s)
DEFINE FIELD OVERWRITE members_moved ON maintenance_log TYPE int DEFAULT 0;
DEFINE FIELD OVERWRITE created_at   ON maintenance_log TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_maintenance_log_action ON maintenance_log FIELDS action;
DEFINE INDEX OVERWRITE idx_maintenance_log_created ON maintenance_log FIELDS created_at;
