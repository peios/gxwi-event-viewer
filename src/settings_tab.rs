//! The Settings tab: eventd's settings, in groups, and who may read what.
//! Each is read on a thread, since the policy asks the authority who each
//! SID is, and each is read again after anything here changes it.

use std::sync::Weak;

use libgxwi::{Fields, Surface, escape};

use crate::chart::{self, Unit};
use crate::policy::{self, Entry, Policy, Space};
use crate::settings::{self, GROUPS, Here, Kind, Set, Setting};
use crate::viewer::Viewer;

/// What one change came to, and of what: a setting's name, or a pattern's
/// space and name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Said {
    of: String,
    result: Result<String, String>,
}

pub struct SettingsTab {
    documented: Option<Result<Vec<Setting>, String>>,
    here: Option<Result<Here, String>>,
    policy: Option<Policy>,
    said: Option<Said>,
    /// The pattern the person has asked to remove, and is asked whether
    /// they mean it.
    removing: Option<(Space, String)>,
}

impl SettingsTab {
    pub fn new() -> SettingsTab {
        SettingsTab { documented: None, here: None, policy: None, said: None, removing: None }
    }

    /// Reads the settings and the policy, on threads.
    pub fn read(&mut self, window: &Weak<Surface<Viewer>>) {
        self.read_settings(window);
        self.read_policy(window);
    }

    pub fn read_settings(&self, window: &Weak<Surface<Viewer>>) {
        let Some(window) = window.upgrade() else { return };
        std::thread::spawn(move || {
            let documented = std::fs::read_to_string(settings::REGMAN)
                .map(|page| settings::documented(&page))
                .map_err(|error| format!("What eventd's settings mean could not be read from {}: {error}.", settings::REGMAN));
            let here = settings::read();
            window.update(|viewer, fields| viewer.settings.settings_read(documented, here, fields));
        });
    }

    pub fn read_policy(&self, window: &Weak<Surface<Viewer>>) {
        let Some(window) = window.upgrade() else { return };
        std::thread::spawn(move || {
            let read = policy::read();
            window.update(|viewer, _| viewer.settings.policy = Some(read));
        });
    }

    /// The settings as read, each one's value put in its field: what it is
    /// set to, or its default.
    fn settings_read(&mut self, documented: Result<Vec<Setting>, String>, here: Result<Here, String>, fields: &mut Fields) {
        if let (Ok(documented), Ok(here)) = (&documented, &here) {
            for setting in documented.iter().filter(|setting| matches!(setting.kind, Kind::Dword | Kind::Qword)) {
                let value = here.values.get(&setting.name).map_or_else(|| setting.default.clone(), Set::said);
                fields.set(&format!("v-{}", setting.name), &value);
            }
        }
        self.documented = Some(documented);
        self.here = Some(here);
    }

    fn setting(&self, name: &str) -> Option<&Setting> {
        self.documented.as_ref()?.as_ref().ok()?.iter().find(|setting| setting.name == name)
    }

    /// Sets a value to what is typed in its field.
    pub fn set(&mut self, name: &str, fields: &Fields, window: &Weak<Surface<Viewer>>) {
        let Some(setting) = self.setting(name).cloned() else { return };
        let result = settings::value(&setting, fields.get(&format!("v-{name}"))).and_then(|number| {
            settings::write(&setting, number)?;
            Ok(if setting.at_restart() { "Saved. eventd uses it when it next starts.".into() } else { "Saved. eventd uses it from now on.".into() })
        });
        // What was typed stays to be put right when it was refused.
        let saved = result.is_ok();
        self.said = Some(Said { of: name.into(), result });
        if saved {
            self.read_settings(window);
        }
    }

    pub fn unset(&mut self, name: &str, window: &Weak<Surface<Viewer>>) {
        let result = settings::unset(name).map(|()| "Back to its default.".to_string());
        self.said = Some(Said { of: name.into(), result });
        self.read_settings(window);
    }

