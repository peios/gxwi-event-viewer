//! eventd's settings: every value under `Machine\System\eventd`, as
//! eventd's own regman page documents it (`/usr/share/regman/eventd.regman`,
//! which `regman` shows on a terminal), and what each is set to here.
//!
//! The meaning, the default, the range and when a change applies are the
//! page's, not this program's, so a new eventd with a new value is shown
//! with nothing here changed. What this adds is which group a value goes
//! in. A value set in the registry that the page does not know is shown
//! too, as it is.
//!
//! A change is written to the registry, where eventd watches for it and
//! applies it at once, or at its next start for the few that say so. Who
//! may change them is the key's to say: the tab asks for the access a
//! change takes, and says why not when it is refused.

use std::collections::BTreeMap;

use peios::registry::{Key, KeyAccess, OpenFlags, ValueType};

pub const REGMAN: &str = "/usr/share/regman/eventd.regman";
pub const KEY: &str = "Machine\\System\\eventd";
const EACCES: i32 = 13;
const ENOENT: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dword,
    Qword,
    Text,
    Other,
}

/// One of eventd's values, as its regman page documents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub name: String,
    pub kind: Kind,
    pub default: String,
    /// The range, in the page's words.
    pub valid: String,
    pub applies: String,
    /// The first paragraph of what the page says of it.
    pub about: String,
}

impl Setting {
    /// The range as numbers, where the page gives it as `A to B`.
    pub fn range(&self) -> Option<(u64, u64)> {
        range(&self.valid)
    }

    /// Whether a change waits for eventd's next start.
    pub fn at_restart(&self) -> bool {
        self.applies.trim().eq_ignore_ascii_case("restart")
    }
}

/// eventd's values, from its regman page's text: the records documenting
/// one value of `Machine\System\eventd`.
pub fn documented(page: &str) -> Vec<Setting> {
    // A record is a fence line, a header to the first blank line, and a
    // body to the next fence.
    let mut records: Vec<Vec<&str>> = Vec::new();
    for line in page.lines() {
        if line.starts_with("--- ") {
            records.push(Vec::new());
        } else if let Some(record) = records.last_mut() {
            record.push(line);
        }
    }
    let mut settings = Vec::new();
    for record in records {
        let mut lines = record.into_iter();
        let mut header: BTreeMap<String, String> = BTreeMap::new();
        for line in lines.by_ref() {
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                header.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let body: Vec<&str> = lines.collect();
        let Some(canonical) = header.get("canonical") else { continue };
        let Some(name) = canonical.strip_prefix(KEY).and_then(|rest| rest.strip_prefix(' ')) else { continue };
        let about = body.join("\n").trim().split("\n\n").next().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
        let kind = match header.get("type").map(String::as_str) {
            Some("REG_DWORD") => Kind::Dword,
            Some("REG_QWORD") => Kind::Qword,
            Some("REG_SZ") => Kind::Text,
            _ => Kind::Other,
        };
        let field = |name: &str| header.get(name).cloned().unwrap_or_default();
        settings.push(Setting { name: name.trim().into(), kind, default: field("default"), valid: field("valid"), applies: field("applies"), about });
    }
    settings
}

/// `A to B`, or `A–B`, as numbers.
fn range(valid: &str) -> Option<(u64, u64)> {
    let (low, high) = valid.split_once(" to ").or_else(|| valid.split_once('–'))?;
    let number = |text: &str| text.trim().replace([',', '_'], "").parse::<u64>().ok();
    Some((number(low)?, number(high)?))
}

/// The groups the tab shows values in, in order.
pub const GROUPS: [&str; 6] = ["Keeping records", "Taking in events", "Taking in logs", "Taking in metrics", "Answering queries", "Storage"];

/// Which group a value goes in, by its name.
pub fn group(name: &str) -> &'static str {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| name.starts_with(prefix));
    if name.contains("Retention") {
        "Keeping records"
    } else if name.ends_with("Path") || starts(&["WalCheckpoint", "StorageShards"]) {
        "Storage"
    } else if starts(&["Log", "MaxLogDatagram"]) {
        "Taking in logs"
    } else if starts(&["Metric", "MaxMetricDatagram", "AdaptiveRollup", "HealthMetric"]) {
        "Taking in metrics"
    } else if starts(&["MaxBatch", "Shedding", "EmergencyShedding", "AdaptiveIndex"]) {
        "Taking in events"
    } else {
        "Answering queries"
    }
}

