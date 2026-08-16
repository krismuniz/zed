use crate::grammar::Grammar;
use crate::highlight_map::{CaptureId, CapturedRange, HighlightId};
use collections::FxHasher;
use lru::LruCache;
use parking_lot::Mutex;
use smallvec::{Array, SmallVec};
use std::{fmt, hash::Hash, hash::Hasher as _, ops::Range, sync::Arc};

pub const MAX_TEXT_CAPTURES_CACHE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TEXT_CAPTURES_ENTRY_BYTES: usize = MAX_TEXT_CAPTURES_CACHE_BYTES / 8;
pub const MAX_CHUNK_HIGHLIGHT_CACHE_BYTES: usize = 10 * 1024 * 1024;

struct CostBudgetedLru<K: Hash + Eq, V> {
    entries: LruCache<K, (V, usize)>,
    total_cost: usize,
    max_total_cost: usize,
    max_entry_cost: usize,
}

impl<K: Hash + Eq, V> CostBudgetedLru<K, V> {
    fn new(max_total_cost: usize, max_entry_cost: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_cost: 0,
            max_total_cost,
            max_entry_cost,
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(value, _)| value)
    }

    fn insert(&mut self, key: K, value: V, cost: usize) {
        if cost > self.max_entry_cost {
            return;
        }
        if let Some((_, old_cost)) = self.entries.put(key, (value, cost)) {
            self.total_cost -= old_cost;
        }
        self.total_cost += cost;
        while self.total_cost > self.max_total_cost {
            let Some((_, (_, evicted_cost))) = self.entries.pop_lru() else {
                break;
            };
            self.total_cost -= evicted_cost;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total_cost = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextCapturesKey {
    text_hash: u64,
    text_len: usize,
    range: Range<usize>,
}

impl TextCapturesKey {
    pub fn new<'a>(
        text_chunks: impl Iterator<Item = &'a str>,
        text_len: usize,
        range: Range<usize>,
    ) -> Self {
        let mut hasher = FxHasher::default();
        for chunk in text_chunks {
            hasher.write(chunk.as_bytes());
        }
        Self {
            text_hash: hasher.finish(),
            text_len,
            range,
        }
    }
}

struct TextCapturesEntry {
    text: Arc<str>,
    captures: Arc<[CapturedRange]>,
}

pub struct TextHighlightCache(Mutex<CostBudgetedLru<TextCapturesKey, TextCapturesEntry>>);

impl Default for TextHighlightCache {
    fn default() -> Self {
        Self(Mutex::new(CostBudgetedLru::new(
            MAX_TEXT_CAPTURES_CACHE_BYTES,
            MAX_TEXT_CAPTURES_ENTRY_BYTES,
        )))
    }
}

impl TextHighlightCache {
    pub fn get<'a>(
        &self,
        key: &TextCapturesKey,
        text_chunks: impl Iterator<Item = &'a str>,
    ) -> Option<Arc<[CapturedRange]>> {
        let mut cache = self.0.lock();
        let entry = cache.get(key)?;
        if !chunks_match_text(&entry.text, text_chunks) {
            return None;
        }
        Some(entry.captures.clone())
    }

    pub fn insert(&self, key: TextCapturesKey, text: Arc<str>, captures: Arc<[CapturedRange]>) {
        let cost = text.len()
            + captures
                .iter()
                .map(|captured| {
                    size_of::<CapturedRange>() + small_vec_heap_bytes(&captured.capture_ids)
                })
                .sum::<usize>();
        self.0
            .lock()
            .insert(key, TextCapturesEntry { text, captures }, cost);
    }
}

fn small_vec_heap_bytes<A: Array>(vec: &SmallVec<A>) -> usize {
    if vec.spilled() {
        vec.capacity() * size_of::<A::Item>()
    } else {
        0
    }
}

fn chunks_match_text<'a>(text: &str, text_chunks: impl Iterator<Item = &'a str>) -> bool {
    let mut remaining = text;
    for chunk in text_chunks {
        let Some(rest) = remaining.strip_prefix(chunk) else {
            return false;
        };
        remaining = rest;
    }
    remaining.is_empty()
}

pub type HighlightRun = (Range<usize>, HighlightId);

pub type RowChunkId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightCaptureRef {
    pub grammar_index: usize,
    pub capture_id: CaptureId,
}

#[derive(Clone, Debug)]
pub struct ChunkCaptureRun {
    pub range: Range<usize>,
    pub stack: SmallVec<[HighlightCaptureRef; 4]>,
}

#[derive(Clone)]
pub struct ChunkCaptures {
    pub grammars: SmallVec<[Arc<Grammar>; 2]>,
    pub runs: Arc<[ChunkCaptureRun]>,
}

pub struct ChunkHighlightCache(Mutex<CostBudgetedLru<RowChunkId, ChunkCaptures>>);

impl Default for ChunkHighlightCache {
    fn default() -> Self {
        Self(Mutex::new(CostBudgetedLru::new(
            MAX_CHUNK_HIGHLIGHT_CACHE_BYTES,
            MAX_CHUNK_HIGHLIGHT_CACHE_BYTES,
        )))
    }
}

impl fmt::Debug for ChunkHighlightCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkHighlightCache")
            .finish_non_exhaustive()
    }
}

impl ChunkHighlightCache {
    pub fn get(&self, chunk_id: RowChunkId) -> Option<ChunkCaptures> {
        self.0.lock().get(&chunk_id).cloned()
    }

    pub fn insert(&self, chunk_id: RowChunkId, captures: ChunkCaptures) {
        let cost = captures.grammars.len() * size_of::<Arc<Grammar>>()
            + captures
                .runs
                .iter()
                .map(|run| size_of::<ChunkCaptureRun>() + small_vec_heap_bytes(&run.stack))
                .sum::<usize>();
        self.0.lock().insert(chunk_id, captures, cost);
    }

    pub fn clear(&mut self) {
        self.0.get_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_evicts_least_recently_used() {
        let mut cache = CostBudgetedLru::<&str, u32>::new(100, 100);
        cache.insert("a", 1, 40);
        cache.insert("b", 2, 40);
        cache.get(&"a");
        cache.insert("c", 3, 40);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.total_cost, 80);
    }

    #[test]
    fn test_replacing_an_entry_updates_the_budget() {
        let mut cache = CostBudgetedLru::<&str, u32>::new(100, 100);
        cache.insert("a", 1, 60);
        cache.insert("a", 2, 30);
        assert_eq!(cache.total_cost, 30);
        assert_eq!(cache.get(&"a"), Some(&2));
        cache.insert("b", 3, 70);
        assert_eq!(cache.get(&"a"), Some(&2));
        assert_eq!(cache.get(&"b"), Some(&3));
    }

    #[test]
    fn test_oversized_entries_are_rejected_without_flushing() {
        let mut cache = CostBudgetedLru::<&str, u32>::new(100, 50);
        cache.insert("a", 1, 40);
        cache.insert("b", 2, 60);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.total_cost, 40);
    }

    #[test]
    fn test_clear_resets_the_budget() {
        let mut cache = CostBudgetedLru::<&str, u32>::new(100, 100);
        cache.insert("a", 1, 90);
        cache.clear();
        assert_eq!(cache.total_cost, 0);
        cache.insert("b", 2, 90);
        assert_eq!(cache.get(&"b"), Some(&2));
    }
}
