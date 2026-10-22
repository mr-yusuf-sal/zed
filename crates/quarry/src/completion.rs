//! Completions for the query editor: the engine's keywords and functions, which are fixed, plus
//! the names that only the server knows — schemas, tables, columns, collections, keys — which the
//! query view fetches through `Driver::catalog` and shares here.
//!
//! There is no SQL parser behind this. What it understands is the part that pays for itself:
//! after `x.` it offers what lives in `x`, where `x` may be a schema, a table, or an alias
//! introduced by `FROM`/`JOIN`/`UPDATE`/`INTO`. Everything else is a fuzzy match over every name.

use crate::driver::{CatalogEntry, Engine, ObjectKind};
use collections::HashMap;
use editor::{CompletionProvider, Editor};
use fuzzy::StringMatchCandidate;
use gpui::{Context, Entity, Task, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};
use std::{cell::RefCell, rc::Rc};

const MAX_COMPLETIONS: usize = 100;

pub struct QuarryCompletionProvider {
    engine: Engine,
    /// Filled in by the query view once the server answers, and replaced when the editor is
    /// pointed at another database.
    catalog: Rc<RefCell<Vec<CatalogEntry>>>,
}

impl QuarryCompletionProvider {
    pub fn new(engine: Engine, catalog: Rc<RefCell<Vec<CatalogEntry>>>) -> Self {
        Self { engine, catalog }
    }
}

#[derive(Clone)]
struct Candidate {
    label: String,
    detail: Option<String>,
    is_keyword: bool,
}

impl CompletionProvider for QuarryCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let cursor = buffer_position.to_offset(buffer);
        let text = buffer.text();
        let word_start = word_start(&text, cursor);
        let query = text.get(word_start..cursor).unwrap_or_default().to_string();
        let qualifier = qualifier_before(&text, word_start);
        let replace_range = buffer.anchor_before(word_start)..buffer_position;

        let candidates = {
            let catalog = self.catalog.borrow();
            match &qualifier {
                Some(qualifier) => qualified_candidates(qualifier, &text, &catalog),
                None => unqualified_candidates(self.engine, &text, word_start, &catalog),
            }
        };
        if candidates.is_empty() {
            return Task::ready(Ok(Vec::new()));
        }

        let lowercase_keywords =
            !query.is_empty() && query.chars().all(|character| !character.is_uppercase());
        let match_candidates: Vec<StringMatchCandidate> = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| StringMatchCandidate::new(index, &candidate.label))
            .collect();
        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            let matches = if query.is_empty() {
                Vec::new()
            } else {
                fuzzy::match_strings(
                    &match_candidates,
                    &query,
                    false,
                    true,
                    MAX_COMPLETIONS,
                    &Default::default(),
                    executor,
                )
                .await
            };
            // With nothing typed yet (right after `alias.`), offer the qualified names in their
            // natural order instead of an empty list.
            let chosen: Vec<&Candidate> = if query.is_empty() {
                candidates.iter().take(MAX_COMPLETIONS).collect()
            } else {
                matches
                    .iter()
                    .filter_map(|string_match| candidates.get(string_match.candidate_id))
                    .collect()
            };

            let completions = chosen
                .into_iter()
                .map(|candidate| {
                    // Keywords follow the case the user is typing in; names are inserted exactly
                    // as the server spells them, since quoted identifiers are case-sensitive.
                    let new_text = if candidate.is_keyword && lowercase_keywords {
                        candidate.label.to_lowercase()
                    } else {
                        candidate.label.clone()
                    };
                    Completion {
                        replace_range: replace_range.clone(),
                        label: CodeLabel::plain(new_text.clone(), None),
                        new_text,
                        documentation: candidate
                            .detail
                            .clone()
                            .map(|detail| CompletionDocumentation::SingleLine(detail.into())),
                        source: CompletionSource::Custom,
                        icon_path: None,
                        icon_color: None,
                        match_start: None,
                        snippet_deduplication_key: None,
                        insert_text_mode: None,
                        confirm: None,
                        group: None,
                    }
                })
                .collect();

            Ok(vec![CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
                is_incomplete: false,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars().last().is_some_and(|character| {
            is_word_character(character) || character == '.' || character == '$'
        })
    }

    fn sort_completions(&self) -> bool {
        false
    }
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | ':' | '$' | '@' | '#')
}

