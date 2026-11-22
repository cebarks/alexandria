-- Audit administrative recovery issuance without storing the link secret or its digest.
DEFINE TABLE OVERWRITE admin_recovery_log SCHEMAFULL;
DEFINE FIELD OVERWRITE actor      ON admin_recovery_log TYPE record<account>;
DEFINE FIELD OVERWRITE target     ON admin_recovery_log TYPE record<account>;
DEFINE FIELD OVERWRITE credential ON admin_recovery_log TYPE record<credential>;
DEFINE FIELD OVERWRITE created_at ON admin_recovery_log TYPE datetime;