    /// Opens a pattern in the permissions editor.
    pub fn edit(&mut self, space: Space, pattern: &str, window: &Weak<Surface<Viewer>>) {
        let (changed, of) = (window.clone(), format!("{}:{pattern}", space.name()));
        let applied = of.clone();
        let result = policy::edit(space, pattern, move || {
            if let Some(window) = changed.upgrade() {
                let of = applied.clone();
                window.update(move |viewer, _| {
                    viewer.settings.said = Some(Said { of, result: Ok("Saved. eventd checks it from the next question asked of it.".into()) });
                    viewer.policy_changed();
                });
            }
        });
        self.said = Some(Said { of, result: result.map(|()| "Open in the permissions editor.".into()) });
    }

    /// Adds a pattern of what is typed for `space`, and opens it.
    pub fn add(&mut self, space: Space, fields: &mut Fields, window: &Weak<Surface<Viewer>>) {
        let pattern = fields.get(&format!("add-{}", space.name())).trim().to_string();
        match policy::add(space, &pattern) {
            Ok(()) => {
                fields.set(&format!("add-{}", space.name()), "");
                self.edit(space, &pattern, window);
                self.read_policy(window);
            }
            Err(why) => self.said = Some(Said { of: format!("add:{}", space.name()), result: Err(why) }),
        }
    }

    pub fn ask_remove(&mut self, space: Space, pattern: &str) {
        self.removing = Some((space, pattern.into()));
    }

    pub fn keep(&mut self) {
        self.removing = None;
    }

    pub fn remove(&mut self, space: Space, pattern: &str, window: &Weak<Surface<Viewer>>) {
        self.removing = None;
        let result = policy::remove(space, pattern).map(|()| format!("Removed {pattern}. The broader pattern above it now decides who may read these."));
        self.said = Some(Said { of: format!("removed:{}", space.name()), result });
        self.read_policy(window);
    }

    pub fn body(&self) -> String {
        let settings = self.settings_html();
        let policy = self.policy_html();
        format!("<div class=\"settings-page\" id=\"settings-page\">{settings}{policy}</div>")
    }

    fn said(&self, of: &str) -> String {
        match &self.said {
            Some(Said { of: said, result }) if said == of => match result {
                Ok(text) => format!("<p class=\"said\">{}</p>", escape(text)),
                Err(why) => format!("<p class=\"said bad\">{}</p>", escape(why)),
            },
            _ => String::new(),
        }
    }

    fn settings_html(&self) -> String {
        let (documented, here) = match (&self.documented, &self.here) {
            (Some(Ok(documented)), Some(Ok(here))) => (documented, here),
            (Some(Err(why)), _) | (_, Some(Err(why))) => return format!("<section class=\"group\"><h2>eventd's settings</h2><p class=\"trouble\">{}</p></section>", escape(why)),
            _ => return "<section class=\"group\"><h2>eventd's settings</h2><p class=\"more\">Reading eventd's settings…</p></section>".into(),
        };
        let changeable = here.unchangeable.is_none();
        let note = here.unchangeable.as_ref().map(|why| format!("<p class=\"note\">{}</p>", escape(why))).unwrap_or_default();
        let groups: String = GROUPS
            .iter()
            .map(|group| {
                let rows: String = documented.iter().filter(|setting| settings::group(&setting.name) == *group).map(|setting| self.row(setting, here, changeable)).collect();
                if rows.is_empty() { String::new() } else { format!("<section class=\"group\"><h2>{group}</h2>{rows}</section>") }
            })
            .collect();
        let unknown: String = here
            .values
            .iter()
            .filter(|(name, _)| !documented.iter().any(|setting| &setting.name == *name))
            .map(|(name, set)| format!("<div class=\"setting\"><div class=\"about\"><span class=\"name\">{}</span></div><p class=\"facts\">Set to {}</p></div>", escape(name), escape(&set.said())))
            .collect();
        let unknown = if unknown.is_empty() {
            String::new()
        } else {
            format!("<section class=\"group\"><h2>Set here, and not in eventd's documentation</h2><p class=\"hint\">eventd ignores values it does not know.</p>{unknown}</section>")
        };
        format!("{note}{groups}{unknown}")
    }