fn word_start(text: &str, cursor: usize) -> usize {
    text.get(..cursor)
        .and_then(|before| {
            before
                .char_indices()
                .rev()
                .take_while(|(_, character)| is_word_character(*character))
                .last()
                .map(|(index, _)| index)
        })
        .unwrap_or(cursor)
}

/// For `schema.ta|` or `"Alias".co|`, the identifier before the dot with any quoting removed.
fn qualifier_before(text: &str, word_start: usize) -> Option<String> {
    let before = text.get(..word_start)?;
    let before_dot = before.strip_suffix('.')?;
    let identifier_start = before_dot
        .char_indices()
        .rev()
        .take_while(|(_, character)| {
            is_word_character(*character) || matches!(character, '"' | '`' | '[' | ']')
        })
        .last()
        .map(|(index, _)| index)?;
    let identifier = before_dot.get(identifier_start..)?;
    let unquoted = identifier.trim_matches(|character| matches!(character, '"' | '`' | '[' | ']'));
    (!unquoted.is_empty()).then(|| unquoted.to_string())
}

fn qualified_candidates(qualifier: &str, text: &str, catalog: &[CatalogEntry]) -> Vec<Candidate> {
    let aliases = table_aliases(text);
    let table = aliases
        .get(&qualifier.to_lowercase())
        .cloned()
        .unwrap_or_else(|| qualifier.to_string());

    let parent_is = |entry: &CatalogEntry, name: &str| {
        entry
            .parent
            .as_ref()
            .is_some_and(|parent| parent.eq_ignore_ascii_case(name))
    };
    let mut candidates: Vec<Candidate> = catalog
        .iter()
        .filter(|entry| match entry.kind {
            ObjectKind::Column => parent_is(entry, &table),
            ObjectKind::Table | ObjectKind::View | ObjectKind::Routine | ObjectKind::Collection => {
                parent_is(entry, qualifier)
            }
            _ => false,
        })
        .map(|entry| Candidate {
            label: entry.label.to_string(),
            detail: describe(entry),
            is_keyword: false,
        })
        .collect();
    dedup_by_label(&mut candidates);
    candidates
}

fn unqualified_candidates(
    engine: Engine,
    text: &str,
    word_start: usize,
    catalog: &[CatalogEntry],
) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    // For Redis the first word of a line is the command and everything after it is arguments,
    // usually key names: offering commands in argument position would only get in the way.
    let redis_argument = engine == Engine::Redis
        && text
            .get(..word_start)
            .and_then(|before| before.rsplit('\n').next())
            .is_some_and(|line| !line.trim().is_empty());

    if !redis_argument {
        candidates.extend(keywords(engine).iter().map(|keyword| Candidate {
            label: (*keyword).to_string(),
            detail: Some(keyword_detail(engine).to_string()),
            is_keyword: true,
        }));
    }

    // Columns of the tables the statement already mentions come before every other column, so
    // `SELECT na|` offers `name` from the table being queried rather than from anywhere.
    let mentioned: Vec<String> = table_aliases(text).into_values().collect();
    let mut columns_in_scope = Vec::new();
    let mut other_columns = Vec::new();
    for entry in catalog {
        let candidate = Candidate {
            label: entry.label.to_string(),
            detail: describe(entry),
            is_keyword: false,
        };
        match entry.kind {
            ObjectKind::Column => {
                let in_scope = entry.parent.as_ref().is_some_and(|parent| {
                    mentioned
                        .iter()
                        .any(|table| table.eq_ignore_ascii_case(parent))
                });
                if in_scope {
                    columns_in_scope.push(candidate);
                } else {
                    other_columns.push(candidate);
                }
            }
            ObjectKind::Key if engine == Engine::Redis && !redis_argument => {}
            _ => candidates.push(candidate),
        }
    }
    candidates.splice(0..0, columns_in_scope);
    candidates.extend(other_columns);
    dedup_by_label(&mut candidates);
    candidates
}

