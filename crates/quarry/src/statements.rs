//! Splitting editor text into the statements that get sent to the server, and finding the one
//! under the cursor.
//!
//! Every engine runs one statement per request — ClickHouse's HTTP interface rejects a second
//! one, Postgres' simple protocol would run them all but hands back their rows mixed together —
//! so the query view splits first and runs each piece on its own. Where a statement ends is
//! engine-specific: a `;` for the SQL dialects, a `GO` line for SQL Server (whose procedure
//! bodies are full of semicolons), a line for Redis, a blank line for MongoDB's JSON commands.

use crate::driver::Engine;
use std::ops::Range;

/// The byte ranges of the statements in `text`, trimmed, in order. Pieces that hold nothing but
/// whitespace and comments are left out, so a trailing `;` or a commented-out block does not
/// turn into an empty request.
pub fn split(text: &str, engine: Engine) -> Vec<Range<usize>> {
    segments(text, engine)
        .into_iter()
        .filter_map(|segment| segment.content)
        .collect()
}

/// The statement the cursor at `offset` is in. A cursor in the gap after a statement — just past
/// its `;`, or on the blank lines below it — still means that statement, because that is where
/// the cursor is left after typing one.
pub fn statement_at(text: &str, offset: usize, engine: Engine) -> Option<Range<usize>> {
    let segments = segments(text, engine);
    let position = segments
        .iter()
        .position(|segment| segment.span.contains(&offset) || segment.span.end == offset)?;
    let segment = segments.get(position)?;
    if let Some(content) = &segment.content
        && offset >= content.start
    {
        return Some(content.clone());
    }
    segments
        .get(..position)?
        .iter()
        .rev()
        .find_map(|segment| segment.content.clone())
        .or_else(|| {
            segments
                .get(position..)?
                .iter()
                .find_map(|segment| segment.content.clone())
        })
}

struct Segment {
    /// From just after the previous separator to just after this one, so every offset in the
    /// text falls in exactly one segment.
    span: Range<usize>,
    /// The statement inside, trimmed; `None` when there is only whitespace or comments.
    content: Option<Range<usize>>,
}

fn segments(text: &str, engine: Engine) -> Vec<Segment> {
    let boundaries = match engine {
        Engine::Postgres => separators(text, Dialect::Postgres),
        Engine::ClickHouse => separators(text, Dialect::ClickHouse),
        Engine::SqlServer => separators(text, Dialect::TransactSql),
        Engine::Redis => line_separators(text),
        Engine::MongoDb => blank_line_separators(text),
    };

    let mut segments = Vec::new();
    let mut start = 0;
    for Boundary {
        content_end,
        segment_end,
    } in boundaries.into_iter().chain(std::iter::once(Boundary {
        content_end: text.len(),
        segment_end: text.len(),
    })) {
        if segment_end < start {
            continue;
        }
        let content = text
            .get(start..content_end)
            .and_then(|piece| trimmed_content(piece, engine))
            .map(|range| start + range.start..start + range.end);
        segments.push(Segment {
            span: start..segment_end,
            content,
        });
        start = segment_end;
    }
    segments
}

struct Boundary {
    /// Where the statement's own text stops: before the `;`, or before the `GO` line.
    content_end: usize,
    /// Where the next segment starts: after the `;` or after the `GO` line.
    segment_end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Postgres,
    ClickHouse,
    TransactSql,
}

