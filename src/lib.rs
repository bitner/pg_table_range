use pgrx::prelude::*;
use pgrx::GucSetting;
#[cfg(not(test))]
use pgrx::{GucContext, GucFlags, GucRegistry};

::pgrx::pg_module_magic!(name, version);

mod index_am;
mod prune_hook;
mod summary_build;

/// Master switch for planner-side partition pruning.
pub(crate) static TABLE_RANGE_ENABLE_PRUNING: GucSetting<bool> = GucSetting::<bool>::new(true);
/// Emit per-partition pruning decisions as debug log lines.
pub(crate) static TABLE_RANGE_LOG_PRUNING_DEBUG: GucSetting<bool> = GucSetting::<bool>::new(false);

#[cfg(not(test))]
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_bool_guc(
        c"table_range.enable_pruning",
        c"Enable table_range planner pruning.",
        c"Master switch for planner-side partition candidate pruning.",
        &TABLE_RANGE_ENABLE_PRUNING,
        GucContext::Userset,
        GucFlags::default(),
    );

    GucRegistry::define_bool_guc(
        c"table_range.log_pruning_debug",
        c"Emit pruning diagnostics.",
        c"Logs per-partition prune/keep decisions for debugging.",
        &TABLE_RANGE_LOG_PRUNING_DEBUG,
        GucContext::Userset,
        GucFlags::default(),
    );

    // Install the real planner-time partition pruning hooks.
    prune_hook::install();
    // Register the transaction callback used by the index AM's staleness memo.
    index_am::install();
}

extension_sql!(
    r#"
    -- One summary per (registered relation, leaf partition, column).
    --   index_oid: the registering parent relation OID (or index OID via the AM).
    --   relid:     the leaf partition (heap) OID the planner sees.
    --   attnum:    the leaf partition's attnum for the column (resolved by name).
    --   kind:      'minmax' -> min_summary/max_summary hold the column's btree min/max;
    --             'overlap' -> min_summary holds the covering extent for && pruning
    --                          (range types and PostGIS geometry).
    CREATE TABLE IF NOT EXISTS table_range_summary (
        index_oid oid NOT NULL,
        relid oid NOT NULL,
        attnum int2 NOT NULL,
        kind text NOT NULL DEFAULT 'minmax',
        type_name text,
        min_summary text,
        max_summary text,
        has_nulls boolean NOT NULL DEFAULT false,
        all_nulls boolean NOT NULL DEFAULT false,
        stale boolean NOT NULL DEFAULT false,
        tuple_version int4 NOT NULL DEFAULT 1,
        PRIMARY KEY (index_oid, relid, attnum)
    );

    -- Fast per-partition lookups during planning (the hot path).
    CREATE INDEX IF NOT EXISTS table_range_summary_relid_attnum_idx
        ON table_range_summary (relid, attnum);

    -- Fast maintenance scans for stale summaries only.
    CREATE INDEX IF NOT EXISTS table_range_summary_stale_idx
        ON table_range_summary (relid)
        WHERE stale;

    -- Registration of parent relations whose partitions are summarized for pruning.
    CREATE TABLE IF NOT EXISTS table_range_registered (
        parent_relid oid PRIMARY KEY,
        columns text[] NOT NULL,
        created_at timestamptz NOT NULL DEFAULT now(),
        refreshed_at timestamptz NOT NULL DEFAULT now()
    );

    -- Row/statement trigger that marks a partition's summaries stale when its data
    -- changes. The planner hook ignores stale summaries (treats them as KEEP), so any
    -- modification automatically and conservatively disables pruning for that partition
    -- until table_range_refresh() recomputes it. This is the correctness safety net:
    -- no INSERT/UPDATE/DELETE/TRUNCATE can ever cause a false negative.
    CREATE OR REPLACE FUNCTION table_range_stale_trigger() RETURNS trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            UPDATE table_range_summary SET stale = true WHERE relid = TG_RELID;
            RETURN NULL;
        END;
        $$;

    -- Drop summaries/registration for any relation that is dropped, so a dropped
    -- table_range index can never leave behind a summary that nothing keeps stale.
    CREATE FUNCTION table_range_drop_cleanup() RETURNS event_trigger
        LANGUAGE c AS 'MODULE_PATHNAME', 'table_range_drop_cleanup';
    CREATE EVENT TRIGGER table_range_drop_trg ON sql_drop
        EXECUTE FUNCTION table_range_drop_cleanup();
    "#,
    name = "table_range_bootstrap_sql"
);

/// Diagnostic accessors (test/benchmark only): how many times the planner loaded
/// summaries from the catalog. One load per top-level plan demonstrates the per-plan
/// cache — the count does not grow with the number of partitions.
#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_cache_load_count() -> i64 {
    prune_hook::cache_load_count() as i64
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_extern]
fn table_range_reset_cache_load_count() {
    prune_hook::reset_cache_load_count();
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    // End-to-end real-pruning tests (partitioned tables, EXPLAIN, on/off parity).
    include!("e2e_tests.rs");
    // Custom access method (`CREATE INDEX ... USING table_range`) tests.
    include!("index_am_tests.rs");
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
