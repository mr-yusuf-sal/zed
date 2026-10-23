//! Comparing two schemas of the same engine, possibly on different servers, and writing the
//! script that makes the second look like the first.
//!
//! What a schema holds is read by each driver into the same plain shape; the comparison is
//! done here, once. Writing the statements is the driver's, since every engine spells
//! `ALTER TABLE` differently. The script is opened for review, never run: anything that loses
//! data — dropping a table or a column — is written commented out, to be enabled by hand.

use crate::driver::Driver;
use ui::SharedString;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnShape {
    pub name: String,
    /// The type as the engine writes it in a CREATE TABLE.
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// Whatever else makes the column what it is and cannot be changed with a plain ALTER:
    /// an identity, a computed expression.
    pub extra: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableShape {
    pub name: String,
    pub columns: Vec<ColumnShape>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaSnapshot {
    pub tables: Vec<TableShape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnChange {
    pub before: ColumnShape,
    pub after: ColumnShape,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableChange {
    pub table: String,
    pub added: Vec<ColumnShape>,
    pub dropped: Vec<ColumnShape>,
    pub changed: Vec<ColumnChange>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaDiff {
    /// Tables only the source has.
    pub missing: Vec<String>,
    /// Tables only the target has.
    pub extra: Vec<String>,
    pub changed: Vec<TableChange>,
}

impl SchemaDiff {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.changed.is_empty()
    }
}

/// What turns `target` into `source`. Tables and columns are matched by name; a renamed one
/// reads as dropped and added, since nothing in a catalog says it was renamed.
pub fn diff(source: &SchemaSnapshot, target: &SchemaSnapshot) -> SchemaDiff {
    let find = |snapshot: &SchemaSnapshot, name: &str| {
        snapshot
            .tables
            .iter()
            .find(|table| table.name == name)
            .cloned()
    };
    let mut result = SchemaDiff::default();
    for table in &source.tables {
        let Some(existing) = find(target, &table.name) else {
            result.missing.push(table.name.clone());
            continue;
        };
        let mut change = TableChange {
            table: table.name.clone(),
            ..TableChange::default()
        };
        for column in &table.columns {
            match existing
                .columns
                .iter()
                .find(|other| other.name == column.name)
            {
                None => change.added.push(column.clone()),
                Some(other) if other != column => change.changed.push(ColumnChange {
                    before: other.clone(),
                    after: column.clone(),
                }),
                Some(_) => {}
            }
        }
        for column in &existing.columns {
            if !table.columns.iter().any(|other| other.name == column.name) {
                change.dropped.push(column.clone());
            }
        }
        if !(change.added.is_empty() && change.dropped.is_empty() && change.changed.is_empty()) {
            result.changed.push(change);
        }
    }
    for table in &target.tables {
        if find(source, &table.name).is_none() {
            result.extra.push(table.name.clone());
        }
    }
    result
}

/// Compares the schema at `source_path` on `source` with the one at `target_path` on
/// `target`, and writes the script that makes the target match. The drivers are of one engine;
/// they may be different connections.
pub async fn compare(
    source: &dyn Driver,
    source_path: &[SharedString],
    source_label: &str,
    target: &dyn Driver,
    target_path: &[SharedString],
    target_label: &str,
) -> anyhow::Result<String> {
    let source_snapshot = source.schema_snapshot(source_path).await?;
    let target_snapshot = target.schema_snapshot(target_path).await?;
    let difference = diff(&source_snapshot, &target_snapshot);
    let mut created = Vec::new();
    for table in &difference.missing {
        let mut table_path = source_path.to_vec();
        table_path.push(SharedString::from(table.clone()));
        let script = source.create_script(&table_path).await?;
        created.push((
            table.clone(),
            target.retarget_script(&script, source_path, target_path),
        ));
    }
    Ok(render(
        target,
        source_label,
        target_path,
        target_label,
        &difference,
        &created,
    ))
}

/// The full script for `diff`, with the CREATE script of each missing table in `created`
/// (already pointed at the target), written in `driver`'s dialect for the schema at `target`.
pub fn render(
    driver: &dyn Driver,
    source_label: &str,
    target: &[SharedString],
    target_label: &str,
    diff: &SchemaDiff,
    created: &[(String, String)],
) -> String {
    let comment = driver.comment_prefix();
    let mut script = format!(
        "{comment} Makes {target_label} match {source_label}.\n\
         {comment} Review before running: nothing here has been run. Statements that lose data\n\
         {comment} (dropping a table or a column) are commented out; enable them by hand.\n"
    );
    if diff.is_empty() {
        script.push_str(&format!(
            "{comment} The two schemas have the same tables and columns.\n"
        ));
        return script;
    }
    for (table, create) in created {
        script.push_str(&format!(
            "\n{comment} {table}: only in the source\n{create}"
        ));
        if !create.ends_with('\n') {
            script.push('\n');
        }
    }
    for change in &diff.changed {
        script.push_str(&format!("\n{comment} {}: columns differ\n", change.table));
        for statement in driver.alter_table(target, change) {
            script.push_str(&statement);
            script.push('\n');
        }
    }
    for table in &diff.extra {
        script.push_str(&format!("\n{comment} {table}: only in the target\n"));
        script.push_str(&format!("{comment} {}\n", driver.drop_table(target, table)));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: &str, nullable: bool) -> ColumnShape {
        ColumnShape {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable,
            default: None,
            extra: None,
        }
    }

    fn table(name: &str, columns: Vec<ColumnShape>) -> TableShape {
        TableShape {
            name: name.to_string(),
            columns,
        }
    }

    #[test]
    fn finds_tables_and_columns_on_either_side() {
        let source = SchemaSnapshot {
            tables: vec![
                table(
                    "orders",
                    vec![
                        column("id", "integer", false),
                        column("total", "numeric(12,2)", false),
                        column("note", "text", true),
                    ],
                ),
                table("customers", vec![column("id", "integer", false)]),
            ],
        };
        let target = SchemaSnapshot {
            tables: vec![
                table(
                    "orders",
                    vec![
                        column("id", "integer", false),
                        column("total", "numeric(10,2)", true),
                        column("legacy", "text", true),
                    ],
                ),
                table("audit", vec![column("id", "integer", false)]),
            ],
        };
        let result = diff(&source, &target);
        assert_eq!(result.missing, ["customers"]);
        assert_eq!(result.extra, ["audit"]);
        let [change] = result.changed.as_slice() else {
            panic!("expected one changed table: {result:#?}");
        };
        assert_eq!(change.table, "orders");
        assert_eq!(change.added, [column("note", "text", true)]);
        assert_eq!(change.dropped, [column("legacy", "text", true)]);
        assert_eq!(change.changed.len(), 1);
        assert_eq!(change.changed[0].after.data_type, "numeric(12,2)");

        assert!(diff(&source, &source).is_empty());
    }
}
