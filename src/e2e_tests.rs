// End-to-end pruning tests. This file is `include!`d inside `#[pg_schema] mod tests`
// in lib.rs so the generated SQL wrapper functions land in the `tests` schema that
// the pgrx test harness invokes.
//
// These exercise the full path: create a partitioned table, populate disjoint value
// ranges per partition, build summaries with `CREATE INDEX ... USING table_range`, then
// verify that (a) the planner eliminates non-matching partitions (via EXPLAIN) and
// (b) results are identical with pruning on and off (no false negatives).
//
// The partition key (`region`) deliberately differs from the queried data column, so
// native PostgreSQL partition pruning cannot help — only the table_range summaries can.

/// Build summaries for `cols` of `table` via a table_range index named `<table>_tr`.
fn e2e_build(table: &str, cols: &str) {
    Spi::run(&format!(
        "CREATE INDEX {table}_tr ON {table} USING table_range ({cols})"
    ))
    .expect("create table_range index");
}

/// Build a 3-way LIST-partitioned table with disjoint `val` ranges:
///   events_r1: region=1, val in [0, 99]
///   events_r2: region=2, val in [100, 199]
///   events_r3: region=3, val in [200, 299]
/// Then summarize `val`.
fn e2e_setup_events() {
    Spi::run(
        "DROP TABLE IF EXISTS events CASCADE;
         CREATE TABLE events (region int, val bigint) PARTITION BY LIST (region);
         CREATE TABLE events_r1 PARTITION OF events FOR VALUES IN (1);
         CREATE TABLE events_r2 PARTITION OF events FOR VALUES IN (2);
         CREATE TABLE events_r3 PARTITION OF events FOR VALUES IN (3);
         INSERT INTO events SELECT 1, g FROM generate_series(0, 99) g;
         INSERT INTO events SELECT 2, g FROM generate_series(100, 199) g;
         INSERT INTO events SELECT 3, g FROM generate_series(200, 299) g;",
    )
    .expect("setup events");
    e2e_build("events", "val");
}

