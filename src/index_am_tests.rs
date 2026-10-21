// Tests for the `CREATE INDEX ... USING table_range` custom access method.
// Included into `#[pg_schema] mod tests` in lib.rs.

fn am_explain(table: &str, pred: &str) -> String {
    Spi::connect(|client| {
        let q = format!("EXPLAIN (COSTS OFF) SELECT * FROM {table} WHERE {pred}");
        let t = client.select(&q, None, &[])?;
        let mut out = String::new();
        for row in t {
            if let Ok(Some(line)) = row.get::<String>(1) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        Ok::<String, pgrx::spi::SpiError>(out)
    })
    .expect("explain")
}

fn am_setup() {
    Spi::run(
        "DROP TABLE IF EXISTS amt CASCADE;
         CREATE TABLE amt (region int, val bigint, created date) PARTITION BY LIST (region);
         CREATE TABLE amt_1 PARTITION OF amt FOR VALUES IN (1);
         CREATE TABLE amt_2 PARTITION OF amt FOR VALUES IN (2);
         CREATE TABLE amt_3 PARTITION OF amt FOR VALUES IN (3);
         INSERT INTO amt SELECT 1, g, date '2024-01-01' + g FROM generate_series(0,99) g;
         INSERT INTO amt SELECT 2, g, date '2024-06-01' + (g-100) FROM generate_series(100,199) g;
         INSERT INTO amt SELECT 3, g, date '2024-11-01' + (g-200) FROM generate_series(200,299) g;",
    )
    .expect("setup amt");
    Spi::run("SET table_range.enable_pruning = on").unwrap();
}

#[pg_test]
fn am_create_index_builds_and_prunes() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").expect("create index");

    // Each leaf index holds its summary on its own metapage. Check one leaf's page.
    let leaf_summary = Spi::get_one::<String>(
        "SELECT table_range_test_read_summary(i.inhrelid)
         FROM pg_inherits i WHERE i.inhparent = 'amt_tr'::regclass LIMIT 1",
    )
    .unwrap()
    .unwrap();
    assert!(
        leaf_summary.contains("min=Some") && leaf_summary.contains("max=Some"),
        "leaf index must carry a summary: {leaf_summary}"
    );

    let plan = am_explain("amt", "val >= 250");
    assert!(plan.contains("amt_3"), "r3 kept:\n{plan}");
    assert!(!plan.contains("amt_1"), "r1 pruned:\n{plan}");
    assert!(!plan.contains("amt_2"), "r2 pruned:\n{plan}");
}

#[pg_test]
fn am_index_results_identical_on_off() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    for pred in ["val >= 250", "val < 50", "val = 150", "val IN (5, 250)", "val > 1000"] {
        Spi::run("SET table_range.enable_pruning = off").unwrap();
        let off = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM amt WHERE {pred}"))
            .unwrap()
            .unwrap();
        Spi::run("SET table_range.enable_pruning = on").unwrap();
        let on = Spi::get_one::<i64>(&format!("SELECT count(*)::bigint FROM amt WHERE {pred}"))
            .unwrap()
            .unwrap();
        assert_eq!(on, off, "AM pruning changed results for `{pred}`");
    }
}

#[pg_test]
fn am_multicolumn_index_prunes() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val, created)").unwrap();
    // Prune on the second indexed column (a date), which is not the partition key.
    let plan = am_explain("amt", "created >= date '2024-11-01'");
    assert!(plan.contains("amt_3"), "r3 kept:\n{plan}");
    assert!(!plan.contains("amt_1"), "r1 pruned:\n{plan}");
    assert!(!plan.contains("amt_2"), "r2 pruned:\n{plan}");
}

#[pg_test]
fn am_insert_then_correct_without_rebuild() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    // Insert a value outside r1's built range; aminsert widens the summary to cover it.
    Spi::run("INSERT INTO amt VALUES (1, 500, date '2025-01-01')").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val = 500")
            .unwrap()
            .unwrap(),
        1,
        "widened summary must cover the newly inserted row"
    );
}

#[pg_test]
fn am_bulk_insert_widens_summary() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    Spi::run("SET table_range.enable_pruning = on").unwrap();

    // Bulk insert many out-of-range rows into r1 in one statement. Each row widens the
    // summary as needed, so all of them are found.
    Spi::run("INSERT INTO amt SELECT 1, g FROM generate_series(1000, 1099) g").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val BETWEEN 1000 AND 1099")
            .unwrap()
            .unwrap(),
        100,
        "widened summary must cover bulk-inserted rows"
    );
    // A predicate matching r3's data still works (no over-pruning).
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val = 250")
            .unwrap()
            .unwrap(),
        1
    );
}

#[pg_test]
fn am_drop_index_stops_pruning_and_stays_correct() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    Spi::run("SET table_range.enable_pruning = on").unwrap();

    // With the summary owned by the index, DROP INDEX removes it (the index's storage is
    // gone) — no side table to clean up. Pruning simply stops; results stay correct.
    Spi::run("DROP INDEX amt_tr").unwrap();
    let plan = am_explain("amt", "val >= 250");
    assert!(plan.contains("amt_1"), "no index -> no pruning:\n{plan}");

    Spi::run("INSERT INTO amt VALUES (1, 5000)").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val = 5000")
            .unwrap()
            .unwrap(),
        1
    );
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val >= 250")
            .unwrap()
            .unwrap(),
        51
    );
}

