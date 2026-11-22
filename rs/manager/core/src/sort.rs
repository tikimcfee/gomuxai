//! Sorting a list by its columns: a [`SortOrder`] is the columns a person picked, in the
//! order picked, each with its direction. Clicking a column sorts by it alone; with
//! "add columns" on, a click adds it as the next key. Each column sorts in its natural
//! order first ([`Natural`]): text A to Z, times newest first, numbers largest first,
//! states in a fixed meaningful order. Ties fall back to the list's default order, since
//! the sort is stable and starts from it; an empty order is the default.

use std::cmp::Ordering;

/// How a column sorts when first picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Natural {
    /// A to Z, ignoring case.
    Text,
    /// Newest first.
    Time,
    /// Largest first.
    Number,
    /// A fixed order the column defines (most in need of a look first).
    Rank,
}

impl Natural {
    /// Whether the natural direction is ascending (smallest, oldest or first rank first).
    fn ascending(self) -> bool {
        matches!(self, Natural::Text | Natural::Rank)
    }

    /// The direction in words: natural, then reversed.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Natural::Text => ("A–Z", "Z–A"),
            Natural::Time => ("newest first", "oldest first"),
            Natural::Number => ("largest first", "smallest first"),
            Natural::Rank => ("in order", "reversed"),
        }
    }
}

/// One row's value in one column, for comparing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Nothing to sort by (not counted, no time): last in either direction.
    Missing,
    /// Compared as given: lower-case it first to ignore case (see [`text`]).
    Text(String),
    /// RFC 3339 in UTC, which sorts as text.
    Time(String),
    Num(i64),
    /// A position in the column's fixed order.
    Rank(u32),
}

/// A text value ignoring case; empty is [`Value::Missing`].
pub fn text(s: &str) -> Value {
    if s.is_empty() {
        Value::Missing
    } else {
        Value::Text(s.to_lowercase())
    }
}

/// A time value; empty is [`Value::Missing`].
pub fn time(s: &str) -> Value {
    if s.is_empty() {
        Value::Missing
    } else {
        Value::Time(s.to_string())
    }
}

/// An optional number.
pub fn num(n: Option<i64>) -> Value {
    n.map_or(Value::Missing, Value::Num)
}

/// The position of an agent or session status in the order that puts what needs a look
/// first: waiting, error, processing, idle, unknown, then ended.
pub fn status_rank(status: &str) -> u32 {
    match status {
        "waiting" => 0,
        "error" => 1,
        "processing" => 2,
        "idle" => 3,
        "unknown" => 5,
        s if s.starts_with("ended") => 6,
        _ => 4,
    }
}

/// Compares two values in the natural order of their column (missing ones last).
fn compare_natural(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Time(a), Value::Time(b)) => b.cmp(a),
        (Value::Num(a), Value::Num(b)) => b.cmp(a),
        (Value::Rank(a), Value::Rank(b)) => a.cmp(b),
        // Mixed kinds don't happen in one column; keep them stable.
        _ => Ordering::Equal,
    }
}

/// A list's columns.
pub trait Column: Copy + Eq + std::fmt::Debug + 'static {
    /// Every column, in the order the header shows them.
    const ALL: &'static [Self];
    /// What the list is ordered by with no column picked.
    const DEFAULT: &'static str;
    /// The column's name, in the header and in the order's words.
    fn label(self) -> &'static str;
    fn natural(self) -> Natural;
    /// A [`Natural::Rank`] column's direction in words: natural, then reversed.
    fn rank_words(self) -> (&'static str, &'static str) {
        Natural::Rank.words()
    }
}

/// One key of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey<C> {
    pub column: C,
    /// Opposite to the column's natural order.
    pub reversed: bool,
}

impl<C: Column> SortKey<C> {
    /// Whether it sorts ascending (A to Z, smallest, oldest, first rank first).
    pub fn ascending(&self) -> bool {
        self.column.natural().ascending() != self.reversed
    }

