//! The Metrics tab: every metric eventd keeps, with its latest values, and
//! the person's dashboards of charts, read again every few seconds.
//!
//! A chart is one query, for one value in each window of the dashboard's
//! range (PSPU §3.25): eventd folds the samples, so a week of them costs
//! it no more to answer than an hour. Metrics cannot be followed as logs
//! and events are (§3.27), so a thread asks again, as often as a window is
//! wide but never more than every five seconds, until the tab is left or
//! what it shows changes.
//!
//! What the person may not read eventd leaves out, and the tab says so, as
//! the others do, from the read policy checked as the person.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use eventd_client::access::{self, Namespace, Readable};
use eventd_client::{Error, Record, Value};
use libgxwi::{Fields, Surface, escape};

use crate::chart::{self, Line, Plot, Reading, Unit};
use crate::dashboards::{self, Chart, Dashboard, Function, Span, Transform};
use crate::viewer::{Viewer, trouble};
use crate::words::{self, Clock};

/// What a metric result carries beside its labels (PSPU §3.22).
const FIXED: [&str; 6] = ["timestamp", "boot_id", "name", "type", "value", "overflow"];

/// Every series of every type, at its latest; histograms by their 95th
/// percentile, since a histogram has no one value (§3.25).
const CATALOGUE: [(&str, &str); 3] = [
    ("counter", "METRIC *[type=\"counter\"]"),
    ("gauge", "METRIC *[type=\"gauge\"]"),
    ("histogram", "METRIC *[type=\"histogram\"] P95"),
];

/// A metric as the catalogue lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    pub metric_type: String,
    /// Each series' labels, latest reading, and when that was.
    pub series: Vec<(String, Option<Reading>, Option<i64>)>,
}

/// What a chart last read.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The start of the first window, and each one's width, in nanoseconds.
    first: i64,
    width: i64,
    lines: Vec<Line>,
    trouble: Option<String>,
}

pub struct Metrics {
    catalogue: Option<Result<Vec<Metric>, String>>,
    dashboards: Vec<Dashboard>,
    /// Which dashboard is shown.
    shown: usize,
    /// What each chart of it last read, in its order, and the query each
    /// was read with, so that a chart asked for again keeps showing what
    /// it had until the new answer comes.
    answers: Vec<Option<Answer>>,
    queries: Vec<String>,
    /// The chart whose settings are open.
    editing: Option<usize>,
    /// The person has asked to delete the shown dashboard, and is asked
    /// whether they mean it.
    deleting: bool,
    /// Why the dashboards could not be read or kept.
    keeping: Option<String>,
    readable: Option<Readable>,
    /// The dashboards have been read.
    read: bool,
    /// Changes whenever the charts must be asked for afresh, or no longer:
    /// the thread asking watches it.
    generation: Arc<AtomicU64>,
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics {
            catalogue: None,
            dashboards: Vec::new(),
            shown: 0,
            answers: Vec::new(),
            queries: Vec::new(),
            editing: None,
            deleting: false,
            keeping: None,
            readable: None,
            read: false,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn dashboard(&self) -> Option<&Dashboard> {
        self.dashboards.get(self.shown)
    }

    /// The tab is shown: the person's dashboards, the first time, and then
    /// every metric there is, what they may read, and the charts.
    pub fn open(&mut self, window: &Weak<Surface<Viewer>>, socket: &Path, fields: &mut Fields) {
        if !self.read {
            let (dashboards, trouble) = dashboards::read();
            self.dashboards = dashboards;
            self.keeping = trouble;
            self.read = true;
        }
        self.show(fields);
        self.reread(window, socket);
    }

    /// The tab is left: the charts are no longer asked for.
    pub fn close(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Reads everything again: the catalogue, the policy and the charts.
    pub fn reread(&mut self, window: &Weak<Surface<Viewer>>, socket: &Path) {
        self.read_catalogue(window, socket);
        self.read_policy(window);
        self.ask(window, socket);
    }

    /// Puts the shown dashboard in the fields.
    fn show(&mut self, fields: &mut Fields) {
        self.shown = self.shown.min(self.dashboards.len().saturating_sub(1));
        if let Some(dashboard) = self.dashboards.get(self.shown) {
            fields.set("board", &self.shown.to_string());
            fields.set("boardname", &dashboard.name);
            fields.set("span", dashboard.span.name());
        }
    }

    fn read_catalogue(&self, window: &Weak<Surface<Viewer>>, socket: &Path) {
        let Some(window) = window.upgrade() else { return };
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            let read = catalogue(&socket);
            window.update(|viewer, _| viewer.metrics.catalogue = Some(read));
        });
    }

