//! Keyed list watches as `reset` + `delta` frames.
//!
//! `WatchChats` / `WatchSessions` / `WatchDevices` re-serialized the whole list
//! whenever one row changed — ~640 KB of chats (mostly archived) for one chat's
//! activity stamp. A subscriber passing `{"listDelta": true}` instead receives a
//! [`ListFrame::Reset`] first, then one [`ListFrame::Delta`] per watch wake-up
//! carrying only the rows that changed. Wake-ups with nothing changed still
//! produce an (empty) delta: viewports evaluate presence expiry against the
//! clock when a frame arrives, and the engine's periodic presence tick relies on
//! that cadence.
//!
//! Both ends share [`diff`] and [`apply`], so `apply(prev, diff(prev, next))`
//! reproduces `next` exactly — rows and order. Frames of one subscription are
//! produced by one task and delivered in order over one connection; a viewport
//! that resubscribes starts again from a reset.

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Subscription param opting into [`ListFrame`]s. Engines that predate it ignore
/// it and keep sending whole lists as JSON arrays, which [`ListMirror`] accepts.
pub const LIST_DELTA_PARAM: &str = "listDelta";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ListFrame<T> {
    /// The whole list.
    Reset { rows: Vec<T> },
    /// Changes since the previous frame of the same subscription.
    Delta {
        /// New or changed rows, in their new list order.
        #[serde(default = "Vec::new", skip_serializing_if = "Vec::is_empty")]
        upsert: Vec<T>,
        /// Keys of rows that left the list.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        remove: Vec<String>,
        /// Every key in list order — present only when applying `remove` and
        /// `upsert` (new keys appended) would not already give the new order.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListDeltaError {
    /// A delta's `order` is not a permutation of the mirrored keys.
    OrderMismatch,
    /// A delta arrived before any reset (or after one with duplicate keys).
    NotKeyed,
    Malformed(String),
}

impl std::fmt::Display for ListDeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OrderMismatch => f.write_str("list delta order does not match the mirrored rows"),
            Self::NotKeyed => f.write_str("list delta without a uniquely keyed base"),
            Self::Malformed(error) => write!(f, "malformed list frame: {error}"),
        }
    }
}

impl std::error::Error for ListDeltaError {}

fn unique_keys<T>(rows: &[T], key: impl Fn(&T) -> &str) -> bool {
    let mut seen = HashSet::with_capacity(rows.len());
    rows.iter().all(|row| seen.insert(key(row)))
}

/// The frame taking a subscriber holding `prev` to `next`. Lists whose keys
/// are not unique cannot be addressed by key and are sent whole.
pub fn diff<T: PartialEq + Clone>(prev: &[T], next: &[T], key: impl Fn(&T) -> &str) -> ListFrame<T> {
    if !unique_keys(prev, &key) || !unique_keys(next, &key) {
        return ListFrame::Reset { rows: next.to_vec() };
    }
    let before: HashMap<&str, &T> = prev.iter().map(|row| (key(row), row)).collect();
    let after: HashSet<&str> = next.iter().map(&key).collect();
    let remove: Vec<String> = prev
        .iter()
        .map(&key)
        .filter(|k| !after.contains(k))
        .map(str::to_owned)
        .collect();
    let upsert: Vec<T> = next
        .iter()
        .filter(|&row| before.get(key(row)).is_none_or(|&old| old != row))
        .cloned()
        .collect();
    // The order `apply` arrives at without help: survivors in their old
    // places, then new keys in their new order.
    let implied = prev
        .iter()
        .map(&key)
        .filter(|k| after.contains(k))
        .chain(next.iter().map(&key).filter(|k| !before.contains_key(k)));
    let order = (!implied.eq(next.iter().map(&key)))
        .then(|| next.iter().map(|row| key(row).to_owned()).collect());
    ListFrame::Delta { upsert, remove, order }
}

/// Apply `frame` to `rows`. A delta must follow a reset whose keys were unique;
/// on error `rows` is unspecified and the subscriber should resubscribe.
pub fn apply<T>(rows: &mut Vec<T>, frame: ListFrame<T>, key: impl Fn(&T) -> &str) -> Result<(), ListDeltaError> {
    let (upsert, remove, order) = match frame {
        ListFrame::Reset { rows: fresh } => {
            *rows = fresh;
            return Ok(());
        }
        ListFrame::Delta { upsert, remove, order } => (upsert, remove, order),
    };
    if !unique_keys(rows, &key) {
        return Err(ListDeltaError::NotKeyed);
    }
    if !remove.is_empty() {
        let remove: HashSet<&str> = remove.iter().map(String::as_str).collect();
        rows.retain(|row| !remove.contains(key(row)));
    }
    if !upsert.is_empty() {
        let mut index: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (key(row).to_owned(), i))
            .collect();
        for row in upsert {
            match index.get(key(&row)) {
                Some(&i) => rows[i] = row,
                None => {
                    index.insert(key(&row).to_owned(), rows.len());
                    rows.push(row);
                }
            }
        }
    }
    if let Some(order) = order {
        if order.len() != rows.len() {
            return Err(ListDeltaError::OrderMismatch);
        }
        let mut by_key: HashMap<String, T> = rows.drain(..).map(|row| (key(&row).to_owned(), row)).collect();
        for k in &order {
            rows.push(by_key.remove(k).ok_or(ListDeltaError::OrderMismatch)?);
        }
    }
    Ok(())
}

/// A subscriber's copy of a keyed list watch: feed it every frame's JSON and
/// read back the whole list. Accepts both [`ListFrame`]s and the plain arrays
/// of engines (or fixtures) that don't speak `listDelta`.
pub struct ListMirror<T> {
    rows: Vec<T>,
    key: fn(&T) -> &str,
}