/// Scans for statement separators while tracking strings, quoted identifiers and comments, so a
/// `;` inside `'a;b'` or `-- done;` is not mistaken for the end of a statement.
fn separators(text: &str, dialect: Dialect) -> Vec<Boundary> {
    let bytes = text.as_bytes();
    let mut boundaries = Vec::new();
    let mut index = 0;
    let mut at_line_start = true;

    while index < bytes.len() {
        let Some(&byte) = bytes.get(index) else {
            break;
        };

        if dialect == Dialect::TransactSql && at_line_start {
            if let Some(line_end) = go_line_end(text, index) {
                boundaries.push(Boundary {
                    content_end: index,
                    segment_end: line_end,
                });
                index = line_end;
                continue;
            }
        }
        at_line_start = byte == b'\n';

        match byte {
            b'\'' => index = skip_quoted(bytes, index, b'\'', dialect == Dialect::ClickHouse),
            b'"' => index = skip_quoted(bytes, index, b'"', dialect == Dialect::ClickHouse),
            b'`' if dialect == Dialect::ClickHouse => index = skip_quoted(bytes, index, b'`', true),
            b'[' if dialect == Dialect::TransactSql => {
                index = skip_quoted(bytes, index, b']', false)
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = bytes
                    .get(index..)
                    .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
                    .map_or(bytes.len(), |newline| index + newline);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = text
                    .get(index + 2..)
                    .and_then(|rest| rest.find("*/"))
                    .map_or(bytes.len(), |close| index + 2 + close + 2);
            }
            b'$' if dialect == Dialect::Postgres => {
                index = skip_dollar_quoted(text, index).unwrap_or(index + 1);
            }
            b';' if dialect != Dialect::TransactSql => {
                boundaries.push(Boundary {
                    content_end: index,
                    segment_end: index + 1,
                });
                index += 1;
            }
            _ => index += 1,
        }
    }
    boundaries
}

/// Returns the index just past the closing quote, or the end of the text when the quote is never
/// closed. A doubled quote is an escaped one in every dialect; ClickHouse also takes backslash
/// escapes.
fn skip_quoted(bytes: &[u8], open: usize, close: u8, backslash_escapes: bool) -> usize {
    let mut index = open + 1;
    while let Some(&byte) = bytes.get(index) {
        if backslash_escapes && byte == b'\\' {
            index += 2;
            continue;
        }
        if byte == close {
            if bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

/// Postgres dollar quoting: `$$ ... $$` or `$tag$ ... $tag$`, which is how function bodies full of
/// semicolons are written. Returns `None` when `$` does not start one (e.g. a `$1` parameter).
fn skip_dollar_quoted(text: &str, start: usize) -> Option<usize> {
    let rest = text.get(start + 1..)?;
    let tag_length = rest.find('$')?;
    let tag = rest.get(..tag_length)?;
    if !tag
        .chars()
        .all(|character| character.is_alphanumeric() || character == '_')
        || tag.starts_with(|character: char| character.is_ascii_digit())
    {
        return None;
    }
    let delimiter = format!("${tag}$");
    let body_start = start + delimiter.len();
    let close = text.get(body_start..)?.find(&delimiter)?;
    Some(body_start + close + delimiter.len())
}

/// If the line starting at `line_start` is a `GO` batch separator (optionally with a repeat
/// count, which is ignored), returns the index just past it.
fn go_line_end(text: &str, line_start: usize) -> Option<usize> {
    let rest = text.get(line_start..)?;
    let line_length = rest.find('\n').map_or(rest.len(), |newline| newline + 1);
    let line = rest.get(..line_length)?.trim();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    if !first.eq_ignore_ascii_case("go") {
        return None;
    }
    match words.next() {
        None => {}
        Some(count) if count.chars().all(|character| character.is_ascii_digit()) => {}
        Some(_) => return None,
    }
    words.next().is_none().then_some(line_start + line_length)
}

fn line_separators(text: &str) -> Vec<Boundary> {
    text.match_indices('\n')
        .map(|(index, _)| Boundary {
            content_end: index,
            segment_end: index + 1,
        })
        .collect()
}

/// MongoDB commands are JSON documents, often spread over several lines, so they are separated by
/// blank lines rather than by newlines.
fn blank_line_separators(text: &str) -> Vec<Boundary> {
    let mut boundaries = Vec::new();
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        if line.trim().is_empty() {
            boundaries.push(Boundary {
                content_end: line_start,
                segment_end: line_end,
            });
        }
        line_start = line_end;
    }
    boundaries
}

/// The range of `piece` left after trimming whitespace, or `None` if nothing but whitespace and
/// comments remains.
fn trimmed_content(piece: &str, engine: Engine) -> Option<Range<usize>> {
    let start = piece.len() - piece.trim_start().len();
    let end = piece.trim_end().len();
    if start >= end {
        return None;
    }
    let trimmed = piece.get(start..end)?;
    let has_code = match engine {
        Engine::Postgres | Engine::ClickHouse | Engine::SqlServer => {
            !strip_sql_comments(trimmed).trim().is_empty()
        }
        Engine::Redis | Engine::MongoDb => true,
    };
    has_code.then_some(start..end)
}

fn strip_sql_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let line_comment = rest.find("--");
        let block_comment = rest.find("/*");
        let next = match (line_comment, block_comment) {
            (Some(line), Some(block)) => Some(line.min(block)),
            (line, block) => line.or(block),
        };
        let Some(comment_start) = next else {
            output.push_str(rest);
            return output;
        };
        output.push_str(rest.get(..comment_start).unwrap_or_default());
        let after = rest.get(comment_start..).unwrap_or_default();
        rest = if after.starts_with("--") {
            after
                .find('\n')
                .map_or("", |newline| after.get(newline..).unwrap_or_default())
        } else {
            after
                .find("*/")
                .map_or("", |close| after.get(close + 2..).unwrap_or_default())
        };
    }
}