    /// ↑ ascending, ↓ descending.
    pub fn arrow(&self) -> &'static str {
        if self.ascending() { "↑" } else { "↓" }
    }

    /// `tokens largest first`.
    pub fn words(&self) -> String {
        let (natural, reversed) = match self.column.natural() {
            Natural::Rank => self.column.rank_words(),
            n => n.words(),
        };
        format!(
            "{} {}",
            self.column.label(),
            if self.reversed { reversed } else { natural }
        )
    }
}

/// The columns a list is sorted by, first key first; empty is the list's default order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortOrder<C> {
    keys: Vec<SortKey<C>>,
}

impl<C> Default for SortOrder<C> {
    fn default() -> Self {
        SortOrder { keys: Vec::new() }
    }
}

impl<C: Column> SortOrder<C> {
    pub fn keys(&self) -> &[SortKey<C>] {
        &self.keys
    }

    pub fn is_default(&self) -> bool {
        self.keys.is_empty()
    }

    /// A header was clicked. Alone (`add` false): sort by that column alone, in its
    /// natural order, or flip it if it already is the only key. Adding: a column not in
    /// the order becomes the next key; one already in it flips.
    pub fn click(&mut self, column: C, add: bool) {
        let at = self.keys.iter().position(|k| k.column == column);
        match (add, at) {
            (true, Some(i)) => self.keys[i].reversed = !self.keys[i].reversed,
            (true, None) => self.keys.push(SortKey {
                column,
                reversed: false,
            }),
            (false, Some(0)) if self.keys.len() == 1 => {
                self.keys[0].reversed = !self.keys[0].reversed
            }
            (false, _) => {
                self.keys = vec![SortKey {
                    column,
                    reversed: false,
                }]
            }
        }
    }

    /// Takes key `i` out of the order (no-op past the end).
    pub fn remove_at(&mut self, i: usize) {
        if i < self.keys.len() {
            self.keys.remove(i);
        }
    }

    /// Back to the list's default order.
    pub fn reset(&mut self) {
        self.keys.clear();
    }

    /// Where `column` is in the order, and its key.
    pub fn find(&self, column: C) -> Option<(usize, SortKey<C>)> {
        self.keys
            .iter()
            .position(|k| k.column == column)
            .map(|i| (i, self.keys[i]))
    }

    /// The header's text: the direction when it is the only key, or the position and
    /// direction when there are several (`1↑ name`), then the label. The marker goes
    /// first so a narrow column cuts the label, not the marker.
    pub fn header(&self, column: C) -> String {
        match self.find(column) {
            None => column.label().to_string(),
            Some((_, k)) if self.keys.len() == 1 => format!("{} {}", k.arrow(), column.label()),
            Some((i, k)) => format!("{}{} {}", i + 1, k.arrow(), column.label()),
        }
    }

    /// The order in words: `name A–Z, then model Z–A`, or the default's.
    pub fn words(&self) -> String {
        if self.keys.is_empty() {
            return format!("default: {}", C::DEFAULT);
        }
        self.keys
            .iter()
            .map(|k| k.words())
            .collect::<Vec<_>>()
            .join(", then ")
    }

