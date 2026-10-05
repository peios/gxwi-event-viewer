//! Who may read what of eventd's: its read policy, a descriptor for each
//! pattern of event types, log origins or metric names, and the one for
//! changing eventd's own policy (eventd TRM §7.2).
//!
//! Each pattern is said in words, from its access list: who may read it,
//! who may read only some of its fields, who may not, and for metrics who
//! may publish. A pattern opens in gxwi-sd-editor to be changed, and what
//! comes back is written over the registry value it came from. Adding a
//! pattern starts it with the descriptor that applies to that name now, so
//! nothing changes until the person changes it; the wildcards and the
//! administrative descriptor cannot be removed, since eventd denies
//! everything a missing one would have covered.
//!
//! Whether each can be changed, added or removed is the registry's to say,
//! and the tab asks for the access each takes rather than guessing.

use std::collections::BTreeMap;

use eventd_client::access::{
    self, ADMIN_KEY, EVENTD_ADMINISTER, EVENTD_PUBLISH, EVENTD_READ, GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, Namespace, SECURITY_ROOT,
};
use eventd_client::text;
use gxwi_sd_editor::names::Names;
use gxwi_sd_editor::{Can, Children, Generic, Object, Part, Request, Right, splice};
use peios::registry::{CreateFlags, Key, KeyAccess, OpenFlags, ValueType};
use peios::security::{AceType, SecurityDescriptor};

const EACCES: i32 = 13;
const ENOENT: i32 = 2;

/// The generic rights as an access mask carries them, before mapping.
const ALL: u32 = 0x1000_0000;
const WRITE: u32 = 0x4000_0000;
const READ: u32 = 0x8000_0000;

/// Which part of the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Space {
    Events,
    Logs,
    Metrics,
    Admin,
}

impl Space {
    pub const ALL: [Space; 4] = [Space::Events, Space::Logs, Space::Metrics, Space::Admin];

    pub fn name(self) -> &'static str {
        match self {
            Space::Events => "events",
            Space::Logs => "logs",
            Space::Metrics => "metrics",
            Space::Admin => "admin",
        }
    }

    pub fn named(name: &str) -> Option<Space> {
        Space::ALL.into_iter().find(|space| space.name() == name)
    }

    fn namespace(self) -> Option<Namespace> {
        match self {
            Space::Events => Some(Namespace::Events),
            Space::Logs => Some(Namespace::Logs),
            Space::Metrics => Some(Namespace::Metrics),
            Space::Admin => None,
        }
    }

    fn path(self, pattern: &str) -> String {
        self.namespace().map_or_else(|| ADMIN_KEY.to_string(), |namespace| access::descriptor_path(namespace, pattern))
    }

    pub fn heading(self) -> &'static str {
        match self {
            Space::Events => "Events",
            Space::Logs => "Logs",
            Space::Metrics => "Metrics",
            Space::Admin => "Changing eventd's indexes",
        }
    }

    /// What a pattern of this part covers, for the person.
    pub fn covers(self, pattern: &str) -> String {
        match (self, pattern) {
            (Space::Events, "*") => "Every event, unless a pattern below says otherwise".into(),
            (Space::Logs, "*") => "Every log, unless a pattern below says otherwise".into(),
            (Space::Metrics, "*") => "Every metric, unless a pattern below says otherwise".into(),
            (Space::Events, pattern) => format!("Events of type {pattern} and {pattern}.*"),
            (Space::Logs, pattern) => format!("Logs from {pattern}, and from what runs under it"),
            (Space::Metrics, pattern) => format!("Metrics named {pattern} and {pattern}.*"),
            (Space::Admin, _) => "Who may ask eventd to index a field (INDEX)".into(),
        }
    }

    /// Its rights, as the editor offers them.
    fn rights(self) -> Vec<Right> {
        let right = |name: &str, mask: u32| Right { name: name.into(), mask, general: true };
        match self {
            Space::Events | Space::Logs => vec![right("Read", EVENTD_READ)],
            Space::Metrics => vec![right("Read", EVENTD_READ), right("Publish", EVENTD_PUBLISH)],
            Space::Admin => vec![right("Administer", EVENTD_ADMINISTER)],
        }
    }

    /// Whether a pattern of this part may be removed: not a wildcard, which
    /// is load-bearing, not the administrative descriptor, and not the
    /// one eventd keeps for its own health metrics, which it makes again
    /// at its next start (eventd TRM §5.7).
    fn removable(self, pattern: &str) -> bool {
        self != Space::Admin && pattern != "*" && (self, pattern) != (Space::Metrics, "eventd")
    }
}