    pub fn read_policy(&self, window: &Weak<Surface<Viewer>>) {
        let Some(window) = window.upgrade() else { return };
        std::thread::spawn(move || {
            let readable = access::readable(Namespace::Metrics);
            window.update(|viewer, _| viewer.metrics.readable = Some(readable));
        });
    }

    /// Asks for every chart of the shown dashboard now, and again every
    /// little while, until something changes.
    pub fn ask(&mut self, window: &Weak<Surface<Viewer>>, socket: &Path) {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(dashboard) = self.dashboards.get(self.shown) else {
            self.answers.clear();
            self.queries.clear();
            return;
        };
        let queries: Vec<String> = dashboard.charts.iter().map(|chart| chart.query(dashboard.span)).collect();
        self.answers = queries
            .iter()
            .map(|query| self.queries.iter().position(|asked| asked == query).and_then(|was| self.answers.get(was).cloned().flatten()))
            .collect();
        self.queries = queries;
        let Some(window) = window.upgrade() else { return };
        let window = Arc::downgrade(&window);
        let (span, charts) = (dashboard.span, dashboard.charts.clone());
        let (socket, generation) = (socket.to_path_buf(), self.generation.clone());
        std::thread::spawn(move || poll(&window, &generation, mine, &socket, span, &charts));
    }

    fn answered(&mut self, generation: u64, answers: Vec<Answer>) {
        if self.generation.load(Ordering::SeqCst) == generation {
            self.answers = answers.into_iter().map(Some).collect();
        }
    }

    /// Keeps the person's dashboards, saying so if they could not be kept.
    fn keep(&mut self) {
        self.keeping = dashboards::write(&self.dashboards).err().map(|why| format!("Your dashboards could not be saved: {why}."));
    }

    /// Adds a chart of `metric` to the shown dashboard.
    pub fn add(&mut self, metric: &str, metric_type: &str) {
        if !eventd_client::text::is_identifier(metric) {
            return;
        }
        let Some(dashboard) = self.dashboards.get_mut(self.shown) else { return };
        if dashboard.charts.len() >= dashboards::MOST_CHARTS {
            self.keeping = Some(format!("A dashboard holds at most {} charts.", dashboards::MOST_CHARTS));
            return;
        }
        dashboard.charts.push(Chart::of(metric, metric_type));
        self.keep();
    }

    /// Changes one chart: `set` is what the menu item says to do.
    pub fn change(&mut self, chart: usize, set: &str) {
        let Some(dashboard) = self.dashboards.get_mut(self.shown) else { return };
        if chart >= dashboard.charts.len() {
            return;
        }
        let (what, to) = set.split_once(':').unwrap_or((set, ""));
        match what {
            "transform" => {
                let Some(transform) = Transform::named(to) else { return };
                dashboard.charts[chart].transform = transform;
            }
            "function" => {
                let Some(function) = Function::named(to) else { return };
                dashboard.charts[chart].function = function;
            }
            "lines" => dashboard.charts[chart].combined = to == "one",
            "earlier" if chart > 0 => {
                dashboard.charts.swap(chart, chart - 1);
                self.editing = self.editing.map(|editing| if editing == chart { chart - 1 } else { editing });
            }
            "later" if chart + 1 < dashboard.charts.len() => {
                dashboard.charts.swap(chart, chart + 1);
                self.editing = self.editing.map(|editing| if editing == chart { chart + 1 } else { editing });
            }
            "remove" => {
                dashboard.charts.remove(chart);
                self.editing = None;
            }
            _ => return,
        }
        self.keep();
    }

    /// Opens or closes a chart's settings, putting its own in the fields.
    pub fn edit(&mut self, chart: usize, fields: &mut Fields) {
        if self.editing == Some(chart) {
            self.editing = None;
            return;
        }
        let Some(shown) = self.dashboard().and_then(|dashboard| dashboard.charts.get(chart)) else { return };
        fields.set("edit-transform", shown.transform.name());
        fields.set("edit-function", shown.function.name());
        fields.set("edit-lines", if shown.combined { "one" } else { "each" });
        self.editing = Some(chart);
    }

    /// One of the open settings changed.
    pub fn edited(&mut self, name: &str, fields: &Fields) {
        let Some(chart) = self.editing else { return };
        let set = match name {
            "edit-transform" => format!("transform:{}", fields.get(name)),
            "edit-function" => format!("function:{}", fields.get(name)),
            "edit-lines" => format!("lines:{}", fields.get(name)),
            _ => return,
        };
        self.change(chart, &set);
    }

    /// Asks whether the shown dashboard is to be deleted, or stops asking.
    pub fn ask_delete(&mut self, ask: bool) {
        self.deleting = ask;
    }