/// Whether running `statement` may change data or schema. Used to ask before running on a
/// production connection, so it errs towards yes: a false alarm costs a click, a miss costs data.
///
/// SQL is judged by its words rather than its first keyword, because one T-SQL batch can hold
/// any number of statements and a Postgres `WITH` can wrap a `DELETE`. Words inside strings,
/// quoted identifiers and comments are skipped, so `WHERE note = 'delete me'` stays a read.
pub fn may_write(statement: &str, engine: Engine) -> bool {
    match engine {
        Engine::Postgres | Engine::SqlServer | Engine::ClickHouse => sql_words(statement, engine)
            .iter()
            .any(|word| SQL_WRITE_WORDS.contains(&word.as_str())),
        Engine::MongoDb => mongo_may_write(statement),
        Engine::Redis => {
            let command = statement
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            !REDIS_READ_COMMANDS.contains(&command.as_str())
        }
    }
}

/// Words that start or are part of a statement that changes something. `INTO` catches
/// `SELECT ... INTO`, which creates a table; `EXEC` and `CALL` run code that could do anything.
const SQL_WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "REPLACE", "DROP", "ALTER", "CREATE",
    "TRUNCATE", "RENAME", "GRANT", "REVOKE", "DENY", "EXEC", "EXECUTE", "CALL", "DO", "COPY",
    "INTO", "REINDEX", "CLUSTER", "VACUUM", "OPTIMIZE", "KILL", "SYSTEM", "ATTACH", "DETACH",
    "EXCHANGE", "BACKUP", "RESTORE", "DBCC", "BULK", "LOCK", "REFRESH", "COMMENT", "SECURITY",
    "IMPORT", "LOAD", "SHUTDOWN",
];

