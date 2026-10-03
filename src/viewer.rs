//! The window: eventd's logs or events, newest first, and live.
//!
//! WHAT IS SHOWN comes from one query (`query`): its first page, then what
//! is recorded after it as it is recorded, on a thread of its own so that
//! eventd, which drops a reader that falls behind, is read as fast as it
//! writes. The window keeps one such query at a time; eventd lets the whole
//! machine have only so many. Older records come a page at a time, when
//! the person asks for them, and the window holds at most `HELD` records:
//! following, the oldest go; paging back, the newest go and the window
//! stops following until it is taken back to the newest.
//!
//! WHAT IS HIDDEN eventd leaves out without saying so (PSPU §3.28). The
//! window reads eventd's read policy and checks it as the person
//! (`eventd_client::access`), and says what it keeps from them, or that it
//! keeps everything, rather than showing an empty list as if there were
//! nothing.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Weak};
use std::time::Duration;

use eventd_client::access::{self, Namespace, Readable};
use eventd_client::{Error, Record, Tail, Tailed, Value};
use libgxwi::{Facts, Fields, Live, Surface, escape};

use crate::query::{Filter, Kind, PAGE, Range, SOURCES};
use crate::words::{self, Clock};

/// The most records the window holds.
pub const HELD: usize = 1000;

/// How many reports a tail may have waiting before eventd is left to end
/// it for being read too slowly.
const WAITING: usize = 64;

/// The least time between two batches put on the screen while following: a
/// busy machine records many a second, and each would be a render.
const BETWEEN: Duration = Duration::from_millis(250);

/// The most origins or types offered as suggestions.
const SUGGESTED: usize = 500;

struct Row {
    /// The window's own: records carry no identity of their own, and the
    /// page tells rows apart by id.
    id: u64,
    record: Record,
}

/// Whether the window is following what is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Following {
    /// The first page has been asked for.
    Starting,
    Yes,
    /// The person paused it.
    Paused,
    /// Paged back past what the window holds, so the newest went.
    Back,
    /// It ended, and why.
    Stopped(String),
}

pub struct Viewer {
    kind: Kind,
    /// What the records shown were asked for with.
    filter: Filter,
    /// Newest first.
    rows: VecDeque<Row>,
    next_id: u64,
    picked: Option<u64>,
    following: Following,
    /// Why the first page could not be had.
    trouble: Option<String>,
    /// An older page under way.
    paging: bool,
    /// Every record in the range has been shown.
    complete: bool,
    /// Why an older page could not be had.
    paging_trouble: Option<String>,
    /// What the person may read, of logs and of events, once read.
    readable: [Option<Readable>; 2],
    /// What may be typed as an origin or a type, for the range shown.
    suggestions: Vec<String>,
    /// Changes whenever the rows start again, so that what was asked for
    /// before is not mixed into them.
    epoch: Arc<AtomicU64>,
    /// Changes whenever a tail should stop: its thread watches it.
    generation: Arc<AtomicU64>,
    clock: Clock,
    socket: PathBuf,
    pub window: Weak<Surface<Viewer>>,
}

impl Viewer {
    pub fn new(kind: Kind) -> Viewer {
        Viewer {
            kind,
            filter: Filter::new(kind),
            rows: VecDeque::new(),
            next_id: 0,
            picked: None,
            following: Following::Starting,
            trouble: None,
            paging: false,
            complete: false,
            paging_trouble: None,
            readable: [None, None],
            suggestions: Vec::new(),
            epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
            clock: Clock::Machine,
            socket: PathBuf::from(eventd_client::DEFAULT_SOCKET),
            window: Weak::new(),
        }
    }

    /// The fields as the window opens: the range it opens on, and an origin
    /// if it was opened on one.
    pub fn fill(&mut self, fields: &mut Fields, origin: Option<&str>) {
        fields.set("range", Range::DEFAULT.name());
        if let Some(origin) = origin {
            fields.set("origin", origin);
        }
        self.apply(fields);
        self.read_policy();
    }

    /// What the fields say, for the kind shown.
    fn chosen(&self, fields: &Fields) -> Filter {
        let mut filter = Filter::new(self.kind);
        filter.range = Range::named(fields.get("range")).unwrap_or(Range::DEFAULT);
        filter.origin = fields.get("origin").trim().to_string();
        filter.errors = !fields.get("errors").is_empty();
        filter.containing = fields.get("containing").trim().to_string();
        filter.event_type = fields.get("type").trim().to_string();
        filter.source = fields.get("source").parse().ok().filter(|source| SOURCES.iter().any(|(class, _)| class == source));
        filter
    }

