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

/// The summary of a leaf's table_range index, rendered by `table_range_test_read_summary`.
fn am_leaf_summary(leaf: &str) -> String {
    Spi::get_one::<String>(&format!(
        "SELECT table_range_test_read_summary(i.indexrelid)
         FROM pg_index i WHERE i.indrelid = '{leaf}'::regclass"
    ))
    .unwrap()
    .unwrap()
}

#[pg_test]
fn am_bulk_insert_widens_summary() {
    am_setup();
    Spi::run(
        "CREATE INDEX amt_tr ON amt USING table_range (val);
         INSERT INTO amt SELECT 1, g FROM generate_series(1000, 1099) g;",
    )
    .unwrap();
    let summary = am_leaf_summary("amt_1");
    assert!(
        summary.contains("max=Some(\"1099\")") && summary.contains("unbounded=false"),
        "{summary}"
    );
    assert_eq!(e2e_same_on_off("amt", "val BETWEEN 1000 AND 1099"), 100);
    // EXPLAIN aliases partitions (`amt_3 amt_2`), so match the scanned table name.
    let plan = am_explain("amt", "val > 1500");
    assert!(!plan.contains("on amt_1"), "a widened leaf still prunes:\n{plan}");
}

/// The table_range index on `leaf`.
fn am_leaf_index(leaf: &str) -> String {
    format!("(SELECT indexrelid FROM pg_index WHERE indrelid = '{leaf}'::regclass)")
}

#[pg_test]
fn am_summarize_narrows_to_the_data() {
    am_setup();
    Spi::run("CREATE INDEX amt_tr ON amt USING table_range (val)").unwrap();
    // amt_1 holds val 0..99; claim [-1000, 1000] so narrowing is observable.
    Spi::run(&format!(
        "SELECT table_range_test_set_bounds({}, 2::int2, '-1000', '1000')",
        am_leaf_index("amt_1")
    ))
    .unwrap();
    assert!(am_explain("amt", "val = -500").contains("on amt_1"));
    let rebuilt = Spi::get_one::<i32>("SELECT table_range_summarize('amt_tr'::regclass)")
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt, 3, "every leaf is rebuilt");
    let summary = am_leaf_summary("amt_1");
    assert!(
        summary.contains("summarizing=false")
            && summary.contains("min=Some(\"0\")")
            && summary.contains("max=Some(\"99\")"),
        "{summary}"
    );
    let plan = am_explain("amt", "val = -500");
    assert!(!plan.contains("on amt_1"), "the rebuilt summary prunes again:\n{plan}");
    assert_eq!(e2e_same_on_off("amt", "val BETWEEN 0 AND 99"), 100);
}

#[pg_test]
fn am_summarize_keeps_column_unbounded_while_value_exists() {
    e2e_two_partitions("ub", "s text, n bigint");
    Spi::run(
        "INSERT INTO ub VALUES (1, 'm', 1), (2, 'a', 2);
         CREATE INDEX ub_tr ON ub USING table_range (s, n);
         INSERT INTO ub VALUES (1, 'z' || repeat('x', 9000), 3);",
    )
    .unwrap();
    Spi::run(&format!(
        "SELECT table_range_test_set_bounds({}, 3::int2, '-1000', '1000')",
        am_leaf_index("ub_1")
    ))
    .unwrap();
    Spi::run("SELECT table_range_summarize('ub_tr'::regclass)").unwrap();
    let summary = am_leaf_summary("ub_1");
    assert!(
        summary.contains("attnum=2 overlap=false min=None max=None")
            && summary.contains("unbounded=true"),
        "the oversized value still exists: {summary}"
    );
    assert!(
        summary.contains("attnum=3 overlap=false min=Some(\"1\") max=Some(\"3\")"),
        "the other column narrows: {summary}"
    );
    assert_eq!(e2e_same_on_off("ub", "s > 'y'"), 1);
}

#[pg_test]
fn am_summary_being_rebuilt_never_prunes() {
    // Hand-written summaries claiming val in [0, 0]: an ordinary one prunes `val = 5`
    // (the page lies, so nothing is found), one flagged mid-rebuild must not.
    Spi::run(
        "DROP TABLE IF EXISTS sb CASCADE;
         CREATE TABLE sb (val bigint);
         INSERT INTO sb VALUES (5);
         CREATE INDEX sb_tr ON sb USING table_range (val);",
    )
    .unwrap();
    let write = |hex: &str| {
        Spi::run(&format!(
            "SELECT table_range_test_write_blob('sb_tr'::regclass::oid, '\\x{hex}')"
        ))
        .unwrap()
    };
    // ncols 1 | attnum 1 | minmax, no flags | collation 0 | "int8" | min "0" | max "0"
    let body = "0100" .to_owned() + "0100" + "0000" + "00000000" + "0400696e7438" + "0101003001010030";
    write(&format!("02{body}"));
    e2e_set_pruning(true);
    assert_eq!(e2e_count_in("sb", "val = 5"), 0, "the plain summary is used");
    write(&format!("0301{body}"));
    assert_eq!(e2e_same_on_off("sb", "val = 5"), 1);
}

