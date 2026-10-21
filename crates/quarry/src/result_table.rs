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
