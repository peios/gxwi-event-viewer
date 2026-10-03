//! A chart, drawn as the desktop's page can show it. No script of an app's
//! runs there and no inline style applies, so the lines are an SVG whose
//! geometry is all in its attributes, stretched over the chart, and their
//! colours are classes the stylesheet gives. Over them lies a column per
//! window, and the stylesheet alone shows, on the one the pointer is over,
//! every line's value in that window.

use libgxwi::escape;

/// One line: what it stands for, and what it read in each window, if it
/// read anything there.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub label: String,
    pub readings: Vec<Option<Reading>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    Value(f64),
    /// A percentile above the histogram's highest bucket: eventd knows it
    /// is higher, not what it is (PSPU §3.25).
    Overflow,
}

/// How a value is said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Plain,
    Bytes,
    Percent,
    PerSecond,
    BytesPerSecond,
}

/// How many colours the stylesheet has for lines; past them they repeat.
pub const COLOURS: usize = 8;

/// The most lines drawn on one chart. Past them, they are named as left
/// out, rather than drawn into a tangle nobody could read.
pub const MOST_LINES: usize = 12;

/// What one chart shows.
pub struct Plot<'a> {
    pub id: u64,
    /// Each window, said as a time, for the readout.
    pub windows: &'a [String],
    pub lines: &'a [Line],
    pub unit: Unit,
    /// The times at the chart's two ends.
    pub ends: (&'a str, &'a str),
}

/// The plotting area's height in the SVG's own units; its width is one
/// unit per window.
const HEIGHT: f64 = 100.0;

pub fn plot(plot: &Plot) -> String {
    let count = plot.windows.len();
    let drawn = &plot.lines[..plot.lines.len().min(MOST_LINES)];
    let values = || drawn.iter().flat_map(|line| line.readings.iter()).filter_map(|reading| match reading {
        Some(Reading::Value(value)) if value.is_finite() => Some(*value),
        _ => None,
    });
    let (Some(least), Some(most)) = (values().reduce(f64::min), values().reduce(f64::max)) else {
        return "<p class=\"no-samples\">Nothing was recorded in this range.</p>".into();
    };
    let (low, high) = match plot.unit {
        Unit::Bytes | Unit::BytesPerSecond => scale_bytes(least, most),
        _ => scale(least, most),
    };
    let y = |value: f64| HEIGHT - (value - low) / (high - low) * HEIGHT;

    let mut lines = String::new();
    for (index, line) in drawn.iter().enumerate() {
        let colour = index % COLOURS;
        let mut run: Vec<String> = Vec::new();
        let mut finish = |run: &mut Vec<String>| {
            if !run.is_empty() {
                lines += &format!("<polyline class=\"k{colour}\" points=\"{}\"/>", run.join(" "));
                run.clear();
            }
        };
        for (window, reading) in line.readings.iter().enumerate().take(count) {
            match reading {
                Some(Reading::Value(value)) if value.is_finite() => {
                    let point = format!("{:.2},{:.2}", window as f64 + 0.5, y(*value));
                    // A window alone between gaps is drawn as a dot: a line
                    // from the point to itself, whose round ends are one.
                    if run.is_empty() {
                        run.push(point.clone());
                    }
                    run.push(point);
                }
                _ => finish(&mut run),
            }
        }
        finish(&mut run);
    }
    let grid: String = [0.0, HEIGHT / 2.0, HEIGHT].iter().map(|at| format!("<line class=\"grid\" x1=\"0\" y1=\"{at}\" x2=\"{count}\" y2=\"{at}\"/>")).collect();
    let svg = format!(
        "<svg class=\"lines\" viewBox=\"0 0 {count} {HEIGHT}\" preserveAspectRatio=\"none\" overflow=\"visible\" aria-hidden=\"true\">{grid}{lines}</svg>"
    );

    let columns: String = plot
        .windows
        .iter()
        .enumerate()
        .map(|(window, said)| {
            let readings: String = drawn
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    let reading = match line.readings.get(window).copied().flatten() {
                        Some(Reading::Value(value)) => say(value, plot.unit),
                        Some(Reading::Overflow) => "above its highest bucket".into(),
                        None => "nothing".into(),
                    };
                    format!("<span class=\"k{}\"><b>{}</b> {}</span>", index % COLOURS, escape(&line.label), escape(&reading))
                })
                .collect();
            let side = if window * 2 >= count { " right" } else { "" };
            format!("<div class=\"col{side}\"><div class=\"tip\"><span class=\"at\">{}</span>{readings}</div></div>", escape(said))
        })
        .collect();

    let legend: String = drawn.iter().enumerate().map(|(index, line)| format!("<li class=\"k{}\">{}</li>", index % COLOURS, escape(&line.label))).collect();
    let left_out = match plot.lines.len().saturating_sub(MOST_LINES) {
        0 => String::new(),
        more => format!("<li class=\"left-out\">and {more} more not drawn: show them as one line, or narrow the metric</li>"),
    };
    let legend = if plot.lines.len() > 1 || !left_out.is_empty() { format!("<ul class=\"legend\">{legend}{left_out}</ul>") } else { String::new() };
    format!(
        "<div class=\"plot\" id=\"plot-{id}\"><div class=\"y\"><span>{high}</span><span>{middle}</span><span>{low}</span></div>\
         <div class=\"area\">{svg}<div class=\"cols\">{columns}</div></div>\
         <div class=\"x\"><span>{start}</span><span>{end}</span></div></div>{legend}",
        id = plot.id,
        high = escape(&say(high, plot.unit)),
        middle = escape(&say((high + low) / 2.0, plot.unit)),
        low = escape(&say(low, plot.unit)),
        start = escape(plot.ends.0),
        end = escape(plot.ends.1),
    )
}