/// One pattern, said in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub space: Space,
    pub pattern: String,
    /// Who, and what they may or may not do, in the access list's order.
    pub grants: Vec<(String, String)>,
    /// Why it cannot be changed, if it cannot.
    pub unchangeable: Option<String>,
    pub removable: bool,
}

/// The whole policy, as the person may see and change it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub entries: Vec<Entry>,
    /// The parts the person may add a pattern to.
    pub addable: Vec<Space>,
    /// What could not be read, in words.
    pub trouble: Vec<String>,
}

/// Reads the policy, finding out who each SID is: on a thread, since the
/// authority is asked.
pub fn read() -> Policy {
    let mut names = Names::new();
    let mut policy = Policy::default();
    for space in Space::ALL {
        let patterns = match space.namespace() {
            Some(namespace) => match access::patterns(namespace) {
                Ok(mut patterns) => {
                    // The wildcard first, then by name.
                    patterns.sort_by_key(|pattern| (pattern != "*", pattern.clone()));
                    patterns
                }
                Err(error) => {
                    policy.trouble.push(format!("{} patterns could not be read: {}.", space.heading(), unreadable(&error)));
                    continue;
                }
            },
            None => vec!["Admin".to_string()],
        };
        for pattern in patterns {
            match entry(space, &pattern, &mut names) {
                Ok(Some(entry)) => policy.entries.push(entry),
                Ok(None) => {}
                Err(why) => policy.trouble.push(format!("{}: {why}.", space.covers(&pattern))),
            }
        }
        if let Some(namespace) = space.namespace() {
            let parent = format!("{SECURITY_ROOT}\\{}", namespace.registry_name());
            if Key::open(None, &parent, KeyAccess::CREATE_SUB_KEY, OpenFlags::empty()).is_ok() {
                policy.addable.push(space);
            }
        }
    }
    policy
}

fn entry(space: Space, pattern: &str, names: &mut Names) -> Result<Option<Entry>, String> {
    let path = space.path(pattern);
    let Some(descriptor) = load(&path)? else { return Ok(None) };
    let unchangeable = match Key::open(None, &path, KeyAccess::SET_VALUE, OpenFlags::empty()) {
        Ok(_) => None,
        Err(error) if error.raw_os_error() == Some(EACCES) => Some("You may not change this. On this machine, administrators may.".to_string()),
        Err(error) => Some(format!("Whether you may change this could not be found out: {error}.")),
    };
    let removable = space.removable(pattern) && Key::open(None, &path, KeyAccess::DELETE, OpenFlags::empty()).is_ok();
    Ok(Some(Entry { space, pattern: pattern.into(), grants: grants(&descriptor, space, names), unchangeable, removable }))
}

fn load(path: &str) -> Result<Option<SecurityDescriptor>, String> {
    let key = match Key::open(None, path, KeyAccess::QUERY_VALUE, OpenFlags::empty()) {
        Ok(key) => key,
        Err(error) if error.raw_os_error() == Some(ENOENT) => return Ok(None),
        Err(error) => return Err(unreadable(&error)),
    };
    let value = match key.query_value(b"", None) {
        Ok(value) => value,
        Err(error) if error.raw_os_error() == Some(ENOENT) => return Ok(None),
        Err(error) => return Err(unreadable(&error)),
    };
    if value.ty != ValueType::BINARY {
        return Err("it is not a security descriptor".into());
    }
    SecurityDescriptor::from_validated_bytes(value.data).map(Some).map_err(|error| format!("it is not a security descriptor ({error})"))
}

