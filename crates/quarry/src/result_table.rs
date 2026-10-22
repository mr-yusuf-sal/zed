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
pub fn to_table_content(result: &ResultSet) -> TableLikeContent {
    let number_of_cols = result.columns.len();

    let headers = TableRow::from_vec(
        result
            .columns
            .iter()
            .map(|name| TableCell::Generated(name.clone()))
            .collect(),
        number_of_cols,
    );

    let rows = result
        .rows
        .iter()
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

    // The grid shows these in its gutter. Results have no file, so they number the rows.
    let line_numbers = (0..rows.len())
        .map(|index| LineNumber::Line(index + 1))
        .collect();

    TableLikeContent {
        number_of_cols,
        headers,
        rows,
        line_numbers,
    }
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
        let content = to_table_content(&result);
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
    fn pads_a_short_row_instead_of_dropping_it() {
        let result = ResultSet {
            columns: vec![SharedString::from("a"), SharedString::from("b")],
            rows: vec![vec![cell("only one")]],
            ..Default::default()
        };
        let content = to_table_content(&result);
        assert_eq!(content.rows.len(), 1);
        assert_eq!(content.rows[0].cols(), 2);
    }
}