    pub fn switch(&mut self, fields: &mut Fields) {
        if let Ok(shown) = fields.get("board").parse::<usize>()
            && shown < self.dashboards.len()
        {
            self.shown = shown;
            self.editing = None;
            self.deleting = false;
        }
        self.show(fields);
    }

    pub fn span(&mut self, fields: &Fields) {
        let (Some(span), Some(dashboard)) = (Span::named(fields.get("span")), self.dashboards.get_mut(self.shown)) else { return };
        dashboard.span = span;
        self.keep();
    }

    pub fn rename(&mut self, fields: &mut Fields) {
        let name: String = fields.get("boardname").chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(80).collect();
        if let Some(dashboard) = self.dashboards.get_mut(self.shown)
            && !name.is_empty()
        {
            dashboard.name = name;
            self.keep();
        }
        self.show(fields);
    }

    pub fn create(&mut self, fields: &mut Fields) {
        if self.dashboards.len() >= dashboards::MOST_DASHBOARDS {
            self.keeping = Some(format!("You can keep at most {} dashboards.", dashboards::MOST_DASHBOARDS));
            return;
        }
        let name = (1..).map(|number| format!("Dashboard {number}")).find(|name| !self.dashboards.iter().any(|dashboard| &dashboard.name == name)).unwrap_or_default();
        self.dashboards.push(Dashboard::new(&name));
        self.shown = self.dashboards.len() - 1;
        self.editing = None;
        self.deleting = false;
        self.keep();
        self.show(fields);
    }

    /// Deletes the shown dashboard. The last one gone, eventd's health
    /// stands in again.
    pub fn delete(&mut self, fields: &mut Fields) {
        self.deleting = false;
        self.editing = None;
        if self.shown < self.dashboards.len() {
            self.dashboards.remove(self.shown);
        }
        if self.dashboards.is_empty() {
            self.dashboards.push(dashboards::health());
        }
        self.keep();
        self.show(fields);
    }

    /// The bar's second line: which dashboard, its name and its range.
    pub fn bar(&self, fields: &Fields) -> String {
        let boards: String = self
            .dashboards
            .iter()
            .enumerate()
            .map(|(index, dashboard)| format!("<option value=\"{index}\"{}>{}</option>", selected(index == self.shown), escape(&dashboard.name)))
            .collect();
        let spans: String =
            Span::ALL.iter().map(|span| format!("<option value=\"{}\"{}>{}</option>", span.name(), selected(fields.get("span") == span.name()), span.words())).collect();
        format!(
            "<form class=\"filters\" fx-submit=\"rename\">\
             <select name=\"board\" aria-label=\"Dashboard\">{boards}</select>\
             <input name=\"boardname\" autocomplete=\"off\" placeholder=\"Its name\" aria-label=\"Dashboard name\" title=\"Type a new name and press Enter to rename this dashboard\">\
             <select name=\"span\" aria-label=\"Time range\">{spans}</select>\
             <button type=\"button\" fx-click=\"board-new\">New dashboard</button>\
             <button type=\"button\" fx-click=\"board-delete-ask\" title=\"Delete this dashboard\">Delete…</button></form>"
        )
    }

    pub fn body(&self, fields: &Fields, clock: &Clock) -> String {
        format!(
            "<div class=\"split\" id=\"msplit\" fx-columns=\"260px minmax(0, 1fr)\"><div class=\"body metrics\">{catalogue}{board}</div></div>",
            catalogue = self.listed(fields.get("find"), i64::try_from(clock.now().0.as_nanosecond()).unwrap_or(i64::MAX)),
            board = self.board(clock),
        )
    }

    /// The catalogue, narrowed to names with `find` in them.
    fn listed(&self, find: &str, now: i64) -> String {
        let find = find.trim().to_lowercase();
        let (items, said) = match &self.catalogue {
            None => (String::new(), "<p class=\"more\">Reading metrics…</p>".to_string()),
            Some(Err(why)) => (String::new(), format!("<p class=\"trouble\">{}</p>", escape(why))),
            Some(Ok(metrics)) => {
                let shown: Vec<&Metric> = metrics.iter().filter(|metric| metric.name.to_lowercase().contains(&find)).collect();
                let items: String = shown.iter().map(|metric| item(metric, now)).collect();
                let said = match (metrics.is_empty(), shown.is_empty()) {
                    (true, _) if self.readable == Some(Readable::Nothing) => "<p class=\"more\">You can't read metrics on this machine.</p>".into(),
                    (true, _) => "<p class=\"more\">No metrics have been recorded that you may read.</p>".into(),
                    (false, true) => "<p class=\"more\">No metric's name has that in it.</p>".into(),
                    (false, false) => String::new(),
                };
                (items, said)
            }
        };
        format!(
            "<aside class=\"catalogue\" aria-label=\"Metrics\"><input name=\"find\" type=\"search\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"Find a metric\" aria-label=\"Find a metric\">\
             <p class=\"hint\">Pick one to chart it.</p><ul class=\"names\">{items}</ul>{said}</aside>"
        )
    }