    fn row(&self, setting: &Setting, here: &Here, changeable: bool) -> String {
        let name = escape(&setting.name);
        let set = here.values.get(&setting.name);
        let applies = if setting.at_restart() { "applies when eventd next starts" } else { "applies at once" };
        let range = if setting.valid.is_empty() { String::new() } else { format!("{} · ", escape(&setting.valid)) };
        let now = match set {
            Some(set) => format!("Set to {}", escape(&said_value(setting, &set.said()))),
            None => format!("Its default, {}", escape(&said_value(setting, &setting.default))),
        };
        let value = match setting.kind {
            Kind::Dword | Kind::Qword => {
                let unit = settings::unit(&setting.name).map(|unit| format!("<span class=\"unit\">{unit}</span>")).unwrap_or_default();
                let buttons = if changeable {
                    let default = if set.is_some() { format!("<button type=\"button\" fx-click=\"setting-default\" fx-value-name=\"{name}\">Use the default</button>") } else { String::new() };
                    format!("<button type=\"submit\">Save</button>{default}")
                } else {
                    String::new()
                };
                format!(
                    "<form class=\"value\" fx-submit=\"setting\" fx-value-name=\"{name}\"><input name=\"v-{name}\" inputmode=\"numeric\" autocomplete=\"off\" spellcheck=\"false\" aria-label=\"{name}\"{}>{unit}{buttons}</form>",
                    if changeable { "" } else { " disabled" }
                )
            }
            Kind::Text | Kind::Other => format!("<p class=\"value text\"><code>{}</code></p>", escape(&set.map_or_else(|| setting.default.clone(), Set::said))),
        };
        format!(
            "<div class=\"setting\" id=\"s-{name}\"><div class=\"about\"><span class=\"name\">{name}</span><p>{about}</p></div>{value}<p class=\"facts\">{now} · {range}{applies}</p>{said}</div>",
            about = escape(&setting.about),
            said = self.said(&setting.name),
        )
    }

    fn policy_html(&self) -> String {
        let Some(policy) = &self.policy else {
            return "<section class=\"group policy\"><h2>Who may read what</h2><p class=\"more\">Reading eventd's read policy…</p></section>".into();
        };
        let trouble: String = policy.trouble.iter().map(|why| format!("<p class=\"trouble\">{}</p>", escape(why))).collect();
        let spaces: String = Space::ALL
            .iter()
            .map(|space| {
                let entries: String = policy.entries.iter().filter(|entry| entry.space == *space).map(|entry| self.entry(entry)).collect();
                let add = if policy.addable.contains(space) {
                    format!(
                        "<form class=\"add\" fx-submit=\"policy-add\" fx-value-space=\"{name}\"><input name=\"add-{name}\" autocomplete=\"off\" spellcheck=\"false\" placeholder=\"{example}\" aria-label=\"A new pattern for {lower}\"><button type=\"submit\">Add a pattern</button></form>{said}",
                        name = space.name(),
                        example = match space {
                            Space::Events => "A type, such as kacs",
                            Space::Logs => "A service, such as sshd",
                            _ => "A name, such as cpu",
                        },
                        lower = space.heading().to_lowercase(),
                        said = self.said(&format!("add:{}", space.name())),
                    )
                } else {
                    String::new()
                };
                let removed = self.said(&format!("removed:{}", space.name()));
                format!("<h3>{}</h3><ul class=\"patterns\">{entries}</ul>{removed}{add}", space.heading())
            })
            .collect();
        format!(
            "<section class=\"group policy\"><h2>Who may read what</h2>\
             <p class=\"hint\">eventd checks every question asked of it against these, as whoever asks, and leaves out what they may not read. \
             The most specific pattern that matches a name is the one that applies.</p>{trouble}{spaces}</section>"
        )
    }