fn dedup_by_label(candidates: &mut Vec<Candidate>) {
    let mut seen = collections::HashSet::default();
    candidates.retain(|candidate| seen.insert(candidate.label.clone()));
}

fn describe(entry: &CatalogEntry) -> Option<String> {
    let kind = match entry.kind {
        ObjectKind::Database => "database",
        ObjectKind::Schema => "schema",
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::Column => "column",
        ObjectKind::Index => "index",
        ObjectKind::Collection => "collection",
        ObjectKind::Key => "key",
        ObjectKind::Folder => "prefix",
        ObjectKind::Routine => "routine",
        ObjectKind::Sequence => "sequence",
    };
    let mut description = kind.to_string();
    if let Some(parent) = &entry.parent {
        description.push_str(&format!(" in {parent}"));
    }
    if let Some(detail) = &entry.detail {
        description.push_str(&format!(" · {detail}"));
    }
    Some(description)
}

/// Maps each alias (and each bare table name) introduced by `FROM`, `JOIN`, `UPDATE` or `INTO`
/// to the table it stands for, lowercased for lookup. Schema prefixes are dropped because the
/// catalog files columns under the bare table name.
fn table_aliases(text: &str) -> HashMap<String, String> {
    let tokens: Vec<&str> = text
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | '(' | ')' | ';')
        })
        .filter(|token| !token.is_empty())
        .collect();

    let mut aliases = HashMap::default();
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        index += 1;
        if !matches!(
            token.to_ascii_uppercase().as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO"
        ) {
            continue;
        }
        let Some(table_token) = tokens.get(index) else {
            break;
        };
        let table = unquote(table_token.rsplit('.').next().unwrap_or(table_token));
        if table.is_empty() || is_reserved(&table) {
            continue;
        }
        aliases.insert(table.to_lowercase(), table.clone());

        let mut alias_index = index + 1;
        if tokens
            .get(alias_index)
            .is_some_and(|token| token.eq_ignore_ascii_case("AS"))
        {
            alias_index += 1;
        }
        if let Some(alias) = tokens.get(alias_index) {
            let alias = unquote(alias);
            if !alias.is_empty() && !is_reserved(&alias) {
                aliases.insert(alias.to_lowercase(), table);
            }
        }
    }
    aliases
}

fn unquote(token: &str) -> String {
    token
        .trim_matches(|character| matches!(character, '"' | '`' | '[' | ']'))
        .to_string()
}

/// Words that can follow a table name and must not be taken for its alias.
fn is_reserved(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "WHERE"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "CROSS"
            | "ON"
            | "USING"
            | "GROUP"
            | "ORDER"
            | "LIMIT"
            | "SET"
            | "VALUES"
            | "SELECT"
            | "UNION"
            | "HAVING"
            | "WINDOW"
            | "FINAL"
            | "SAMPLE"
            | "PREWHERE"
            | "FORMAT"
            | "SETTINGS"
            | "OFFSET"
            | "FETCH"
            | "RETURNING"
            | "WITH"
            | "TOP"
            | "OUTPUT"
            | "AS"
    )
}

fn keyword_detail(engine: Engine) -> &'static str {
    match engine {
        Engine::Redis => "command",
        Engine::MongoDb => "command field / operator",
        Engine::Postgres | Engine::SqlServer | Engine::ClickHouse => "keyword",
    }
}