    /// Asks again with what the fields say, from the newest.
    fn apply(&mut self, fields: &Fields) {
        let filter = self.chosen(fields);
        let suggest = filter.range != self.filter.range || filter.kind != self.filter.kind || self.suggestions.is_empty();
        self.filter = filter;
        if let Some(window) = self.window.upgrade() {
            window.retitle(&title(&self.filter));
        }
        self.restart();
        if suggest {
            self.suggest();
        }
    }

    /// Lets go of every record and asks for the newest again.
    fn restart(&mut self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.rows.clear();
        self.picked = None;
        self.trouble = None;
        self.paging = false;
        self.complete = false;
        self.paging_trouble = None;
        self.follow();
    }

    /// Starts the one query that gives the first page and then what is
    /// recorded after it. Whatever the window was following stops.
    fn follow(&mut self) {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.following = Following::Starting;
        let Some(window) = self.window.upgrade() else { return };
        let window = Arc::downgrade(&window);
        let (socket, text, generation) = (self.socket.clone(), self.filter.first_page(), self.generation.clone());
        std::thread::spawn(move || tail(&window, &generation, mine, &socket, &text));
    }

    /// Stops following, keeping what is shown.
    fn stop(&mut self, now: Following) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.following = now;
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn row(&mut self, record: Record) -> Row {
        self.next_id += 1;
        Row { id: self.next_id, record }
    }

    /// The first page.
    fn initial(&mut self, generation: u64, records: Vec<Record>) {
        if !self.current(generation) {
            return;
        }
        self.complete = records.len() < PAGE;
        self.rows = records.into_iter().map(|record| self.row(record)).collect();
        self.following = Following::Yes;
    }

    /// Records committed since, newest first however they came (PEI-1237).
    fn live(&mut self, generation: u64, mut records: Vec<Record>) {
        if !self.current(generation) {
            return;
        }
        records.sort_by_key(|record| std::cmp::Reverse(words::timestamp(record)));
        for record in records.into_iter().rev() {
            let row = self.row(record);
            self.rows.push_front(row);
        }
        while self.rows.len() > HELD {
            self.rows.pop_back();
            self.complete = false;
        }
        self.keep_picked();
    }

    /// The query ended, or never started.
    fn ended(&mut self, generation: u64, error: &Error) {
        if !self.current(generation) {
            return;
        }
        let why = trouble(error);
        if self.following == Following::Starting {
            self.trouble = Some(why);
            self.following = Following::Stopped(String::new());
        } else {
            self.following = Following::Stopped(why);
        }
    }