    fn entry(&self, entry: &Entry) -> String {
        let (space, pattern) = (entry.space.name(), escape(&entry.pattern));
        let grants: String = entry.grants.iter().map(|(who, what)| format!("<li><b>{}</b> {}</li>", escape(who), escape(what))).collect();
        let change = if entry.unchangeable.is_some() { "Look closer…" } else { "Change…" };
        let remove = if entry.removable { format!("<button type=\"button\" fx-click=\"policy-remove-ask\" fx-value-space=\"{space}\" fx-value-pattern=\"{pattern}\">Remove…</button>") } else { String::new() };
        let asking = if self.removing.as_ref().is_some_and(|(asked, name)| *asked == entry.space && *name == entry.pattern) {
            format!(
                "<p class=\"note\">Remove this pattern? Then the broader pattern above it decides who may read these. \
                 <button type=\"button\" class=\"link\" fx-click=\"policy-remove\" fx-value-space=\"{space}\" fx-value-pattern=\"{pattern}\">Remove it</button> \
                 <button type=\"button\" class=\"link\" fx-click=\"policy-keep\" fx-key=\"Escape\">Keep it</button></p>"
            )
        } else {
            String::new()
        };
        let why = entry.unchangeable.as_ref().map(|why| format!("<p class=\"why\">{}</p>", escape(why))).unwrap_or_default();
        format!(
            "<li class=\"pattern\" id=\"p-{space}-{pattern}\"><div class=\"titled\"><span class=\"covers\">{covers}</span>{code}</div>\
             <ul class=\"grants\">{grants}</ul>{why}<div class=\"actions\"><button type=\"button\" fx-click=\"policy-edit\" fx-value-space=\"{space}\" fx-value-pattern=\"{pattern}\">{change}</button>{remove}</div>{asking}{said}</li>",
            covers = escape(&entry.space.covers(&entry.pattern)),
            code = if entry.space == Space::Admin { String::new() } else { format!("<code>{pattern}</code>") },
            said = self.said(&format!("{space}:{}", entry.pattern)),
        )
    }

    pub fn footer(&self) -> String {
        let set = match &self.here {
            Some(Ok(here)) => match here.values.len() {
                1 => "1 of eventd's values is set here; the rest are their defaults".to_string(),
                count => format!("{count} of eventd's values are set here; the rest are their defaults"),
            },
            _ => String::new(),
        };
        format!("<footer class=\"status\"><span>{}</span><span class=\"hidden\">From eventd's regman page</span></footer>", escape(&set))
    }
}

/// A value as a person reads it: a number of bytes with its size beside it.
fn said_value(setting: &Setting, value: &str) -> String {
    match (settings::unit(&setting.name), value.parse::<u64>()) {
        (Some("bytes"), Ok(bytes)) if bytes >= 1024 => {
            #[allow(clippy::cast_precision_loss, reason = "a size is said to three figures")]
            let size = chart::say(bytes as f64, Unit::Bytes);
            format!("{value} ({size})")
        }
        (Some(unit), Ok(_)) => format!("{value} {unit}"),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(name: &str) -> Setting {
        Setting { name: name.into(), kind: Kind::Qword, default: "0".into(), valid: String::new(), applies: "live".into(), about: String::new() }
    }

    #[test]
    fn values_are_said_with_their_units() {
        assert_eq!(said_value(&setting("MaxQueryHeldBytes"), "268435456"), "268435456 (256 MiB)");
        assert_eq!(said_value(&setting("QueryTimeoutMs"), "30000"), "30000 ms");
        assert_eq!(said_value(&setting("MaxQueriesPerUser"), "16"), "16");
        assert_eq!(said_value(&setting("EventStorePath"), "(required)"), "(required)");
    }
}