const SQL_COMMON: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "AS",
    "DISTINCT",
    "ORDER BY",
    "GROUP BY",
    "HAVING",
    "ASC",
    "DESC",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "ON",
    "USING",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "EXCEPT",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "ALTER TABLE",
    "DROP TABLE",
    "DROP VIEW",
    "TRUNCATE TABLE",
    "PRIMARY KEY",
    "FOREIGN KEY",
    "REFERENCES",
    "DEFAULT",
    "CHECK",
    "UNIQUE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "WITH",
    "OVER",
    "PARTITION BY",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "NULLIF",
    "CAST",
    "LOWER",
    "UPPER",
    "TRIM",
    "LENGTH",
    "SUBSTRING",
    "REPLACE",
    "ROUND",
    "ABS",
    "ROW_NUMBER",
    "RANK",
    "DENSE_RANK",
    "LAG",
    "LEAD",
    "TRUE",
    "FALSE",
];

const POSTGRES: &[&str] = &[
    "LIMIT",
    "OFFSET",
    "RETURNING",
    "ILIKE",
    "SIMILAR TO",
    "ON CONFLICT",
    "DO NOTHING",
    "DO UPDATE",
    "LATERAL",
    "MATERIALIZED",
    "REFRESH MATERIALIZED VIEW",
    "EXPLAIN",
    "ANALYZE",
    "VACUUM",
    "SERIAL",
    "BIGSERIAL",
    "TEXT",
    "VARCHAR",
    "INTEGER",
    "BIGINT",
    "NUMERIC",
    "BOOLEAN",
    "TIMESTAMPTZ",
    "TIMESTAMP",
    "DATE",
    "INTERVAL",
    "JSONB",
    "JSON",
    "UUID",
    "BYTEA",
    "ARRAY",
    "now()",
    "current_date",
    "current_timestamp",
    "date_trunc",
    "extract",
    "to_char",
    "to_date",
    "string_agg",
    "array_agg",
    "jsonb_build_object",
    "jsonb_agg",
    "generate_series",
    "unnest",
    "greatest",
    "least",
    "concat",
    "split_part",
    "regexp_replace",
    "pg_sleep",
    "FILTER",
    "DISTINCT ON",
];

const SQL_SERVER: &[&str] = &[
    "TOP",
    "OFFSET",
    "FETCH NEXT",
    "ROWS ONLY",
    "OUTPUT",
    "MERGE",
    "EXEC",
    "EXECUTE",
    "DECLARE",
    "PRINT",
    "GO",
    "USE",
    "IF",
    "WHILE",
    "BEGIN TRAN",
    "COMMIT TRAN",
    "NOLOCK",
    "IDENTITY",
    "NVARCHAR",
    "VARCHAR",
    "INT",
    "BIGINT",
    "DECIMAL",
    "MONEY",
    "BIT",
    "DATETIME2",
    "DATE",
    "UNIQUEIDENTIFIER",
    "GETDATE()",
    "SYSDATETIME()",
    "DATEADD",
    "DATEDIFF",
    "DATEPART",
    "FORMAT",
    "ISNULL",
    "IIF",
    "CHOOSE",
    "CONVERT",
    "TRY_CAST",
    "TRY_CONVERT",
    "STRING_AGG",
    "CONCAT",
    "LEN",
    "CHARINDEX",
    "NEWID()",
    "@@ROWCOUNT",
    "@@IDENTITY",
    "SCOPE_IDENTITY()",
    "CROSS APPLY",
    "OUTER APPLY",
    "WITH (NOLOCK)",
    "SET NOCOUNT ON",
];