/// The uppercased words of `statement` outside strings, quoted identifiers and comments.
fn sql_words(statement: &str, engine: Engine) -> Vec<String> {
    let dialect = match engine {
        Engine::SqlServer => Dialect::TransactSql,
        Engine::ClickHouse => Dialect::ClickHouse,
        _ => Dialect::Postgres,
    };
    let bytes = statement.as_bytes();
    let mut words = Vec::new();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\'' => index = skip_quoted(bytes, index, b'\'', dialect == Dialect::ClickHouse),
            b'"' => index = skip_quoted(bytes, index, b'"', dialect == Dialect::ClickHouse),
            b'`' if dialect == Dialect::ClickHouse => index = skip_quoted(bytes, index, b'`', true),
            b'[' if dialect == Dialect::TransactSql => {
                index = skip_quoted(bytes, index, b']', false)
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = bytes
                    .get(index..)
                    .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
                    .map_or(bytes.len(), |newline| index + newline);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = statement
                    .get(index + 2..)
                    .and_then(|rest| rest.find("*/"))
                    .map_or(bytes.len(), |close| index + 2 + close + 2);
            }
            b'$' if dialect == Dialect::Postgres => {
                index = skip_dollar_quoted(statement, index).unwrap_or(index + 1);
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let length = bytes.get(index..).map_or(0, |rest| {
                    rest.iter()
                        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                        .count()
                });
                // A word right after a dot is a name (`t.update`), and one right before a `(`
                // is a function call (`replace(name, ...)`), not a keyword. T-SQL's dynamic
                // `EXEC('...')` is the exception that runs anything.
                let after_dot = index > 0 && bytes.get(index - 1) == Some(&b'.');
                let before_parenthesis = bytes.get(index + length) == Some(&b'(');
                if !after_dot && let Some(word) = statement.get(index..index + length) {
                    let word = word.to_ascii_uppercase();
                    if !before_parenthesis || word == "EXEC" || word == "EXECUTE" {
                        words.push(word);
                    }
                }
                index += length;
            }
            _ => index += 1,
        }
    }
    words
}

/// MongoDB commands that only read. Anything else, including a command this list has never
/// heard of, counts as a write; so does an aggregation that ends in `$out` or `$merge`.
const MONGO_READ_COMMANDS: &[&str] = &[
    "find",
    "aggregate",
    "count",
    "distinct",
    "listCollections",
    "listIndexes",
    "listDatabases",
    "dbStats",
    "collStats",
    "serverStatus",
    "ping",
    "explain",
    "hello",
    "isMaster",
    "buildInfo",
    "connectionStatus",
    "getMore",
    "hostInfo",
    "currentOp",
    "validate",
    "dataSize",
];

fn mongo_may_write(statement: &str) -> bool {
    let Ok(serde_json::Value::Object(command)) =
        serde_json::from_str::<serde_json::Value>(statement)
    else {
        return true;
    };
    let Some(name) = command.keys().next() else {
        return true;
    };
    if !MONGO_READ_COMMANDS.contains(&name.as_str()) {
        return true;
    }
    name == "aggregate" && (statement.contains("\"$out\"") || statement.contains("\"$merge\""))
}

