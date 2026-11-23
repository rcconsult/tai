//! Terminals by alias (ADR 0013): each terminal has an internal ID that's
//! never reused, a number alias (the lowest free) and an optional name.
//! Both aliases are unique among live terminals and freed when the
//! terminal closes. ppxai's `/terminal [alias]` carries an alias as the
//! side effect's `name`; tai keeps the map.

use std::collections::BTreeMap;

/// A terminal's internal ID. Users never see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermId(u64);

/// What a user calls a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alias {
    Number(u32),
    Name(String),
}

/// The longest name: Kubernetes' DNS label rule (RFC 1123).
const NAME_MAX: usize = 63;

impl Alias {
    /// Read an alias: digits are a number; anything else must be a name
    /// that follows the DNS label rule and contains a letter. The same
    /// rule as ppxai's (docs/upstream/ppxai-terminals-by-alias.md).
    pub fn parse(text: &str) -> Result<Self, String> {
        if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            return match text.parse::<u32>() {
                Ok(0) | Err(_) => Err(format!("`{text}`: a terminal number is 1 or more")),
                Ok(n) => Ok(Self::Number(n)),
            };
        }
        check_name(text).map(|()| Self::Name(text.to_string()))
    }
}

/// Whether `name` may name a terminal, and if not, why.
pub fn check_name(name: &str) -> Result<(), String> {
    let why = if name.is_empty() {
        "a terminal name can't be empty"
    } else if name.len() > NAME_MAX {
        "a terminal name is at most 63 characters"
    } else if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        "a terminal name has only lowercase letters, digits and -"
    } else if name.starts_with('-') || name.ends_with('-') {
        "a terminal name starts and ends with a letter or digit"
    } else if !name.bytes().any(|b| b.is_ascii_lowercase()) {
        "a terminal name has a letter, so it isn't read as a number"
    } else {
        return Ok(());
    };
    Err(format!("`{name}`: {why}"))
}

struct Entry<P> {
    number: u32,
    name: Option<String>,
    pane: Option<P>,
}

/// The live terminals, in the order they were created. `P` is the pane,
/// None when its shell didn't start.
pub struct Terminals<P> {
    next: u64,
    entries: BTreeMap<TermId, Entry<P>>,
}

impl<P> Default for Terminals<P> {
    fn default() -> Self {
        Self {
            next: 0,
            entries: BTreeMap::new(),
        }
    }
}

impl<P> Terminals<P> {
    /// Add a terminal: with `number` when it's free, else the lowest free
    /// number, and with `name` (checked by the caller) when it's free.
    pub fn add(&mut self, number: Option<u32>, name: Option<String>, pane: Option<P>) -> TermId {
        let number = number
            .filter(|n| *n > 0 && self.by_number(*n).is_none())
            .unwrap_or_else(|| self.lowest_free());
        let name = name.filter(|n| self.by_name(n).is_none());
        let id = TermId(self.next);
        self.next += 1;
        self.entries.insert(id, Entry { number, name, pane });
        id
    }

    fn lowest_free(&self) -> u32 {
        (1..).find(|n| self.by_number(*n).is_none()).unwrap_or(1)
    }

    fn by_number(&self, number: u32) -> Option<TermId> {
        self.entries
            .iter()
            .find(|(_, e)| e.number == number)
            .map(|(id, _)| *id)
    }

    fn by_name(&self, name: &str) -> Option<TermId> {
        self.entries
            .iter()
            .find(|(_, e)| e.name.as_deref() == Some(name))
            .map(|(id, _)| *id)
    }

    /// The live terminal an alias names.
    pub fn find(&self, alias: &Alias) -> Option<TermId> {
        match alias {
            Alias::Number(n) => self.by_number(*n),
            Alias::Name(name) => self.by_name(name),
        }
    }

    /// Add a terminal for an alias that no live terminal holds: number 7
    /// takes number 7; a name takes the lowest free number.
    pub fn add_for(&mut self, alias: &Alias, pane: Option<P>) -> TermId {
        match alias {
            Alias::Number(n) => self.add(Some(*n), None, pane),
            Alias::Name(name) => self.add(None, Some(name.clone()), pane),
        }
    }