const CLICKHOUSE: &[&str] = &[
    "LIMIT",
    "OFFSET",
    "FINAL",
    "PREWHERE",
    "SAMPLE",
    "ARRAY JOIN",
    "GLOBAL",
    "FORMAT",
    "SETTINGS",
    "ENGINE",
    "MergeTree",
    "ReplacingMergeTree",
    "AggregatingMergeTree",
    "SummingMergeTree",
    "PARTITION BY",
    "PRIMARY KEY",
    "TTL",
    "OPTIMIZE TABLE",
    "SHOW TABLES",
    "SHOW DATABASES",
    "DESCRIBE TABLE",
    "SYSTEM",
    "UInt8",
    "UInt32",
    "UInt64",
    "Int32",
    "Int64",
    "Float64",
    "Decimal",
    "String",
    "FixedString",
    "Date",
    "DateTime",
    "DateTime64",
    "UUID",
    "LowCardinality",
    "Nullable",
    "Array",
    "Tuple",
    "Map",
    "Enum8",
    "count()",
    "uniq",
    "uniqExact",
    "quantile",
    "groupArray",
    "argMax",
    "argMin",
    "any",
    "toDate",
    "toDateTime",
    "toStartOfDay",
    "toStartOfHour",
    "toYYYYMM",
    "now()",
    "today()",
    "formatReadableSize",
    "arrayJoin",
    "arrayMap",
    "arrayFilter",
    "if",
    "multiIf",
    "toString",
    "toUInt32",
    "JSONExtractString",
    "sumIf",
    "countIf",
    "avgIf",
];

const MONGO: &[&str] = &[
    "\"find\"",
    "\"filter\"",
    "\"projection\"",
    "\"sort\"",
    "\"limit\"",
    "\"skip\"",
    "\"aggregate\"",
    "\"pipeline\"",
    "\"cursor\"",
    "\"count\"",
    "\"query\"",
    "\"distinct\"",
    "\"key\"",
    "\"insert\"",
    "\"documents\"",
    "\"update\"",
    "\"updates\"",
    "\"q\"",
    "\"u\"",
    "\"upsert\"",
    "\"multi\"",
    "\"delete\"",
    "\"deletes\"",
    "\"listCollections\"",
    "\"listIndexes\"",
    "\"createIndexes\"",
    "\"dropIndexes\"",
    "\"collStats\"",
    "\"dbStats\"",
    "\"$db\"",
    "$match",
    "$group",
    "$project",
    "$sort",
    "$limit",
    "$skip",
    "$unwind",
    "$lookup",
    "$addFields",
    "$set",
    "$unset",
    "$count",
    "$facet",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$push",
    "$addToSet",
    "$first",
    "$last",
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$and",
    "$or",
    "$not",
    "$exists",
    "$regex",
    "$elemMatch",
    "$size",
    "$inc",
];

const REDIS: &[&str] = &[
    "GET",
    "SET",
    "SETEX",
    "SETNX",
    "MGET",
    "MSET",
    "GETDEL",
    "GETEX",
    "APPEND",
    "STRLEN",
    "INCR",
    "INCRBY",
    "DECR",
    "DECRBY",
    "DEL",
    "UNLINK",
    "EXISTS",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "TTL",
    "PTTL",
    "PERSIST",
    "TYPE",
    "RENAME",
    "COPY",
    "DUMP",
    "SCAN",
    "KEYS",
    "RANDOMKEY",
    "DBSIZE",
    "SELECT",
    "FLUSHDB",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "CONFIG GET",
    "CLIENT LIST",
    "MEMORY USAGE",
    "OBJECT ENCODING",
    "HGET",
    "HSET",
    "HMGET",
    "HGETALL",
    "HDEL",
    "HEXISTS",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HINCRBY",
    "HSCAN",
    "LPUSH",
    "RPUSH",
    "LPOP",
    "RPOP",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LSET",
    "LREM",
    "LTRIM",
    "LINSERT",
    "LMOVE",
    "SADD",
    "SREM",
    "SMEMBERS",
    "SISMEMBER",
    "SCARD",
    "SPOP",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SSCAN",
    "ZADD",
    "ZREM",
    "ZRANGE",
    "ZREVRANGE",
    "ZRANGEBYSCORE",
    "ZSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZCARD",
    "ZCOUNT",
    "ZINCRBY",
    "ZSCAN",
    "XADD",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XREAD",
    "XDEL",
    "XTRIM",
    "XINFO STREAM",
    "GEOADD",
    "GEOPOS",
    "GEODIST",
    "GEOSEARCH",
    "PFADD",
    "PFCOUNT",
    "PFMERGE",
    "PUBLISH",
    "PUBSUB CHANNELS",
    "WITHSCORES",
    "MATCH",
    "COUNT",
    "EX",
    "PX",
    "NX",
    "XX",
    "KEEPTTL",
];