/// The range the chart's height stands for: from zero, or below it when a
/// value is, up to a round number at or above the largest.
fn scale(least: f64, most: f64) -> (f64, f64) {
    let low = if least < 0.0 { -round_up(-least) } else { 0.0 };
    let high = if most > 0.0 { round_up(most) } else { 0.0 };
    if high > low { (low, high) } else { (low, low + 1.0) }
}

/// As `scale`, but round in the binary unit the values are said in, so
/// that the top of a chart of bytes is 8 MiB rather than 9.54 MiB.
fn scale_bytes(least: f64, most: f64) -> (f64, f64) {
    let size = least.abs().max(most.abs());
    let unit = [1_099_511_627_776.0, 1_073_741_824.0, 1_048_576.0, 1024.0].into_iter().find(|unit| size >= *unit).unwrap_or(1.0);
    let (low, high) = scale(least / unit, most / unit);
    (low * unit, high * unit)
}

/// The least of 1, 2, 2.5 and 5 times a power of ten that is at least
/// `value`, which is above zero.
fn round_up(value: f64) -> f64 {
    let power = 10f64.powf(value.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|step| step * power).find(|candidate| *candidate >= value * (1.0 - 1e-12)).unwrap_or(10.0 * power)
}

/// A value as a person reads it, in its unit.
pub fn say(value: f64, unit: Unit) -> String {
    match unit {
        Unit::Plain => number(value),
        Unit::PerSecond => format!("{}/s", number(value)),
        Unit::Percent => format!("{}%", number(value)),
        Unit::Bytes => bytes(value),
        Unit::BytesPerSecond => format!("{}/s", bytes(value)),
    }
}

/// A number to three significant figures, with k, M, G or T for the large
/// ones; a whole number below ten thousand as it is.
fn number(value: f64) -> String {
    let size = value.abs();
    if size == value.trunc().abs() && size < 10_000.0 {
        return format!("{value:.0}");
    }
    for (step, suffix) in [(1e12, "T"), (1e9, "G"), (1e6, "M"), (1e3, "k")] {
        if size >= step {
            return format!("{}{suffix}", significant(value / step));
        }
    }
    significant(value)
}

fn bytes(value: f64) -> String {
    let size = value.abs();
    for (step, suffix) in [(1_099_511_627_776.0, "TiB"), (1_073_741_824.0, "GiB"), (1_048_576.0, "MiB"), (1024.0, "KiB")] {
        if size >= step {
            return format!("{} {suffix}", significant(value / step));
        }
    }
    format!("{} B", number(value))
}

