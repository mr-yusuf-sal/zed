use crate::driver::ResultSet;
use tabular_data_preview::types::{LineNumber, TableCell, TableLikeContent};
use ui::{SharedString, table_row::TableRow};

/// Zed already has a virtualised, filterable grid behind [`TableLikeContent`], built for the
/// CSV preview. Query results are the same shape, so they are converted into it rather than
/// given a second grid implementation.
///
/// Every cell is `TableCell::Generated`, the variant with no backing text buffer — the CSV
/// preview uses buffer anchors to map a cell back to the file it came from, and a result set
/// has no file.
///
/// Only the rows at `rows` are included, in that order: the query view passes the ones its
/// search matched. The gutter keeps numbering them by their place in the whole result.
pub fn to_table_content(result: &ResultSet, rows: &[usize]) -> TableLikeContent {
    let number_of_cols = result.columns.len();

    let headers = TableRow::from_vec(
        result
            .columns
            .iter()
            .map(|name| TableCell::Generated(name.clone()))
            .collect(),
        number_of_cols,
    );

    let line_numbers = rows
        .iter()
        .map(|index| LineNumber::Line(index + 1))
        .collect();
    let rows = rows
        .iter()
        .filter_map(|index| result.rows.get(*index))
        .map(|row| {
            TableRow::from_vec(
                (0..number_of_cols)
                    .map(|index| match row.get(index) {
                        // A NULL has to read differently from an empty string, or a column of
                        // blanks says nothing about which rows actually have no value.
                        Some(None) => TableCell::Generated(NULL.clone()),
                        Some(Some(value)) => TableCell::Generated(value.clone()),
                        // The row was shorter than the header; pad rather than drop the row.
                        None => TableCell::Virtual,
                    })
                    .collect(),
                number_of_cols,
            )
        })
        .collect::<Vec<_>>();

    TableLikeContent {
        number_of_cols,
        headers,
        rows,
        line_numbers,
    }
}

/// Adds `page`, the rows that follow `result`'s in the same table, to the end of `result`.
///
/// Columns are matched by name rather than by position, because MongoDB builds each page's
/// columns from the documents in it: a later page can list the fields in another order or bring
/// fields no earlier document had. Those go after the existing columns, so the indexes in
/// `result.edit_target` keep naming the same columns, and the rows already there read NULL in
/// them. A name that appears twice is matched occurrence by occurrence.
pub fn append_page(result: &mut ResultSet, page: ResultSet) {
    // Documents stay only while every row has its own; otherwise they no longer line up.
    if result.documents.len() == result.rows.len() && page.documents.len() == page.rows.len() {
        result.documents.extend(page.documents);
    } else {
        result.documents.clear();
    }
    let mut matched = vec![false; result.columns.len()];
    let mut positions = Vec::with_capacity(page.columns.len());
    for column in page.columns {
        let existing = result
            .columns
            .iter()
            .zip(&matched)
            .position(|(name, matched)| !matched && *name == column);
        let position = existing.unwrap_or_else(|| {
            result.columns.push(column);
            matched.push(false);
            result.columns.len() - 1
        });
        if let Some(matched) = matched.get_mut(position) {
            *matched = true;
        }
        positions.push(position);
    }

    let width = result.columns.len();
    for row in &mut result.rows {
        if row.len() < width {
            row.resize(width, None);
        }
    }
    result.rows.extend(page.rows.into_iter().map(|cells| {
        let mut row = vec![None; width];
        for (cell, &position) in cells.into_iter().zip(&positions) {
            if let Some(slot) = row.get_mut(position) {
                *slot = cell;
            }
        }
        row
    }));
    result.truncated = page.truncated;
    // Notices describe the object as it is now, such as a Redis key's time to live.
    result.notices = page.notices;
}

/// Row `row` as indented JSON for the record view: the document it was read as when the driver
/// kept one, or else an object of its columns. A cell holding a JSON object or array, which is
/// how nested values are shown, is embedded as JSON rather than as a string of it.
pub fn row_json(result: &ResultSet, row: usize) -> Option<String> {
    if let Some(Some(document)) = result.documents.get(row) {
        return Some(document.to_string());
    }
    let cells = result.rows.get(row)?;
    let object: serde_json::Map<String, serde_json::Value> = result
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let value = match cells.get(index) {
                Some(Some(text)) if text.starts_with(['{', '[']) => {
                    serde_json::from_str(text).unwrap_or_else(|_| text.to_string().into())
                }
                Some(Some(text)) => serde_json::Value::String(text.to_string()),
                _ => serde_json::Value::Null,
            };
            (column.to_string(), value)
        })
        .collect();
    serde_json::to_string_pretty(&object).ok()
}

