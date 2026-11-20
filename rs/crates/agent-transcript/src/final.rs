use std::collections::HashMap;

use crate::events::{Event, KIND_SAY, OP_ADD, OP_UPDATE, Pos, SayEvent, TurnRef};

/// Finals merges final replies a harness reports before its transcript holds
/// them into a stream. Claude Code writes a turn's closing reply to its
/// transcript only after its Stop hook has run, while the hook already has the
/// text (`last_assistant_message`): a reader that waits for the transcript
/// shows every turn one reply behind.
///
/// [`Finals::provisional`] makes the reply a `say` event of its own, id
/// `"<promptId>#final"`, marked provisional. When the transcript's own copy
/// lands, [`Finals::merge`] sends it as an update of that id, so a reader
/// replaces the provisional reply instead of showing it twice; later updates of
/// the copy (a subagent's reply with no stop reason marked final when its turn
/// ends) keep that id. The copy is the turn's next final reply, or its next
/// reply with the same text, which is how a reply that isn't final when it
/// lands (no stop reason yet, a `max_tokens` stop) is told. A turn a held
/// delivery continues has a Stop, and a final reply, per delivery: the reply of
/// the nth Stop is `"<promptId>#final<n>"`.
///
/// `Finals` doesn't number events: the stream that interleaves them sets `seq`.
#[derive(Debug, Clone, Default)]
pub struct Finals {
    turns: HashMap<String, TurnRef>,
    /// Counts each turn's provisional replies, and `seen` the final replies its
    /// transcript gave: together they number the turn's Stops.
    made: HashMap<String, usize>,
    seen: HashMap<String, usize>,
    /// Holds each turn's provisional replies whose copy hasn't landed, oldest
    /// first; `said`, the text of the turn's latest reply in the transcript.
    waiting: HashMap<String, Vec<WaitingReply>>,
    said: HashMap<String, String>,
    /// Maps each copy to the provisional reply's id it was sent under.
    sent_as: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct WaitingReply {
    id: String,
    text: String,
}

/// Returns a [`Finals`] with no replies waiting.
pub fn new_finals() -> Finals {
    Finals::default()
}

/// The id of a turn's nth provisional reply, counting from 0.
fn final_id(prompt_id: &str, n: usize) -> String {
    if n == 0 {
        format!("{prompt_id}#final")
    } else {
        format!("{prompt_id}#final{}", n + 1)
    }
}

fn same_text(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

impl Finals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the event for a final reply the harness reported for the turn
    /// `prompt_id`, at `ts`, while the transcript ended at `pos`. Returns
    /// `None` when the text is empty, or the transcript has already given that
    /// reply: no provisional reply is waiting and the turn's latest reply has
    /// that text. A Stop reported only after its reply landed thus adds
    /// nothing, and the turn's next Stop still gets the next id.
    pub fn provisional(
        &mut self,
        prompt_id: &str,
        text: &str,
        ts: &str,
        pos: Pos,
    ) -> Option<Event> {
        if text.is_empty() || prompt_id.is_empty() {
            return None;
        }
        let waiting_empty = self.waiting.get(prompt_id).is_none_or(Vec::is_empty);
        let latest_said = self.said.get(prompt_id).map(String::as_str).unwrap_or("");
        if waiting_empty && same_text(latest_said, text) {
            return None;
        }
        // Final replies the transcript gave before any Stop was reported for
        // them count among the turn's Stops.
        let made = self.made.get(prompt_id).copied().unwrap_or(0);
        let seen = self.seen.get(prompt_id).copied().unwrap_or(0);
        let n = made.max(seen);
        self.made.insert(prompt_id.to_string(), n + 1);
        let id = final_id(prompt_id, n);
        self.waiting
            .entry(prompt_id.to_string())
            .or_default()
            .push(WaitingReply {
                id: id.clone(),
                text: text.to_string(),
            });
        let turn = self
            .turns
            .get(prompt_id)
            .cloned()
            .unwrap_or_else(|| TurnRef {
                id: prompt_id.to_string(),
                index: 0,
                derived: false,
            });
        Some(Event {
            id,
            op: OP_ADD.to_string(),
            at: pos.at,
            line: pos.line,
            ts: ts.to_string(),
            turn: Some(turn),
            kind: KIND_SAY.to_string(),
            say: Some(SayEvent {
                text: text.to_string(),
                r#final: true,
                provisional: true,
            }),
            ..Event::default()
        })
    }

    /// Passes an event of the transcript's stream through: the copy of a
    /// provisional reply, and every later update of it, comes out as an update
    /// of that reply, under its id.
    pub fn merge(&mut self, mut e: Event) -> Event {
        if let Some(id) = self.sent_as.get(&e.id) {
            e.id = id.clone();
            e.op = OP_UPDATE.to_string();
            return e;
        }
        let Some(ref turn_ref) = e.turn else {
            return e;
        };
        let turn = turn_ref.id.clone();
        self.turns.insert(turn.clone(), turn_ref.clone());
        let Some(ref say) = e.say else {
            return e;
        };
        if e.op != OP_ADD {
            return e;
        }
        self.said.insert(turn.clone(), say.text.clone());
        if say.r#final {
            *self.seen.entry(turn.clone()).or_insert(0) += 1;
        }
        let Some(w) = self.waiting.get_mut(&turn) else {
            return e;
        };
        if w.is_empty() || (!say.r#final && !same_text(&say.text, &w[0].text)) {
            return e;
        }
        let matched = w.remove(0);
        self.sent_as.insert(e.id.clone(), matched.id.clone());
        e.id = matched.id;
        e.op = OP_UPDATE.to_string();
        e
    }
}