    fn board(&self, clock: &Clock) -> String {
        let Some(dashboard) = self.dashboard() else { return "<section class=\"board\"></section>".into() };
        let mut keeping = self.keeping.as_ref().map(|why| format!("<p class=\"note bad\">{}</p>", escape(why))).unwrap_or_default();
        if self.deleting {
            let only = if self.dashboards.len() == 1 { " It is your only one, so eventd's health will be shown in its place." } else { "" };
            keeping += &format!(
                "<p class=\"note\">Delete {} and its charts? This cannot be undone.{only} \
                 <button type=\"button\" class=\"link\" fx-click=\"board-delete\">Delete it</button> \
                 <button type=\"button\" class=\"link\" fx-click=\"board-keep\" fx-key=\"Escape\">Keep it</button></p>",
                escape(&dashboard.name)
            );
        }
        let charts: String = dashboard.charts.iter().enumerate().map(|(index, chart)| self.figure(index, chart, dashboard.span, clock)).collect();
        let empty = if dashboard.charts.is_empty() { "<p class=\"more\">This dashboard has no charts yet. Pick a metric on the left to chart it here.</p>" } else { "" };
        let menus: String = dashboard.charts.iter().enumerate().map(|(index, chart)| self.menu(index, chart, dashboard.charts.len())).collect();
        format!("<section class=\"board\" aria-label=\"{}\">{keeping}<div class=\"charts\">{charts}</div>{empty}{menus}</section>", escape(&dashboard.name))
    }

    fn metric_type(&self, metric: &str) -> Option<&str> {
        match &self.catalogue {
            Some(Ok(metrics)) => metrics.iter().find(|candidate| candidate.name == metric).map(|candidate| candidate.metric_type.as_str()),
            _ => None,
        }
    }

    fn figure(&self, index: usize, chart: &Chart, span: Span, clock: &Clock) -> String {
        let shown = match self.answers.get(index).cloned().flatten() {
            None => "<p class=\"more\">Reading…</p>".to_string(),
            Some(Answer { trouble: Some(why), .. }) => format!("<p class=\"trouble\">{}</p>", escape(&why)),
            Some(answer) => {
                let (_, zone) = clock.now();
                let windows: Vec<String> = (0..answer.lines.first().map_or(0, |line| line.readings.len()))
                    .map(|window| said_window(answer.first + answer.width * window as i64, answer.width, span, &zone))
                    .collect();
                let start = said_time(answer.first, span, &zone);
                chart::plot(&Plot { id: index as u64, windows: &windows, lines: &answer.lines, unit: unit(chart), ends: (&start, "now") })
            }
        };
        let metric_type = self.metric_type(&chart.metric);
        let fits = metric_type.is_none_or(|metric_type| chart.transform.fits(metric_type));
        let unfit = if fits { "" } else { "<p class=\"note\">A metric of this type can't be shown that way. Pick another way in its settings.</p>" };
        let settings = if self.editing == Some(index) { self.settings(index, chart, metric_type) } else { String::new() };
        format!(
            "<figure class=\"chart\" id=\"chart-{index}\" fx-menu=\"chart-menu-{index}\" fx-value-chart=\"{index}\"><figcaption><span class=\"name\">{name}</span><span class=\"how\">{how}</span>\
             <button type=\"button\" class=\"more-button\" fx-click=\"edit\" fx-value-chart=\"{index}\" aria-expanded=\"{open}\" title=\"Change this chart\" aria-label=\"Change this chart\">⋯</button></figcaption>{settings}{unfit}{shown}</figure>",
            name = escape(&chart.metric),
            how = escape(&how(chart, span)),
            open = self.editing == Some(index),
        )
    }

