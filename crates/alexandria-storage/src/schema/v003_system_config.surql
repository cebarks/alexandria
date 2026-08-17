-- v003: System config for migration tracking and model safety (Alexandria v0.2)

DEFINE TABLE IF NOT EXISTS system_config SCHEMAFULL;
DEFINE FIELD key        ON system_config TYPE string;
DEFINE FIELD value      ON system_config TYPE string;
DEFINE FIELD updated_at ON system_config TYPE datetime DEFAULT time::now();
DEFINE INDEX idx_system_config_key ON system_config FIELDS key UNIQUE;