#[pg_test]
fn storage_page_roundtrip() {
    Spi::run(
        "DROP TABLE IF EXISTS pr CASCADE; CREATE TABLE pr (val bigint);
         INSERT INTO pr VALUES (1);
         CREATE INDEX pr_tr ON pr USING table_range (val);",
    )
    .unwrap();
    let out = Spi::get_one::<String>(
        "SELECT table_range_test_page_roundtrip('pr_tr'::regclass::oid, 'hello-page-42')",
    )
    .unwrap()
    .unwrap();
    assert_eq!(out, "hello-page-42", "blob must round-trip through the index metapage");
}

#[pg_test]
fn ambuild_writes_summary_to_page() {
    Spi::run(
        "DROP TABLE IF EXISTS ap CASCADE;
         CREATE TABLE ap (val bigint) PARTITION BY RANGE (val);
         CREATE TABLE ap_1 PARTITION OF ap FOR VALUES FROM (0) TO (100);
         INSERT INTO ap SELECT g FROM generate_series(0, 99) g;
         CREATE INDEX ap_tr ON ap USING table_range (val);",
    )
    .unwrap();
    // The per-partition summary lives on the leaf child index (on ap_1), not the
    // partitioned parent. Find that child index and read its metapage summary.
    let out = Spi::get_one::<String>(
        "SELECT table_range_test_read_summary(i.inhrelid)
         FROM pg_inherits i WHERE i.inhparent = 'ap_tr'::regclass",
    )
    .unwrap()
    .unwrap();
    assert!(
        out.contains("min=Some(\"0\")") && out.contains("max=Some(\"99\")"),
        "ambuild must persist the summary to the index page: {out}"
    );
}

#[pg_test]
fn am_reindex_rebuilds_summary() {
    // REINDEX rebuilds the summary from the table's data.
    Spi::run(
        "DROP TABLE IF EXISTS rx CASCADE;
         CREATE TABLE rx (val bigint);
         INSERT INTO rx SELECT g FROM generate_series(1, 100) g;
         CREATE INDEX rx_tr ON rx USING table_range (val);
         INSERT INTO rx VALUES (500);
         REINDEX INDEX rx_tr;",
    )
    .unwrap();
    let summary =
        Spi::get_one::<String>("SELECT table_range_test_read_summary('rx_tr'::regclass::oid)")
            .unwrap()
            .unwrap();
    assert!(
        summary.contains("min=Some(\"1\")") && summary.contains("max=Some(\"500\")"),
        "REINDEX must rebuild the summary from the data: {summary}"
    );
    assert_eq!(e2e_same_on_off("rx", "val = 500"), 1);
    assert_eq!(e2e_same_on_off("rx", "val = 300"), 0);
}

#[pg_test(error = "table_range indexes cannot be partial")]
fn am_partial_index_rejected() {
    Spi::run(
        "DROP TABLE IF EXISTS pi CASCADE;
         CREATE TABLE pi (val bigint, flag bool);
         CREATE INDEX pi_tr ON pi USING table_range (val) WHERE flag;",
    )
    .unwrap();
}

#[pg_test]
fn am_build_reports_heap_row_count() {
    // index_build stores the reported heap_tuples as the table's reltuples.
    Spi::run(
        "DROP TABLE IF EXISTS rt CASCADE;
         CREATE TABLE rt (val bigint);
         INSERT INTO rt SELECT g FROM generate_series(1, 500) g;
         ANALYZE rt;
         CREATE INDEX rt_tr ON rt USING table_range (val);",
    )
    .unwrap();
    let reltuples = Spi::get_one::<f32>("SELECT reltuples FROM pg_class WHERE relname = 'rt'")
        .unwrap()
        .unwrap();
    assert_eq!(reltuples, 500.0);
}

#[pg_test]
fn am_oversized_value_makes_column_unbounded() {
    e2e_two_partitions("ov", "s text");
    Spi::run(
        "INSERT INTO ov VALUES (1, 'm'), (2, 'a');
         CREATE INDEX ov_tr ON ov USING table_range (s);
         INSERT INTO ov VALUES (1, 'z' || repeat('x', 9000));",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("ov", "s > 'y'"), 1);
    let summary = Spi::get_one::<String>(
        "SELECT table_range_test_read_summary(i.indexrelid)
         FROM pg_index i WHERE i.indrelid = 'ov_1'::regclass",
    )
    .unwrap()
    .unwrap();
    assert!(summary.contains("unbounded=true"), "{summary}");
}

#[pg_test]
fn am_range_extent_widens_on_insert() {
    e2e_two_partitions("rg", "r int4range");
    Spi::run(
        "INSERT INTO rg VALUES (1, '[0,10)'), (2, '[100,110)');
         CREATE INDEX rg_tr ON rg USING table_range (r);
         INSERT INTO rg VALUES (1, '[500,501)'), (1, 'empty');",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("rg", "r && '[500,500]'::int4range"), 1);
    // After the insert rg_1's extent is [0,501): past it, both partitions prune.
    let plan = e2e_explain_on("rg", "r && '[600,601)'::int4range");
    assert!(!plan.contains("rg_1") && !plan.contains("rg_2"), "{plan}");
}

#[pg_test]
fn am_unknown_summary_format_is_ignored() {
    // A well-formed summary in another format version claiming val in [0, 0].
    Spi::run(
        "DROP TABLE IF EXISTS fmt CASCADE;
         CREATE TABLE fmt (val bigint);
         INSERT INTO fmt VALUES (5);
         CREATE INDEX fmt_tr ON fmt USING table_range (val);",
    )
    .unwrap();
    // version 1 | ncols=1 | attnum=1 | overlap=0 | flags=0 | "int8" | min "0" | max "0"
    Spi::run(
        "SELECT table_range_test_write_blob('fmt_tr'::regclass::oid,
           '\\x010100010000000400696e74380101003001010030')",
    )
    .unwrap();
    assert_eq!(e2e_same_on_off("fmt", "val = 5"), 1);
}