    /// A chart's settings, open under its title.
    fn settings(&self, index: usize, chart: &Chart, metric_type: Option<&str>) -> String {
        let option = |value: &str, label: &str, chosen: bool| format!("<option value=\"{value}\"{}>{label}</option>", selected(chosen));
        let transforms: String = Transform::ALL
            .iter()
            .filter(|transform| metric_type.is_none_or(|metric_type| transform.fits(metric_type)) || **transform == chart.transform)
            .map(|transform| option(transform.name(), transform.words(), *transform == chart.transform))
            .collect();
        let functions: String = Function::ALL.iter().map(|function| option(function.name(), function.words(), *function == chart.function)).collect();
        let action = |set: &str, label: &str, disabled: bool| {
            format!(
                "<button type=\"button\" fx-click=\"chart\" fx-value-chart=\"{index}\" fx-value-set=\"{set}\"{}>{label}</button>",
                if disabled { " disabled" } else { "" }
            )
        };
        let count = self.dashboard().map_or(0, |dashboard| dashboard.charts.len());
        format!(
            "<div class=\"settings\"><label>Show <select name=\"edit-transform\">{transforms}</select></label>\
             <label>In each window, the <select name=\"edit-function\">{functions}</select></label>\
             <label>Lines <select name=\"edit-lines\">{each}{one}</select></label>\
             <div class=\"actions\">{earlier}{later}{remove}<button type=\"button\" fx-click=\"edit\" fx-value-chart=\"{index}\">Done</button></div></div>",
            each = option("each", "One for each series", !chart.combined),
            one = option("one", "One for all of them", chart.combined),
            earlier = action("earlier", "Move earlier", index == 0),
            later = action("later", "Move later", index + 1 >= count),
            remove = action("remove", "Remove", false),
        )
    }

    fn menu(&self, index: usize, chart: &Chart, count: usize) -> String {
        let item = |set: &str, label: &str, current: bool, disabled: bool| {
            format!(
                "<li><button type=\"button\" fx-click=\"chart\" fx-value-set=\"{set}\"{}>{}{label}</button></li>",
                if disabled { " disabled" } else { "" },
                if current { "✓ " } else { "" }
            )
        };
        let metric_type = self.metric_type(&chart.metric);
        let transforms: String = Transform::ALL
            .iter()
            .filter(|transform| metric_type.is_none_or(|metric_type| transform.fits(metric_type)))
            .map(|transform| item(&format!("transform:{}", transform.name()), transform.words(), *transform == chart.transform, false))
            .collect();
        let functions: String = Function::ALL
            .iter()
            .map(|function| item(&format!("function:{}", function.name()), &format!("{} of each window", function.words()), *function == chart.function, false))
            .collect();
        format!(
            "<menu id=\"chart-menu-{index}\" hidden>{transforms}<hr>{functions}<hr>{each}{one}<hr>{earlier}{later}{remove}</menu>",
            each = item("lines:each", "A line for each series", !chart.combined, false),
            one = item("lines:one", "One line for all of them", chart.combined, false),
            earlier = item("earlier", "Move earlier", false, index == 0),
            later = item("later", "Move later", false, index + 1 >= count),
            remove = item("remove", "Remove this chart", false, false),
        )
    }

    pub fn footer(&self) -> String {
        let metrics = match &self.catalogue {
            Some(Ok(metrics)) if metrics.len() == 1 => "1 metric".to_string(),
            Some(Ok(metrics)) => format!("{} metrics", metrics.len()),
            _ => String::new(),
        };
        let every = self.dashboard().map(|dashboard| format!(" · charts read every {} seconds", dashboard.span.refresh_seconds())).unwrap_or_default();
        let hidden = match &self.readable {
            Some(Readable::Nothing) => "You can't read metrics on this machine.".to_string(),
            Some(Readable::Unknown(why)) => format!("Some metrics may be hidden from you: {why}."),
            Some(Readable::Some { hidden }) => {
                let named: Vec<String> = hidden.iter().filter(|pattern| *pattern != "*").map(|pattern| format!("{pattern}*")).collect();
                if named.is_empty() || hidden.iter().any(|pattern| pattern == "*") {
                    "Only some metrics are yours to read.".into()
                } else {
                    format!("Hidden from you: metrics named {}.", named.join(", "))
                }
            }
            Some(Readable::Everything) | None => String::new(),
        };
        format!("<footer class=\"status\"><span>{metrics}{every}</span><span class=\"hidden\">{}</span></footer>", escape(&hidden))
    }
}

fn selected(yes: bool) -> &'static str {
    if yes { " selected" } else { "" }
}