/// The rows with a cell containing `query`, ignoring case. NULL is matched by the word it is
/// shown as, so searching for "null" finds the rows that have one.
pub fn matching_rows(result: &ResultSet, query: &str) -> Vec<usize> {
    let query = fold_case(query.trim());
    if query.is_empty() {
        return (0..result.rows.len()).collect();
    }
    result
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.iter().any(|cell| match cell {
                Some(value) => fold_case(value).contains(&query),
                None => "null".contains(&query),
            })
        })
        .map(|(index, _)| index)
        .collect()
}

/// Lowercases for a search that ignores case the way a Turkish reader expects: `İ` lowercases
/// to `i` plus a combining dot, which is dropped, and dotless `ı` is matched as `i`, so
/// "izmir" finds "İZMİR" and "Izmir" alike.
fn fold_case(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|character| *character != '\u{307}')
        .map(|character| if character == 'ı' { 'i' } else { character })
        .collect()
}

// A single shared value rather than one allocation per NULL cell; a wide result set is mostly
// NULLs often enough for this to matter.
static NULL: std::sync::LazyLock<SharedString> =
    std::sync::LazyLock::new(|| SharedString::from("NULL"));

/// RFC 4180 CSV. A NULL becomes an empty field — CSV has no way to tell it from an empty string,
/// and inventing a `NULL` marker would turn into the literal text "NULL" in whatever reads it.
pub fn to_csv(result: &ResultSet) -> String {
    let mut output = String::new();
    write_csv_row(
        &mut output,
        result.columns.iter().map(|column| Some(column.as_ref())),
    );
    for row in &result.rows {
        write_csv_row(
            &mut output,
            (0..result.columns.len()).map(|index| row.get(index).and_then(|cell| cell.as_deref())),
        );
    }
    output
}

fn write_csv_row<'a>(output: &mut String, cells: impl Iterator<Item = Option<&'a str>>) {
    for (index, cell) in cells.enumerate() {
        if index > 0 {
            output.push(',');
        }
        let Some(cell) = cell else {
            continue;
        };
        if cell.contains([',', '"', '\n', '\r']) {
            output.push('"');
            output.push_str(&cell.replace('"', "\"\""));
            output.push('"');
        } else {
            output.push_str(cell);
        }
    }
    output.push_str("\r\n");
}