    /// Asks for the page before the oldest record shown.
    fn older(&mut self) {
        if self.paging || self.complete {
            return;
        }
        let Some((anchor, oldest)) = self.rows.back().and_then(|row| Some((row.id, words::timestamp(&row.record)?))) else { return };
        let shown = self.rows.iter().rev().take_while(|row| words::timestamp(&row.record) == Some(oldest)).count();
        let Some(window) = self.window.upgrade() else { return };
        self.paging = true;
        self.paging_trouble = None;
        let (socket, text) = (self.socket.clone(), self.filter.older(oldest, shown));
        let epoch = self.epoch.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            let page = eventd_client::query(&socket, &text);
            window.update(|viewer, _| viewer.paged(epoch, anchor, page));
        });
    }

    /// An older page, or why there is none. It follows the row `anchor`,
    /// the oldest when it was asked for; if that has been let go since, to
    /// make room for new ones, the page would leave a gap, and is dropped.
    fn paged(&mut self, epoch: u64, anchor: u64, page: Result<Vec<Record>, Error>) {
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        self.paging = false;
        if self.rows.back().map(|row| row.id) != Some(anchor) {
            return;
        }
        let records = match page {
            Ok(records) => records,
            Err(error) => {
                self.paging_trouble = Some(trouble(&error));
                return;
            }
        };
        self.complete = records.len() < PAGE;
        for record in records {
            let row = self.row(record);
            self.rows.push_back(row);
        }
        if self.rows.len() > HELD {
            while self.rows.len() > HELD {
                self.rows.pop_front();
            }
            if matches!(self.following, Following::Yes | Following::Starting) {
                self.stop(Following::Back);
            } else if self.following == Following::Paused {
                self.following = Following::Back;
            }
            self.keep_picked();
        }
    }

    /// Lets go of the picked record if it has gone.
    fn keep_picked(&mut self) {
        if let Some(picked) = self.picked
            && !self.rows.iter().any(|row| row.id == picked)
        {
            self.picked = None;
        }
    }

    /// Reads what the person may read, on a thread: the registry is read.
    fn read_policy(&self) {
        let Some(window) = self.window.upgrade() else { return };
        std::thread::spawn(move || {
            let logs = access::readable(Namespace::Logs);
            let events = access::readable(Namespace::Events);
            window.update(|viewer, _| viewer.readable = [Some(logs), Some(events)]);
        });
    }

    /// Finds what may be typed as an origin or a type, on a thread.
    fn suggest(&mut self) {
        let Some(window) = self.window.upgrade() else { return };
        let (socket, text, filter) = (self.socket.clone(), self.filter.suggestions(), self.filter.clone());
        std::thread::spawn(move || {
            let field = match filter.kind {
                Kind::Logs => "origin",
                Kind::Events => "event_type",
            };
            let found: Vec<String> = eventd_client::query(&socket, &text)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|mut record| match record.remove(field) {
                    Some(Value::String(text)) => Some(text),
                    _ => None,
                })
                .take(SUGGESTED)
                .collect();
            window.update(|viewer, _| {
                if viewer.filter.kind == filter.kind && viewer.filter.range == filter.range {
                    viewer.suggestions = found;
                }
            });
        });
    }

    fn readable(&self) -> Option<&Readable> {
        self.readable[match self.kind {
            Kind::Logs => 0,
            Kind::Events => 1,
        }]
        .as_ref()
    }

    fn noun(&self) -> &'static str {
        match self.kind {
            Kind::Logs => "logs",
            Kind::Events => "events",
        }
    }

    /// What the policy keeps from the person, in a sentence, if anything.
    fn hidden(&self) -> Option<String> {
        let noun = self.noun();
        let from = match self.kind {
            Kind::Logs => "from",
            Kind::Events => "of type",
        };
        match self.readable()? {
            Readable::Everything => None,
            Readable::Nothing => Some(format!("You can't read {noun} on this machine.")),
            Readable::Unknown(why) => Some(format!("Some {noun} may be hidden from you: {why}.")),
            Readable::Some { hidden } => {
                let named: Vec<String> = hidden.iter().filter(|pattern| *pattern != "*").map(|pattern| format!("{pattern}*")).collect();
                let rest = hidden.iter().any(|pattern| pattern == "*");
                Some(match (named.as_slice(), rest) {
                    ([], _) => format!("Only some {noun} are yours to read: those {from} a name eventd's policy lets you read."),
                    (named, false) => format!("Hidden from you: {noun} {from} {}.", named.join(", ")),
                    (named, true) => format!("Only some {noun} are yours to read. Hidden: {noun} {from} {}, and any {from} a name the policy does not list.", named.join(", ")),
                })
            }
        }
    }

    fn bar(&self, facts: &Facts) -> String {
        let fields = facts.fields;
        let tab = |kind: Kind, label: &str| {
            format!(
                "<button type=\"button\" class=\"tab\" fx-click=\"kind\" fx-value-kind=\"{}\" aria-pressed=\"{}\">{label}</button>",
                kind.name(),
                self.kind == kind
            )
        };
        let ranges: String = Range::ALL
            .iter()
            .map(|range| format!("<option value=\"{}\"{}>{}</option>", range.name(), if fields.get("range") == range.name() { " selected" } else { "" }, range.words()))
            .collect();
        let range = format!("<select name=\"range\" aria-label=\"Time range\">{ranges}</select>");
        let suggestions: String = self.suggestions.iter().map(|text| format!("<option value=\"{}\">", escape(text))).collect();
        let filters = match self.kind {
            Kind::Logs => "<input name=\"origin\" list=\"suggestions\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"From (a service)\" aria-label=\"From\">\
                 <input name=\"containing\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"Containing\" aria-label=\"Containing\">\
                 <label class=\"check\"><input type=\"checkbox\" name=\"errors\"> Errors only</label>"
                .to_string(),
            Kind::Events => {
                let sources: String = SOURCES
                    .iter()
                    .map(|(class, name)| format!("<option value=\"{class}\"{}>{name}</option>", if fields.get("source") == class.to_string() { " selected" } else { "" }))
                    .collect();
                format!(
                    "<input name=\"type\" list=\"suggestions\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"Type, * for any part\" aria-label=\"Type\">\
                     <select name=\"source\" aria-label=\"Source\"><option value=\"\">Any source</option>{sources}</select>"
                )
            }
        };
        let live = match &self.following {
            Following::Yes | Following::Starting => "<button type=\"button\" class=\"live on\" fx-click=\"pause\" title=\"New records appear as they are recorded. Pause to keep the list still.\">Pause</button>".to_string(),
            Following::Paused => "<button type=\"button\" class=\"live\" fx-click=\"resume\" title=\"Show the newest again, and follow what is recorded\">Resume</button>".to_string(),
            Following::Back | Following::Stopped(_) => "<button type=\"button\" class=\"live\" fx-click=\"resume\" title=\"Show the newest again, and follow what is recorded\">Newest</button>".to_string(),
        };
        format!(
            "<div class=\"bar\"><div class=\"top\"><span class=\"tabs\" role=\"group\" aria-label=\"Show\">{events}{logs}</span>\
             {live}<button type=\"button\" class=\"refresh\" fx-click=\"refresh\" fx-key=\"F5\" title=\"Read again from the newest (F5)\">Refresh</button></div>\
             <form class=\"filters\" fx-submit=\"apply\">{filters}{range}<button type=\"submit\">Apply</button></form>\
             <datalist id=\"suggestions\">{suggestions}</datalist></div>",
            logs = tab(Kind::Logs, "Logs"),
            events = tab(Kind::Events, "Events"),
        )
    }

    fn listing(&self, now: jiff::Timestamp, zone: &jiff::tz::TimeZone) -> String {
        let when = |row: &Row| words::timestamp(&row.record).map(|at| words::when(at, now, zone)).unwrap_or_default();
        let text = |record: &Record, field: &str| record.get(field).map(words::value).unwrap_or_default();
        let rows: String = self
            .rows
            .iter()
            .map(|row| {
                let record = &row.record;
                let (class, cells) = match self.kind {
                    Kind::Logs => {
                        let error = record.get("is_error") == Some(&Value::Bool(true));
                        (
                            if error { " error" } else { "" },
                            format!(
                                "<span class=\"when\">{}</span><span class=\"origin\">{}</span><span class=\"message\">{}</span>",
                                escape(&when(row)),
                                escape(&text(record, "origin")),
                                escape(text(record, "message").trim_end()),
                            ),
                        )
                    }
                    Kind::Events => (
                        "",
                        format!(
                            "<span class=\"when\">{}</span><span class=\"type\">{}</span><span class=\"source\">{}</span>",
                            escape(&when(row)),
                            escape(&text(record, "event_type")),
                            escape(&record.get("origin_class").map(words::source).unwrap_or_default()),
                        ),
                    ),
                };
                format!(
                    "<li id=\"r{id}\"><button type=\"button\" class=\"row{class}\" fx-click=\"pick\" fx-menu=\"row-menu\" fx-value-row=\"{id}\" aria-selected=\"{picked}\">{cells}</button></li>",
                    id = row.id,
                    picked = self.picked == Some(row.id),
                )
            })
            .collect();
        let (columns, head) = match self.kind {
            Kind::Logs => ("150px minmax(0, 160px) minmax(0, 1fr)", "<span>Time</span><span>From</span><span>Message</span>"),
            Kind::Events => ("150px minmax(0, 1fr) 140px", "<span>Time</span><span>Type</span><span>Source</span>"),
        };
        let noun = self.noun();
        let empty = if !self.rows.is_empty() {
            String::new()
        } else if let Some(why) = &self.trouble {
            format!("<p class=\"trouble\">{}</p>", escape(why))
        } else if self.following == Following::Starting {
            format!("<p class=\"more\">Reading {noun}…</p>")
        } else if self.readable() == Some(&Readable::Nothing) {
            format!("<p class=\"more\">You can't read {noun} on this machine, so there are none to show.</p>")
        } else if self.filter.narrowed() {
            format!("<p class=\"more\">No {noun} match in the {}.</p>", self.filter.range.words().to_lowercase())
        } else if self.filter.range == Range::Any {
            format!("<p class=\"more\">There are no {noun} you may read.</p>")
        } else {
            format!("<p class=\"more\">There are no {noun} you may read in the {}.</p>", self.filter.range.words().to_lowercase())
        };
        format!(
            "<div class=\"listing\" id=\"listing-{kind}\" fx-columns=\"{columns}\"><div class=\"head\">{head}</div>\
             <div class=\"scroll\">{back}<ul class=\"entries\">{rows}</ul>{empty}{foot}</div></div>",
            kind = self.kind.name(),
            back = self.back(),
            foot = self.foot(),
        )
    }

    /// Above the rows: that the newest are not shown, when they are not.
    fn back(&self) -> String {
        match &self.following {
            Following::Back => format!(
                "<p class=\"note\">Showing older {}. Newer ones were let go to make room. <button type=\"button\" class=\"link\" fx-click=\"resume\">Show the newest</button></p>",
                self.noun()
            ),
            Following::Stopped(why) if !why.is_empty() => format!(
                "<p class=\"note bad\">New {} stopped coming: {}. <button type=\"button\" class=\"link\" fx-click=\"resume\">Show the newest</button></p>",
                self.noun(),
                escape(why)
            ),
            _ => String::new(),
        }
    }

    /// Below the rows: older ones, or that there are none.
    fn foot(&self) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        if let Some(why) = &self.paging_trouble {
            return format!("<p class=\"trouble\">{} <button type=\"button\" class=\"link\" fx-click=\"older\">Try again</button></p>", escape(why));
        }
        if self.paging {
            return "<p class=\"more\">Reading older ones…</p>".into();
        }
        if self.complete {
            return match self.filter.range {
                Range::Any => format!("<p class=\"more\">That is every one of the {} you may read.</p>", self.noun()),
                range => format!("<p class=\"more\">That is everything in the {}.</p>", range.words().to_lowercase()),
            };
        }
        "<p class=\"more\"><button type=\"button\" class=\"older\" fx-click=\"older\">Show older</button></p>".into()
    }

    fn details(&self, zone: &jiff::tz::TimeZone) -> String {
        let Some(row) = self.picked.and_then(|picked| self.rows.iter().find(|row| row.id == picked)) else {
            return format!("<aside class=\"details empty\" aria-label=\"Details\"><p>Pick one of the {} to see all of it.</p></aside>", self.noun());
        };
        let record = &row.record;
        let said = |field: &str, item: &Value| match field {
            "timestamp" => words::timestamp(record).map_or_else(|| words::value(item), |at| words::when_exactly(at, zone)),
            "origin_class" => words::source(item),
            "is_error" if *item == Value::Bool(true) => "Standard error".into(),
            "is_error" => "Standard output".into(),
            _ => words::value(item),
        };
        let fact = |field: &str, item: &Value| {
            let name = words::field_name(field).map_or_else(|| escape(field), |name| format!("{name}<code>{}</code>", escape(field)));
            format!("<dt>{name}</dt><dd>{}</dd>", escape(&said(field, item)))
        };
        let copy: String = record.iter().map(|(field, item)| format!("{field}: {}\n", said(field, item))).collect();
        let (title, body, only) = match self.kind {
            Kind::Logs => {
                let origin = record.get("origin").map(words::value).unwrap_or_default();
                let facts: String = ["timestamp", "is_error", "job_id", "boot_id"].iter().filter_map(|field| record.get(*field).map(|item| fact(field, item))).collect();
                let message = record.get("message").map(words::value).unwrap_or_default();
                let error = if record.get("is_error") == Some(&Value::Bool(true)) { " error" } else { "" };
                (
                    origin.clone(),
                    format!("<pre class=\"message{error}\">{}</pre><dl>{facts}</dl>", escape(&message)),
                    format!("<button type=\"button\" fx-click=\"only\" fx-value-row=\"{}\">Show only {}</button>", row.id, escape(&origin)),
                )
            }
            Kind::Events => {
                let event_type = record.get("event_type").map(words::value).unwrap_or_default();
                let headers: String = words::HEADERS.iter().filter(|field| **field != "event_type").filter_map(|field| record.get(*field).map(|item| fact(field, item))).collect();
                let own: String = record.iter().filter(|(field, _)| !words::HEADERS.contains(&field.as_str())).map(|(field, item)| fact(field, item)).collect();
                let own = if own.is_empty() { String::new() } else { format!("<h3>Its fields</h3><dl>{own}</dl>") };
                (
                    event_type.clone(),
                    format!("<dl>{headers}</dl>{own}"),
                    format!("<button type=\"button\" fx-click=\"only\" fx-value-row=\"{}\">Show only {}</button>", row.id, escape(&event_type)),
                )
            }
        };
        format!(
            "<aside class=\"details\" aria-label=\"Details\"><h2>{title}</h2>{body}\
             <div class=\"actions\">{only}<button type=\"button\" fx-copy=\"text\" fx-value-text=\"{copy}\">Copy</button></div></aside>",
            title = escape(&title),
            copy = escape(&copy),
        )
    }

    fn footer(&self) -> String {
        let count = match self.rows.len() {
            1 => format!("1 of the {}", self.noun()),
            count => format!("{count} {}", self.noun()),
        };
        let state = match &self.following {
            Following::Yes => " · live",
            Following::Paused => " · paused",
            _ => "",
        };
        let hidden = self.hidden().map(|hidden| escape(&hidden)).unwrap_or_default();
        format!("<footer class=\"status\"><span>{count}{state}</span><span class=\"hidden\">{hidden}</span></footer>")
    }

    /// The row `step` rows from the picked one, for the arrow keys.
    fn near(&self, step: isize) -> String {
        let at = self.picked.and_then(|picked| self.rows.iter().position(|row| row.id == picked));
        let to = match at {
            Some(at) => at.saturating_add_signed(step).min(self.rows.len().saturating_sub(1)),
            None => 0,
        };
        self.rows.get(to).map(|row| row.id.to_string()).unwrap_or_default()
    }

    /// Narrows to what `row` came from: its origin, or its type.
    fn only(&mut self, row: u64, fields: &mut Fields) {
        let Some(row) = self.rows.iter().find(|candidate| candidate.id == row) else { return };
        match self.kind {
            Kind::Logs => {
                let Some(Value::String(origin)) = row.record.get("origin") else { return };
                fields.set("origin", &origin.clone());
            }
            Kind::Events => {
                let Some(Value::String(event_type)) = row.record.get("event_type") else { return };
                fields.set("type", &event_type.clone());
            }
        }
        self.apply(fields);
    }
}

