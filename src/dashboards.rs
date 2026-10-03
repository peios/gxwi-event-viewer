//! A person's dashboards: named pages of charts, each over a range of time.
//!
//! They are the person's own, kept at `CurrentUser\Software\gxwi-event-
//! viewer`, value `Dashboards`, a REG_MULTI_SZ of lines, as other GXWI
//! programs keep what is a person's. A `dashboard` line starts each, and a
//! `chart` line follows for each of its charts:
//!
//! ```text
//! dashboard 1h eventd's health
//! chart eventd.events.stored rate sum one
//! chart eventd.store.bytes none max each
//! ```
//!
//! Until they have saved one, they are shown eventd's own health. What in
//! the value cannot be read is left out, and the rest is kept.

use std::time::Duration;

use eventd_client::text;
use peios::registry::{CreateFlags, Key, KeyAccess, OpenFlags, ValueType};

const PARENT: &str = "CurrentUser\\Software";
const CHILD: &str = "gxwi-event-viewer";
const OWN: &str = "CurrentUser\\Software\\gxwi-event-viewer";
const NAME: &str = "Dashboards";
const ENOENT: i32 = 2;

/// The most dashboards, and charts on one, that are kept.
pub const MOST_DASHBOARDS: usize = 64;
pub const MOST_CHARTS: usize = 32;

/// How far back a dashboard looks, and how wide its windows are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Minutes15,
    Hour,
    Hours6,
    Day,
    Week,
}

impl Span {
    pub const ALL: [Span; 5] = [Span::Minutes15, Span::Hour, Span::Hours6, Span::Day, Span::Week];

    pub fn name(self) -> &'static str {
        match self {
            Span::Minutes15 => "15m",
            Span::Hour => "1h",
            Span::Hours6 => "6h",
            Span::Day => "24h",
            Span::Week => "7d",
        }
    }

    pub fn named(name: &str) -> Option<Span> {
        Span::ALL.into_iter().find(|span| span.name() == name)
    }

    pub fn words(self) -> &'static str {
        match self {
            Span::Minutes15 => "Last 15 minutes",
            Span::Hour => "Last hour",
            Span::Hours6 => "Last 6 hours",
            Span::Day => "Last 24 hours",
            Span::Week => "Last 7 days",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            Span::Minutes15 => 15 * 60,
            Span::Hour => 3_600,
            Span::Hours6 => 6 * 3_600,
            Span::Day => 86_400,
            Span::Week => 7 * 86_400,
        }
    }

    /// Each window's width: some sixty to a hundred windows to a chart.
    pub fn window_seconds(self) -> i64 {
        match self {
            Span::Minutes15 => 15,
            Span::Hour => 60,
            Span::Hours6 => 5 * 60,
            Span::Day => 15 * 60,
            Span::Week => 2 * 3_600,
        }
    }

    /// How often the charts are asked for again: about a window's width,
    /// and never more often than every five seconds or less than a minute.
    pub fn refresh_seconds(self) -> u64 {
        self.window_seconds().clamp(5, 60).unsigned_abs()
    }
}

/// What is done to each series' samples before they are charted (PSPU
/// §3.25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// The samples as recorded.
    None,
    Rate,
    Delta,
    P50,
    P95,
    P99,
}

impl Transform {
    pub const ALL: [Transform; 6] = [Transform::None, Transform::Rate, Transform::Delta, Transform::P50, Transform::P95, Transform::P99];

    pub fn name(self) -> &'static str {
        match self {
            Transform::None => "none",
            Transform::Rate => "rate",
            Transform::Delta => "delta",
            Transform::P50 => "p50",
            Transform::P95 => "p95",
            Transform::P99 => "p99",
        }
    }

    pub fn named(name: &str) -> Option<Transform> {
        Transform::ALL.into_iter().find(|transform| transform.name() == name)
    }

    /// Its keyword in a query, if it has one.
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            Transform::None => None,
            Transform::Rate => Some("RATE"),
            Transform::Delta => Some("DELTA"),
            Transform::P50 => Some("P50"),
            Transform::P95 => Some("P95"),
            Transform::P99 => Some("P99"),
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Transform::None => "As recorded",
            Transform::Rate => "Rate per second",
            Transform::Delta => "Change in each window",
            Transform::P50 => "Median (P50)",
            Transform::P95 => "95th percentile",
            Transform::P99 => "99th percentile",
        }
    }

    /// Which metric types it applies to: RATE and DELTA to counters, the
    /// percentiles to histograms, and none to counters and gauges.
    pub fn fits(self, metric_type: &str) -> bool {
        match self {
            Transform::None => matches!(metric_type, "counter" | "gauge"),
            Transform::Rate | Transform::Delta => metric_type == "counter",
            Transform::P50 | Transform::P95 | Transform::P99 => metric_type == "histogram",
        }
    }

    /// What a new chart of a metric of this type shows: a counter's rate,
    /// a gauge as it is, a histogram's 95th percentile.
    pub fn first(metric_type: &str) -> Transform {
        match metric_type {
            "counter" => Transform::Rate,
            "histogram" => Transform::P95,
            _ => Transform::None,
        }
    }
}

