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

    // Summaries should exist for the three leaves.
    let n = Spi::get_one::<i64>(
        "SELECT count(DISTINCT relid)::bigint FROM table_range_summary",
    )
    .unwrap()
    .unwrap();
    assert!(n >= 3, "expected >=3 summarized leaves, got {n}");

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
    // Insert a value outside r1's built range; staleness must keep results correct.
    Spi::run("INSERT INTO amt VALUES (1, 500, date '2025-01-01')").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val = 500")
            .unwrap()
            .unwrap(),
        1,
        "stale summary must not prune away the newly inserted row"
    );
}

#[pg_test]
fn am_bulk_insert_stays_correct_under_stale_memo() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    Spi::run("SET table_range.enable_pruning = on").unwrap();

    // Bulk insert many out-of-range rows into r1 in one statement. This exercises the
    // per-transaction stale memo (r1 is marked stale once, not once per row); all rows
    // must still be found despite the now-stale summary.
    Spi::run("INSERT INTO amt SELECT 1, g FROM generate_series(1000, 1099) g").unwrap();
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val BETWEEN 1000 AND 1099")
            .unwrap()
            .unwrap(),
        100,
        "stale summary must not prune away bulk-inserted rows"
    );
    // A predicate matching original r3 data still works (no over-pruning).
    assert_eq!(
        Spi::get_one::<i64>("SELECT count(*)::bigint FROM amt WHERE val = 250")
            .unwrap()
            .unwrap(),
        1
    );
}

#[pg_test]
fn am_drop_index_cleans_summaries_and_stays_correct() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    let before = Spi::get_one::<i64>("SELECT count(*)::bigint FROM table_range_summary")
        .unwrap()
        .unwrap();
    assert!(before >= 3, "summaries built before drop: {before}");

    Spi::run("DROP INDEX amt_tr").unwrap();

    // The sql_drop event trigger must remove the index's summaries, so a later insert
    // (no longer tracked by any index/trigger) cannot cause a stale-prune false negative.
    let after = Spi::get_one::<i64>("SELECT count(*)::bigint FROM table_range_summary")
        .unwrap()
        .unwrap();
    assert_eq!(after, 0, "summaries must be cleaned on DROP INDEX, found {after}");

    Spi::run("SET table_range.enable_pruning = on").unwrap();
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
