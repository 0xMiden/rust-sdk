#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::Duration;

use comfy_table::{Attribute, Cell, ContentArrangement, Table, presets};

use crate::metrics::BenchmarkResult;

/// Creates a dynamic table with bold headers (same style as miden-cli)
fn create_dynamic_table(headers: &[&str]) -> Table {
    let header_cells = headers
        .iter()
        .map(|header| Cell::new(header).add_attribute(Attribute::Bold))
        .collect::<Vec<_>>();

    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth)
        .set_header(header_cells);

    table
}

/// Prints benchmark results as a pretty table
pub fn print_results(results: &[BenchmarkResult], title: &str, total_duration: Duration) {
    println!();

    let mut table = create_dynamic_table(&[title, "Mean", "Min", "Max"]);

    for result in results {
        let mut row = vec![
            result.name.clone(),
            format_duration(result.mean()),
            format_duration(result.min()),
            format_duration(result.max()),
        ];

        // Add output size info to the benchmark name if present
        if let Some(size) = result.output_size {
            row[0] = format!("{}\n  Output: {}", result.name, format_size(size));
        }

        table.add_row(row);
    }

    println!("{table}");

    // Summary line
    println!(
        "\nTotal benchmarks: {} | Total time: {}",
        results.len(),
        format_duration(total_duration)
    );
}

// SCALING RESULTS
// ================================================================================================

/// The measurements taken at one point of a scaling sweep.
pub struct ScalingPoint {
    /// Column header, naming the input size the measurements ran against.
    pub label: String,
    /// One result per measured operation.
    pub results: Vec<BenchmarkResult>,
}

/// Prints a scaling sweep as a table: one row per operation, one column per size, and the growth
/// from the first size to the last.
///
/// The growth column is what the sweep is for. An operation served by an index stays near `1.00x`
/// no matter the size, while one that scans grows with it.
pub fn print_scaling_results(points: &[ScalingPoint], title: &str) {
    if points.is_empty() {
        return;
    }

    println!();

    // Rows follow the order of the first point, and an operation only measured at a later size is
    // appended when it first shows up.
    let mut operations: Vec<&str> = Vec::new();
    for point in points {
        for result in &point.results {
            if !operations.contains(&result.name.as_str()) {
                operations.push(result.name.as_str());
            }
        }
    }

    let mut headers = vec![title];
    headers.extend(points.iter().map(|point| point.label.as_str()));
    headers.push("Growth");

    let mut table = create_dynamic_table(&headers);

    for operation in operations {
        let means: Vec<Option<Duration>> = points
            .iter()
            .map(|point| {
                point
                    .results
                    .iter()
                    .find(|result| result.name == operation)
                    .map(BenchmarkResult::mean)
            })
            .collect();

        let mut row = vec![operation.to_string()];
        row.extend(means.iter().map(|mean| mean.map_or_else(|| "-".to_string(), format_duration)));
        row.push(format_growth(&means));

        table.add_row(row);
    }

    println!("{table}");
}

/// Formats the ratio between the last and the first measurement of a row.
fn format_growth(means: &[Option<Duration>]) -> String {
    let measured: Vec<Duration> = means.iter().flatten().copied().collect();
    let (Some(first), Some(last)) = (measured.first(), measured.last()) else {
        return "-".to_string();
    };

    if first.is_zero() {
        return "-".to_string();
    }

    format!("{:.2}x", last.as_secs_f64() / first.as_secs_f64())
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    format!("{ms:.2}ms")
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.2} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
