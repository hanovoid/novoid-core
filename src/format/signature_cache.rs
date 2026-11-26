use crate::constants::{signature_store_path, MIN_SIGNATURE_LENGTH};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::{Mutex, OnceLock};

const STORE_VERSION: u32 = 1;
const DEFAULT_MAX_TOOL_SIGNATURES: usize = 20_000;
const DEFAULT_MAX_THINKING_SIGNATURES: usize = 10_000;

fn parse_pos_env(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(fallback)
}

#[derive(Default)]
struct OrderedLru {
    map: HashMap<String, (String, u64)>,
    order: BTreeMap<u64, String>,
    seq: u64,
    max_entries: usize,
}

impl OrderedLru {
    fn new(max_entries: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            seq: 0,
            max_entries,
        }
    }

    fn set(&mut self, key: String, value: String) -> bool {
        let changed = self
            .map
            .get(&key)
            .map(|(v, _)| v != &value)
            .unwrap_or(true);

        if let Some((_, old_seq)) = self.map.remove(&key) {
            self.order.remove(&old_seq);
        }

        self.seq += 1;
        let s = self.seq;
        self.map.insert(key.clone(), (value, s));
        self.order.insert(s, key);

        while self.map.len() > self.max_entries {
            if let Some((&oldest_seq, oldest_key)) = self.order.iter().next() {
                let k = oldest_key.clone();
                self.order.remove(&oldest_seq);
                self.map.remove(&k);
            } else {
                break;
            }
        }
        changed
    }

    fn get(&mut self, key: &str) -> Option<String> {
        let (val, old_seq) = self.map.get(key)?.clone();
        self.order.remove(&old_seq);
        self.seq += 1;
        let s = self.seq;
        self.map.insert(key.to_string(), (val.clone(), s));
        self.order.insert(s, key.to_string());
        Some(val)
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    fn to_map(&self) -> HashMap<String, String> {
        let mut out = HashMap::with_capacity(self.map.len());
        for (_, key) in &self.order {
            if let Some((val, _)) = self.map.get(key) {
                out.insert(key.clone(), val.clone());
            }
        }
        out
    }
}

#[derive(Serialize, Deserialize)]
struct StorePayload {
    version: u32,
    #[serde(default)]
    tool: HashMap<String, String>,
    #[serde(default)]
    think: HashMap<String, String>,
}

struct SignatureCacheState {
    tool: OrderedLru,
    think: OrderedLru,
    dirty: bool,
    generation: u64,
}

impl SignatureCacheState {
    fn new() -> Self {
        let max_tool = parse_pos_env(
            "ANTIGRAVITY_SIGNATURE_CACHE_MAX_TOOL",
            DEFAULT_MAX_TOOL_SIGNATURES,
        );
        let max_think = parse_pos_env(
            "ANTIGRAVITY_SIGNATURE_CACHE_MAX_THINK",
            DEFAULT_MAX_THINKING_SIGNATURES,
        );
        let mut st = Self {
            tool: OrderedLru::new(max_tool),
            think: OrderedLru::new(max_think),
            dirty: false,
            generation: 0,
        };
        st.load_from_disk();
        st
    }

    fn load_from_disk(&mut self) {
        let path = signature_store_path();
        let Ok(raw) = fs::read_to_string(&path) else {
            return;
        };
        if raw.trim().is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<StorePayload>(&raw) else {
            return;
        };
        if parsed.version != STORE_VERSION {
            return;
        }
        let pruned = parsed.tool.len() > self.tool.max_entries
            || parsed.think.len() > self.think.max_entries;

        for (id, sig) in parsed.tool {
            self.tool.set(id, sig);
        }
        for (sig, fam) in parsed.think {
            self.think.set(sig, fam);
        }
        if pruned {
            self.dirty = true;
        }
    }
}

static SIG_CACHE: OnceLock<Mutex<SignatureCacheState>> = OnceLock::new();

fn cache_lock() -> &'static Mutex<SignatureCacheState> {
    SIG_CACHE.get_or_init(|| Mutex::new(SignatureCacheState::new()))
}

pub fn init_background_flush() {
    let _ = cache_lock();
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            flush_signature_store().await;
        }
    });
}

pub fn cache_signature(tool_use_id: &str, signature: &str) {
    if tool_use_id.is_empty() || signature.is_empty() {
        return;
    }
    if let Ok(mut guard) = cache_lock().lock() {
        if guard.tool.set(tool_use_id.to_string(), signature.to_string()) {
            guard.dirty = true;
        }
    }
}

pub fn get_cached_signature(tool_use_id: &str) -> Option<String> {
    if tool_use_id.is_empty() {
        return None;
    }
    cache_lock().lock().ok()?.tool.get(tool_use_id)
}

pub fn cache_thinking_signature(signature: &str, model_family: &str) {
    if signature.len() < MIN_SIGNATURE_LENGTH || model_family.is_empty() {
        return;
    }
    if let Ok(mut guard) = cache_lock().lock() {
        if guard
            .think
            .set(signature.to_string(), model_family.to_string())
        {
            guard.dirty = true;
        }
    }
}

pub fn get_cached_signature_family(signature: &str) -> Option<String> {
    if signature.is_empty() {
        return None;
    }
    cache_lock().lock().ok()?.think.get(signature)
}

pub async fn flush_signature_store() {
    let (data, gen) = {
        let Ok(mut guard) = cache_lock().lock() else {
            return;
        };
        if !guard.dirty {
            return;
        }
        let payload = StorePayload {
            version: STORE_VERSION,
            tool: guard.tool.to_map(),
            think: guard.think.to_map(),
        };
        let Ok(serialized) = serde_json::to_string(&payload) else {
            return;
        };
        guard.dirty = false;
        (serialized, guard.generation)
    };

    let target = signature_store_path();
    if let Some(parent) = target.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let tmp = target.with_extension(format!("json.tmp-{}", std::process::id()));
    if tokio::fs::write(&tmp, data).await.is_ok() {
        let still_valid = cache_lock()
            .lock()
            .map(|g| g.generation == gen)
            .unwrap_or(false);
        if still_valid {
            let _ = tokio::fs::rename(&tmp, &target).await;
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

pub fn flush_signature_store_sync() {
    let Ok(mut guard) = cache_lock().lock() else {
        return;
    };
    if !guard.dirty {
        return;
    }
    guard.generation += 1;
    let payload = StorePayload {
        version: STORE_VERSION,
        tool: guard.tool.to_map(),
        think: guard.think.to_map(),
    };
    if let Ok(serialized) = serde_json::to_string(&payload) {
        let target = signature_store_path();
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = target.with_extension(format!("json.tmp-sync-{}", std::process::id()));
        if fs::write(&tmp, serialized).is_ok() && fs::rename(&tmp, &target).is_ok() {
            guard.dirty = false;
        }
    }
}

pub fn clear_all_signature_caches() {
    if let Ok(mut guard) = cache_lock().lock() {
        guard.tool.clear();
        guard.think.clear();
        guard.dirty = false;
        guard.generation += 1;
    }
    let _ = fs::remove_file(signature_store_path());
}

pub fn clear_thinking_signature_cache() {
    if let Ok(mut guard) = cache_lock().lock() {
        guard.think.clear();
        guard.dirty = true;
    }
}
