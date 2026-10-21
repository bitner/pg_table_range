-- pg_table_range 0.1.2 -> 0.2.0
--
-- Adds table_range_summarize(). Summaries written by 0.1.2 (format version 2) are still
-- read, so no REINDEX is needed.
CREATE FUNCTION "table_range_summarize"(
	"index" oid
) RETURNS INT
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'table_range_summarize_wrapper';