fn e2e_explain(query: &str) -> String {
    Spi::connect(|client| {
        let table = client.select(&format!("EXPLAIN (COSTS OFF) {query}"), None, &[])?;
        let mut out = String::new();
        for row in table {
            if let Ok(Some(line)) = row.get::<String>(1) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        Ok::<String, pgrx::spi::SpiError>(out)
    })
    .expect("explain")
}

fn e2e_explain_on(table: &str, pred: &str) -> String {
    e2e_explain(&format!("SELECT * FROM {table} WHERE {pred}"))
}

fn e2e_set_pruning(on: bool) {
    Spi::run(&format!(
        "SET table_range.enable_pruning = {}",
        if on { "on" } else { "off" }
    ))
    .expect("set guc");
}

fn e2e_count_where(pred: &str) -> i64 {
    Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM events WHERE {pred}"))
        .expect("count")
        .expect("count value")
}

/// Count rows of `table` matching `pred`.
fn e2e_count_in(table: &str, pred: &str) -> i64 {
    Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM {table} WHERE {pred}"))
        .expect("count")
        .expect("count value")
}

/// Count matching rows with pruning on and off, assert they agree, and return the count.
fn e2e_same_on_off(table: &str, pred: &str) -> i64 {
    e2e_set_pruning(false);
    let off = e2e_count_in(table, pred);
    e2e_set_pruning(true);
    let on = e2e_count_in(table, pred);
    assert_eq!(on, off, "pruning changed results for `{pred}`: on={on} off={off}");
    on
}

/// A table `name` LIST-partitioned on `k` into `{name}_1` (k=1) and `{name}_2` (k=2).
fn e2e_two_partitions(name: &str, columns: &str) {
    Spi::run(&format!(
        "DROP TABLE IF EXISTS {name} CASCADE;
         CREATE TABLE {name} (k int, {columns}) PARTITION BY LIST (k);
         CREATE TABLE {name}_1 PARTITION OF {name} FOR VALUES IN (1);
         CREATE TABLE {name}_2 PARTITION OF {name} FOR VALUES IN (2);"
    ))
    .expect("create partitioned table");
}

#[pg_test]
fn e2e_prunes_lower_partitions_for_high_predicate() {
    e2e_setup_events();
    e2e_set_pruning(true);
    let plan = e2e_explain("SELECT * FROM events WHERE val >= 250");
    assert!(plan.contains("events_r3"), "r3 must remain:\n{plan}");
    assert!(!plan.contains("events_r1"), "r1 must be pruned:\n{plan}");
    assert!(!plan.contains("events_r2"), "r2 must be pruned:\n{plan}");
}

#[pg_test]
fn e2e_prunes_upper_partitions_for_low_predicate() {
    e2e_setup_events();
    e2e_set_pruning(true);
    let plan = e2e_explain("SELECT * FROM events WHERE val < 50");
    assert!(plan.contains("events_r1"), "r1 must remain:\n{plan}");
    assert!(!plan.contains("events_r2"), "r2 must be pruned:\n{plan}");
    assert!(!plan.contains("events_r3"), "r3 must be pruned:\n{plan}");
}

#[pg_test]
fn e2e_keeps_all_partitions_for_wide_predicate() {
    e2e_setup_events();
    e2e_set_pruning(true);
    let plan = e2e_explain("SELECT * FROM events WHERE val >= 0");
    assert!(plan.contains("events_r1"));
    assert!(plan.contains("events_r2"));
    assert!(plan.contains("events_r3"));
}

#[pg_test]
fn e2e_results_identical_with_pruning_on_and_off() {
    e2e_setup_events();
    let predicates = [
        "val >= 250",
        "val < 50",
        "val = 150",
        "val BETWEEN 90 AND 110",
        "val > 1000",
        "val <= 0",
        "val IN (5, 150, 295)",
    ];
    for pred in predicates {
        e2e_set_pruning(false);
        let off = e2e_count_where(pred);
        e2e_set_pruning(true);
        let on = e2e_count_where(pred);
        assert_eq!(on, off, "pruning changed results for `{pred}`: on={on} off={off}");
    }
}

#[pg_test]
fn e2e_boundary_equality_keeps_correct_partition() {
    e2e_setup_events();
    e2e_set_pruning(true);
    let plan = e2e_explain("SELECT * FROM events WHERE val = 100");
    assert!(plan.contains("events_r2"), "r2 must remain:\n{plan}");
    assert!(!plan.contains("events_r1"), "r1 pruned:\n{plan}");
    assert!(!plan.contains("events_r3"), "r3 pruned:\n{plan}");
    assert_eq!(e2e_count_where("val = 100"), 1);
}

#[pg_test]
fn e2e_disabled_pruning_scans_all_partitions() {
    e2e_setup_events();
    e2e_set_pruning(false);
    let plan = e2e_explain("SELECT * FROM events WHERE val >= 250");
    assert!(plan.contains("events_r1"));
    assert!(plan.contains("events_r2"));
    assert!(plan.contains("events_r3"));
}

#[pg_test]
fn e2e_works_on_plain_unpartitioned_table() {
    Spi::run(
        "DROP TABLE IF EXISTS plain_t CASCADE;
         CREATE TABLE plain_t (val bigint);
         INSERT INTO plain_t SELECT g FROM generate_series(0, 99) g;",
    )
    .unwrap();
    e2e_build("plain_t", "val");
    e2e_set_pruning(true);
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM plain_t WHERE val >= 50")
            .unwrap()
            .unwrap(),
        50
    );
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM plain_t WHERE val > 1000")
            .unwrap()
            .unwrap(),
        0
    );
}

#[pg_test]
fn e2e_insert_widens_summary() {
    e2e_setup_events();
    e2e_set_pruning(true);
    // Sanity: before the insert, val=500 prunes everything (no partition covers it).
    assert_eq!(e2e_count_where("val = 500"), 0);

    // Insert a value far outside r1's summarized range. aminsert widens r1's summary to
    // cover it, so the new row is found and r1 is kept for it.
    Spi::run("INSERT INTO events VALUES (1, 500)").expect("insert");
    assert_eq!(
        e2e_count_where("val = 500"),
        1,
        "widened summary must cover the newly inserted row"
    );
    let plan = e2e_explain("SELECT * FROM events WHERE val = 500");
    assert!(plan.contains("events_r1"), "r1 kept after widening:\n{plan}");
}

#[pg_test]
fn e2e_delete_keeps_results_correct() {
    e2e_setup_events();
    e2e_set_pruning(true);
    // Deleting rows can only shrink a partition's true range; a now-too-wide summary is
    // conservative (safe). Results must stay correct.
    Spi::run("DELETE FROM events WHERE region = 2").expect("delete");
    for pred in ["val = 150", "val >= 100 AND val < 200", "val < 50"] {
        e2e_set_pruning(false);
        let off = e2e_count_where(pred);
        e2e_set_pruning(true);
        let on = e2e_count_where(pred);
        assert_eq!(on, off, "delete made `{pred}` incorrect: on={on} off={off}");
    }
}