/// Three significant figures, without trailing zeros.
fn significant(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let places = (2 - value.abs().log10().floor() as i32).clamp(0, 6) as usize;
    let text = format!("{value:.places$}");
    if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(label: &str, readings: &[Option<f64>]) -> Line {
        Line { label: label.into(), readings: readings.iter().map(|reading| reading.map(Reading::Value)).collect() }
    }

    fn windows(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("w{index}")).collect()
    }

    #[test]
    fn numbers_are_said_to_three_figures_in_their_unit() {
        assert_eq!(say(0.0, Unit::Plain), "0");
        assert_eq!(say(42.0, Unit::Plain), "42");
        assert_eq!(say(9_999.0, Unit::Plain), "9999");
        assert_eq!(say(12_345.0, Unit::Plain), "12.3k");
        assert_eq!(say(2_624_820.0, Unit::Plain), "2.62M");
        assert_eq!(say(1.998_131_85, Unit::PerSecond), "2/s");
        assert_eq!(say(0.012_34, Unit::PerSecond), "0.0123/s");
        assert_eq!(say(201_152.0, Unit::Bytes), "196 KiB");
        assert_eq!(say(512.0, Unit::Bytes), "512 B");
        assert_eq!(say(3.5 * 1_048_576.0, Unit::BytesPerSecond), "3.5 MiB/s");
        assert_eq!(say(75.0, Unit::Percent), "75%");
        assert_eq!(say(-2.5, Unit::Plain), "-2.5");
    }

    #[test]
    fn the_height_runs_from_zero_to_a_round_number() {
        assert_eq!(scale(3.0, 7.3), (0.0, 10.0));
        assert_eq!(scale(0.0, 201_152.0), (0.0, 250_000.0));
        assert_eq!(scale(-3.0, 4.0), (-5.0, 5.0));
        assert_eq!(scale(0.0, 0.0), (0.0, 1.0));
        assert_eq!(scale(5.0, 5.0), (0.0, 5.0));
        assert_eq!(scale_bytes(0.0, 7.0 * 1_048_576.0), (0.0, 10.0 * 1_048_576.0));
        assert_eq!(say(scale_bytes(0.0, 4.4 * 1_048_576.0).1, Unit::Bytes), "5 MiB");
        assert_eq!(scale_bytes(0.0, 700.0), (0.0, 1000.0));
    }

    #[test]
    fn gaps_break_a_line_and_a_lone_window_is_a_dot() {
        let lines = [line("a", &[Some(0.0), Some(10.0), None, Some(5.0)])];
        let html = plot(&Plot { id: 1, windows: &windows(4), lines: &lines, unit: Unit::Plain, ends: ("start", "end") });
        assert!(html.contains("<polyline class=\"k0\" points=\"0.50,100.00 0.50,100.00 1.50,0.00\"/>"), "{html}");
        assert!(html.contains("<polyline class=\"k0\" points=\"3.50,50.00 3.50,50.00\"/>"), "{html}");
        assert!(html.contains("viewBox=\"0 0 4 100\""));
        // One line has no legend; its readout still names it.
        assert!(!html.contains("legend"));
        assert!(html.contains("<b>a</b> nothing"));
    }

    #[test]
    fn every_window_says_every_line() {
        let lines = [line("store=logs", &[Some(1024.0), None]), line("store=events", &[Some(2048.0), Some(4096.0)])];
        let html = plot(&Plot { id: 2, windows: &windows(2), lines: &lines, unit: Unit::Bytes, ends: ("a", "b") });
        assert!(html.contains("<div class=\"col\"><div class=\"tip\"><span class=\"at\">w0</span><span class=\"k0\"><b>store=logs</b> 1 KiB</span><span class=\"k1\"><b>store=events</b> 2 KiB</span></div></div>"), "{html}");
        assert!(html.contains("<div class=\"col right\">"));
        assert!(html.contains("<li class=\"k1\">store=events</li>"));
    }

    #[test]
    fn too_many_lines_are_named_as_left_out_and_nothing_is_said_so() {
        let lines: Vec<Line> = (0..15).map(|index| line(&format!("core={index}"), &[Some(f64::from(index))])).collect();
        let html = plot(&Plot { id: 3, windows: &windows(1), lines: &lines, unit: Unit::Plain, ends: ("a", "b") });
        assert!(html.contains("and 3 more not drawn"));
        assert_eq!(html.matches("<polyline").count(), MOST_LINES);
        let empty = plot(&Plot { id: 4, windows: &windows(3), lines: &[line("a", &[None, None, None])], unit: Unit::Plain, ends: ("a", "b") });
        assert!(empty.contains("Nothing was recorded"));
    }

    #[test]
    fn an_overflowing_percentile_breaks_the_line_and_says_so() {
        let lines = [Line { label: "p99".into(), readings: vec![Some(Reading::Value(1.0)), Some(Reading::Overflow), Some(Reading::Value(2.0))] }];
        let html = plot(&Plot { id: 5, windows: &windows(3), lines: &lines, unit: Unit::Plain, ends: ("a", "b") });
        assert_eq!(html.matches("<polyline").count(), 2);
        assert!(html.contains("above its highest bucket"));
    }
}
