//! A generic searchable list overlay. Model picker, skills, MCP servers, loaded
//! context, resume and the dsh import browser all share this one widget; what
//! differs is the `Kind` (which decides keys and what Enter does) and the items.

use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Model,
    Skills,
    Mcp,
    Context,
    Resume,
    Provider,
    Search,
    ImportWorkspaces,
    /// Sessions of the dsh workspace at this index.
    ImportSessions(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Normal,
    Good,
    Warn,
    Bad,
    Muted,
}

#[derive(Clone, Debug)]
pub struct Item {
    /// Stable key (route, path, server name…); not shown.
    pub id: String,
    pub label: String,
    pub detail: String,
    pub tag: String,
    pub tone: Tone,
}

impl Item {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Item {
        Item {
            id: id.into(),
            label: label.into(),
            detail: String::new(),
            tag: String::new(),
            tone: Tone::Normal,
        }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Item {
        self.detail = d.into();
        self
    }
    pub fn tag(mut self, t: impl Into<String>, tone: Tone) -> Item {
        self.tag = t.into();
        self.tone = tone;
        self
    }
}

pub struct Panel {
    pub kind: Kind,
    pub title: String,
    pub items: Vec<Item>,
    pub sel: usize,
    pub query: String,
    pub filterable: bool,
    /// Multi-select (import): ids ticked with Space.
    pub marks: HashSet<String>,
    pub multi: bool,
    /// Shown under the list; key hints for this kind.
    pub footer: String,
    pub empty: String,
}

impl Panel {
    pub fn new(kind: Kind, title: &str, items: Vec<Item>) -> Panel {
        Panel {
            kind,
            title: title.to_string(),
            items,
            sel: 0,
            query: String::new(),
            filterable: true,
            marks: HashSet::new(),
            multi: false,
            footer: String::new(),
            empty: "nothing here".into(),
        }
    }

    /// Indices into `items` that match the query, in order.
    pub fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                q.is_empty()
                    || it.label.to_lowercase().contains(&q)
                    || it.detail.to_lowercase().contains(&q)
                    || it.tag.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn current(&self) -> Option<&Item> {
        let v = self.visible();
        v.get(self.sel.min(v.len().saturating_sub(1)))
            .map(|&i| &self.items[i])
    }

    pub fn step(&mut self, delta: isize) {
        let n = self.visible().len();
        if n == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as isize + delta).rem_euclid(n as isize) as usize;
    }

    pub fn toggle_mark(&mut self) {
        if let Some(id) = self.current().map(|i| i.id.clone())
            && !self.marks.remove(&id)
        {
            self.marks.insert(id);
        }
    }

    pub fn mark_all(&mut self) {
        let ids: Vec<String> = self
            .visible()
            .into_iter()
            .map(|i| self.items[i].id.clone())
            .collect();
        if ids.iter().all(|i| self.marks.contains(i)) {
            for i in ids {
                self.marks.remove(&i);
            }
        } else {
            self.marks.extend(ids);
        }
    }

    /// Replace items (live refresh) keeping the selection on the same id.
    pub fn refresh(&mut self, items: Vec<Item>) {
        let keep = self.current().map(|i| i.id.clone());
        self.items = items;
        if let Some(id) = keep
            && let Some(pos) = self.visible().iter().position(|&i| self.items[i].id == id)
        {
            self.sel = pos;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> Panel {
        Panel::new(
            Kind::Model,
            "t",
            vec![
                Item::new("a", "alpha").detail("first"),
                Item::new("b", "beta").tag("cur", Tone::Good),
                Item::new("c", "gamma"),
            ],
        )
    }

    #[test]
    fn filter_matches_label_detail_and_tag() {
        let mut p = panel();
        p.query = "fir".into();
        assert_eq!(p.visible(), vec![0]);
        p.query = "CUR".into();
        assert_eq!(p.visible(), vec![1]);
        p.query = "zzz".into();
        assert!(p.visible().is_empty());
        assert!(p.current().is_none());
    }

    #[test]
    fn stepping_wraps_and_marks_toggle() {
        let mut p = panel();
        p.step(-1);
        assert_eq!(p.current().unwrap().id, "c");
        p.step(1);
        assert_eq!(p.current().unwrap().id, "a");
        p.toggle_mark();
        assert!(p.marks.contains("a"));
        p.toggle_mark();
        assert!(p.marks.is_empty());
        p.mark_all();
        assert_eq!(p.marks.len(), 3);
        p.mark_all();
        assert!(p.marks.is_empty());
    }

    #[test]
    fn refresh_keeps_selection_on_same_id() {
        let mut p = panel();
        p.step(1);
        p.refresh(vec![Item::new("z", "zed"), Item::new("b", "beta")]);
        assert_eq!(p.current().unwrap().id, "b");
    }
}