    /// Name a terminal, or take its name away with None. A name another
    /// terminal holds is refused.
    pub fn rename(&mut self, id: TermId, name: Option<&str>) -> Result<(), String> {
        if let Some(name) = name {
            check_name(name)?;
            if self.by_name(name).is_some_and(|other| other != id) {
                return Err(format!("`{name}`: another terminal has that name"));
            }
        }
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "that terminal has closed".to_string())?;
        entry.name = name.map(str::to_string);
        Ok(())
    }

    /// Give a terminal another number. A number another terminal holds is
    /// refused.
    pub fn renumber(&mut self, id: TermId, number: u32) -> Result<(), String> {
        if number == 0 {
            return Err("a terminal number is 1 or more".into());
        }
        if self.by_number(number).is_some_and(|other| other != id) {
            return Err(format!("another terminal has number {number}"));
        }
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "that terminal has closed".to_string())?;
        entry.number = number;
        Ok(())
    }

    /// The tab's title: the name, else "Terminal N", and number 1 is
    /// "Terminal".
    pub fn title(&self, id: TermId) -> String {
        match self.entries.get(&id) {
            Some(Entry {
                name: Some(name), ..
            }) => name.clone(),
            Some(Entry { number: 1, .. }) | None => "Terminal".into(),
            Some(Entry { number, .. }) => format!("Terminal {number}"),
        }
    }

    pub fn name(&self, id: TermId) -> Option<&str> {
        self.entries.get(&id).and_then(|e| e.name.as_deref())
    }

    pub fn contains(&self, id: TermId) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn first(&self) -> Option<TermId> {
        self.entries.keys().next().copied()
    }

    pub fn pane(&self, id: TermId) -> Option<&P> {
        self.entries.get(&id).and_then(|e| e.pane.as_ref())
    }

    pub fn pane_mut(&mut self, id: TermId) -> Option<&mut P> {
        self.entries.get_mut(&id).and_then(|e| e.pane.as_mut())
    }

    /// Put a pane in a terminal's place: a restarted shell.
    pub fn set_pane(&mut self, id: TermId, pane: P) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.pane = Some(pane);
        }
    }

    /// Take a terminal's pane out, keeping the terminal and its aliases.
    pub fn remove_pane(&mut self, id: TermId) -> Option<P> {
        self.entries.get_mut(&id).and_then(|e| e.pane.take())
    }

    pub fn panes_mut(&mut self) -> impl Iterator<Item = &mut P> {
        self.entries.values_mut().filter_map(|e| e.pane.as_mut())
    }

    /// Close a terminal, freeing its aliases; returns its pane.
    pub fn remove(&mut self, id: TermId) -> Option<P> {
        self.entries.remove(&id).and_then(|e| e.pane)
    }

    /// Close every terminal; returns their panes.
    pub fn drain(&mut self) -> Vec<P> {
        std::mem::take(&mut self.entries)
            .into_values()
            .filter_map(|e| e.pane)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_numbers_or_names_with_a_letter() {
        assert_eq!(Alias::parse("2"), Ok(Alias::Number(2)));
        assert_eq!(Alias::parse("007"), Ok(Alias::Number(7)));
        assert_eq!(Alias::parse("build"), Ok(Alias::Name("build".into())));
        assert_eq!(Alias::parse("web-2"), Ok(Alias::Name("web-2".into())));
        assert_eq!(Alias::parse("2a"), Ok(Alias::Name("2a".into())));
        assert!(Alias::parse(&"a".repeat(63)).is_ok());
        for bad in [
            "",
            "0",
            "99999999999",
            "Build",
            "my_term",
            "-a",
            "a-",
            "1-2",
            "a b",
            "é",
            &"a".repeat(64),
        ] {
            assert!(Alias::parse(bad).is_err(), "{bad:?} was taken");
        }
    }

    #[test]
    fn numbers_are_the_lowest_free_and_reused() {
        let mut t = Terminals::<()>::default();
        let a = t.add(None, None, None);
        let b = t.add(None, None, None);
        assert_eq!(
            (t.title(a), t.title(b)),
            ("Terminal".into(), "Terminal 2".into())
        );
        t.remove(a);
        let c = t.add(None, None, None);
        assert_ne!(a, c, "an ID is never reused");
        assert_eq!(t.title(c), "Terminal");
        // /terminal 7 takes 7 and leaves a gap; the next takes 3.
        let seven = t.add_for(&Alias::Number(7), None);
        assert_eq!(t.title(seven), "Terminal 7");
        assert_eq!(t.find(&Alias::Number(7)), Some(seven));
        let d = t.add(None, None, None);
        assert_eq!(t.title(d), "Terminal 3");
    }

    #[test]
    fn names_are_unique_and_freed_on_close() {
        let mut t = Terminals::<()>::default();
        let a = t.add_for(&Alias::Name("build".into()), None);
        assert_eq!(t.title(a), "build");
        assert_eq!(t.find(&Alias::Number(1)), Some(a));
        let b = t.add(None, None, None);
        assert!(t.rename(b, Some("build")).is_err());
        assert!(t.rename(b, Some("Bad")).is_err());
        assert_eq!(t.rename(a, Some("build")), Ok(()), "its own name again");
        assert_eq!(t.rename(b, Some("test")), Ok(()));
        assert_eq!(t.find(&Alias::Name("test".into())), Some(b));
        assert_eq!(t.rename(b, None), Ok(()));
        assert_eq!(t.title(b), "Terminal 2");
        t.remove(a);
        assert_eq!(t.find(&Alias::Name("build".into())), None);
        assert_eq!(t.rename(b, Some("build")), Ok(()));
    }

    #[test]
    fn a_terminal_takes_a_free_number() {
        let mut t = Terminals::<()>::default();
        let a = t.add(None, None, None);
        let b = t.add(None, None, None);
        let c = t.add(None, None, None);
        t.remove(b);
        assert!(t.renumber(c, 1).is_err(), "number 1 is taken");
        assert!(t.renumber(c, 0).is_err());
        assert_eq!(t.renumber(c, 2), Ok(()));
        assert_eq!(t.title(c), "Terminal 2");
        assert_eq!(t.find(&Alias::Number(2)), Some(c));
        assert_eq!(t.find(&Alias::Number(3)), None);
        assert_eq!(t.renumber(a, 1), Ok(()), "its own number again");
    }
}
