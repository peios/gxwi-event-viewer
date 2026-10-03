//! The query text the window sends eventd (PSPU book 3), made from what the
//! person chose. What they typed goes in quoted (`eventd_client::text`), so
//! nothing typed can change what the query is.
//!
//! The first page and the live tail are one query: `TAKE` applies only to
//! the result at the start of a `STREAM` (§3.27), so `… TAKE 200 STREAM`
//! is the newest page and then everything after it. Older pages are queries
//! of their own, newest first from where the window has got to: every
//! record at or before the oldest one shown, less those shown already at
//! that very time, of which there may be several. The order is total
//! (§3.21), so the ones shown come first and `SKIP` steps over exactly them.

use eventd_client::text;

/// How many records a page is.
pub const PAGE: usize = 200;

/// Which of eventd's records the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Logs,
    Events,
}

impl Kind {
    pub fn named(name: &str) -> Option<Kind> {
        match name {
            "logs" => Some(Kind::Logs),
            "events" => Some(Kind::Events),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Logs => "logs",
            Kind::Events => "events",
        }
    }
}

/// How far back the window looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Minutes15,
    Hour,
    Day,
    Week,
    Any,
}

impl Range {
    /// In the order the person is offered them.
    pub const ALL: [Range; 5] = [Range::Minutes15, Range::Hour, Range::Day, Range::Week, Range::Any];

    /// What the window opens on.
    pub const DEFAULT: Range = Range::Day;

    pub fn name(self) -> &'static str {
        match self {
            Range::Minutes15 => "15m",
            Range::Hour => "1h",
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Any => "any",
        }
    }

    pub fn named(name: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|range| range.name() == name)
    }

    pub fn words(self) -> &'static str {
        match self {
            Range::Minutes15 => "Last 15 minutes",
            Range::Hour => "Last hour",
            Range::Day => "Last 24 hours",
            Range::Week => "Last 7 days",
            Range::Any => "Any time",
        }
    }

    /// Its `SINCE`, if it has one.
    fn since(self) -> Option<&'static str> {
        match self {
            Range::Minutes15 => Some("15m ago"),
            Range::Hour => Some("1h ago"),
            Range::Day => Some("24h ago"),
            Range::Week => Some("7d ago"),
            Range::Any => None,
        }
    }
}

/// What an event came from (`origin_class`, §3.23), as the person picks it.
pub const SOURCES: [(u8, &str); 4] = [(0, "Programs"), (1, "Kernel"), (2, "Security (KACS)"), (3, "Registry (LCS)")];

/// What the person has chosen to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub kind: Kind,
    pub range: Range,
    /// Logs: the origin, a service's name, with what runs under it
    /// (`sshd/ExecStartPre[0]`, `sshd/HealthCheck`). Empty is every origin.
    pub origin: String,
    /// Logs: only what came from standard error.
    pub errors: bool,
    /// Logs: only messages with this in them, case folded.
    pub containing: String,
    /// Events: a type pattern, where `*` matches anything. Empty is every
    /// type.
    pub event_type: String,
    /// Events: only those from this origin class.
    pub source: Option<u8>,
}

impl Filter {
    pub fn new(kind: Kind) -> Filter {
        Filter { kind, range: Range::DEFAULT, origin: String::new(), errors: false, containing: String::new(), event_type: String::new(), source: None }
    }

    /// The query without paging: what is chosen.
    fn chosen(&self) -> String {
        let mut query = String::new();
        match self.kind {
            Kind::Logs => {
                query.push_str("LOGS");
                if let Some(since) = self.range.since() {
                    query += &format!(" SINCE {since}");
                }
                if self.errors {
                    query.push_str(" ERROR ONLY");
                }
                let containing = self.containing.trim();
                if !containing.is_empty() {
                    query += &format!(" CONTAINING {}", text::string(containing));
                }
                let origin = self.origin.trim();
                if !origin.is_empty() {
                    query += &format!(" WHERE origin == {} OR origin STARTS_WITH {}", text::string(origin), text::string(&format!("{origin}/")));
                }
            }
            Kind::Events => {
                query.push_str("EVENTS");
                let pattern = self.event_type.trim();
                if !pattern.is_empty() {
                    query += &format!(" {}", text::string(pattern));
                }
                if let Some(since) = self.range.since() {
                    query += &format!(" SINCE {since}");
                }
                if let Some(source) = self.source {
                    query += &format!(" WHERE origin_class == {source}");
                }
            }
        }
        query
    }