/// What a descriptor grants and denies, principal by principal, in its
/// access list's order.
fn grants(descriptor: &SecurityDescriptor, space: Space, names: &mut Names) -> Vec<(String, String)> {
    let Ok(view) = descriptor.view() else { return vec![("Nobody".into(), "its access list could not be read".into())] };
    let Some(dacl) = view.dacl() else { return vec![("Everyone".into(), "anything: it has no access list".into())] };
    let fields = known_fields();
    let mut said: Vec<(String, Vec<String>)> = Vec::new();
    for ace in dacl.iter() {
        let Some(sid) = ace.sid() else { continue };
        let mask = ace.mask();
        let does = |right: u32, generic: u32| mask & (right | generic | ALL) != 0;
        let mut rights = Vec::new();
        match space {
            Space::Events | Space::Logs => {
                if does(EVENTD_READ, READ) {
                    rights.push("read");
                }
            }
            Space::Metrics => {
                if does(EVENTD_READ, READ) {
                    rights.push("read");
                }
                if does(EVENTD_PUBLISH, WRITE) {
                    rights.push("publish");
                }
            }
            Space::Admin => {
                if does(EVENTD_ADMINISTER, WRITE) {
                    rights.push("index fields");
                }
            }
        }
        if rights.is_empty() {
            continue;
        }
        let rights = rights.join(" and ");
        let field = || {
            ace.object_type().map_or_else(
                || "a field".to_string(),
                |guid| fields.get(guid).cloned().unwrap_or_else(|| "a field of its own".into()),
            )
        };
        let what = match ace.ace_type() {
            AceType::AccessAllowed => format!("may {rights}"),
            AceType::AccessDenied => format!("may not {rights}"),
            AceType::Other(0x05 | 0x0b) => format!("may {rights} {}", field()),
            AceType::Other(0x06 | 0x0c) => format!("may not {rights} {}", field()),
            _ => continue,
        };
        names.learn(sid);
        let who = names.of(sid);
        match said.iter_mut().find(|(name, _)| *name == who) {
            Some((_, whats)) => whats.push(what),
            None => said.push((who, vec![what])),
        }
    }
    if said.is_empty() {
        return vec![("Nobody".into(), "may read these".into())];
    }
    said.into_iter().map(|(who, whats)| (who, whats.join("; "))).collect()
}

/// The fields eventd always has, by their GUIDs, so that a grant of one
/// can be said by name; a payload field's is said as "a field of its own".
fn known_fields() -> BTreeMap<[u8; 16], String> {
    crate::words::HEADERS
        .iter()
        .chain(["origin", "is_error", "message", "job_id", "name", "type", "value"].iter())
        .map(|field| (access::field_guid(field), format!("its {field} field")))
        .collect()
}

/// Opens `pattern` in gxwi-sd-editor; `changed` is called after each
/// change is written, and the editor goes when the person closes it.
pub fn edit(space: Space, pattern: &str, changed: impl Fn() + Send + 'static) -> Result<(), String> {
    let path = space.path(pattern);
    let (key, can) = match Key::open(None, &path, KeyAccess::QUERY_VALUE | KeyAccess::SET_VALUE, OpenFlags::empty()) {
        Ok(key) => (key, Can { dacl: true, ..Can::default() }),
        Err(error) if error.raw_os_error() == Some(EACCES) => {
            let key = Key::open(None, &path, KeyAccess::QUERY_VALUE, OpenFlags::empty()).map_err(|error| unreadable(&error))?;
            (key, Can { dacl: false, why: Some("You may not change who may read these. On this machine, administrators may.".into()), ..Can::default() })
        }
        Err(error) => return Err(unreadable(&error)),
    };
    let current = load(&path)?.ok_or("it has gone")?;
    let request = Request {
        object: Object { name: space.covers(pattern), kind: format!("eventd's {} policy", space.heading().to_lowercase()), container: false, children: Children::All, ..Object::default() },
        sd: current.as_bytes().to_vec(),
        rights: space.rights(),
        generic: Generic { read: GENERIC_READ, write: GENERIC_WRITE, execute: GENERIC_EXECUTE, all: GENERIC_ALL },
        can,
        ..Request::default()
    };
    let apply = move |sd: &[u8], parts: &[Part]| {
        let now = load(&path)?.ok_or("it has gone")?;
        let value = splice(now.as_bytes(), sd, parts)?;
        key.set_value(b"", ValueType::BINARY, &value).call().map_err(|error| refused(&error))?;
        changed();
        Ok(())
    };
    gxwi_sd_editor::edit(&request, apply, || {}).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound { "The permissions editor, gxwi-sd-editor, is not installed.".into() } else { format!("The permissions editor could not be started: {error}.") }
    })
}

