//! Plain-text rendering: padded GitHub-flavoured Markdown tables, which read
//! well in a terminal and can be pasted into documentation unchanged, plus the
//! number formats used in them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct Table {
    columns: Vec<(String, Align)>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(columns: &[(&str, Align)]) -> Self {
        Self {
            columns: columns
                .iter()
                .map(|(header, align)| (sanitize(header), *align))
                .collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row. Missing cells are left blank and surplus cells are dropped,
    /// so a row can never break the column structure.
    pub fn row<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut cells: Vec<String> = cells
            .into_iter()
            .map(|cell| sanitize(cell.as_ref()))
            .collect();
        cells.resize(self.columns.len(), String::new());
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self) -> String {
        let widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, (header, _))| {
                self.rows
                    .iter()
                    .map(|row| display_width(&row[index]))
                    .fold(display_width(header).max(3), usize::max)
            })
            .collect();
        let mut output = String::new();
        let layout = || self.columns.iter().zip(&widths);
        push_line(
            &mut output,
            layout().map(|((header, align), width)| pad(header, *width, *align)),
        );
        push_line(
            &mut output,
            layout().map(|((_, align), width)| match align {
                Align::Left => "-".repeat(*width),
                Align::Right => format!("{}:", "-".repeat(*width - 1)),
            }),
        );
        for row in &self.rows {
            push_line(
                &mut output,
                row.iter()
                    .zip(layout())
                    .map(|(cell, ((_, align), width))| pad(cell, *width, *align)),
            );
        }
        output
    }
}

fn push_line(output: &mut String, cells: impl Iterator<Item = String>) {
    output.push('|');
    for cell in cells {
        output.push(' ');
        output.push_str(&cell);
        output.push_str(" |");
    }
    output.push('\n');
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let fill = " ".repeat(width.saturating_sub(display_width(text)));
    match align {
        Align::Left => format!("{text}{fill}"),
        Align::Right => format!("{fill}{text}"),
    }
}

/// Keeps a cell on one line and inside its column.
fn sanitize(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '|' => clean.push_str("\\|"),
            character if character.is_control() => clean.push(' '),
            character => clean.push(character),
        }
    }
    clean
}

/// Terminal columns a string occupies, counting East Asian wide and fullwidth
/// characters as two. An approximation (combining marks are not treated
/// specially); it only affects padding, never the validity of the table.
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| match u32::from(character) {
            0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

/// A fraction in `0..=1` with three decimals, as in the project's documentation.
pub fn rate(value: f64) -> String {
    format!("{value:.3}")
}

/// Milliseconds with microsecond resolution, or `-` when there is no sample.
pub fn millis(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.3}"))
}

/// Like [`millis`], with a trailing `*` when the percentile was taken from
/// fewer samples than it needs.
pub fn percentile(value: Option<f64>, undersampled: bool) -> String {
    match value {
        Some(value) if undersampled => format!("{value:.3}*"),
        other => millis(other),
    }
}

/// A byte count in binary units.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if value < 1024 {
        return format!("{value} B");
    }
    let mut scaled = value as f64 / 1024.0;
    let mut unit = 0;
    while scaled >= 1024.0 && unit + 1 < UNITS.len() {
        scaled /= 1024.0;
        unit += 1;
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

/// A size given in KiB, as `/proc` reports memory, or `-` when unavailable.
pub fn kib(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| bytes(value.saturating_mul(1024)))
}

/// At most `max_chars` characters, with an ellipsis when something was cut.
pub fn shorten(text: &str, max_chars: usize) -> String {
    solascope_core::truncate_text(text, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_padded_markdown_table() {
        let mut table = Table::new(&[
            ("variant", Align::Left),
            ("queries", Align::Right),
            ("top1", Align::Right),
        ]);
        assert!(table.is_empty());
        table.row(["lexical", "18", "1.000"]);
        table.row(["hybrid+rerank", "5", "0.800"]);
        assert!(!table.is_empty());
        assert_eq!(
            table.render(),
            "\
| variant       | queries |  top1 |
| ------------- | ------: | ----: |
| lexical       |      18 | 1.000 |
| hybrid+rerank |       5 | 0.800 |
"
        );
    }

    #[test]
    fn short_columns_keep_a_valid_delimiter_row() {
        let mut table = Table::new(&[("a", Align::Left), ("b", Align::Right)]);
        table.row(["1", "2"]);
        assert_eq!(
            table.render(),
            "| a   |   b |\n| --- | --: |\n| 1   |   2 |\n"
        );
        let header_only = Table::new(&[("only", Align::Left)]);
        assert_eq!(header_only.render(), "| only |\n| ---- |\n");
    }

    #[test]
    fn cells_cannot_break_rows_or_columns() {
        let mut table = Table::new(&[("query", Align::Left), ("n", Align::Right)]);
        table.row(["a | b\nc\td", "1", "surplus"]);
        table.row(["missing"]);
        assert_eq!(
            table.render(),
            "\
| query      |   n |
| ---------- | --: |
| a \\| b c d |   1 |
| missing    |     |
"
        );
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("查找用户"), 8);
        assert_eq!(display_width("a查b"), 4);
        let mut table = Table::new(&[("query", Align::Left), ("n", Align::Right)]);
        table.row(["查找用户", "1"]);
        table.row(["find a user", "2"]);
        let rendered = table.render();
        let widths: Vec<usize> = rendered.lines().map(display_width).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{rendered}"
        );
    }

    #[test]
    fn number_formats() {
        assert_eq!(rate(1.0), "1.000");
        assert_eq!(rate(2.0 / 3.0), "0.667");
        assert_eq!(millis(Some(21.4)), "21.400");
        assert_eq!(millis(None), "-");
        assert_eq!(percentile(Some(0.5), false), "0.500");
        assert_eq!(percentile(Some(0.5), true), "0.500*");
        assert_eq!(percentile(None, true), "-");
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(69_632), "68.0 KiB");
        assert_eq!(bytes(108_003_328), "103.0 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(kib(Some(6_524)), "6.4 MiB");
        assert_eq!(kib(None), "-");
        assert_eq!(shorten("abcdef", 3), "abc…");
        assert_eq!(shorten("abc", 3), "abc");
    }
}