/// How each window's samples, and the lines, are brought to one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    Avg,
    Min,
    Max,
    Sum,
}

impl Function {
    pub const ALL: [Function; 4] = [Function::Avg, Function::Min, Function::Max, Function::Sum];

    pub fn name(self) -> &'static str {
        match self {
            Function::Avg => "avg",
            Function::Min => "min",
            Function::Max => "max",
            Function::Sum => "sum",
        }
    }

    pub fn named(name: &str) -> Option<Function> {
        Function::ALL.into_iter().find(|function| function.name() == name)
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Function::Avg => "AVG_OVER",
            Function::Min => "MIN_OVER",
            Function::Max => "MAX_OVER",
            Function::Sum => "SUM_OVER",
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Function::Avg => "Average",
            Function::Min => "Least",
            Function::Max => "Greatest",
            Function::Sum => "Total",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    /// The metric's name, which is an identifier (§3.10).
    pub metric: String,
    pub transform: Transform,
    pub function: Function,
    /// One line for every series together, rather than one each.
    pub combined: bool,
}

impl Chart {
    /// A chart of `metric`, of type `metric_type`, as it is first added.
    pub fn of(metric: &str, metric_type: &str) -> Chart {
        let transform = Transform::first(metric_type);
        Chart { metric: metric.into(), transform, function: if transform == Transform::Rate { Function::Sum } else { Function::Avg }, combined: false }
    }

    /// The query for this chart over `span`: one value per window, for
    /// each series or for them all.
    pub fn query(&self, span: Span) -> String {
        let brackets = if self.combined { "" } else { "[]" };
        let transform = self.transform.keyword().map(|keyword| format!(" {keyword}")).unwrap_or_default();
        let said = |seconds: i64| text::duration(Duration::from_secs(seconds.unsigned_abs())).unwrap_or_else(|| format!("{seconds}s"));
        format!(
            "METRIC {}{brackets}{transform} SINCE {} ago {} {}",
            self.metric,
            said(span.seconds()),
            self.function.keyword(),
            said(span.window_seconds())
        )
    }