/// Adds a pattern, with the descriptor that applies to that name now.
pub fn add(space: Space, pattern: &str) -> Result<(), String> {
    let pattern = pattern.trim();
    let Some(namespace) = space.namespace() else { return Err("There is only one administrative descriptor.".into()) };
    if !text::is_identifier(pattern) {
        return Err(format!("A pattern is a name such as sshd or kacs.access_denied: letters, digits, dots, dashes and underscores, not starting with a digit. \"{pattern}\" is not one."));
    }
    let path = access::descriptor_path(namespace, pattern);
    if load(&path)?.is_some() {
        return Err(format!("There is a pattern for {pattern} already."));
    }
    let now = match access::resolve(namespace, pattern) {
        Ok(Some((_, descriptor))) => descriptor,
        Ok(None) => return Err("No pattern applies to that name now, not even the wildcard, so there is nothing to start it from.".into()),
        Err(error) => return Err(unreadable(&error)),
    };
    let parent_path = format!("{SECURITY_ROOT}\\{}", namespace.registry_name());
    let (parent, _) = Key::create(None, &parent_path, KeyAccess::CREATE_SUB_KEY, CreateFlags::empty(), None, None).map_err(|error| refused(&error))?;
    let (key, _) = Key::create(Some(&parent), pattern, KeyAccess::SET_VALUE, CreateFlags::empty(), None, None).map_err(|error| refused(&error))?;
    key.set_value(b"", ValueType::BINARY, now.as_bytes()).call().map_err(|error| refused(&error))
}

pub fn remove(space: Space, pattern: &str) -> Result<(), String> {
    if !space.removable(pattern) {
        return Err(if pattern == "*" || space == Space::Admin {
            "This one cannot be removed: eventd would let nobody read what it covers.".into()
        } else {
            "This one cannot be removed: eventd makes it again when it next starts.".into()
        });
    }
    let key = Key::open(None, &space.path(pattern), KeyAccess::DELETE, OpenFlags::empty()).map_err(|error| refused(&error))?;
    key.delete_key(None, None).map_err(|error| refused(&error))
}

fn unreadable(error: &peios::Error) -> String {
    if error.raw_os_error() == Some(EACCES) { "you may not read it".into() } else { error.to_string() }
}

fn refused(error: &peios::Error) -> String {
    if error.raw_os_error() == Some(EACCES) { "you are not allowed to".into() } else { error.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peios::security::sddl;

    fn said(text: &str, space: Space) -> Vec<(String, String)> {
        grants(&sddl::parse(text).unwrap(), space, &mut Names::offline())
    }

    #[test]
    fn a_pattern_is_said_principal_by_principal() {
        assert_eq!(
            said("O:SYG:SYD:P(A;;0x1;;;SY)(A;;0x1;;;BA)(A;;0x1;;;AU)", Space::Logs),
            [
                ("Local System".to_string(), "may read".to_string()),
                ("BUILTIN\\Administrators".into(), "may read".into()),
                ("Authenticated Users".into(), "may read".into())
            ]
        );
        assert_eq!(
            said("O:SYG:SYD:P(A;;0x9;;;SY)(A;;0x1;;;AU)", Space::Metrics),
            [("Local System".to_string(), "may read and publish".to_string()), ("Authenticated Users".into(), "may read".into())]
        );
        assert_eq!(said("O:SYG:SYD:P(A;;GA;;;BA)", Space::Admin), [("BUILTIN\\Administrators".to_string(), "may index fields".to_string())]);
        assert_eq!(said("O:SYG:SYD:P(A;;0x4;;;SY)", Space::Logs), [("Nobody".to_string(), "may read these".to_string())]);
    }

    #[test]
    fn a_grant_of_fields_names_them() {
        let text = format!(
            "O:SYG:SYD:P(D;;0x1;;;AN)(A;;0x1;;;SY)(OA;;0x1;{};;BA)(OA;;0x1;{};;BA)(OA;;0x1;{};;BA)",
            "341d2267-b9db-536b-b36c-94ab6cd47e4c", "fe639b5f-4f7f-54c5-9702-f20fac24fa0e", "11111111-2222-5333-8444-555555555555"
        );
        assert_eq!(
            said(&text, Space::Logs),
            [
                ("Anonymous".to_string(), "may not read".to_string()),
                ("Local System".into(), "may read".into()),
                ("BUILTIN\\Administrators".into(), "may read its timestamp field; may read its message field; may read a field of its own".into())
            ]
        );
    }

    #[test]
    fn the_wildcards_and_the_administrative_descriptor_stay() {
        assert!(!Space::Logs.removable("*"));
        assert!(!Space::Admin.removable("Admin"));
        assert!(!Space::Metrics.removable("eventd"));
        assert!(Space::Metrics.removable("cpu"));
        assert!(Space::Logs.removable("sshd"));
        assert_eq!(Space::Logs.covers("sshd"), "Logs from sshd, and from what runs under it");
        assert_eq!(Space::Admin.path("Admin"), ADMIN_KEY);
        assert_eq!(Space::Metrics.path("eventd"), "Machine\\System\\eventd\\Security\\Metrics\\eventd");
    }
}