#[pg_test(error = "a column can appear only once in a table_range index")]
fn am_column_indexed_twice_rejected() {
    Spi::run(
        "DROP TABLE IF EXISTS dc CASCADE;
         CREATE TABLE dc (t text);
         CREATE INDEX dc_tr ON dc USING table_range (t COLLATE \"C\", t COLLATE \"POSIX\");",
    )
    .unwrap();
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
    let summary = am_leaf_summary("ov_1");
    assert!(summary.contains("unbounded=true"), "{summary}");
}

#[pg_test]
fn am_toasted_values_keep_exact_bounds() {
    // Out-of-line values (~3 KB, uncompressed) are detoasted when kept as bounds.
    e2e_two_partitions("lv", "s text");
    Spi::run(
        "ALTER TABLE lv ALTER COLUMN s SET STORAGE EXTERNAL;
         INSERT INTO lv VALUES (1, 'm'), (2, 'm');
         CREATE INDEX lv_tr ON lv USING table_range (s);
         INSERT INTO lv SELECT 1, repeat(md5(g::text), 94) FROM generate_series(1, 50) g;",
    )
    .unwrap();
    let (lo, hi) = Spi::get_two::<String, String>("SELECT min(s), max(s) FROM lv_1").unwrap();
    let summary = am_leaf_summary("lv_1");
    assert!(
        summary.contains(&format!("min=Some({:?})", lo.unwrap()))
            && summary.contains(&format!("max=Some({:?})", hi.unwrap())),
        "bounds must equal the data's min and max"
    );
    e2e_same_on_off("lv", "s = repeat(md5('7'), 94)");
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

#[pg_test]
fn am_one_statement_keeps_exact_bounds() {
    // Rows widen in both directions, interleaved with covered rows and NULLs, so the
    // per-statement decoded bounds are both read and updated many times. `z` is all NULL
    // when the index is built; `d` is a domain.
    Spi::run("DROP DOMAIN IF EXISTS os_amount CASCADE; CREATE DOMAIN os_amount AS numeric;")
        .unwrap();
    e2e_two_partitions("os", "n bigint, s text, r int4range, z int, d os_amount");
    Spi::run(
        "INSERT INTO os VALUES (1, 0, 'm', '[0,1)', NULL, 0), (2, 0, 'm', '[0,1)', NULL, 0);
         CREATE INDEX os_tr ON os USING table_range (n, s, r, z, d);
         INSERT INTO os
         SELECT 1,
                CASE WHEN g % 7 = 0 THEN NULL ELSE (g * 37 % 1001) - 500 END,
                CASE WHEN g % 5 = 0 THEN NULL ELSE chr(97 + g % 26) || g END,
                int4range(g % 300 - 150, g % 300 - 140),
                CASE WHEN g % 3 = 0 THEN g % 97 END,
                (g * 13 % 501) / 7.0 - 30
         FROM generate_series(1, 2000) g;",
    )
    .unwrap();
    let expected = Spi::get_one::<String>(
        "SELECT format('min=Some(\"%s\") max=Some(\"%s\") has_nulls=true',
                       min(n), max(n))
                || ' | ' ||
                format('min=Some(\"%s\") max=Some(\"%s\") has_nulls=true',
                       min(s COLLATE \"default\"), max(s COLLATE \"default\"))
                || ' | ' ||
                format('min=Some(\"%s\")', range_merge(range_agg(r)))
                || ' | ' ||
                format('min=Some(\"%s\") max=Some(\"%s\") has_nulls=true all_nulls=false',
                       min(z), max(z))
                || ' | ' ||
                format('min=Some(\"%s\") max=Some(\"%s\")', min(d), max(d))
         FROM os_1",
    )
    .unwrap()
    .unwrap();
    let summary = am_leaf_summary("os_1");
    for want in expected.split(" | ") {
        assert!(summary.contains(want), "summary {summary}\nmissing {want}");
    }
    for pred in ["n = -500", "n = 500", "s = 'z25'", "r && '[149,150)'::int4range"] {
        e2e_same_on_off("os", pred);
    }
}
