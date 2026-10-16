//! Mutation observers (DOM §4.3): who watches which node for what, and the
//! records a change to the tree leaves for them.
//!
//! The registrations live here, beside the tree, rather than with the
//! observers' script: every change goes through [`crate::DocumentMutator`],
//! and it asks here whether anyone is interested before it builds a record.
//! A page with no observer pays one emptiness check per change. The records
//! wait here until script takes them, at the microtask checkpoint, and calls
//! the observers' callbacks.

use crate::NodeId;
use crate::tree::Document;

/// What one `observe()` call asked to hear about (DOM §4.3.1
/// `MutationObserverInit`, already validated and defaulted by the caller).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MutationOptions {
    /// Children added to or removed from the node.
    pub child_list: bool,
    /// Its attributes changing.
    pub attributes: bool,
    /// Its data changing, for a text, comment or processing instruction.
    pub character_data: bool,
    /// The same, for every node under it as well.
    pub subtree: bool,
    /// Keep an attribute's value from before the change.
    pub attribute_old_value: bool,
    /// Keep character data from before the change.
    pub character_data_old_value: bool,
    /// Only these attribute local names, when given.
    pub attribute_filter: Option<Vec<String>>,
}

/// Which of the three kinds of change a record reports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MutationKind {
    /// `"childList"`.
    ChildList,
    /// `"attributes"`.
    Attributes,
    /// `"characterData"`.
    CharacterData,
}

/// One `MutationRecord`, for one observer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationRecord {
    /// The observer it is for, as the script side numbered it.
    pub observer: u32,
    /// What changed.
    pub kind: MutationKind,
    /// The node that changed: the parent, for a change to children.
    pub target: NodeId,
    /// Children added, in order.
    pub added: Vec<NodeId>,
    /// Children removed, in order.
    pub removed: Vec<NodeId>,
    /// The sibling before the added or removed children.
    pub previous: Option<NodeId>,
    /// The sibling after them.
    pub next: Option<NodeId>,
    /// The attribute's local name and namespace, for an attribute change.
    pub attribute: Option<(String, String)>,
    /// What the attribute or the data was before, when the observer asked.
    pub old_value: Option<String>,
}

/// The document's observers and the records waiting for them.
#[derive(Debug, Default)]
pub(crate) struct MutationObservers {
    registrations: Vec<(u32, NodeId, MutationOptions)>,
    records: Vec<MutationRecord>,
}

/// A change about to be reported, before it is known who wants it.
pub(crate) struct Change<'a> {
    pub kind: MutationKind,
    pub target: NodeId,
    pub added: &'a [NodeId],
    pub removed: &'a [NodeId],
    pub previous: Option<NodeId>,
    pub next: Option<NodeId>,
    pub attribute: Option<(&'a str, &'a str)>,
    pub old_value: Option<&'a str>,
}

impl MutationObservers {
    /// Whether anything is observed at all: the one check a change makes
    /// when nothing is.
    pub(crate) fn any(&self) -> bool {
        !self.registrations.is_empty()
    }
}

impl Document {
    /// Register `observer` on `target` (DOM §4.3.1 `observe()`), replacing
    /// what it asked of that node before.
    pub fn observe_mutations(&mut self, observer: u32, target: NodeId, options: MutationOptions) {
        let registrations = &mut self.observers.registrations;
        match registrations
            .iter_mut()
            .find(|(id, node, _)| *id == observer && *node == target)
        {
            Some(existing) => existing.2 = options,
            None => registrations.push((observer, target, options)),
        }
    }

    /// Stop `observer` observing anything, and drop what was waiting for it
    /// (DOM §4.3.1 `disconnect()`).
    pub fn disconnect_mutations(&mut self, observer: u32) {
        self.observers
            .registrations
            .retain(|(id, _, _)| *id != observer);
        self.observers
            .records
            .retain(|record| record.observer != observer);
    }

    /// The waiting records, in the order the changes happened.
    pub fn peek_mutation_records(&self) -> &[MutationRecord] {
        &self.observers.records
    }

    /// Whether records are waiting for their observers.
    pub fn has_mutation_records(&self) -> bool {
        !self.observers.records.is_empty()
    }

    /// Take every waiting record, in the order the changes happened; with
    /// `observer`, only that observer's (`takeRecords()`).
    pub fn take_mutation_records(&mut self, observer: Option<u32>) -> Vec<MutationRecord> {
        match observer {
            None => std::mem::take(&mut self.observers.records),
            Some(observer) => {
                let (taken, kept) = std::mem::take(&mut self.observers.records)
                    .into_iter()
                    .partition(|record| record.observer == observer);
                self.observers.records = kept;
                taken
            }
        }
    }

    /// Queue a record of `change` for each observer interested in it (DOM
    /// §4.3.2 "queue a mutation record"): one registered on the target, or on
    /// an ancestor with `subtree`, whose options take this kind of change.
    pub(crate) fn queue_mutation(&mut self, change: &Change<'_>) {
        if !self.observers.any() {
            return;
        }
        // Each interested observer once, with the old value if any of its
        // registrations asked for it.
        let mut interested: Vec<(u32, bool)> = Vec::new();
        let mut node = Some(change.target);
        while let Some(current) = node {
            for (observer, registered, options) in &self.observers.registrations {
                if *registered != current || (current != change.target && !options.subtree) {
                    continue;
                }
                let wants_old = match change.kind {
                    MutationKind::ChildList if options.child_list => false,
                    MutationKind::Attributes if options.attributes => {
                        let filtered_out =
                            options.attribute_filter.as_ref().is_some_and(|filter| {
                                change.attribute.is_none_or(|(local, namespace)| {
                                    !namespace.is_empty()
                                        || !filter.iter().any(|name| name == local)
                                })
                            });
                        if filtered_out {
                            continue;
                        }
                        options.attribute_old_value
                    }
                    MutationKind::CharacterData if options.character_data => {
                        options.character_data_old_value
                    }
                    _ => continue,
                };
                match interested.iter_mut().find(|(id, _)| id == observer) {
                    Some(entry) => entry.1 |= wants_old,
                    None => interested.push((*observer, wants_old)),
                }
            }
            node = self.get(current).and_then(|node| node.parent);
        }
        for (observer, wants_old) in interested {
            self.observers.records.push(MutationRecord {
                observer,
                kind: change.kind,
                target: change.target,
                added: change.added.to_vec(),
                removed: change.removed.to_vec(),
                previous: change.previous,
                next: change.next,
                attribute: change
                    .attribute
                    .map(|(local, namespace)| (local.to_owned(), namespace.to_owned())),
                old_value: if wants_old {
                    change.old_value.map(str::to_owned)
                } else {
                    None
                },
            });
        }
    }

    /// Whether any observer is registered: what a change checks before it
    /// gathers what a record would need.
    pub(crate) fn observed(&self) -> bool {
        self.observers.any()
    }
}
