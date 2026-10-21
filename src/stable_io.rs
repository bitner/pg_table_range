use pgrx::pg_sys;

// shortcut: summary bounds are stored as text, so these GUCs must be pinned around every
// write and read; drop this module when bounds are stored as binary datums.

/// Run `f` with DateStyle, IntervalStyle and extra_float_digits pinned to values whose
/// text output round-trips exactly. `None` (f not run) in parallel mode: treat as unknown.
pub(crate) unsafe fn with_stable_io<T>(f: impl FnOnce() -> T) -> Option<T> {
    // ISO output parses the same under any DateStyle field order.
    let need_date = pg_sys::DateStyle != pg_sys::USE_ISO_DATES as i32;
    let need_interval = pg_sys::IntervalStyle != pg_sys::INTSTYLE_POSTGRES as i32;
    // Values < 1 drop float precision; any value >= 1 is shortest-exact.
    let need_float = pg_sys::extra_float_digits < 1;
    if !(need_date || need_interval || need_float) {
        return Some(f());
    }
    if pg_sys::IsInParallelMode() {
        return None;
    }

    let level = pg_sys::NewGUCNestLevel();
    if need_date {
        set_guc(c"DateStyle", c"ISO");
    }
    if need_interval {
        set_guc(c"IntervalStyle", c"postgres");
    }
    if need_float {
        set_guc(c"extra_float_digits", c"1");
    }
    Some(
        pgrx::PgTryBuilder::new(std::panic::AssertUnwindSafe(f))
            .finally(|| pg_sys::AtEOXact_GUC(true, level))
            .execute(),
    )
}

unsafe fn set_guc(name: &std::ffi::CStr, value: &std::ffi::CStr) {
    pg_sys::set_config_option(
        name.as_ptr(),
        value.as_ptr(),
        pg_sys::GucContext::PGC_USERSET,
        pg_sys::GucSource::PGC_S_SESSION,
        pg_sys::GucAction::GUC_ACTION_SAVE,
        true,
        0,
        false,
    );
}
