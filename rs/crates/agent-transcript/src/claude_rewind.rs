//! Claude Code keeps a conversation as a tree: each record names the one before
//! it, its `parentUuid`. Editing a prompt and sending it again (Esc to edit, or
//! a rewind to an earlier prompt) writes the new prompt as a child of the
//! record the old one answered, beside the old one, which stays in the file
//! with everything that answered it. That branch is left: the model never sees
//! it again, and no later record continues it.

use std::collections::HashSet;

use crate::claude::{ClaudeLine, ClaudeRecord};
use crate::events::{
    Event, KIND_PROMPT, OP_ADD, OP_UPDATE, STATUS_INTERRUPTED, STATUS_PENDING, TurnRef,
};

impl ClaudeRecord {
    /// Returns the uuid a record's `parentUuid` names, or `""` for `null`.
    pub(crate) fn parent_uuid(&self) -> String {
        self.parent
            .as_ref()
            .and_then(|raw| agent_base::gostd::json::from_str::<String>(raw.as_str()).ok())
            .unwrap_or_default()
    }
}

/// One record of the tree: its line, the records that name it as their parent,
/// and the ids of the events it added.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeNode {
    pub(crate) line: usize,
    pub(crate) kids: Vec<String>,
    pub(crate) events: Vec<String>,
}

impl ClaudeLine<'_> {
    /// Places the line's record in the conversation's tree and returns the
    /// line's events: its adds, then its updates, then, when it is a prompt
    /// that replaces an earlier one, an update of every event of the branch it
    /// abandons, abandoned. An abandoned event stays so: its later updates (a
    /// background call that finishes after all) are marked too.
    pub(crate) fn tree(&mut self) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::with_capacity(self.adds.len() + self.updates.len());
        out.extend(self.adds.iter().cloned());
        out.extend(self.updates.iter().cloned());
        for e in &mut out {
            if self.c.abandoned.get(&e.id).copied().unwrap_or(false) {
                e.abandoned = true;
            }
        }
        // A compaction starts the tree again (its boundary has no parentUuid):
        // what came before it is a summary to the model, and no prompt there
        // can be edited. Forgetting it keeps the parse's memory to the
        // conversation since the last compaction.
        if self.r.meta.r#type == "system" && self.r.subtype == "compact_boundary" {
            self.c.nodes.clear();
            self.c.asked.clear();
            self.c.latest.clear();
        }
        // What a rewind may later send again: the events of records in the
        // tree, as they last were.
        for e in &out {
            if self.c.latest.contains_key(&e.id) || (e.op == OP_ADD && !self.r.uuid.is_empty()) {
                self.c.latest.insert(e.id.clone(), e.clone());
            }
        }
        if self.r.uuid.is_empty() {
            return out;
        }
        let parent_uuid = self.r.parent_uuid();
        if !self.c.nodes.contains_key(&self.r.uuid) {
            self.c.nodes.insert(
                self.r.uuid.clone(),
                ClaudeNode {
                    line: self.n,
                    kids: Vec::new(),
                    events: Vec::new(),
                },
            );
            if parent_uuid != self.r.uuid
                && let Some(parent) = self.c.nodes.get_mut(&parent_uuid)
            {
                parent.kids.push(self.r.uuid.clone());
            }
        }
        if let Some(node) = self.c.nodes.get_mut(&self.r.uuid) {
            for e in &self.adds {
                node.events.push(e.id.clone());
            }
        }
        // A session's first prompt has a null parent: editing it makes two.
        // A record without the field isn't placed.
        let has_parent_field = self.r.parent.as_ref().is_some_and(|p| !p.is_empty());
        let has_prompt = self.adds.iter().any(|e| e.kind == KIND_PROMPT);
        if self.r.meta.r#type != "user" || !has_parent_field || !has_prompt {
            return out;
        }
        let old = self.c.asked.get(&parent_uuid).cloned().unwrap_or_default();
        self.c.asked.insert(parent_uuid, self.r.uuid.clone());
        if old.is_empty() || old == self.r.uuid {
            return out;
        }
        self.renumber(&old, &mut out);
        out.extend(self.abandon(&old));
        out
    }

    /// Gives the prompt that replaces the one record `old` holds that one's
    /// turn index, in a file that numbers its turns by counting human prompts:
    /// it is the same prompt, edited, as Claude Code's `promptIndex`, where a
    /// file has it, says. The prompts of the abandoned branch don't count.
    fn renumber(&mut self, old: &str, out: &mut [Event]) {
        if !self.c.counted(self.r) || self.c.turn.is_none() {
            return;
        }
        let mut was: Option<TurnRef> = None;
        if let Some(old_node) = self.c.nodes.get(old) {
            for id in &old_node.events {
                if let Some(e) = self.c.latest.get(id)
                    && e.kind == KIND_PROMPT
                {
                    was = e.turn.clone();
                    break;
                }
            }
        }
        let Some(was) = was else {
            return;
        };
        let cur_turn = self.c.turn.clone().unwrap();
        if was.index == cur_turn.index {
            return;
        }
        self.c.prompts = was.index;
        self.c.set_turn(&cur_turn.id, was.index, cur_turn.derived);
        let updated_turn = self.c.turn.clone();
        for e in out.iter_mut() {
            if e.turn.as_ref() == Some(&cur_turn) {
                e.turn = updated_turn.clone();
                self.c.latest.insert(e.id.clone(), e.clone());
            }
        }
    }

    /// Marks every event of the branch from the record `old` down abandoned: an
    /// update of each, in transcript order, at this line. A call of the branch
    /// still waiting for its result won't get one; a background call's task may
    /// still report back.
    fn abandon(&mut self, old: &str) -> Vec<Event> {
        let mut branch: Vec<(usize, Vec<String>)> = Vec::new();
        let mut stack = vec![old.to_string()];
        let mut visited = HashSet::new();
        while let Some(uuid) = stack.pop() {
            if !visited.insert(uuid.clone()) {
                continue;
            }
            if let Some(node) = self.c.nodes.get(&uuid) {
                branch.push((node.line, node.events.clone()));
                stack.extend(node.kids.iter().cloned());
            }
        }
        branch.sort_by_key(|(line, _)| *line);
        let mut out = Vec::new();
        for (_, events) in branch {
            for id in events {
                if self.c.abandoned.get(&id).copied().unwrap_or(false) {
                    continue;
                }
                let Some(mut e) = self.c.latest.get(&id).cloned() else {
                    continue;
                };
                self.c.abandoned.insert(id.clone(), true);
                e.op = OP_UPDATE.to_string();
                e.at = self.at;
                e.line = self.n;
                e.ts = self.r.timestamp.clone();
                e.abandoned = true;
                self.c.latest.insert(id.clone(), e.clone());
                out.push(e.clone());
                if let Some(call) = self.c.tools.get(&id).cloned() {
                    let is_pending = call
                        .tool
                        .as_ref()
                        .and_then(|t| t.result.as_ref())
                        .is_some_and(|r| r.status == STATUS_PENDING);
                    if is_pending {
                        self.c.settled(&id, STATUS_INTERRUPTED);
                    } else {
                        self.c.tools.insert(id.clone(), e);
                    }
                }
                if self.c.closing.as_ref().is_some_and(|c| c.id == id) {
                    self.c.closing = None;
                }
            }
        }
        out
    }
}
