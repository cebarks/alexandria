-- v004: Maintenance log for tracking cluster split/merge operations

DEFINE TABLE maintenance_log SCHEMAFULL;
DEFINE FIELD action       ON maintenance_log TYPE string;          -- "split" or "merge"
DEFINE FIELD source_id    ON maintenance_log TYPE string;          -- cluster that was split or merged away
DEFINE FIELD target_ids   ON maintenance_log TYPE array<string>;   -- resulting cluster(s)
DEFINE FIELD members_moved ON maintenance_log TYPE int DEFAULT 0;
DEFINE FIELD created_at   ON maintenance_log TYPE datetime DEFAULT time::now();
DEFINE INDEX idx_maintenance_log_action ON maintenance_log FIELDS action;
DEFINE INDEX idx_maintenance_log_created ON maintenance_log FIELDS created_at;