impl Live for Viewer {
    fn render(&self, facts: &Facts) -> String {
        let (now, zone) = self.clock.now();
        let only = match self.kind {
            Kind::Logs => "Show only from here",
            Kind::Events => "Show only this type",
        };
        format!(
            "<div hidden>\
             <button type=\"button\" fx-key=\"ArrowDown\" fx-click=\"pick\" fx-value-row=\"{next}\"></button>\
             <button type=\"button\" fx-key=\"ArrowUp\" fx-click=\"pick\" fx-value-row=\"{previous}\"></button>\
             </div>{bar}<div class=\"split\" id=\"split\" fx-columns=\"minmax(0, 1fr) 340px\"><div class=\"body\">{listing}{details}</div></div>{footer}\
             <menu id=\"row-menu\" hidden><li><button type=\"button\" fx-click=\"only\">{only}</button></li></menu>",
            next = self.near(1),
            previous = self.near(-1),
            bar = self.bar(facts),
            listing = self.listing(now, &zone),
            details = self.details(&zone),
            footer = self.footer(),
        )
    }

    fn event(&mut self, name: &str, value: &libgxwi::Value, fields: &mut Fields) {
        let row = value["row"].as_str().and_then(|row| row.parse::<u64>().ok());
        match name {
            "pick" => {
                if let Some(row) = row.filter(|row| self.rows.iter().any(|candidate| candidate.id == *row)) {
                    self.picked = Some(row);
                }
            }
            "kind" => {
                if let Some(kind) = value["kind"].as_str().and_then(Kind::named)
                    && kind != self.kind
                {
                    self.kind = kind;
                    self.suggestions.clear();
                    self.apply(fields);
                }
            }
            "apply" => self.apply(fields),
            "older" => self.older(),
            "pause" if matches!(self.following, Following::Yes | Following::Starting) => self.stop(Following::Paused),
            "resume" | "refresh" => self.restart(),
            "only" => {
                if let Some(row) = row {
                    self.only(row, fields);
                }
            }
            _ => {}
        }
    }