impl<T: DeserializeOwned> ListMirror<T> {
    pub fn new(key: fn(&T) -> &str) -> Self {
        Self { rows: Vec::new(), key }
    }

    pub fn ingest(&mut self, frame: serde_json::Value) -> Result<&[T], ListDeltaError> {
        let frame = if frame.is_array() {
            ListFrame::Reset {
                rows: serde_json::from_value(frame).map_err(|e| ListDeltaError::Malformed(e.to_string()))?,
            }
        } else {
            serde_json::from_value(frame).map_err(|e| ListDeltaError::Malformed(e.to_string()))?
        };
        apply(&mut self.rows, frame, self.key)?;
        Ok(&self.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Row {
        id: String,
        v: u32,
    }

    fn row(id: &str, v: u32) -> Row {
        Row { id: id.into(), v }
    }

    fn key(row: &Row) -> &str {
        &row.id
    }

    fn roundtrip(prev: &[Row], next: &[Row]) -> ListFrame<Row> {
        let frame = diff(prev, next, key);
        // Through JSON, exactly as on the wire.
        let wire: ListFrame<Row> = serde_json::from_value(serde_json::to_value(&frame).unwrap()).unwrap();
        let mut rows = prev.to_vec();
        apply(&mut rows, wire, key).unwrap();
        assert_eq!(rows, next, "{prev:?} -> {next:?} via {frame:?}");
        frame
    }

    #[test]
    fn a_changed_row_travels_alone() {
        let prev: Vec<_> = (0..500).map(|i| row(&format!("{i:04}"), 0)).collect();
        let mut next = prev.clone();
        next[250].v = 1;
        let frame = roundtrip(&prev, &next);
        assert_eq!(
            frame,
            ListFrame::Delta { upsert: vec![row("0250", 1)], remove: vec![], order: None }
        );
    }

    #[test]
    fn an_unchanged_wake_up_is_an_empty_delta() {
        let rows = vec![row("a", 1), row("b", 2)];
        let frame = roundtrip(&rows, &rows);
        assert_eq!(serde_json::to_value(&frame).unwrap(), json!({ "kind": "delta" }));
    }

    #[test]
    fn order_is_sent_only_when_appending_would_be_wrong() {
        let prev = vec![row("a", 1), row("c", 3)];
        assert!(matches!(
            roundtrip(&prev, &[row("a", 1), row("c", 3), row("d", 4)]),
            ListFrame::Delta { order: None, .. }
        ));
        assert!(matches!(
            roundtrip(&prev, &[row("a", 1), row("b", 2), row("c", 3)]),
            ListFrame::Delta { order: Some(_), .. }
        ));
        assert!(matches!(roundtrip(&prev, &[row("c", 3)]), ListFrame::Delta { order: None, .. }));
        assert!(matches!(
            roundtrip(&prev, &[row("c", 3), row("a", 1)]),
            ListFrame::Delta { order: Some(_), .. }
        ));
    }

    #[test]
    fn duplicate_keys_fall_back_to_reset() {
        let dup = vec![row("a", 1), row("a", 2)];
        assert!(matches!(roundtrip(&[row("a", 1)], &dup), ListFrame::Reset { .. }));
        assert!(matches!(roundtrip(&dup, &[row("a", 1)]), ListFrame::Reset { .. }));
    }

    #[test]
    fn random_sequences_reproduce_every_list() {
        // Deterministic LCG: inserts, removals, edits and reorders.
        let mut seed: u64 = 0x5eed;
        let mut next_rand = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut prev: Vec<Row> = Vec::new();
        for _ in 0..2_000 {
            let mut next = prev.clone();
            for _ in 0..next_rand(4) {
                match next_rand(4) {
                    0 => next.push(row(&format!("k{}", next_rand(40)), next_rand(3) as u32)),
                    1 if !next.is_empty() => {
                        next.remove(next_rand(next.len() as u64) as usize);
                    }
                    2 if !next.is_empty() => {
                        let i = next_rand(next.len() as u64) as usize;
                        next[i].v += 1;
                    }
                    _ if next.len() > 1 => {
                        let i = next_rand(next.len() as u64) as usize;
                        let moved = next.remove(i);
                        next.insert(next_rand(next.len() as u64) as usize, moved);
                    }
                    _ => {}
                }
            }
            roundtrip(&prev, &next);
            prev = next;
        }
    }

    #[test]
    fn the_mirror_follows_resets_deltas_and_legacy_arrays() {
        let mut mirror = ListMirror::<Row>::new(key);
        assert_eq!(
            mirror.ingest(json!([{ "id": "a", "v": 1 }])).unwrap(),
            [row("a", 1)]
        );
        assert_eq!(
            mirror
                .ingest(json!({ "kind": "delta", "upsert": [{ "id": "b", "v": 2 }] }))
                .unwrap(),
            [row("a", 1), row("b", 2)]
        );
        assert_eq!(
            mirror.ingest(json!({ "kind": "delta", "remove": ["a"] })).unwrap(),
            [row("b", 2)]
        );
        assert_eq!(
            mirror.ingest(json!({ "kind": "delta", "order": ["b", "a"] })),
            Err(ListDeltaError::OrderMismatch)
        );
        assert_eq!(
            mirror
                .ingest(json!({ "kind": "reset", "rows": [{ "id": "c", "v": 3 }] }))
                .unwrap(),
            [row("c", 3)]
        );
        assert!(matches!(mirror.ingest(json!({ "kind": "nope" })), Err(ListDeltaError::Malformed(_))));
    }
}