#[pg_test]
fn e2e_large_tree_prunes_to_single_partition() {
    // 32 range partitions with disjoint val ranges; exercises the per-plan cache.
    Spi::run(
        "DROP TABLE IF EXISTS big CASCADE;
         CREATE TABLE big (val bigint) PARTITION BY RANGE (val);",
    )
    .unwrap();
    for i in 0..32 {
        let lo = i * 100;
        let hi = lo + 100;
        Spi::run(&format!(
            "CREATE TABLE big_p{i} PARTITION OF big FOR VALUES FROM ({lo}) TO ({hi});
             INSERT INTO big SELECT g FROM generate_series({lo}, {hi} - 1) g;"
        ))
        .unwrap();
    }
    e2e_build("big", "val");
    e2e_set_pruning(true);

    // Value 1750 lives only in partition p17 (1700..1799).
    let plan = e2e_explain_on("big", "val = 1750");
    assert!(plan.contains("big_p17"), "p17 kept:\n{plan}");
    let scans = plan.matches("big_p").count();
    assert_eq!(scans, 1, "expected a single surviving partition:\n{plan}");

    let count_big = |pred: &str| {
        Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM big WHERE {pred}"))
            .unwrap()
            .unwrap()
    };
    assert_eq!(count_big("val = 1750"), 1);
    assert_eq!(count_big("val >= 3150"), 50); // p31 (3100..3199): 3150..3199
}


/// True if PostGIS can be created in this environment. Checked via the catalog so a
/// missing extension does not abort the test transaction.
fn postgis_available() -> bool {
    Spi::get_one::<i64>("SELECT count(*)::bigint FROM pg_available_extensions WHERE name = 'postgis'")
        .ok()
        .flatten()
        .unwrap_or(0)
        > 0
}

#[pg_test]
fn e2e_postgis_extent_pruning() {
    // PostGIS is not installed in every test environment (e.g. the pgrx-managed pg18);
    // skip gracefully there. CI installs PostGIS so this runs for real. Creating the
    // extension fires our event trigger, which registers the geometry opclass so
    // CREATE INDEX ... USING table_range (geom) resolves with no manual step.
    if !postgis_available() {
        return;
    }
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    Spi::run(
        "DROP TABLE IF EXISTS ev_g CASCADE;
         CREATE TABLE ev_g (region int, geom geometry) PARTITION BY LIST (region);
         CREATE TABLE ev_g_1 PARTITION OF ev_g FOR VALUES IN (1);
         CREATE TABLE ev_g_2 PARTITION OF ev_g FOR VALUES IN (2);
         CREATE TABLE ev_g_3 PARTITION OF ev_g FOR VALUES IN (3);
         INSERT INTO ev_g SELECT 1, ST_MakePoint(x, y) FROM generate_series(0,10) x, generate_series(0,10) y;
         INSERT INTO ev_g SELECT 2, ST_MakePoint(100+x, 100+y) FROM generate_series(0,10) x, generate_series(0,10) y;
         INSERT INTO ev_g SELECT 3, ST_MakePoint(200+x, 200+y) FROM generate_series(0,10) x, generate_series(0,10) y;",
    )
    .unwrap();
    e2e_build("ev_g", "geom");
    e2e_set_pruning(true);

    // A query box over partition 3's extent prunes partitions 1 and 2.
    let plan = e2e_explain_on("ev_g", "geom && ST_MakeEnvelope(200,200,205,205)");
    assert!(plan.contains("ev_g_3"), "r3 overlaps, must remain:\n{plan}");
    assert!(!plan.contains("ev_g_1"), "r1 pruned:\n{plan}");
    assert!(!plan.contains("ev_g_2"), "r2 pruned:\n{plan}");

    let count = |pred: &str| {
        Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM ev_g WHERE {pred}"))
            .unwrap()
            .unwrap()
    };
    for pred in [
        "geom && ST_MakeEnvelope(200,200,205,205)", // only r3
        "geom && ST_MakeEnvelope(5,5,6,6)",         // only r1
        "geom && ST_MakeEnvelope(500,500,600,600)", // nothing
        "geom && ST_MakeEnvelope(0,0,300,300)",     // everything
    ] {
        e2e_set_pruning(false);
        let off = count(pred);
        e2e_set_pruning(true);
        let on = count(pred);
        assert_eq!(on, off, "geometry overlap pruning changed results for `{pred}`");
    }
}

