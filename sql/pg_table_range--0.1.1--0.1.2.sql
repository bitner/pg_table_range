-- pg_table_range 0.1.1 -> 0.1.2
--
-- No SQL objects change. The on-page summary format does (version 2), and summaries
-- written by 0.1.1 can be narrower than the data they describe, so 0.1.2 never prunes
-- with them. Pruning resumes for each table_range index once it is rebuilt.
DO $$
DECLARE
    n bigint;
BEGIN
    SELECT count(*) INTO n
    FROM pg_catalog.pg_index i
    JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
    JOIN pg_catalog.pg_am am ON am.oid = c.relam
    WHERE am.amname = 'table_range' AND c.relkind = 'i';
    IF n > 0 THEN
        -- Extension scripts run with client_min_messages >= WARNING, so a NOTICE would
        -- never reach the user.
        RAISE WARNING 'pg_table_range: % table_range index(es) must be rebuilt with REINDEX before they prune again', n
            USING HINT = 'Summaries built by 0.1.1 are ignored by 0.1.2. REINDEX each table_range index (on a partitioned table, REINDEX the parent index).';
    END IF;
END
$$;
