//! What eventd's records say, where a person reads them: times on this
//! machine's clock, values, and what an event is about in one line.

use eventd_client::{Record, Value};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::query::SOURCES;

/// What times are said against: this machine's clock, or a fixed one for
/// the tests.
#[derive(Debug, Clone)]
pub enum Clock {
    Machine,
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Timestamp, TimeZone),
}

impl Clock {
    pub fn now(&self) -> (Timestamp, TimeZone) {
        match self {
            Clock::Machine => (Timestamp::now(), TimeZone::system()),
            Clock::Fixed(now, zone) => (*now, zone.clone()),
        }
    }
}

/// An event's header fields (§3.22), in the order the details give them.
/// Every other field is the event's own.
pub const HEADERS: [&str; 9] =
    ["timestamp", "event_type", "origin_class", "process_guid", "effective_token_guid", "true_token_guid", "boot_id", "cpu_id", "sequence"];

/// What a header field, or a log field, is called in the details.
pub fn field_name(field: &str) -> Option<&'static str> {
    Some(match field {
        "timestamp" => "Time",
        "event_type" => "Type",
        "origin_class" => "Source",
        "process_guid" => "Process",
        "effective_token_guid" => "Token",
        "true_token_guid" => "Real token",
        "boot_id" => "Boot",
        "cpu_id" => "Processor",
        "sequence" => "Sequence",
        "origin" => "From",
        "is_error" => "Stream",
        "message" => "Message",
        "job_id" => "Job",
        _ => return None,
    })
}

/// A record's time, in nanoseconds since 1970 (§3.5).
pub fn timestamp(record: &Record) -> Option<i64> {
    match record.get("timestamp")? {
        Value::Signed(at) => Some(*at),
        Value::Unsigned(at) => i64::try_from(*at).ok(),
        _ => None,
    }
}

/// A time as the list has it: the time of day, to the millisecond, and the
/// day when it is not today.
pub fn when(at: i64, now: Timestamp, zone: &TimeZone) -> String {
    let Ok(at) = Timestamp::from_nanosecond(i128::from(at)) else { return at.to_string() };
    let (at, now) = (at.to_zoned(zone.clone()), now.to_zoned(zone.clone()));
    if at.date() == now.date() {
        at.strftime("%H:%M:%S%.3f").to_string()
    } else if at.year() == now.year() {
        at.strftime("%-d %b %H:%M:%S%.3f").to_string()
    } else {
        at.strftime("%-d %b %Y %H:%M:%S").to_string()
    }
}

/// A time in full, as the details have it: to the nanosecond, with the
/// offset it is said at.
pub fn when_exactly(at: i64, zone: &TimeZone) -> String {
    match Timestamp::from_nanosecond(i128::from(at)) {
        Ok(at) => at.to_zoned(zone.clone()).strftime("%A %-d %B %Y, %H:%M:%S%.9f (%:z)").to_string(),
        Err(_) => at.to_string(),
    }
}

/// What an origin class is called.
pub fn source(class: &Value) -> String {
    let number = match class {
        Value::Signed(number) => u8::try_from(*number).ok(),
        Value::Unsigned(number) => u8::try_from(*number).ok(),
        _ => None,
    };
    match number.and_then(|number| SOURCES.iter().find(|(class, _)| *class == number)) {
        Some((_, name)) => (*name).to_string(),
        None => value(class),
    }
}

/// A value, as plain as it will go.
pub fn value(value: &Value) -> String {
    match value {
        Value::Null => "none".into(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::Signed(number) => number.to_string(),
        Value::Unsigned(number) => number.to_string(),
        Value::Float(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Binary(bytes) => sid(bytes).unwrap_or_else(|| hex(bytes)),
        Value::Array(items) => format!("[{}]", items.iter().map(self::value).collect::<Vec<_>>().join(", ")),
        Value::Map(entries) => format!("{{{}}}", entries.iter().map(|(key, item)| format!("{}: {}", self::value(key), self::value(item))).collect::<Vec<_>>().join(", ")),
        Value::Extension(kind, bytes) => format!("extension {kind}: {}", hex(bytes)),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Bytes that are a SID in its binary form, written as one (`S-1-5-18`).
/// Events carry SIDs as bytes, and nobody reads them as hex.
fn sid(bytes: &[u8]) -> Option<String> {
    let (&[revision, count], rest) = bytes.split_first_chunk::<2>()?;
    let (authority, rest) = rest.split_first_chunk::<6>()?;
    if revision != 1 || count > 15 || rest.len() != usize::from(count) * 4 {
        return None;
    }
    let authority = authority.iter().fold(0u64, |sum, byte| sum << 8 | u64::from(*byte));
    let parts: Vec<String> = rest.as_chunks::<4>().0.iter().map(|part| u32::from_le_bytes(*part).to_string()).collect();
    let tail = if parts.is_empty() { String::new() } else { format!("-{}", parts.join("-")) };
    Some(format!("S-1-{authority}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> (Timestamp, TimeZone) {
        // Saturday 3 October 2026, 13:20, on a clock an hour east of UTC.
        ("2026-10-03T12:20:00Z".parse().unwrap(), TimeZone::fixed(jiff::tz::offset(1)))
    }

    #[test]
    fn a_time_says_its_day_only_when_it_is_not_today() {
        let (now, zone) = clock();
        let at = |text: &str| i64::try_from(text.parse::<Timestamp>().unwrap().as_nanosecond()).unwrap();
        assert_eq!(when(at("2026-10-03T12:05:03.123456Z"), now, &zone), "13:05:03.123");
        // Yesterday in UTC, and today on this clock.
        assert_eq!(when(at("2026-10-02T23:30:00Z"), now, &zone), "00:30:00.000");
        assert_eq!(when(at("2026-10-02T12:00:00Z"), now, &zone), "2 Oct 13:00:00.000");
        assert_eq!(when(at("2025-12-31T12:00:00Z"), now, &zone), "31 Dec 2025 13:00:00");
        assert_eq!(when_exactly(at("2026-10-03T12:05:03.000000007Z"), &zone), "Saturday 3 October 2026, 13:05:03.000000007 (+01:00)");
    }

    #[test]
    fn values_are_said_plainly_and_sids_as_sids() {
        assert_eq!(value(&Value::Bool(true)), "yes");
        assert_eq!(value(&Value::Null), "none");
        let local_system = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        assert_eq!(value(&Value::Binary(local_system.to_vec())), "S-1-5-18");
        // Not a SID: its count does not match its length.
        assert_eq!(value(&Value::Binary(vec![1, 2, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0])), "010200000000000512000000");
        assert_eq!(value(&Value::Array(vec![Value::Signed(1), Value::String("a".into())])), "[1, a]");
        assert_eq!(source(&Value::Signed(2)), "Security (KACS)");
        assert_eq!(source(&Value::Signed(9)), "9");
    }

    #[test]
    fn a_records_time_is_its_timestamp() {
        let record: Record = [("timestamp".to_string(), Value::Signed(1))].into_iter().collect();
        assert_eq!(timestamp(&record), Some(1));
        assert_eq!(timestamp(&Record::new()), None);
    }
}