    /// The newest page, and then everything recorded after it.
    pub fn first_page(&self) -> String {
        format!("{} TAKE {PAGE} STREAM", self.chosen())
    }

    /// The page before a record at `timestamp`, `shown` of the records at
    /// that very time being on the screen already.
    pub fn older(&self, timestamp: i64, shown: usize) -> String {
        let skip = if shown > 0 { format!(" SKIP {shown}") } else { String::new() };
        format!("{} WHERE timestamp <= {timestamp}{skip} TAKE {PAGE}", self.chosen())
    }

    /// What may be typed in the origin or type field, from what there is in
    /// the range: every origin, or every event type.
    pub fn suggestions(&self) -> String {
        let since = self.range.since().map(|since| format!(" SINCE {since}")).unwrap_or_default();
        match self.kind {
            Kind::Logs => format!("LOGS{since} DISTINCT origin"),
            Kind::Events => format!("EVENTS{since} DISTINCT event_type"),
        }
    }

    /// Whether anything narrows the records beyond the range.
    pub fn narrowed(&self) -> bool {
        match self.kind {
            Kind::Logs => self.errors || !self.containing.trim().is_empty() || !self.origin.trim().is_empty(),
            Kind::Events => !self.event_type.trim().is_empty() || self.source.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_put_every_choice_in_its_own_clause() {
        let mut filter = Filter::new(Kind::Logs);
        assert_eq!(filter.first_page(), "LOGS SINCE 24h ago TAKE 200 STREAM");
        filter.errors = true;
        filter.containing = "  connection refused ".into();
        filter.origin = "sshd".into();
        filter.range = Range::Any;
        assert_eq!(
            filter.first_page(),
            "LOGS ERROR ONLY CONTAINING \"connection refused\" WHERE origin == \"sshd\" OR origin STARTS_WITH \"sshd/\" TAKE 200 STREAM"
        );
        assert!(filter.narrowed());
    }

    #[test]
    fn what_is_typed_cannot_change_the_query() {
        let mut filter = Filter::new(Kind::Logs);
        filter.containing = "\" STREAM WHERE x == \"y".into();
        assert_eq!(filter.first_page(), "LOGS SINCE 24h ago CONTAINING \"\\\" STREAM WHERE x == \\\"y\" TAKE 200 STREAM");
        let mut filter = Filter::new(Kind::Events);
        filter.event_type = "kacs.* SORT".into();
        assert_eq!(filter.first_page(), "EVENTS \"kacs.* SORT\" SINCE 24h ago TAKE 200 STREAM");
    }

    #[test]
    fn events_take_a_pattern_and_a_source() {
        let mut filter = Filter::new(Kind::Events);
        assert!(!filter.narrowed());
        filter.event_type = "job.*".into();
        filter.source = Some(2);
        filter.range = Range::Hour;
        assert_eq!(filter.first_page(), "EVENTS \"job.*\" SINCE 1h ago WHERE origin_class == 2 TAKE 200 STREAM");
        assert_eq!(filter.suggestions(), "EVENTS SINCE 1h ago DISTINCT event_type");
    }

    #[test]
    fn an_older_page_starts_after_the_records_shown_at_its_time() {
        let mut filter = Filter::new(Kind::Logs);
        filter.range = Range::Any;
        assert_eq!(filter.older(1_791_017_896_125_870_148, 0), "LOGS WHERE timestamp <= 1791017896125870148 TAKE 200");
        assert_eq!(filter.older(1_791_017_896_125_870_148, 2), "LOGS WHERE timestamp <= 1791017896125870148 SKIP 2 TAKE 200");
        assert_eq!(filter.suggestions(), "LOGS DISTINCT origin");
    }

    #[test]
    fn ranges_are_named_both_ways() {
        for range in Range::ALL {
            assert_eq!(Range::named(range.name()), Some(range));
        }
        assert_eq!(Range::named("forever"), None);
    }
}
