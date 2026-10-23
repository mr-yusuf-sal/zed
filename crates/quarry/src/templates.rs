//! Statements to start from, offered in the query tab's Templates menu. Each is written the way
//! the engine's query editor expects it, with placeholder names to replace: `collection`,
//! `table`, `key`.

use crate::driver::Engine;

/// The templates for `engine`, as menu label and text.
pub fn templates(engine: Engine) -> &'static [(&'static str, &'static str)] {
    match engine {
        Engine::MongoDb => MONGODB,
        Engine::Postgres => POSTGRES,
        Engine::SqlServer => SQL_SERVER,
        Engine::ClickHouse => CLICKHOUSE,
        Engine::Redis => REDIS,
    }
}

// MongoDB commands are what `db.runCommand` takes. None of them may contain a blank line,
// since a blank line is where the editor splits one command from the next.
const MONGODB: &[(&str, &str)] = &[
    (
        "find",
        r#"{
  "find": "collection",
  "filter": { "status": "active" },
  "projection": { "_id": 1, "name": 1 },
  "sort": { "_id": -1 },
  "limit": 50
}"#,
    ),
    (
        "aggregate",
        r#"{
  "aggregate": "collection",
  "pipeline": [
    { "$match": { "status": "active" } },
    { "$group": { "_id": "$category", "count": { "$sum": 1 } } },
    { "$sort": { "count": -1 } }
  ],
  "cursor": {}
}"#,
    ),
    (
        "count",
        r#"{ "count": "collection", "query": { "status": "active" } }"#,
    ),
    (
        "distinct",
        r#"{ "distinct": "collection", "key": "category", "query": {} }"#,
    ),
    (
        "insert",
        r#"{
  "insert": "collection",
  "documents": [
    { "name": "example", "createdAt": { "$date": "2026-01-01T00:00:00Z" } }
  ]
}"#,
    ),
    (
        "update",
        r#"{
  "update": "collection",
  "updates": [
    { "q": { "_id": { "$oid": "000000000000000000000000" } }, "u": { "$set": { "status": "done" } } }
  ]
}"#,
    ),
    (
        "delete",
        r#"{
  "delete": "collection",
  "deletes": [
    { "q": { "status": "obsolete" }, "limit": 0 }
  ]
}"#,
    ),
    (
        "createIndexes",
        r#"{
  "createIndexes": "collection",
  "indexes": [
    { "key": { "email": 1 }, "name": "email_1", "unique": true }
  ]
}"#,
    ),
];

const POSTGRES: &[(&str, &str)] = &[
    (
        "SELECT",
        "SELECT *\nFROM table\nWHERE id = 1\nORDER BY id\nLIMIT 100;",
    ),
    (
        "INSERT",
        "INSERT INTO table (name, created_at)\nVALUES ('example', now())\nRETURNING *;",
    ),
    (
        "UPDATE",
        "UPDATE table\nSET name = 'example'\nWHERE id = 1\nRETURNING *;",
    ),
    ("DELETE", "DELETE FROM table\nWHERE id = 1\nRETURNING *;"),
    (
        "CREATE TABLE",
        "CREATE TABLE table (\n    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    name text NOT NULL,\n    created_at timestamptz NOT NULL DEFAULT now()\n);",
    ),
    (
        "Upsert",
        "INSERT INTO table (id, name)\nVALUES (1, 'example')\nON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name;",
    ),
    (
        "Table sizes",
        "SELECT relname AS table, pg_size_pretty(pg_total_relation_size(oid)) AS size\nFROM pg_class\nWHERE relkind = 'r'\nORDER BY pg_total_relation_size(oid) DESC\nLIMIT 20;",
    ),
    (
        "Running queries",
        "SELECT pid, now() - query_start AS running_for, state, query\nFROM pg_stat_activity\nWHERE state <> 'idle' AND pid <> pg_backend_pid()\nORDER BY query_start;",
    ),
];