/// One metric in the catalogue: its name, its type, and its latest value,
/// or how many series it has, each of which its title says.
fn item(metric: &Metric, now: i64) -> String {
    let unit = unit_of(&metric.name, Transform::None);
    let reading = |reading: &Option<Reading>| match reading {
        Some(Reading::Value(value)) => chart::say(*value, unit),
        Some(Reading::Overflow) => "above its highest bucket".into(),
        None => "nothing".into(),
    };
    let stale = |at: &Option<i64>| at.filter(|at| now.saturating_sub(*at) > STALE);
    let newest = metric.series.iter().filter_map(|(_, _, at)| *at).max();
    let stopped = metric.series.iter().all(|(_, _, at)| stale(at).is_some());
    let latest = match (metric.series.as_slice(), newest) {
        (_, Some(newest)) if stopped => format!("nothing since {}", ago(now, newest)),
        ([(_, only, _)], _) => reading(only),
        (series, _) => format!("{} series", series.len()),
    };
    let title: String = metric
        .series
        .iter()
        .map(|(labels, latest, at)| {
            let said = if labels.is_empty() { reading(latest) } else { format!("{labels}: {}", reading(latest)) };
            match stale(at) {
                Some(at) => format!("{said}, last recorded {}", ago(now, at)),
                None => said,
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<li id=\"m-{name}\"><button type=\"button\" class=\"metric{class}\" fx-click=\"add\" fx-value-metric=\"{name}\" fx-value-type=\"{kind}\" title=\"{title}\">\
         <span class=\"name\">{name}</span><span class=\"kind\">{kind}</span><span class=\"latest\">{latest}</span></button></li>",
        class = if stopped { " stopped" } else { "" },
        name = escape(&metric.name),
        kind = escape(&metric.metric_type),
        title = escape(&title),
        latest = escape(&latest),
    )
}

/// How long since a series' last sample before it is said to have stopped.
const STALE: i64 = 10 * 60 * 1_000_000_000;

/// How long ago `at` was, in the largest whole unit.
fn ago(now: i64, at: i64) -> String {
    let seconds = now.saturating_sub(at) / 1_000_000_000;
    let (count, unit) = match seconds {
        ..60 => (seconds, "second"),
        60..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

/// What a chart shows, in words.
fn how(chart: &Chart, span: Span) -> String {
    let window = match span.window_seconds() {
        60 => "minute".to_string(),
        3_600 => "hour".to_string(),
        seconds if seconds % 3_600 == 0 => format!("{} hours", seconds / 3_600),
        seconds if seconds % 60 == 0 => format!("{} minutes", seconds / 60),
        seconds => format!("{seconds} seconds"),
    };
    let together = if chart.combined { ", all series together" } else { "" };
    format!("{}, {} of each {window}{together}", chart.transform.words(), chart.function.words().to_lowercase())
}

/// How a chart's values are said: bytes and percentages by their names'
/// last part, as metric names have them (§3.10), and per second after a
/// rate.
fn unit(chart: &Chart) -> Unit {
    unit_of(&chart.metric, chart.transform)
}

fn unit_of(metric: &str, transform: Transform) -> Unit {
    let last = metric.rsplit('.').next().unwrap_or_default();
    match (last, transform) {
        ("bytes", Transform::Rate) => Unit::BytesPerSecond,
        ("bytes", _) => Unit::Bytes,
        ("percent", Transform::None | Transform::P50 | Transform::P95 | Transform::P99) => Unit::Percent,
        (_, Transform::Rate) => Unit::PerSecond,
        _ => Unit::Plain,
    }
}

fn said_time(at: i64, span: Span, zone: &jiff::tz::TimeZone) -> String {
    let Ok(at) = jiff::Timestamp::from_nanosecond(i128::from(at)) else { return at.to_string() };
    let at = at.to_zoned(zone.clone());
    let format = match span {
        Span::Minutes15 => "%H:%M:%S",
        Span::Hour | Span::Hours6 | Span::Day => "%H:%M",
        Span::Week => "%-d %b %H:%M",
    };
    at.strftime(format).to_string()
}

fn said_window(start: i64, width: i64, span: Span, zone: &jiff::tz::TimeZone) -> String {
    format!("{} to {}", said_time(start, span, zone), said_time(start + width, span, zone))
}

/// Every metric there is that the person may read, at its latest.
fn catalogue(socket: &Path) -> Result<Vec<Metric>, String> {
    let mut metrics: BTreeMap<String, Metric> = BTreeMap::new();
    for (metric_type, text) in CATALOGUE {
        for record in eventd_client::query(socket, text).map_err(|error| trouble(&error))? {
            let Some(Value::String(name)) = record.get("name") else { continue };
            let metric = metrics.entry(name.clone()).or_insert_with(|| Metric { name: name.clone(), metric_type: metric_type.into(), series: Vec::new() });
            metric.series.push((labels(&record), reading(&record), words::timestamp(&record)));
        }
    }
    Ok(metrics.into_values().collect())
}

/// A record's labels, as `key=value`, in their order.
fn labels(record: &Record) -> String {
    record
        .iter()
        .filter(|(field, _)| !FIXED.contains(&field.as_str()))
        .map(|(field, value)| format!("{field}={}", words::value(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn reading(record: &Record) -> Option<Reading> {
    match record.get("value") {
        Some(Value::Float(value)) => Some(Reading::Value(*value)),
        Some(Value::Signed(value)) => Some(Reading::Value(*value as f64)),
        Some(Value::Unsigned(value)) => Some(Reading::Value(*value as f64)),
        Some(Value::Null) if record.get("overflow") == Some(&Value::Bool(true)) => Some(Reading::Overflow),
        _ => None,
    }
}

/// A chart's answer: its windows from the range's start to `now`, and a
/// line for each series, in the order of their labels.
fn answer(span: Span, now: i64, result: Result<Vec<Record>, Error>) -> Answer {
    let width = span.window_seconds() * 1_000_000_000;
    let first = (now - span.seconds() * 1_000_000_000).div_euclid(width) * width;
    let count = usize::try_from((now.div_euclid(width) * width - first) / width + 1).unwrap_or(0);
    let records = match result {
        Ok(records) => records,
        Err(error) => return Answer { first, width, lines: Vec::new(), trouble: Some(trouble(&error)) },
    };
    let mut lines: BTreeMap<String, Vec<Option<Reading>>> = BTreeMap::new();
    for record in &records {
        let Some(at) = words::timestamp(record) else { continue };
        let Ok(window) = usize::try_from((at - first).div_euclid(width)) else { continue };
        if window >= count {
            continue;
        }
        let label = labels(record);
        let label = if label.is_empty() { "all".to_string() } else { label };
        lines.entry(label).or_insert_with(|| vec![None; count])[window] = reading(record);
    }
    if lines.is_empty() {
        lines.insert(String::new(), vec![None; count]);
    }
    Answer { first, width, lines: lines.into_iter().map(|(label, readings)| Line { label, readings }).collect(), trouble: None }
}

/// Asks for every chart, and again every little while, until the window
/// goes or `generation` moves on.
fn poll(window: &Weak<Surface<Viewer>>, generation: &AtomicU64, mine: u64, socket: &Path, span: Span, charts: &[Chart]) {
    let every = Duration::from_secs(span.refresh_seconds());
    let socket: PathBuf = socket.to_path_buf();
    loop {
        if generation.load(Ordering::SeqCst) != mine {
            return;
        }
        let answers: Vec<Answer> = charts
            .iter()
            .map(|chart| {
                let result = eventd_client::query(&socket, &chart.query(span));
                answer(span, jiff::Timestamp::now().as_nanosecond() as i64, result)
            })
            .collect();
        // The catalogue's latest values move with the charts.
        let listed = catalogue(&socket);
        let Some(strong) = window.upgrade() else { return };
        strong.update(|viewer, _| {
            viewer.metrics.answered(mine, answers);
            if viewer.metrics.generation.load(Ordering::SeqCst) == mine {
                viewer.metrics.catalogue = Some(listed);
            }
        });
        drop(strong);
        let mut waited = Duration::ZERO;
        while waited < every {
            if generation.load(Ordering::SeqCst) != mine {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
            waited += Duration::from_millis(250);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1_000_000_000;

    fn record(at: i64, labels: &[(&str, &str)], value: Value) -> Record {
        let mut record: Record = labels.iter().map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string()))).collect();
        record.insert("timestamp".into(), Value::Signed(at));
        record.insert("name".into(), Value::String("eventd.store.bytes".into()));
        record.insert("type".into(), Value::String("gauge".into()));
        record.insert("value".into(), value);
        record
    }

    #[test]
    fn an_answer_has_a_window_for_every_step_and_a_line_for_every_series() {
        let now = 1_791_025_249 * SECOND;
        let first = (now - 900 * SECOND).div_euclid(15 * SECOND) * 15 * SECOND;
        let records = vec![
            record(first, &[("store", "logs")], Value::Float(1.0)),
            record(first + 15 * SECOND, &[("store", "logs")], Value::Float(2.0)),
            record(first + 30 * SECOND, &[("store", "events")], Value::Float(3.0)),
            // Outside the windows: left out.
            record(first - 15 * SECOND, &[("store", "logs")], Value::Float(9.0)),
        ];
        let answer = answer(Span::Minutes15, now, Ok(records));
        assert_eq!(answer.first, first);
        assert_eq!(answer.lines.len(), 2);
        assert_eq!(answer.lines[0].label, "store=events");
        assert_eq!(answer.lines[1].label, "store=logs");
        assert_eq!(answer.lines[1].readings.len(), 61);
        assert_eq!(answer.lines[1].readings[..3], [Some(Reading::Value(1.0)), Some(Reading::Value(2.0)), None]);
        assert_eq!(answer.lines[0].readings[2], Some(Reading::Value(3.0)));
    }

    #[test]
    fn an_answer_says_why_there_is_none() {
        let refused = answer(Span::Hour, 3_600 * SECOND * 1_000, Err(Error::Refused("metric selector spans more than one type".into())));
        assert_eq!(refused.trouble.as_deref(), Some("eventd refused: metric selector spans more than one type."));
        let empty = answer(Span::Hour, 3_600 * SECOND * 1_000, Ok(Vec::new()));
        assert_eq!(empty.lines.len(), 1);
        assert!(empty.lines[0].readings.iter().all(Option::is_none));
    }

    #[test]
    fn units_come_from_the_name_and_the_transform() {
        assert_eq!(unit_of("eventd.store.bytes", Transform::None), Unit::Bytes);
        assert_eq!(unit_of("disk.read.bytes", Transform::Rate), Unit::BytesPerSecond);
        assert_eq!(unit_of("eventd.kmes.ring.fill.percent", Transform::None), Unit::Percent);
        assert_eq!(unit_of("eventd.events.stored", Transform::Rate), Unit::PerSecond);
        assert_eq!(unit_of("eventd.events.stored", Transform::Delta), Unit::Plain);
    }

    #[test]
    fn a_chart_is_described_in_words() {
        let chart = Chart { metric: "eventd.events.stored".into(), transform: Transform::Rate, function: Function::Sum, combined: true };
        assert_eq!(how(&chart, Span::Hour), "Rate per second, total of each minute, all series together");
        assert_eq!(how(&Chart::of("eventd.store.bytes", "gauge"), Span::Week), "As recorded, average of each 2 hours");
        assert_eq!(how(&Chart::of("x", "gauge"), Span::Minutes15), "As recorded, average of each 15 seconds");
    }

    #[test]
    fn the_catalogue_says_each_metric_and_its_latest() {
        let now = 1_791_032_000 * SECOND;
        let one = Metric { name: "eventd.store.bytes".into(), metric_type: "gauge".into(), series: vec![(String::new(), Some(Reading::Value(2048.0)), Some(now - SECOND))] };
        let html = item(&one, now);
        assert!(html.contains("class=\"metric\" fx-click=\"add\" fx-value-metric=\"eventd.store.bytes\" fx-value-type=\"gauge\""));
        assert!(html.contains("<span class=\"latest\">2 KiB</span>"));
        let two = Metric {
            name: "cpu.usage".into(),
            metric_type: "gauge".into(),
            series: vec![("core=0".into(), Some(Reading::Value(3.0)), Some(now)), ("core=1".into(), None, Some(now - 3 * 3_600 * SECOND))],
        };
        let html = item(&two, now);
        assert!(html.contains("<span class=\"latest\">2 series</span>"));
        assert!(html.contains("title=\"core=0: 3\ncore=1: nothing, last recorded 3 hours ago\""));
        // Every series stopped: said so, and when.
        let stopped = Metric { series: vec![("core=0".into(), Some(Reading::Value(3.0)), Some(now - 20 * 60 * SECOND))], ..two };
        let html = item(&stopped, now);
        assert!(html.contains("class=\"metric stopped\""));
        assert!(html.contains("<span class=\"latest\">nothing since 20 minutes ago</span>"));
    }

    #[test]
    fn a_chart_menu_changes_its_chart_and_keeps_to_the_shown_dashboard() {
        let mut metrics = Metrics::new();
        metrics.dashboards = vec![dashboards::health()];
        metrics.read = true;
        let before = metrics.dashboards[0].charts.clone();
        // Saving fails off Peios, which is said; the change stands.
        metrics.change(0, "transform:delta");
        assert_eq!(metrics.dashboards[0].charts[0].transform, Transform::Delta);
        metrics.change(0, "lines:each");
        assert!(!metrics.dashboards[0].charts[0].combined);
        metrics.change(0, "later");
        assert_eq!(metrics.dashboards[0].charts[0], before[1]);
        metrics.change(0, "earlier");
        metrics.change(1, "remove");
        assert_eq!(metrics.dashboards[0].charts.len(), before.len() - 1);
        metrics.change(99, "remove");
        metrics.change(0, "transform:sideways");
        assert_eq!(metrics.dashboards[0].charts.len(), before.len() - 1);
        metrics.add("not a name", "gauge");
        metrics.add("cpu.usage", "gauge");
        assert_eq!(metrics.dashboards[0].charts.last().unwrap().metric, "cpu.usage");
    }
}
