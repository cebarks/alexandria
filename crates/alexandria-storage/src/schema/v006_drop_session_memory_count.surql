-- v006: Drop session.memory_count
--
-- Unread since get_session started reporting the live memory list length
-- (delete_memory cannot decrement a stored counter). REMOVE FIELD first:
-- while the field is still a non-optional int, UNSET would fail the type check.

REMOVE FIELD memory_count ON session;
UPDATE session UNSET memory_count;