#[pg_test]
fn e2e_postgis_inserts_keep_exact_extent() {
    // One statement mixes covered points, widening points and polygons, SRID 4326 points,
    // empty geometries and NULLs; the extent must stay exact enough to prune right past it.
    if !postgis_available() {
        return;
    }
    Spi::run("CREATE EXTENSION IF NOT EXISTS postgis").unwrap();
    e2e_two_partitions("gi", "g geometry");
    Spi::run(
        "INSERT INTO gi VALUES (1, ST_MakePoint(0, 0)), (1, ST_MakePoint(10, 10)),
                               (2, ST_MakePoint(500, 500));
         CREATE INDEX gi_tr ON gi USING table_range (g);
         INSERT INTO gi
         SELECT 1, CASE
             WHEN i % 11 = 0 THEN NULL
             WHEN i % 13 = 0 THEN 'POINT EMPTY'::geometry
             WHEN i % 17 = 0 THEN ST_SetSRID(ST_MakePoint(i % 10, i % 10), 4326)
             WHEN i % 19 = 0 THEN ST_MakeEnvelope(-i, -i, -i + 1, -i + 1)
             WHEN i % 23 = 0 THEN ST_MakePoint(10 + i, 5)
             ELSE ST_MakePoint(i % 10, (i * 7) % 10) END
         FROM generate_series(1, 3000) i;",
    )
    .unwrap();
    for pred in [
        "g && ST_MakeEnvelope(-2995, -2995, -2990, -2990)",
        "g && ST_MakeEnvelope(2950, 4, 3020, 6)",
        "g && ST_MakeEnvelope(3, 3, 4, 4)",
        "g && ST_MakeEnvelope(3, 3, 4, 4, 4326)",
        "g && ST_MakeEnvelope(9000, 9000, 9001, 9001)",
    ] {
        e2e_same_on_off("gi", pred);
    }
    // A domain over geometry resolves PostGIS functions in geometry's schema.
    Spi::run("DROP DOMAIN IF EXISTS gi_point CASCADE; CREATE DOMAIN gi_point AS geometry;")
        .unwrap();
    e2e_two_partitions("gd", "g gi_point");
    Spi::run(
        "INSERT INTO gd VALUES (1, ST_MakePoint(0, 0)), (2, ST_MakePoint(500, 500));
         CREATE INDEX gd_tr ON gd USING table_range (g);
         INSERT INTO gd SELECT 1, ST_MakePoint(i, i) FROM generate_series(1, 50) i;",
    )
    .unwrap();
    assert!(e2e_same_on_off("gd", "g && ST_MakeEnvelope(49, 49, 51, 51)") > 0);
    let plan = e2e_explain_on("gd", "g && ST_MakeEnvelope(60, 60, 61, 61)");
    assert!(!plan.contains("on gd_1"), "{plan}");

    // The data's extent ends at x = 2977.
    let plan = e2e_explain_on("gi", "g && ST_MakeEnvelope(2976.5, 0, 2977, 1)");
    assert!(plan.contains("on gi_1"), "a box at the edge keeps gi_1:\n{plan}");
    let plan = e2e_explain_on("gi", "g && ST_MakeEnvelope(2978, 0, 2979, 1)");
    assert!(!plan.contains("on gi_1"), "a box past the edge prunes gi_1:\n{plan}");
}

#[pg_test]
fn e2e_range_overlap_pruning() {
    // Partitions hold disjoint int8range bands; query with the && (overlap) operator on
    // a non-key range column. Only the overlapping partition survives.
    Spi::run(
        "DROP TABLE IF EXISTS ev_r CASCADE;
         CREATE TABLE ev_r (region int, period int8range) PARTITION BY LIST (region);
         CREATE TABLE ev_r_1 PARTITION OF ev_r FOR VALUES IN (1);
         CREATE TABLE ev_r_2 PARTITION OF ev_r FOR VALUES IN (2);
         CREATE TABLE ev_r_3 PARTITION OF ev_r FOR VALUES IN (3);
         INSERT INTO ev_r SELECT 1, int8range(g*10, g*10+10) FROM generate_series(0,9) g;
         INSERT INTO ev_r SELECT 2, int8range(100+g*10, 100+g*10+10) FROM generate_series(0,9) g;
         INSERT INTO ev_r SELECT 3, int8range(200+g*10, 200+g*10+10) FROM generate_series(0,9) g;",
    )
    .unwrap();
    e2e_build("ev_r", "period");
    e2e_set_pruning(true);

    let plan = e2e_explain_on("ev_r", "period && int8range(250, 260)");
    assert!(plan.contains("ev_r_3"), "r3 overlaps, must remain:\n{plan}");
    assert!(!plan.contains("ev_r_1"), "r1 pruned:\n{plan}");
    assert!(!plan.contains("ev_r_2"), "r2 pruned:\n{plan}");

    let count = |pred: &str| {
        Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM ev_r WHERE {pred}"))
            .unwrap()
            .unwrap()
    };
    for pred in [
        "period && int8range(250, 260)",
        "period && int8range(95, 105)",    // spans r1/r2 boundary
        "period && int8range(1000, 2000)", // matches nothing
        "period && int8range(0, 300)",     // matches everything
    ] {
        e2e_set_pruning(false);
        let off = count(pred);
        e2e_set_pruning(true);
        let on = count(pred);
        assert_eq!(on, off, "range overlap pruning changed results for `{pred}`");
    }
}

