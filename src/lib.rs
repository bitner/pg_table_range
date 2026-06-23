use pgrx::prelude::*;
use pgrx::GucSetting;
#[cfg(not(test))]
use pgrx::{GucContext, GucFlags, GucRegistry};

::pgrx::pg_module_magic!(name, version);

mod index_am;
mod index_storage;
mod prune_hook;
mod summary_build;
mod summary_cache;

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
    // Register the relcache callback that keeps the per-index summary cache coherent.
    summary_cache::register();
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