/// What a value is counted in, from the last word of its name.
pub fn unit(name: &str) -> Option<&'static str> {
    [("Ms", "ms"), ("Seconds", "seconds"), ("Minutes", "minutes"), ("Hours", "hours"), ("Days", "days"), ("Bytes", "bytes"), ("Percent", "%"), ("Pages", "pages"), ("Rows", "rows")]
        .into_iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map(|(_, unit)| unit)
}

/// What a value is set to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Set {
    Number(u64),
    Text(String),
    /// Of a type the tab does not show, as the registry gives it.
    Other(String),
}

impl Set {
    pub fn said(&self) -> String {
        match self {
            Set::Number(number) => number.to_string(),
            Set::Text(text) | Set::Other(text) => text.clone(),
        }
    }
}

/// eventd's values as they are set here, and why they cannot be changed,
/// if they cannot.
pub struct Here {
    pub values: BTreeMap<String, Set>,
    pub unchangeable: Option<String>,
}

pub fn read() -> Result<Here, String> {
    let key = Key::open(None, KEY, KeyAccess::QUERY_VALUE, OpenFlags::empty()).map_err(|error| {
        if error.raw_os_error() == Some(EACCES) { "You may not read eventd's settings.".to_string() } else { format!("eventd's settings could not be read: {error}.") }
    })?;
    let mut values = BTreeMap::new();
    for record in key.query_values_batch(None).map_err(|error| format!("eventd's settings could not be read: {error}."))? {
        let name = String::from_utf8_lossy(&record.name).into_owned();
        if name.is_empty() {
            continue;
        }
        values.insert(name, set(record.ty, &record.data));
    }
    let unchangeable = match Key::open(None, KEY, KeyAccess::SET_VALUE, OpenFlags::empty()) {
        Ok(_) => None,
        Err(error) if error.raw_os_error() == Some(EACCES) => Some("Only those allowed to change eventd's settings may change them: on this machine, administrators.".into()),
        Err(error) => Some(format!("Whether you may change them could not be found out: {error}.")),
    };
    Ok(Here { values, unchangeable })
}

fn set(ty: ValueType, data: &[u8]) -> Set {
    match ty {
        ValueType::DWORD if data.len() == 4 => Set::Number(u64::from(u32::from_le_bytes(data.try_into().expect("four bytes")))),
        ValueType::QWORD if data.len() == 8 => Set::Number(u64::from_le_bytes(data.try_into().expect("eight bytes"))),
        ValueType::SZ | ValueType::EXPAND_SZ => Set::Text(String::from_utf8_lossy(data).trim_end_matches('\0').to_string()),
        _ => Set::Other(format!("{} bytes of type {:#x}", data.len(), ty.0)),
    }
}

/// What `typed` comes to as `setting`'s value, or why it cannot be one.
pub fn value(setting: &Setting, typed: &str) -> Result<u64, String> {
    let number: u64 = typed.trim().replace([',', '_', ' '], "").parse().map_err(|_| format!("{} takes a whole number.", setting.name))?;
    if setting.kind == Kind::Dword && number > u64::from(u32::MAX) {
        return Err(format!("{} takes a number no larger than {}.", setting.name, u32::MAX));
    }
    if let Some((low, high)) = setting.range()
        && !(low..=high).contains(&number)
    {
        return Err(format!("{} takes {} to {}.", setting.name, low, high));
    }
    Ok(number)
}

/// Sets `setting` to `number`.
pub fn write(setting: &Setting, number: u64) -> Result<(), String> {
    let key = Key::open(None, KEY, KeyAccess::SET_VALUE, OpenFlags::empty()).map_err(|error| refused(&error))?;
    let (ty, data) = match setting.kind {
        Kind::Dword => (ValueType::DWORD, u32::try_from(number).map_err(|_| "it is too large".to_string())?.to_le_bytes().to_vec()),
        Kind::Qword => (ValueType::QWORD, number.to_le_bytes().to_vec()),
        Kind::Text | Kind::Other => return Err(format!("{} is not changed here.", setting.name)),
    };
    key.set_value(setting.name.as_bytes(), ty, &data).call().map_err(|error| refused(&error))
}

/// Takes `name` back to its default, by taking its value away.
pub fn unset(name: &str) -> Result<(), String> {
    let key = Key::open(None, KEY, KeyAccess::SET_VALUE, OpenFlags::empty()).map_err(|error| refused(&error))?;
    match key.delete_value(name.as_bytes(), None, None) {
        Err(error) if error.raw_os_error() != Some(ENOENT) => Err(refused(&error)),
        _ => Ok(()),
    }
}