#[pg_test]
fn e2e_or_pruning() {
    e2e_setup_events();
    e2e_set_pruning(true);

    // OR over the same column: only r1 (0..99) and r3 (200..299) can match; r2 pruned.
    let plan = e2e_explain("SELECT * FROM events WHERE val < 50 OR val >= 250");
    assert!(plan.contains("events_r1"), "r1 kept:\n{plan}");
    assert!(!plan.contains("events_r2"), "r2 pruned by both OR branches:\n{plan}");
    assert!(plan.contains("events_r3"), "r3 kept:\n{plan}");

    // An OR where one branch can match every partition keeps all of them.
    let plan_wide = e2e_explain("SELECT * FROM events WHERE val >= 250 OR val >= 0");
    assert!(plan_wide.contains("events_r1"));
    assert!(plan_wide.contains("events_r2"));
    assert!(plan_wide.contains("events_r3"));

    for pred in [
        "val < 50 OR val >= 250",
        "val = 5 OR val = 295",
        "(val >= 100 AND val < 110) OR val = 5",
        "val > 1000 OR val < -10",
    ] {
        e2e_set_pruning(false);
        let off = e2e_count_where(pred);
        e2e_set_pruning(true);
        let on = e2e_count_where(pred);
        assert_eq!(on, off, "OR pruning changed results for `{pred}`: on={on} off={off}");
    }
}

#[pg_test]
fn e2e_in_list_pruning() {
    e2e_setup_events();
    e2e_set_pruning(true);

    // Values only in r1 and r3 -> r2 pruned.
    let plan = e2e_explain("SELECT * FROM events WHERE val IN (5, 250)");
    assert!(plan.contains("events_r1"), "r1 kept:\n{plan}");
    assert!(!plan.contains("events_r2"), "r2 pruned:\n{plan}");
    assert!(plan.contains("events_r3"), "r3 kept:\n{plan}");

    // Values only in r1 -> r2 and r3 pruned.
    let plan2 = e2e_explain("SELECT * FROM events WHERE val IN (5, 25, 75)");
    assert!(plan2.contains("events_r1"));
    assert!(!plan2.contains("events_r2"), "r2 pruned:\n{plan2}");
    assert!(!plan2.contains("events_r3"), "r3 pruned:\n{plan2}");

    for pred in [
        "val IN (5, 250)",
        "val IN (5, 25, 75)",
        "val IN (150)",
        "val IN (1000, 2000)",
        "val IN (5, NULL, 295)",
    ] {
        e2e_set_pruning(false);
        let off = e2e_count_where(pred);
        e2e_set_pruning(true);
        let on = e2e_count_where(pred);
        assert_eq!(on, off, "IN mismatch for `{pred}`: on={on} off={off}");
    }
}

#[pg_test]
fn e2e_timestamptz_pruning() {
    Spi::run(
        "DROP TABLE IF EXISTS ev_ts CASCADE;
         CREATE TABLE ev_ts (region int, ts timestamptz) PARTITION BY LIST (region);
         CREATE TABLE ev_ts_1 PARTITION OF ev_ts FOR VALUES IN (1);
         CREATE TABLE ev_ts_2 PARTITION OF ev_ts FOR VALUES IN (2);
         CREATE TABLE ev_ts_3 PARTITION OF ev_ts FOR VALUES IN (3);
         INSERT INTO ev_ts SELECT 1, timestamptz '2024-01-01' + (g||' days')::interval FROM generate_series(0,27) g;
         INSERT INTO ev_ts SELECT 2, timestamptz '2024-02-01' + (g||' days')::interval FROM generate_series(0,27) g;
         INSERT INTO ev_ts SELECT 3, timestamptz '2024-03-01' + (g||' days')::interval FROM generate_series(0,27) g;",
    )
    .unwrap();
    e2e_build("ev_ts", "ts");
    e2e_set_pruning(true);
    let plan = e2e_explain_on("ev_ts", "ts >= timestamptz '2024-03-01'");
    assert!(plan.contains("ev_ts_3"), "march must remain:\n{plan}");
    assert!(!plan.contains("ev_ts_1"), "jan pruned:\n{plan}");
    assert!(!plan.contains("ev_ts_2"), "feb pruned:\n{plan}");

    let pred = "ts >= timestamptz '2024-02-15' AND ts < timestamptz '2024-03-10'";
    e2e_set_pruning(false);
    let off = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM ev_ts WHERE {pred}"))
        .unwrap()
        .unwrap();
    e2e_set_pruning(true);
    let on = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM ev_ts WHERE {pred}"))
        .unwrap()
        .unwrap();
    assert_eq!(on, off);
}

