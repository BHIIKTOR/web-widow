//! /: the bundled SQLite must compile FTS5 in.
//!
//! This is the verification gate for v0.5
//! premise holds only if the already-bundled engine exposes FTS5. The test also
//! pins the two SQL shapes the store depends on (rowid upsert, `bm25`, `snippet`)
//! so a bundled-engine change cannot silently remove them.

use rusqlite::Connection;

fn compile_options(connection: &Connection) -> Vec<String> {
    connection
        .prepare("PRAGMA compile_options")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[test]
fn bundled_sqlite_compiles_fts5() {
    let connection = Connection::open_in_memory().unwrap();
    let options = compile_options(&connection);
    // Printed only with `--nocapture`; the copy-drill evidence artifact keeps it.
    eprintln!(
        "sqlite {} compile options: {}",
        connection
            .query_row("SELECT sqlite_version()", [], |row| row.get::<_, String>(0))
            .unwrap(),
        options.join(",")
    );
    assert!(
        options.iter().any(|option| option == "ENABLE_FTS5"),
        "bundled SQLite does not report ENABLE_FTS5; compile options: {options:?}"
    );

    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(
                 title,
                 body,
                 tokenize = 'unicode61 remove_diacritics 2'
             );",
        )
        .unwrap();

    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0);
}

#[test]
fn fts5_supports_the_store_query_shapes() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(title, body);
             INSERT INTO probe(rowid, title, body) VALUES (1, 'sqlite wal mode', 'body text');
             INSERT INTO probe(rowid, title, body) VALUES (2, 'unrelated', 'sqlite wal mode explained');",
        )
        .unwrap();

    // 1. The store upsert shape: an FTS5 table has no unique constraint, so
    //    `INSERT OR REPLACE` must still replace the rowid rather than error.
    connection
        .execute(
            "INSERT OR REPLACE INTO probe(rowid, title, body) VALUES (1, 'sqlite wal mode', 'replaced body')",
            [],
        )
        .unwrap();
    let replaced: String = connection
        .query_row("SELECT body FROM probe WHERE rowid = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(replaced, "replaced body");
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "the upsert replaced, it did not duplicate");

    // 2. Quoted-term AND, column-scoped: both terms in the title of row 1.
    let hits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM probe WHERE probe MATCH 'title : \"wal\" AND title : \"sqlite\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // ... and a body-only term still matches, so the column filter is not
    // swallowing one column.
    let body_hits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM probe WHERE probe MATCH 'body : \"explained\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body_hits, 1);

    // 3. `bm25` is available, weighs the title above the body, and is negative
    //    (smaller = better), so the payload can expose `-bm25`.
    let (title_row, title_score): (i64, f64) = connection
        .query_row(
            "SELECT rowid, bm25(probe, 4.0, 1.0) FROM probe WHERE probe MATCH '\"sqlite\"'
             ORDER BY bm25(probe, 4.0, 1.0) ASC, rowid ASC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(title_row, 1, "the title match ranks first");
    assert!(title_score < 0.0, "bm25 is negative: {title_score}");

    // 4. `snippet` returns plain surrounding text with the marker we pass in.
    let snippet: String = connection
        .query_row(
            "SELECT snippet(probe, 1, '', '', ' … ', 24) FROM probe WHERE probe MATCH '\"explained\"'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(snippet.contains("explained"), "snippet: {snippet:?}");
    assert!(
        !snippet.contains('<'),
        "snippet carries no markup: {snippet:?}"
    );
}

#[test]
fn punctuation_only_terms_are_not_syntax_errors() {
    //: the query builder quotes every term, so a caller cannot
    // produce an FTS5 syntax error. Pin that for the shapes an agent writes.
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(title, body);
             INSERT INTO probe(title, body) VALUES ('alpha OR beta', 'NEAR gamma');",
        )
        .unwrap();
    for expression in [
        "\"OR\"",
        "\"*\"",
        "\"-\"",
        "\"...\"",
        "\"title:sqlite\"",
        "\"\"\"\"\"\"",
        "\"alpha\" AND \"OR\"",
    ] {
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH ?1",
                [expression],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("{expression} must not be a syntax error: {error}"));
        assert!(count <= 1, "{expression} matched unexpectedly often");
    }
}