    /// Compares two rows' values, one per key, in the keys' order and directions.
    /// Missing values go last whichever way a key points.
    pub fn compare(&self, a: &[Value], b: &[Value]) -> Ordering {
        for (k, (x, y)) in self.keys.iter().zip(a.iter().zip(b)) {
            let o = match (x, y) {
                (Value::Missing, Value::Missing) => Ordering::Equal,
                (Value::Missing, _) => Ordering::Greater,
                (_, Value::Missing) => Ordering::Less,
                _ if k.reversed => compare_natural(x, y).reverse(),
                _ => compare_natural(x, y),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }

    /// Sorts `items` (given in the default order) by the keys, stably, so ties keep the
    /// default order. Each item's values are read once.
    pub fn sort<T>(&self, items: Vec<T>, value: impl Fn(&T, C) -> Value) -> Vec<T> {
        if self.keys.is_empty() {
            return items;
        }
        let mut keyed: Vec<(Vec<Value>, T)> = items
            .into_iter()
            .map(|t| (self.keys.iter().map(|k| value(&t, k.column)).collect(), t))
            .collect();
        keyed.sort_by(|(a, _), (b, _)| self.compare(a, b));
        keyed.into_iter().map(|(_, t)| t).collect()
    }
}

/// A [`SortOrder`] without its column type, so a UI can drive any list's by column index.
pub trait Orders {
    /// The columns' count.
    fn columns(&self) -> usize;
    fn click(&mut self, column: usize, add: bool);
    fn remove_at(&mut self, i: usize);
    fn reset(&mut self);
    fn header(&self, column: usize) -> String;
    /// Whether the order has `column` among its keys.
    fn sorts_by(&self, column: usize) -> bool;
    fn words(&self) -> String;
    /// Each key in words, first key first.
    fn key_words(&self) -> Vec<String>;
    fn is_default(&self) -> bool;
}

impl<C: Column> Orders for SortOrder<C> {
    fn columns(&self) -> usize {
        C::ALL.len()
    }
    fn click(&mut self, column: usize, add: bool) {
        if let Some(&c) = C::ALL.get(column) {
            SortOrder::click(self, c, add);
        }
    }
    fn remove_at(&mut self, i: usize) {
        SortOrder::remove_at(self, i);
    }
    fn reset(&mut self) {
        SortOrder::reset(self);
    }
    fn header(&self, column: usize) -> String {
        C::ALL
            .get(column)
            .map(|&c| SortOrder::header(self, c))
            .unwrap_or_default()
    }
    fn sorts_by(&self, column: usize) -> bool {
        C::ALL.get(column).is_some_and(|&c| self.find(c).is_some())
    }
    fn words(&self) -> String {
        SortOrder::words(self)
    }
    fn key_words(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.words()).collect()
    }
    fn is_default(&self) -> bool {
        SortOrder::is_default(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Col {
        Name,
        Model,
        Tokens,
        When,
        Status,
    }

    impl Column for Col {
        const ALL: &'static [Col] = &[Col::Name, Col::Model, Col::Tokens, Col::When, Col::Status];
        const DEFAULT: &'static str = "as listed";
        fn label(self) -> &'static str {
            match self {
                Col::Name => "name",
                Col::Model => "model",
                Col::Tokens => "tokens",
                Col::When => "age",
                Col::Status => "status",
            }
        }
        fn natural(self) -> Natural {
            match self {
                Col::Name | Col::Model => Natural::Text,
                Col::Tokens => Natural::Number,
                Col::When => Natural::Time,
                Col::Status => Natural::Rank,
            }
        }
    }

    struct Row {
        name: &'static str,
        model: &'static str,
        tokens: Option<i64>,
        when: &'static str,
        status: &'static str,
    }

    fn rows() -> Vec<Row> {
        let r = |name, model, tokens, when, status| Row {
            name,
            model,
            tokens,
            when,
            status,
        };
        vec![
            r("b", "opus", Some(10), "2026-11-20T10:00:00Z", "idle"),
            r("A", "sonnet", None, "2026-11-20T12:00:00Z", "waiting"),
            r("c", "opus", Some(30), "", "ended (quit)"),
            r("a", "opus", Some(20), "2026-11-20T11:00:00Z", "processing"),
        ]
    }

    fn value(r: &Row, c: Col) -> Value {
        match c {
            Col::Name => text(r.name),
            Col::Model => text(r.model),
            Col::Tokens => num(r.tokens),
            Col::When => time(r.when),
            Col::Status => Value::Rank(status_rank(r.status)),
        }
    }

    fn sorted(o: &SortOrder<Col>) -> Vec<&'static str> {
        o.sort(rows(), value).iter().map(|r| r.name).collect()
    }

    #[test]
    fn the_default_order_is_left_alone() {
        let o = SortOrder::<Col>::default();
        assert!(o.is_default());
        assert_eq!(sorted(&o), ["b", "A", "c", "a"]);
        assert_eq!(o.words(), "default: as listed");
        assert_eq!(o.header(Col::Name), "name");
    }

    #[test]
    fn one_key_in_its_natural_order() {
        let mut o = SortOrder::default();
        // Text A to Z ignoring case; ties ("A", "a") keep the default order.
        o.click(Col::Name, false);
        assert_eq!(sorted(&o), ["A", "a", "b", "c"]);
        assert_eq!(o.header(Col::Name), "↑ name");
        // Numbers largest first, the uncounted last.
        o.click(Col::Tokens, false);
        assert_eq!(o.keys().len(), 1);
        assert_eq!(sorted(&o), ["c", "a", "b", "A"]);
        assert_eq!(o.header(Col::Tokens), "↓ tokens");
        assert_eq!(o.header(Col::Name), "name");
        // Times newest first, no time last.
        o.click(Col::When, false);
        assert_eq!(sorted(&o), ["A", "a", "b", "c"]);
        // Statuses: what needs a look first.
        o.click(Col::Status, false);
        assert_eq!(sorted(&o), ["A", "a", "b", "c"]);
        assert_eq!(o.words(), "status in order");
    }

    #[test]
    fn clicking_the_only_key_flips_it_and_missing_stays_last() {
        let mut o = SortOrder::default();
        o.click(Col::Tokens, false);
        o.click(Col::Tokens, false);
        assert!(o.keys()[0].reversed);
        assert_eq!(sorted(&o), ["b", "a", "c", "A"]);
        assert_eq!(o.header(Col::Tokens), "↑ tokens");
        assert_eq!(o.words(), "tokens smallest first");
        o.click(Col::Tokens, false);
        assert!(!o.keys()[0].reversed);
    }

    #[test]
    fn several_keys_in_the_order_picked() {
        let mut o = SortOrder::default();
        o.click(Col::Model, true);
        o.click(Col::Name, true);
        // opus: a, b, c (name A to Z); then sonnet: A.
        assert_eq!(sorted(&o), ["a", "b", "c", "A"]);
        assert_eq!(o.words(), "model A–Z, then name A–Z");
        assert_eq!(o.header(Col::Model), "1↑ model");
        assert_eq!(o.header(Col::Name), "2↑ name");
        // Adding a key already in the order flips it, in place.
        o.click(Col::Name, true);
        assert_eq!(sorted(&o), ["c", "b", "a", "A"]);
        assert_eq!(o.header(Col::Name), "2↓ name");
        assert_eq!(o.words(), "model A–Z, then name Z–A");
        // A plain click replaces the whole order.
        o.click(Col::Tokens, false);
        assert_eq!(o.keys().len(), 1);
        assert_eq!(o.keys()[0].column, Col::Tokens);
    }

    #[test]
    fn a_plain_click_on_the_first_of_several_keys_sorts_by_it_alone() {
        let mut o = SortOrder::default();
        o.click(Col::Model, true);
        o.click(Col::Name, true);
        o.click(Col::Model, false);
        assert_eq!(
            o.keys(),
            [SortKey {
                column: Col::Model,
                reversed: false
            }]
        );
    }

    #[test]
    fn remove_and_reset() {
        let mut o = SortOrder::default();
        o.click(Col::Model, true);
        o.click(Col::Name, true);
        o.click(Col::Tokens, true);
        o.remove_at(1);
        assert_eq!(o.words(), "model A–Z, then tokens largest first");
        assert_eq!(o.header(Col::Tokens), "2↓ tokens");
        o.remove_at(9);
        assert_eq!(o.keys().len(), 2);
        let dynamic: &mut dyn Orders = &mut o;
        assert_eq!(dynamic.key_words(), ["model A–Z", "tokens largest first"]);
        dynamic.reset();
        assert!(dynamic.is_default());
        assert_eq!(sorted(&o), ["b", "A", "c", "a"]);
    }

    #[test]
    fn orders_by_column_index() {
        let mut o = SortOrder::<Col>::default();
        let d: &mut dyn Orders = &mut o;
        assert_eq!(d.columns(), 5);
        d.click(2, false);
        d.click(99, true);
        assert_eq!(d.header(2), "↓ tokens");
        assert_eq!(d.header(99), "");
        assert!(d.sorts_by(2) && !d.sorts_by(0) && !d.sorts_by(99));
        assert_eq!(d.words(), "tokens largest first");
    }

    #[test]
    fn ties_keep_the_default_order() {
        let mut o = SortOrder::default();
        o.click(Col::Model, false);
        // The opus rows in their default order: b, c, a.
        assert_eq!(sorted(&o), ["b", "c", "a", "A"]);
    }
}