#[pg_test]
fn e2e_text_pruning() {
    Spi::run(
        "DROP TABLE IF EXISTS ev_txt CASCADE;
         CREATE TABLE ev_txt (region int, name text) PARTITION BY LIST (region);
         CREATE TABLE ev_txt_1 PARTITION OF ev_txt FOR VALUES IN (1);
         CREATE TABLE ev_txt_2 PARTITION OF ev_txt FOR VALUES IN (2);
         CREATE TABLE ev_txt_3 PARTITION OF ev_txt FOR VALUES IN (3);
         INSERT INTO ev_txt VALUES (1,'apple'),(1,'banana'),(1,'cherry');
         INSERT INTO ev_txt VALUES (2,'mango'),(2,'nectarine'),(2,'orange');
         INSERT INTO ev_txt VALUES (3,'watermelon'),(3,'xigua'),(3,'zucchini');",
    )
    .unwrap();
    e2e_build("ev_txt", "name");
    e2e_set_pruning(true);
    let plan = e2e_explain_on("ev_txt", "name >= 'watermelon'");
    assert!(plan.contains("ev_txt_3"), "third must remain:\n{plan}");
    assert!(!plan.contains("ev_txt_1"), "first pruned:\n{plan}");
    assert!(!plan.contains("ev_txt_2"), "second pruned:\n{plan}");

    e2e_set_pruning(false);
    let off = Spi::get_one::<i64>("SELECT count(*)::bigint FROM ev_txt WHERE name = 'mango'")
        .unwrap()
        .unwrap();
    e2e_set_pruning(true);
    let on = Spi::get_one::<i64>("SELECT count(*)::bigint FROM ev_txt WHERE name = 'mango'")
        .unwrap()
        .unwrap();
    assert_eq!(on, off);
    assert_eq!(on, 1);
}

#[pg_test]
fn e2e_float_pruning() {
    Spi::run(
        "DROP TABLE IF EXISTS ev_f CASCADE;
         CREATE TABLE ev_f (region int, amt float8) PARTITION BY LIST (region);
         CREATE TABLE ev_f_1 PARTITION OF ev_f FOR VALUES IN (1);
         CREATE TABLE ev_f_2 PARTITION OF ev_f FOR VALUES IN (2);
         INSERT INTO ev_f SELECT 1, g * 1.5 FROM generate_series(0,49) g;
         INSERT INTO ev_f SELECT 2, 100.0 + g * 1.5 FROM generate_series(0,49) g;",
    )
    .unwrap();
    e2e_build("ev_f", "amt");
    e2e_set_pruning(true);
    let plan = e2e_explain_on("ev_f", "amt > 120.0");
    assert!(plan.contains("ev_f_2"));
    assert!(!plan.contains("ev_f_1"), "low partition pruned:\n{plan}");
}

#[pg_test]
fn e2e_multicolumn_and_semantics() {
    Spi::run(
        "DROP TABLE IF EXISTS mc CASCADE;
         CREATE TABLE mc (region int, a bigint, b bigint) PARTITION BY LIST (region);
         CREATE TABLE mc_1 PARTITION OF mc FOR VALUES IN (1);
         CREATE TABLE mc_2 PARTITION OF mc FOR VALUES IN (2);
         CREATE TABLE mc_3 PARTITION OF mc FOR VALUES IN (3);
         INSERT INTO mc SELECT 1, g, g FROM generate_series(0,99) g;
         INSERT INTO mc SELECT 2, 100+g, 100+g FROM generate_series(0,99) g;
         INSERT INTO mc SELECT 3, 200+g, 200+g FROM generate_series(0,99) g;",
    )
    .unwrap();
    e2e_build("mc", "a, b");
    e2e_set_pruning(true);
    // a >= 250 keeps only p3; b < 50 alone keeps only p1; together -> empty.
    let plan = e2e_explain_on("mc", "a >= 250 AND b < 50");
    assert!(!plan.contains("mc_1"), "p1 pruned by a:\n{plan}");
    assert!(!plan.contains("mc_2"), "p2 pruned by both:\n{plan}");
    assert!(!plan.contains("mc_3"), "p3 pruned by b:\n{plan}");

    for pred in [
        "a >= 250 AND b < 50",
        "a < 150 AND b > 50",
        "a = 100 AND b = 100",
        "a > 250 OR b > 250",
    ] {
        e2e_set_pruning(false);
        let off = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM mc WHERE {pred}"))
            .unwrap()
            .unwrap();
        e2e_set_pruning(true);
        let on = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM mc WHERE {pred}"))
            .unwrap()
            .unwrap();
        assert_eq!(on, off, "mismatch for `{pred}`");
    }
}