// Batches are separated by GO, which is where the editor splits T-SQL.
const SQL_SERVER: &[(&str, &str)] = &[
    (
        "SELECT",
        "SELECT TOP (100) *\nFROM dbo.[table]\nWHERE id = 1\nORDER BY id;",
    ),
    (
        "INSERT",
        "INSERT INTO dbo.[table] (name, created_at)\nOUTPUT inserted.*\nVALUES (N'example', SYSUTCDATETIME());",
    ),
    (
        "UPDATE",
        "UPDATE dbo.[table]\nSET name = N'example'\nOUTPUT inserted.*\nWHERE id = 1;",
    ),
    (
        "DELETE",
        "DELETE FROM dbo.[table]\nOUTPUT deleted.*\nWHERE id = 1;",
    ),
    (
        "CREATE TABLE",
        "CREATE TABLE dbo.[table] (\n    id int IDENTITY(1, 1) NOT NULL PRIMARY KEY,\n    name nvarchar(200) NOT NULL,\n    created_at datetime2 NOT NULL DEFAULT SYSUTCDATETIME()\n);\nGO",
    ),
    (
        "Table sizes",
        "SELECT t.name AS [table], SUM(p.rows) AS [rows]\nFROM sys.tables t\nJOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0, 1)\nGROUP BY t.name\nORDER BY [rows] DESC;",
    ),
    (
        "Running queries",
        "SELECT r.session_id, r.status, r.total_elapsed_time, t.text\nFROM sys.dm_exec_requests r\nCROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t\nWHERE r.session_id <> @@SPID;",
    ),
];

const CLICKHOUSE: &[(&str, &str)] = &[
    (
        "SELECT",
        "SELECT *\nFROM database.table\nWHERE id = 1\nORDER BY id\nLIMIT 100",
    ),
    (
        "INSERT",
        "INSERT INTO database.table (id, name)\nVALUES (1, 'example')",
    ),
    (
        "CREATE TABLE",
        "CREATE TABLE database.table\n(\n    id UInt64,\n    name String,\n    created_at DateTime DEFAULT now()\n)\nENGINE = MergeTree\nORDER BY id",
    ),
    (
        "Table sizes",
        "SELECT database, table, formatReadableSize(sum(bytes_on_disk)) AS size, sum(rows) AS rows\nFROM system.parts\nWHERE active\nGROUP BY database, table\nORDER BY sum(bytes_on_disk) DESC\nLIMIT 20",
    ),
    (
        "Running queries",
        "SELECT query_id, elapsed, read_rows, query\nFROM system.processes\nORDER BY elapsed DESC",
    ),
];

// One command per line, as the editor splits Redis.
const REDIS: &[(&str, &str)] = &[
    ("Scan keys", "SCAN 0 MATCH prefix:* COUNT 100"),
    ("String", "SET key \"value\" EX 3600\nGET key"),
    ("Hash", "HSET key field \"value\"\nHGETALL key"),
    ("List", "RPUSH key \"value\"\nLRANGE key 0 99"),
    (
        "Sorted set",
        "ZADD key 1 member\nZRANGE key 0 99 WITHSCORES",
    ),
    ("Time to live", "EXPIRE key 3600\nTTL key"),
    ("Server info", "INFO memory\nDBSIZE"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statements;

    /// Every template has to come out of the editor's splitter as the statements it was
    /// written as; a MongoDB template with a blank line in it would run as two broken halves.
    #[test]
    fn templates_split_into_whole_statements() {
        for template in MONGODB {
            assert_eq!(
                statements::split(template.1, Engine::MongoDb).len(),
                1,
                "{}",
                template.0
            );
            assert!(
                serde_json::from_str::<serde_json::Value>(template.1).is_ok(),
                "{} is not JSON",
                template.0
            );
        }
        for engine in [Engine::Postgres, Engine::SqlServer, Engine::ClickHouse] {
            for template in templates(engine) {
                assert_eq!(
                    statements::split(template.1, engine).len(),
                    1,
                    "{}",
                    template.0
                );
            }
        }
        assert_eq!(statements::split(REDIS[1].1, Engine::Redis).len(), 2);
    }
}