    fn line(&self) -> String {
        format!("chart {} {} {} {}", self.metric, self.transform.name(), self.function.name(), if self.combined { "one" } else { "each" })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dashboard {
    pub name: String,
    pub span: Span,
    pub charts: Vec<Chart>,
}

impl Dashboard {
    pub fn new(name: &str) -> Dashboard {
        Dashboard { name: name.into(), span: Span::Hour, charts: Vec::new() }
    }
}

/// What a person is shown before they have saved anything: how eventd
/// itself is doing (eventd TRM §5.7).
pub fn health() -> Dashboard {
    let chart = |metric: &str, transform, function, combined| Chart { metric: metric.into(), transform, function, combined };
    Dashboard {
        name: "eventd's health".into(),
        span: Span::Hour,
        charts: vec![
            chart("eventd.events.stored", Transform::Rate, Function::Sum, true),
            chart("eventd.logs.stored", Transform::Rate, Function::Sum, true),
            chart("eventd.events.lost", Transform::Delta, Function::Sum, false),
            chart("eventd.kmes.ring.fill.percent", Transform::None, Function::Max, false),
            chart("eventd.store.bytes", Transform::None, Function::Max, false),
            chart("eventd.queries.active", Transform::None, Function::Max, true),
            chart("eventd.queries.refused", Transform::Delta, Function::Sum, false),
            chart("eventd.metrics.stored", Transform::Rate, Function::Sum, true),
        ],
    }
}

/// The person's dashboards as lines.
pub fn lines(dashboards: &[Dashboard]) -> Vec<String> {
    let mut lines = Vec::new();
    for dashboard in dashboards {
        lines.push(format!("dashboard {} {}", dashboard.span.name(), dashboard.name));
        lines.extend(dashboard.charts.iter().map(Chart::line));
    }
    lines
}

/// Dashboards from lines, leaving out what cannot be read.
pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Dashboard> {
    let mut dashboards: Vec<Dashboard> = Vec::new();
    for line in lines {
        let mut words = line.splitn(3, ' ');
        match (words.next(), words.next(), words.next()) {
            (Some("dashboard"), Some(span), Some(name)) if !name.trim().is_empty() && dashboards.len() < MOST_DASHBOARDS => {
                if let Some(span) = Span::named(span) {
                    dashboards.push(Dashboard { name: name.trim().into(), span, charts: Vec::new() });
                }
            }
            (Some("chart"), Some(metric), Some(rest)) => {
                let Some(dashboard) = dashboards.last_mut() else { continue };
                let rest: Vec<&str> = rest.split(' ').collect();
                if let ([transform, function, combined], true) = (rest.as_slice(), text::is_identifier(metric))
                    && let (Some(transform), Some(function), Some(combined)) = (
                        Transform::named(transform),
                        Function::named(function),
                        match *combined {
                            "one" => Some(true),
                            "each" => Some(false),
                            _ => None,
                        },
                    )
                    && dashboard.charts.len() < MOST_CHARTS
                {
                    dashboard.charts.push(Chart { metric: metric.into(), transform, function, combined });
                }
            }
            _ => {}
        }
    }
    dashboards
}

/// The person's dashboards; eventd's health if they have none saved; and
/// why theirs could not be read, if they could not.
pub fn read() -> (Vec<Dashboard>, Option<String>) {
    let saved = Key::open(None, OWN, KeyAccess::QUERY_VALUE, OpenFlags::empty()).and_then(|key| key.query_value(NAME.as_bytes(), None));
    let value = match saved {
        Ok(value) => value,
        Err(error) if error.raw_os_error() == Some(ENOENT) => return (vec![health()], None),
        Err(error) => return (vec![health()], Some(format!("Your saved dashboards could not be read: {error}."))),
    };
    if value.ty != ValueType::MULTI_SZ {
        return (vec![health()], Some("Your saved dashboards are not a list of lines, so they were left as they are.".into()));
    }
    let text = String::from_utf8_lossy(&value.data);
    let dashboards = parse(text.split('\0'));
    if dashboards.is_empty() { (vec![health()], None) } else { (dashboards, None) }
}

/// Saves the person's dashboards, making their key if it is the first
/// time.
pub fn write(dashboards: &[Dashboard]) -> Result<(), String> {
    let (parent, _) = Key::create(None, PARENT, KeyAccess::CREATE_SUB_KEY, CreateFlags::empty(), None, None).map_err(|error| format!("{PARENT}: {error}"))?;
    let (own, _) = Key::create(Some(&parent), CHILD, KeyAccess::SET_VALUE, CreateFlags::empty(), None, None).map_err(|error| format!("{OWN}: {error}"))?;
    let mut bytes = Vec::new();
    for line in lines(dashboards) {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    own.set_value(NAME.as_bytes(), ValueType::MULTI_SZ, &bytes).call().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboards_are_read_as_they_were_written() {
        let mut second = Dashboard::new("Disks and things");
        second.span = Span::Week;
        second.charts.push(Chart::of("disk.read.bytes", "counter"));
        second.charts.push(Chart { metric: "request.duration".into(), transform: Transform::P99, function: Function::Max, combined: true });
        let dashboards = vec![health(), second];
        let lines = lines(&dashboards);
        assert_eq!(lines[0], "dashboard 1h eventd's health");
        assert_eq!(lines[1], "chart eventd.events.stored rate sum one");
        assert_eq!(parse(lines.iter().map(String::as_str)), dashboards);
    }

    #[test]
    fn what_cannot_be_read_is_left_out() {
        let read = parse([
            "chart before.any.dashboard rate sum one",
            "dashboard 1h Mine",
            "chart cpu.usage none avg each",
            "chart \"quoted\" none avg each",
            "chart cpu.usage sideways avg each",
            "chart cpu.usage none avg each extra",
            "dashboard forever Never",
            "dashboard 15m   ",
            "",
        ]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].charts, vec![Chart { metric: "cpu.usage".into(), transform: Transform::None, function: Function::Avg, combined: false }]);
    }

    #[test]
    fn a_chart_asks_for_one_value_per_window() {
        let mut chart = Chart::of("eventd.events.stored", "counter");
        assert_eq!(chart.query(Span::Hour), "METRIC eventd.events.stored[] RATE SINCE 1h ago SUM_OVER 1m");
        chart.combined = true;
        chart.transform = Transform::None;
        chart.function = Function::Max;
        assert_eq!(chart.query(Span::Minutes15), "METRIC eventd.events.stored SINCE 15m ago MAX_OVER 15s");
        assert_eq!(Chart::of("request.duration", "histogram").query(Span::Week), "METRIC request.duration[] P95 SINCE 7d ago AVG_OVER 2h");
    }

    #[test]
    fn transforms_fit_their_types() {
        assert!(Transform::Rate.fits("counter") && !Transform::Rate.fits("gauge"));
        assert!(Transform::None.fits("gauge") && !Transform::None.fits("histogram"));
        assert!(Transform::P95.fits("histogram") && !Transform::P95.fits("counter"));
        for span in Span::ALL {
            assert_eq!(Span::named(span.name()), Some(span));
            assert!((60..=100).contains(&(span.seconds() / span.window_seconds())), "{span:?}");
        }
    }
}
