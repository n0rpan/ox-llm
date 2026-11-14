//! A4: the session tree. Owner: task A4. Signatures below are frozen; bodies are yours.
use std::collections::HashSet;

use oxa_proto::{Entry, EntryBody};

/// All entries of a session file as a tree (`id` / `parent`). The leaf is the tip of the active
/// branch: the last entry in file order unless moved with [`Tree::set_leaf`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tree {
    entries: Vec<Entry>,
    leaf: String,
}

impl Tree {
    /// Entries in file order. An entry whose parent is unknown is treated as a root.
    pub fn new(e: Vec<Entry>) -> Tree {
        let leaf = e.last().map(|x| x.id.clone()).unwrap_or_default();
        Tree { entries: e, leaf }
    }
    /// Id of the active leaf (`""` for an empty tree).
    pub fn leaf(&self) -> &str {
        &self.leaf
    }
    /// Move the active leaf. Returns false (and changes nothing) when `id` is unknown.
    pub fn set_leaf(&mut self, id: &str) -> bool {
        if self.index_of(id).is_some() {
            self.leaf = id.to_string();
            true
        } else {
            false
        }
    }
    /// Add an appended entry; it becomes the leaf when its parent is the current leaf. The
    /// first entry of an empty tree always becomes the leaf.
    pub fn push(&mut self, e: Entry) {
        let becomes_leaf = self.leaf.is_empty() || e.parent.as_deref() == Some(self.leaf.as_str());
        let id = e.id.clone();
        self.entries.push(e);
        if becomes_leaf {
            self.leaf = id;
        }
    }
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    /// The path from the root to `leaf`, root first. Empty when `leaf` is unknown.
    pub fn branch(&self, leaf: &str) -> Vec<&Entry> {
        let Some(mut cur) = self.get(leaf) else {
            return Vec::new();
        };
        let mut out = vec![cur];
        // A parent cycle (malformed file) stops once the path is longer than the file.
        while out.len() <= self.entries.len() {
            match cur.parent.as_deref().and_then(|p| self.get(p)) {
                Some(p) => {
                    out.push(p);
                    cur = p;
                }
                None => break,
            }
        }
        out.reverse();
        out
    }
    /// Direct children in file order.
    pub fn children(&self, id: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| e.parent.as_deref() == Some(id))
            .collect()
    }
    /// Root entries in file order.
    pub fn roots(&self) -> Vec<&Entry> {
        let known: HashSet<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
        self.entries
            .iter()
            .filter(|e| match e.parent.as_deref() {
                None => true,
                Some(p) => !known.contains(p),
            })
            .collect()
    }
    /// Deepest entry that is an ancestor of (or equal to) both.
    pub fn common_ancestor(&self, a: &str, b: &str) -> Option<&str> {
        let pa = self.branch(a);
        let pb = self.branch(b);
        let mut best = None;
        for (x, y) in pa.iter().zip(pb.iter()) {
            if x.id != y.id {
                break;
            }
            best = Some(x.id.as_str());
        }
        best
    }
    /// The current label of `id` (last `Label` entry targeting it wins; `None` when cleared).
    pub fn label(&self, id: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                EntryBody::Label { target, label } if target == id => Some(label.as_deref()),
                _ => None,
            })
            .flatten()
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
}
