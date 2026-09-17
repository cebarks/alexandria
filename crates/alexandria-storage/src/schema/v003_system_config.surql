-- v003: System config for migration tracking and model safety (Alexandria v0.2)

DEFINE TABLE IF NOT EXISTS system_config SCHEMAFULL;
DEFINE FIELD OVERWRITE key        ON system_config TYPE string;
DEFINE FIELD OVERWRITE value      ON system_config TYPE string;
DEFINE FIELD OVERWRITE updated_at ON system_config TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_system_config_key ON system_config FIELDS key UNIQUE;