/// An array of objects keyed by column name. Values stay strings because that is all a driver
/// hands back; guessing that "007" was a number would corrupt it.
pub fn to_json(result: &ResultSet) -> String {
    let rows: Vec<serde_json::Map<String, serde_json::Value>> = result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let value = match row.get(index) {
                        Some(Some(value)) => serde_json::Value::String(value.to_string()),
                        _ => serde_json::Value::Null,
                    };
                    (column.to_string(), value)
                })
                .collect()
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| String::from("[]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::EditTarget;

    fn cell(value: &str) -> Option<SharedString> {
        Some(SharedString::from(value.to_string()))
    }

    #[test]
    fn renders_nulls_distinctly_from_empty_strings() {
        let result = ResultSet {
            columns: vec![SharedString::from("a"), SharedString::from("b")],
            rows: vec![vec![None, cell("")]],
            ..Default::default()
        };
        let content = to_table_content(&result, &[0]);
        let row = &content.rows[0];
        assert_eq!(
            row.expect_get(0usize).display_value().unwrap().as_ref(),
            "NULL"
        );
        assert_eq!(row.expect_get(1usize).display_value().unwrap().as_ref(), "");
    }

    #[test]
    fn exports_csv_and_json() {
        let result = ResultSet {
            columns: vec![SharedString::from("name"), SharedString::from("note")],
            rows: vec![
                vec![cell("Ayşe"), cell("says \"hi\", twice")],
                vec![cell("Can"), None],
            ],
            ..Default::default()
        };
        assert_eq!(
            to_csv(&result),
            "name,note\r\nAyşe,\"says \"\"hi\"\", twice\"\r\nCan,\r\n"
        );
        let json: serde_json::Value = serde_json::from_str(&to_json(&result)).unwrap();
        assert_eq!(json[1]["note"], serde_json::Value::Null);
        assert_eq!(json[0]["name"], "Ayşe");
    }

    #[test]
    fn searches_every_cell_and_keeps_row_numbers() {
        let result = ResultSet {
            columns: vec![SharedString::from("name"), SharedString::from("city")],
            rows: vec![
                vec![cell("Ayşe"), cell("İzmir")],
                vec![cell("Can"), None],
                vec![cell("Deniz"), cell("izmit")],
            ],
            ..Default::default()
        };
        assert_eq!(matching_rows(&result, "  IZM "), vec![0, 2]);
        assert_eq!(matching_rows(&result, "ayşe"), vec![0]);
        assert_eq!(matching_rows(&result, "null"), vec![1]);
        assert_eq!(matching_rows(&result, ""), vec![0, 1, 2]);
        let content = to_table_content(&result, &[2]);
        assert_eq!(content.rows.len(), 1);
        assert!(matches!(
            content.line_numbers.as_slice(),
            [LineNumber::Line(3)]
        ));
    }

    #[test]
    fn appends_a_page_by_column_name() {
        let target = EditTarget {
            path: vec![SharedString::from("app"), SharedString::from("users")],
            key_columns: vec![0],
            read_only_columns: vec![0],
            insert_columns: Vec::new(),
        };
        let mut result = ResultSet {
            columns: vec![SharedString::from("_id"), SharedString::from("name")],
            rows: vec![vec![cell("1"), cell("Ayşe")], vec![cell("2"), None]],
            truncated: true,
            notices: vec![SharedString::from("ttl 10s")],
            edit_target: Some(target.clone()),
            ..Default::default()
        };
        // The next page's documents list their fields in another order and bring a new one.
        let page = ResultSet {
            columns: vec![
                SharedString::from("name"),
                SharedString::from("_id"),
                SharedString::from("nickname"),
            ],
            rows: vec![vec![cell("Can"), cell("3"), cell("Cano")]],
            truncated: false,
            notices: vec![SharedString::from("ttl 9s")],
            edit_target: Some(EditTarget {
                key_columns: vec![1],
                read_only_columns: vec![1],
                ..target.clone()
            }),
            ..Default::default()
        };
        append_page(&mut result, page);

        assert_eq!(
            result.columns,
            vec![
                SharedString::from("_id"),
                SharedString::from("name"),
                SharedString::from("nickname"),
            ]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![cell("1"), cell("Ayşe"), None],
                vec![cell("2"), None, None],
                vec![cell("3"), cell("Can"), cell("Cano")],
            ]
        );
        assert_eq!(result.edit_target, Some(target));
        assert!(!result.truncated);
        assert_eq!(result.notices, vec![SharedString::from("ttl 9s")]);
    }

    #[test]
    fn appends_repeated_column_names_in_order() {
        let mut result = ResultSet {
            columns: vec![SharedString::from("id"), SharedString::from("id")],
            rows: vec![vec![cell("1"), cell("a")]],
            ..Default::default()
        };
        let page = ResultSet {
            columns: vec![SharedString::from("id"), SharedString::from("id")],
            rows: vec![vec![cell("2"), cell("b")]],
            ..Default::default()
        };
        append_page(&mut result, page);
        assert_eq!(result.columns.len(), 2);
        assert_eq!(
            result.rows,
            vec![vec![cell("1"), cell("a")], vec![cell("2"), cell("b")]]
        );
    }

    #[test]
    fn appending_an_empty_last_page_only_ends_the_result() {
        let mut result = ResultSet {
            columns: vec![SharedString::from("member")],
            rows: vec![vec![cell("a")]],
            truncated: true,
            ..Default::default()
        };
        append_page(&mut result, ResultSet::default());
        assert_eq!(result.columns, vec![SharedString::from("member")]);
        assert_eq!(result.rows, vec![vec![cell("a")]]);
        assert!(!result.truncated);
    }

    #[test]
    fn shows_a_row_as_its_document_or_as_an_object() {
        let mut result = ResultSet {
            columns: vec![SharedString::from("name"), SharedString::from("tags")],
            rows: vec![
                vec![cell("Ayşe"), cell("[\"a\",\"b\"]")],
                vec![cell("Can"), None],
            ],
            documents: vec![Some(SharedString::from("{\n  \"kept\": 1\n}")), None],
            ..Default::default()
        };
        assert_eq!(row_json(&result, 0).unwrap(), "{\n  \"kept\": 1\n}");
        let built: serde_json::Value =
            serde_json::from_str(&row_json(&result, 1).unwrap()).unwrap();
        assert_eq!(built["name"], "Can");
        assert_eq!(built["tags"], serde_json::Value::Null);
        result.documents.clear();
        let built: serde_json::Value =
            serde_json::from_str(&row_json(&result, 0).unwrap()).unwrap();
        assert_eq!(built["tags"][1], "b");
        assert!(row_json(&result, 5).is_none());
    }

    #[test]
    fn pads_a_short_row_instead_of_dropping_it() {
        let result = ResultSet {
            columns: vec![SharedString::from("a"), SharedString::from("b")],
            rows: vec![vec![cell("only one")]],
            ..Default::default()
        };
        let content = to_table_content(&result, &[0]);
        assert_eq!(content.rows.len(), 1);
        assert_eq!(content.rows[0].cols(), 2);
    }
}