/// Redis commands that only read. Anything else is treated as a write.
const REDIS_READ_COMMANDS: &[&str] = &[
    "GET",
    "MGET",
    "GETRANGE",
    "STRLEN",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LPOS",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SSCAN",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZRANK",
    "ZREVRANK",
    "ZSCAN",
    "ZRANDMEMBER",
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XINFO",
    "XPENDING",
    "SCAN",
    "KEYS",
    "DBSIZE",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "OBJECT",
    "MEMORY",
    "DUMP",
    "PFCOUNT",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "RANDOMKEY",
    "LASTSAVE",
    "ROLE",
    "COMMAND",
    "SLOWLOG",
    "LATENCY",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_writes_from_reads() {
        for read in [
            "select * from orders where note = 'delete me'",
            "SELECT o.update, o.\"drop\" FROM t o -- delete later",
            "with recent as (select 1) select * from recent",
            "EXPLAIN SELECT 1",
            "select replace(name, 'a', 'b') from t",
        ] {
            assert!(!may_write(read, Engine::Postgres), "{read}");
        }
        for write in [
            "update orders set total = 0",
            "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
            "select * into backup from orders",
            "create table t (id int)",
        ] {
            assert!(may_write(write, Engine::Postgres), "{write}");
        }
        assert!(may_write(
            "DECLARE @x int = 1;\nUPDATE [t] SET a = @x",
            Engine::SqlServer
        ));
        assert!(!may_write("SELECT [delete] FROM [t]", Engine::SqlServer));
        assert!(may_write("EXEC('DROP TABLE t')", Engine::SqlServer));
        assert!(may_write(
            "OPTIMIZE TABLE app.orders FINAL",
            Engine::ClickHouse
        ));
        assert!(!may_write("{\"find\": \"users\"}", Engine::MongoDb));
        assert!(may_write(
            "{\"delete\": \"users\", \"deletes\": []}",
            Engine::MongoDb
        ));
        assert!(may_write(
            "{\"aggregate\": \"a\", \"pipeline\": [{\"$out\": \"b\"}]}",
            Engine::MongoDb
        ));
        assert!(!may_write("hgetall user:1", Engine::Redis));
        assert!(may_write("FLUSHDB", Engine::Redis));
    }

    fn pieces(text: &str, engine: Engine) -> Vec<&str> {
        split(text, engine)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn splits_sql_on_semicolons_outside_strings_and_comments() {
        let text =
            "select 'a;b' as x;\n-- not here;\nselect \"c;d\" from t; /* nor; here */ select 3";
        assert_eq!(
            pieces(text, Engine::Postgres),
            vec![
                "select 'a;b' as x",
                "-- not here;\nselect \"c;d\" from t",
                "/* nor; here */ select 3",
            ]
        );
    }

    #[test]
    fn keeps_postgres_function_bodies_whole() {
        let text = "create function f() returns int as $body$ begin return 1; end $body$ language plpgsql;\nselect f();";
        assert_eq!(
            pieces(text, Engine::Postgres),
            vec![
                "create function f() returns int as $body$ begin return 1; end $body$ language plpgsql",
                "select f()",
            ]
        );
    }

    #[test]
    fn splits_transact_sql_on_go_lines_only() {
        let text = "create procedure p as begin select 1; select 2; end\nGO\nexec p;\ngo 2\n";
        assert_eq!(
            pieces(text, Engine::SqlServer),
            vec![
                "create procedure p as begin select 1; select 2; end",
                "exec p;"
            ]
        );
    }

    #[test]
    fn clickhouse_understands_backslash_escapes() {
        let text = "select 'it\\'s; fine'; select 2";
        assert_eq!(
            pieces(text, Engine::ClickHouse),
            vec!["select 'it\\'s; fine'", "select 2"]
        );
    }

    #[test]
    fn drops_pieces_that_are_only_comments() {
        assert_eq!(
            pieces("select 1; -- trailing\n;", Engine::Postgres),
            vec!["select 1"]
        );
    }

    #[test]
    fn redis_and_mongo_split_by_line_and_block() {
        assert_eq!(
            pieces("GET a\n\nSET b \"x y\"\n", Engine::Redis),
            vec!["GET a", "SET b \"x y\""]
        );
        assert_eq!(
            pieces(
                "{\"find\": \"a\",\n \"limit\": 1}\n\n{\"count\": \"b\"}",
                Engine::MongoDb
            ),
            vec!["{\"find\": \"a\",\n \"limit\": 1}", "{\"count\": \"b\"}"]
        );
    }

    #[test]
    fn finds_the_statement_under_the_cursor() {
        let text = "select 1;\nselect 2;\n\nselect 3";
        let at = |offset| statement_at(text, offset, Engine::Postgres).map(|range| &text[range]);
        assert_eq!(at(3), Some("select 1"));
        // Just past the semicolon still means the statement that was just typed.
        assert_eq!(at(9), Some("select 1"));
        assert_eq!(at(12), Some("select 2"));
        // On the blank line between statements: the one above.
        assert_eq!(at(20), Some("select 2"));
        assert_eq!(at(21), Some("select 3"));
        assert_eq!(at(text.len()), Some("select 3"));
    }

    #[test]
    fn cursor_in_leading_blank_space_picks_the_first_statement() {
        let text = "\n\nselect 1;";
        assert_eq!(
            statement_at(text, 0, Engine::Postgres).map(|range| &text[range]),
            Some("select 1")
        );
    }
}