#[pg_test]
fn e2e_is_null_pruning() {
    Spi::run(
        "DROP TABLE IF EXISTS nt CASCADE;
         CREATE TABLE nt (region int, val bigint) PARTITION BY LIST (region);
         CREATE TABLE nt_nonull PARTITION OF nt FOR VALUES IN (1);
         CREATE TABLE nt_allnull PARTITION OF nt FOR VALUES IN (2);
         CREATE TABLE nt_mixed PARTITION OF nt FOR VALUES IN (3);
         INSERT INTO nt SELECT 1, g FROM generate_series(1,50) g;
         INSERT INTO nt SELECT 2, NULL FROM generate_series(1,50) g;
         INSERT INTO nt SELECT 3, CASE WHEN g % 2 = 0 THEN g ELSE NULL END FROM generate_series(1,50) g;",
    )
    .unwrap();
    e2e_build("nt", "val");
    e2e_set_pruning(true);

    // IS NULL: the no-null partition can be pruned; all-null and mixed remain.
    let plan_null = e2e_explain_on("nt", "val IS NULL");
    assert!(!plan_null.contains("nt_nonull"), "no-null pruned:\n{plan_null}");
    assert!(plan_null.contains("nt_allnull"), "all-null kept:\n{plan_null}");
    assert!(plan_null.contains("nt_mixed"), "mixed kept:\n{plan_null}");

    // IS NOT NULL: the all-null partition can be pruned.
    let plan_nn = e2e_explain_on("nt", "val IS NOT NULL");
    assert!(!plan_nn.contains("nt_allnull"), "all-null pruned:\n{plan_nn}");
    assert!(plan_nn.contains("nt_nonull"));
    assert!(plan_nn.contains("nt_mixed"));

    for pred in ["val IS NULL", "val IS NOT NULL"] {
        e2e_set_pruning(false);
        let off = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM nt WHERE {pred}"))
            .unwrap()
            .unwrap();
        e2e_set_pruning(true);
        let on = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM nt WHERE {pred}"))
            .unwrap()
            .unwrap();
        assert_eq!(on, off, "null mismatch for `{pred}`");
    }
}

#[pg_test(error = "division by zero")]
fn e2e_planning_error_is_reported_as_error() {
    // A planning-time ERROR passes through the planner hooks and must not abort the backend.
    Spi::run("SELECT 1/0").unwrap();
}

#[pg_test]
fn e2e_cached_plan_replanned_after_widening() {
    // PL/pgSQL static SQL uses a cached generic plan, like PREPARE.
    e2e_two_partitions("cp", "val bigint");
    Spi::run(
        "INSERT INTO cp VALUES (1, 1), (2, 100);
         CREATE INDEX cp_tr ON cp USING table_range (val);
         CREATE FUNCTION cp_count() RETURNS bigint LANGUAGE plpgsql AS
           $$ BEGIN RETURN (SELECT count(*) FROM cp WHERE val = 5000); END $$;",
    )
    .unwrap();
    e2e_set_pruning(true);
    for _ in 0..6 {
        assert_eq!(Spi::get_one::<i64>("SELECT cp_count()").unwrap(), Some(0));
    }
    Spi::run("INSERT INTO cp VALUES (2, 5000)").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT cp_count()").unwrap(),
        Some(1),
        "cached plan must be replanned after the summary widens"
    );
}

#[pg_test]
fn e2e_each_widening_visible_later_in_transaction() {

    e2e_two_partitions("sx", "val bigint");
    Spi::run(
        "INSERT INTO sx VALUES (1, 1), (2, 100);
         CREATE INDEX sx_tr ON sx USING table_range (val);",
    )
    .unwrap();
    e2e_set_pruning(true);
    Spi::run("INSERT INTO sx VALUES (1, 7000)").unwrap();
    assert_eq!(e2e_count_in("sx", "val = 7000"), 1);
    Spi::run("INSERT INTO sx VALUES (1, 8000)").unwrap();
    assert_eq!(e2e_count_in("sx", "val = 8000"), 1);
}