fn refused(error: &peios::Error) -> String {
    if error.raw_os_error() == Some(EACCES) { "You may not change eventd's settings.".into() } else { error.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "--- machine\\system\\eventd
canonical: Machine\\System\\eventd

eventd's storage, ingestion and query policy.

--- machine\\system\\eventd eventstorepath
canonical: Machine\\System\\eventd EventStorePath
type: REG_SZ
default: (required; standard /var/state/eventd/events/)
applies: restart

Provisioned protected directory holding event shards.

--- machine\\system\\eventd maxqueriesperuser
canonical: Machine\\System\\eventd MaxQueriesPerUser
type: REG_DWORD
default: 16
valid: 1 to 4096
applies: live

Queries, streaming or not, that one caller's user SID may have running at
once. SYSTEM's are not counted.

Excess requests are rejected, not queued.

--- machine\\system\\eventd eventretentionmaxbytes
canonical: Machine\\System\\eventd EventRetentionMaxBytes
type: REG_QWORD
default: 0
valid: 0–18446744073709551615
applies: live

The most the event stores may hold; 0 is no limit.

--- machine\\system\\kmes other
canonical: Machine\\System\\KMES Other
type: REG_DWORD

Not eventd's.
";

    #[test]
    fn the_page_gives_each_value_its_meaning() {
        let settings = documented(PAGE);
        let names: Vec<&str> = settings.iter().map(|setting| setting.name.as_str()).collect();
        assert_eq!(names, ["EventStorePath", "MaxQueriesPerUser", "EventRetentionMaxBytes"]);
        let per_user = &settings[1];
        assert_eq!(per_user.kind, Kind::Dword);
        assert_eq!(per_user.default, "16");
        assert_eq!(per_user.range(), Some((1, 4096)));
        assert!(!per_user.at_restart());
        assert_eq!(per_user.about, "Queries, streaming or not, that one caller's user SID may have running at once. SYSTEM's are not counted.");
        assert_eq!(settings[2].kind, Kind::Qword);
        assert_eq!(settings[2].range(), Some((0, u64::MAX)));
        assert!(settings[0].at_restart());
        assert_eq!(settings[0].range(), None);
    }

    #[test]
    fn a_typed_value_is_checked_against_the_range() {
        let settings = documented(PAGE);
        let per_user = &settings[1];
        assert_eq!(value(per_user, " 32 "), Ok(32));
        assert_eq!(value(per_user, "1,024"), Ok(1024));
        assert_eq!(value(per_user, "0"), Err("MaxQueriesPerUser takes 1 to 4096.".into()));
        assert_eq!(value(per_user, "lots"), Err("MaxQueriesPerUser takes a whole number.".into()));
        assert_eq!(value(&settings[2], "268435456"), Ok(268_435_456));
    }

    #[test]
    fn values_are_grouped_and_counted_by_their_names() {
        assert_eq!(group("EventRetentionDays"), "Keeping records");
        assert_eq!(group("RetentionCheckIntervalMinutes"), "Keeping records");
        assert_eq!(group("LogStorePath"), "Storage");
        assert_eq!(group("LogMaxBatchSize"), "Taking in logs");
        assert_eq!(group("HealthMetricIntervalSeconds"), "Taking in metrics");
        assert_eq!(group("SheddingWindowSeconds"), "Taking in events");
        assert_eq!(group("MaxQueriesPerUser"), "Answering queries");
        assert_eq!(group("CrossTypeWindowMs"), "Answering queries");
        assert!(GROUPS.contains(&group("Anything")));
        assert_eq!(unit("QueryTimeoutMs"), Some("ms"));
        assert_eq!(unit("MaxQueryHeldBytes"), Some("bytes"));
        assert_eq!(unit("MaxQueriesPerUser"), None);
    }

    #[test]
    fn values_are_read_as_their_types() {
        assert_eq!(set(ValueType::DWORD, &16u32.to_le_bytes()), Set::Number(16));
        assert_eq!(set(ValueType::QWORD, &(1u64 << 40).to_le_bytes()), Set::Number(1 << 40));
        assert_eq!(set(ValueType::SZ, b"/var/state/eventd/events/\0"), Set::Text("/var/state/eventd/events/".into()));
        assert_eq!(set(ValueType::DWORD, &[1, 2]), Set::Other("2 bytes of type 0x4".into()));
    }
}