fn keywords(engine: Engine) -> Vec<&'static str> {
    match engine {
        Engine::Postgres => SQL_COMMON.iter().chain(POSTGRES).copied().collect(),
        Engine::SqlServer => SQL_COMMON.iter().chain(SQL_SERVER).copied().collect(),
        Engine::ClickHouse => SQL_COMMON.iter().chain(CLICKHOUSE).copied().collect(),
        Engine::MongoDb => MONGO.to_vec(),
        Engine::Redis => REDIS.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::SharedString;

    fn entry(label: &str, kind: ObjectKind, parent: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            label: SharedString::from(label.to_string()),
            kind,
            parent: parent.map(|parent| SharedString::from(parent.to_string())),
            detail: None,
        }
    }

    fn labels(candidates: &[Candidate]) -> Vec<&str> {
        candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect()
    }

    fn catalog() -> Vec<CatalogEntry> {
        vec![
            entry("shop", ObjectKind::Schema, None),
            entry("customers", ObjectKind::Table, Some("shop")),
            entry("orders", ObjectKind::Table, Some("shop")),
            entry("id", ObjectKind::Column, Some("customers")),
            entry("city", ObjectKind::Column, Some("customers")),
            entry("total", ObjectKind::Column, Some("orders")),
        ]
    }

    #[test]
    fn resolves_aliases_to_their_tables() {
        let text = "select c. from shop.customers c join shop.orders as o on o.customer_id = c.id";
        let aliases = table_aliases(text);
        assert_eq!(aliases.get("c").map(String::as_str), Some("customers"));
        assert_eq!(aliases.get("o").map(String::as_str), Some("orders"));
        assert_eq!(aliases.get("orders").map(String::as_str), Some("orders"));
    }

    #[test]
    fn offers_columns_after_an_alias_and_tables_after_a_schema() {
        let text = "select c. from shop.customers c";
        assert_eq!(
            labels(&qualified_candidates("c", text, &catalog())),
            vec!["id", "city"]
        );
        assert_eq!(
            labels(&qualified_candidates(
                "shop",
                "select * from shop.",
                &catalog()
            )),
            vec!["customers", "orders"]
        );
    }

    #[test]
    fn finds_the_qualifier_before_the_word() {
        let text = "select \"c\".ci";
        let start = word_start(text, text.len());
        assert_eq!(&text[start..], "ci");
        assert_eq!(qualifier_before(text, start).as_deref(), Some("c"));
        assert_eq!(qualifier_before("select ci", 7), None);
    }

    #[test]
    fn columns_of_mentioned_tables_come_first() {
        let candidates =
            unqualified_candidates(Engine::Postgres, "select  from shop.orders", 7, &catalog());
        let position = |label: &str| {
            candidates
                .iter()
                .position(|candidate| candidate.label == label)
        };
        assert!(position("total") < position("id"));
        assert!(position("total") < position("SELECT"));
    }

    #[test]
    fn redis_offers_keys_only_in_argument_position() {
        let catalog = vec![entry("user:1", ObjectKind::Key, None)];
        let command_position = unqualified_candidates(Engine::Redis, "HG", 0, &catalog);
        assert!(labels(&command_position).contains(&"HGET"));
        assert!(!labels(&command_position).contains(&"user:1"));

        let argument_position = unqualified_candidates(Engine::Redis, "HGETALL us", 8, &catalog);
        assert!(labels(&argument_position).contains(&"user:1"));
        assert!(!labels(&argument_position).contains(&"HGET"));
    }
}