#[pg_test]
fn e2e_cross_type_comparisons() {
    // `f > 100.1::float4` means f > 100.09999847..., not f > 100.1.
    e2e_two_partitions("xt", "f float8, b bigint");
    Spi::run(
        "INSERT INTO xt VALUES (1, 1.0, 1), (1, 100.09999999, 2), (2, 500, 100);
         CREATE INDEX xt_tr ON xt USING table_range (f, b);",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("xt", "f > 100.1::float4"), 2);
    assert_eq!(e2e_same_on_off("xt", "f >= 100.1::float4 AND f < 101"), 1);
    let plan = e2e_explain_on("xt", "b = 100::int4");
    assert!(!plan.contains("xt_1"), "int8 = int4 must prune:\n{plan}");
    let plan = e2e_explain_on("xt", "f > 400::float4");
    assert!(!plan.contains("xt_1"), "float8 > float4 must prune:\n{plan}");
}

#[pg_test]
fn e2e_other_collation_is_not_pruned() {

    let Some(coll) = Spi::get_one::<String>(
        "SELECT collname::text FROM pg_collation
         WHERE collname IN ('und-x-icu', 'en_US.utf8', 'en_US.UTF-8', 'en_US')
           AND collencoding IN (-1, pg_char_to_encoding(current_setting('server_encoding')))
         ORDER BY collname = 'und-x-icu' DESC LIMIT 1",
    )
    .unwrap() else {
        return; // no linguistic collation in this build
    };
    e2e_two_partitions("co", &format!("s text COLLATE \"{coll}\""));
    Spi::run(
        "INSERT INTO co VALUES (1, 'apple'), (1, 'Zebra'), (2, 'mango');
         CREATE INDEX co_tr ON co USING table_range (s);",
    )
    .unwrap();
    // Linguistically 'apple' < 'Zebra'; bytewise 'Zebra' < 'apple'.
    assert_eq!(e2e_same_on_off("co", "s > 'Zebra' COLLATE \"C\""), 2);
    let plan = e2e_explain_on("co", "s > 'Zebra' COLLATE \"C\"");
    assert!(plan.contains("co_1"), "another collation must not prune:\n{plan}");
    // Under the summary's own collation, 'mango' > 'b'.
    let plan = e2e_explain_on("co", "s < 'b'");
    assert!(!plan.contains("co_2"), "the summary's collation must prune:\n{plan}");

    // Inserts widen under the summary's collation too: linguistically 'B' sorts after 'b',
    // bytewise before it.
    e2e_two_partitions("cw", &format!("s text COLLATE \"{coll}\""));
    Spi::run(
        "INSERT INTO cw VALUES (1, 'A'), (1, 'b'), (2, 'x');
         CREATE INDEX cw_tr ON cw USING table_range (s);
         INSERT INTO cw VALUES (1, 'A'), (1, 'B');",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("cw", "s = 'B'"), 1);
}

#[pg_test]
fn e2e_bounds_independent_of_session_io_settings() {
    // Bounds are stored as text, which these session settings would otherwise change.
    e2e_two_partitions("ss", "d date, f float8");
    Spi::run(
        "INSERT INTO ss VALUES (1, '2024-03-05', 1), (2, '2025-01-01', 500);
         CREATE INDEX ss_tr ON ss USING table_range (d, f);
         SET DateStyle = 'SQL, DMY';
         INSERT INTO ss VALUES (1, '01/12/2024', NULL);
         SET DateStyle = 'ISO, MDY';
         SET extra_float_digits = -10;
         INSERT INTO ss VALUES (1, NULL, 123444.4);
         RESET extra_float_digits;",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("ss", "d >= '2024-11-01'"), 2);
    assert_eq!(e2e_same_on_off("ss", "f > 123441 AND f < 200000"), 1);
    Spi::run("RESET DateStyle").unwrap();
}

#[pg_test]
fn e2e_types_without_min_max_aggregates_prune() {
    // uuid/bool lack min()/max(); varchar/enum compare via text/anyenum procs.
    Spi::run(
        "DROP TYPE IF EXISTS e2e_mood CASCADE;
         CREATE TYPE e2e_mood AS ENUM ('sad', 'ok', 'happy');",
    )
    .unwrap();
    e2e_two_partitions("ty", "u uuid, fl bool, m e2e_mood, v varchar");
    Spi::run(
        "INSERT INTO ty VALUES
           (1, '00000000-0000-0000-0000-000000000001', false, 'sad', 'aa'),
           (2, 'ffffffff-0000-0000-0000-000000000001', true, 'happy', 'zz');
         CREATE INDEX ty_tr ON ty USING table_range (u, fl, m, v);",
    )
    .unwrap();
    e2e_set_pruning(true);
    for pred in [
        "u = 'ffffffff-0000-0000-0000-000000000001'",
        "m = 'happy'",
        "v = 'zz'",
    ] {
        let plan = e2e_explain_on("ty", pred);
        assert!(!plan.contains("ty_1"), "`{pred}` must prune ty_1:\n{plan}");
        assert_eq!(e2e_same_on_off("ty", pred), 1);
    }
    // Widening on insert works for these types too.
    Spi::run("INSERT INTO ty VALUES (1, NULL, NULL, 'happy', 'zzz')").unwrap();
    assert_eq!(e2e_same_on_off("ty", "v = 'zzz'"), 1);
    assert_eq!(e2e_same_on_off("ty", "m = 'happy'"), 2);
}