    /// A checkbox or a list applies as soon as it changes; what is typed
    /// waits for Enter.
    fn input(&mut self, name: &str, fields: &mut Fields) {
        if matches!(name, "errors" | "range" | "source") {
            self.apply(fields);
        }
    }
}

/// What the window is called: what it shows, where that is one service's
/// logs or one type of event, so that two windows can be told apart.
pub fn title(filter: &Filter) -> String {
    let narrowed = match filter.kind {
        Kind::Logs => &filter.origin,
        Kind::Events => &filter.event_type,
    };
    if narrowed.is_empty() { "Event Viewer".into() } else { format!("Event Viewer: {narrowed}") }
}

/// Follows `text` until it ends, the window goes, or another takes its
/// place, putting what comes on the screen no more often than `BETWEEN`.
fn tail(window: &Weak<Surface<Viewer>>, generation: &AtomicU64, mine: u64, socket: &std::path::Path, text: &str) {
    let tail = match Tail::start(socket, text, WAITING) {
        Ok(tail) => tail,
        Err(error) => {
            if let Some(window) = window.upgrade() {
                window.update(|viewer, _| viewer.ended(mine, &error));
            }
            return;
        }
    };
    loop {
        if generation.load(Ordering::SeqCst) != mine {
            return;
        }
        let first = match tail.updates().recv_timeout(Duration::from_millis(500)) {
            Ok(first) => first,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Everything waiting goes on the screen at once.
        let mut batch = Vec::new();
        let mut ended = None;
        let mut initial = None;
        for report in std::iter::once(first).chain(std::iter::from_fn(|| tail.updates().try_recv().ok())) {
            match report {
                Tailed::Initial(records) => initial = Some(records),
                Tailed::Live(records) => batch.extend(records),
                Tailed::Ended(error) => ended = Some(error),
            }
        }
        let Some(window) = window.upgrade() else { return };
        window.update(|viewer, _| {
            if let Some(records) = initial {
                viewer.initial(mine, records);
            }
            if !batch.is_empty() {
                viewer.live(mine, batch);
            }
            if let Some(error) = &ended {
                viewer.ended(mine, error);
            }
        });
        if ended.is_some() {
            return;
        }
        drop(window);
        std::thread::sleep(BETWEEN);
    }
}

/// Why eventd gave nothing, in words.
fn trouble(error: &Error) -> String {
    match error {
        Error::Connect { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied => {
            "eventd, which keeps the logs and events, doesn't let you ask it anything.".into()
        }
        Error::Connect { .. } => "eventd, which keeps the logs and events, can't be reached. It may not be running.".into(),
        Error::Channel(error) => format!("The connection to eventd failed: {error}."),
        Error::Refused(message) => format!("eventd refused: {message}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(at: i64, fields: &[(&str, Value)]) -> Record {
        let mut record: Record = fields.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
        record.insert("timestamp".into(), Value::Signed(at));
        record
    }

    fn log(at: i64, origin: &str, message: &str, error: bool) -> Record {
        record(at, &[("origin", Value::String(origin.into())), ("message", Value::String(message.into())), ("is_error", Value::Bool(error))])
    }

    fn viewer(kind: Kind) -> Viewer {
        let mut viewer = Viewer::new(kind);
        viewer.clock = Clock::Fixed("2026-10-03T12:20:00Z".parse().unwrap(), jiff::tz::TimeZone::UTC);
        viewer
    }

    fn shown(viewer: &Viewer) -> String {
        let mut fields = Fields::default();
        fields.set("range", "24h");
        viewer.render(&Facts { views: 1, fields: &fields })
    }

    fn messages(viewer: &Viewer) -> Vec<String> {
        viewer.rows.iter().map(|row| words::value(&row.record["message"])).collect()
    }

    #[test]
    fn the_first_page_then_live_batches_newest_first_whatever_order_they_came_in() {
        let mut viewer = viewer(Kind::Logs);
        viewer.initial(0, vec![log(20, "sshd", "b", false), log(10, "sshd", "a", true)]);
        assert_eq!(viewer.following, Following::Yes);
        assert!(viewer.complete);
        viewer.live(0, vec![log(30, "sshd", "c", false), log(40, "sshd", "d", false)]);
        assert_eq!(messages(&viewer), ["d", "c", "b", "a"]);
        // A tail that was replaced says nothing more.
        viewer.live(7, vec![log(50, "sshd", "e", false)]);
        assert_eq!(viewer.rows.len(), 4);
        let html = shown(&viewer);
        assert!(html.contains("class=\"row error\""));
        assert!(html.contains("That is everything in the last 24 hours."));
        assert!(html.contains("4 logs · live"));
    }

    #[test]
    fn following_lets_the_oldest_go_and_paging_back_the_newest() {
        let mut viewer = viewer(Kind::Logs);
        viewer.initial(0, (0..PAGE as i64).rev().map(|at| log(1000 + at, "a", "x", false)).collect());
        assert!(!viewer.complete);
        viewer.live(0, (0..HELD as i64).map(|at| log(5000 + at, "a", "new", false)).collect());
        assert_eq!(viewer.rows.len(), HELD);
        assert!(viewer.rows.iter().all(|row| words::value(&row.record["message"]) == "new"));
        // Paged back past what is held, the newest go and it stops following.
        let anchor = viewer.rows.back().unwrap().id;
        viewer.paged(0, anchor, Ok((0..PAGE as i64).rev().map(|at| log(at, "a", "old", false)).collect()));
        assert_eq!(viewer.rows.len(), HELD);
        assert_eq!(viewer.following, Following::Back);
        assert!(shown(&viewer).contains("Showing older logs. Newer ones were let go to make room."));
    }

    #[test]
    fn a_page_asked_for_before_its_rows_were_let_go_is_dropped() {
        let mut viewer = viewer(Kind::Logs);
        viewer.initial(0, (0..PAGE as i64).rev().map(|at| log(1000 + at, "a", "x", false)).collect());
        let anchor = viewer.rows.back().unwrap().id;
        // While the page is on its way, enough new ones come that the row
        // it was asked from goes.
        viewer.live(0, (0..HELD as i64).map(|at| log(5000 + at, "a", "new", false)).collect());
        let oldest = viewer.rows.back().unwrap().id;
        viewer.paged(0, anchor, Ok((0..PAGE as i64).rev().map(|at| log(at, "a", "old", false)).collect()));
        assert_eq!(viewer.rows.len(), HELD);
        assert_eq!(viewer.rows.back().unwrap().id, oldest);
        assert_eq!(viewer.following, Following::Yes);
        assert!(!viewer.paging);
        assert!(shown(&viewer).contains("Show older</button>"));
    }

    #[test]
    fn an_older_page_steps_over_the_records_shown_at_its_time() {
        let mut viewer = viewer(Kind::Logs);
        viewer.initial(0, (0..PAGE as i64).map(|at| log(if at < 197 { 100 - at } else { 3 }, "a", "x", false)).collect());
        assert_eq!(viewer.filter.older(3, 3), "LOGS SINCE 24h ago WHERE timestamp <= 3 SKIP 3 TAKE 200");
        let oldest = viewer.rows.back().and_then(|row| words::timestamp(&row.record));
        let shown = viewer.rows.iter().rev().take_while(|row| words::timestamp(&row.record) == oldest).count();
        assert_eq!((oldest, shown), (Some(3), 3));
    }

    #[test]
    fn a_failure_is_said_in_words_and_where_it_happened() {
        let mut viewer = viewer(Kind::Events);
        viewer.ended(0, &Error::Refused("query needs more memory than the collector allows queries".into()));
        assert!(shown(&viewer).contains("eventd refused: query needs more memory than the collector allows queries."));
        let mut viewer = self::viewer(Kind::Logs);
        viewer.initial(0, vec![log(1, "a", "x", false)]);
        viewer.ended(0, &Error::Channel(eventd_client::wire::Error::UnexpectedStatus));
        let html = shown(&viewer);
        assert!(html.contains("New logs stopped coming:"));
        assert!(html.contains("fx-click=\"resume\">Show the newest</button>"));
    }

    #[test]
    fn what_the_policy_hides_is_said() {
        let mut viewer = viewer(Kind::Logs);
        viewer.initial(0, Vec::new());
        assert!(shown(&viewer).contains("There are no logs you may read in the last 24 hours."));
        viewer.readable = [Some(Readable::Nothing), None];
        let html = shown(&viewer);
        assert!(html.contains("You can't read logs on this machine, so there are none to show."));
        viewer.readable = [Some(Readable::Some { hidden: vec!["sshd".into(), "authd".into()] }), None];
        assert!(shown(&viewer).contains("Hidden from you: logs from sshd*, authd*."));
        viewer.readable = [Some(Readable::Some { hidden: vec!["*".into()] }), None];
        assert!(shown(&viewer).contains("Only some logs are yours to read"));
        viewer.readable = [Some(Readable::Unknown("its policy can't be read (access denied)".into())), None];
        assert!(shown(&viewer).contains("Some logs may be hidden from you: its policy can't be read (access denied)."));
    }

    #[test]
    fn the_details_give_every_field() {
        let mut viewer = viewer(Kind::Events);
        let event = record(
            1_791_017_896_126_088_777,
            &[
                ("event_type", Value::String("graph.operation_terminal".into())),
                ("origin_class", Value::Signed(0)),
                ("service", Value::String("timed".into())),
                ("user_sid", Value::Binary(vec![1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0])),
            ],
        );
        viewer.initial(0, vec![event]);
        let id = viewer.rows[0].id;
        viewer.event("pick", &serde_json::json!({ "row": id.to_string() }), &mut Fields::default());
        let html = shown(&viewer);
        assert!(html.contains("<h2>graph.operation_terminal</h2>"));
        assert!(html.contains("<dt>Source<code>origin_class</code></dt><dd>Programs</dd>"));
        assert!(html.contains("<dt>Time<code>timestamp</code></dt><dd>Saturday 3 October 2026, 08:58:16.126088777 (+00:00)</dd>"));
        assert!(html.contains("<h3>Its fields</h3><dl><dt>service</dt><dd>timed</dd><dt>user_sid</dt><dd>S-1-5-18</dd></dl>"));
        assert!(html.contains("<span class=\"type\">graph.operation_terminal</span><span class=\"source\">Programs</span></button>"));
        // Events come first.
        assert!(html.find("fx-value-kind=\"events\"") < html.find("fx-value-kind=\"logs\""));
    }
}
