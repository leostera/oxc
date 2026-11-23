//! JS/TS config values for `--cache`: answered from the last run's when what Node loaded for
//! them reads the same, else evaluated by Node; either way remembered for the next run.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use rustc_hash::FxHashMap;

use oxc_cache::{Inputs, Recorder, Timestamp};

use super::{
    NodeReads,
    state::{JsConfigValue, JsConfigs},
};
use crate::core::{JsConfigLoaderCb, JsDependenciesCb, JsDependencyKind};

/// The JS/TS config values of a run: the last run's where they still hold, and what Node read.
pub struct JsConfigMemo {
    /// Shared with the loader it hands out, which config discovery calls from its threads.
    inner: Arc<Memo>,
    /// Asks Node what it read, for configs or in the format workers; `None` without Node.
    pub dependencies: Option<JsDependenciesCb>,
}

/// What [`JsConfigMemo::loader`] answers from and records.
struct Memo {
    /// The last run's values by config path (JSON, or `None` for a config without a value),
    /// if what Node loaded for them reads the same.
    kept: FxHashMap<String, Option<String>>,
    /// The values loaded this run, kept or evaluated: what the next run keeps.
    seen: Mutex<Vec<JsConfigValue>>,
    /// Whether Node evaluated a config this run: then what it loaded is asked for.
    evaluated: AtomicBool,
}

impl JsConfigMemo {
    /// A memo answering from `kept`, the last run's values if they still hold.
    pub fn new(kept: Option<&JsConfigs>) -> Self {
        let kept = kept.map_or_default(|js| {
            js.values.iter().map(|c| (c.path.clone(), c.value.clone())).collect()
        });
        let inner =
            Arc::new(Memo { kept, seen: Mutex::default(), evaluated: AtomicBool::new(false) });
        Self { inner, dependencies: None }
    }

    /// A loader answering kept values, and calling `inner` for the others.
    pub fn loader(&self, inner: JsConfigLoaderCb) -> JsConfigLoaderCb {
        let memo = Arc::clone(&self.inner);
        Arc::new(move |path: String| {
            let value = match memo.kept.get(&path) {
                Some(Some(json)) => serde_json::from_str(json).map_err(|err| err.to_string())?,
                Some(None) => serde_json::Value::Null,
                None => {
                    memo.evaluated.store(true, Ordering::Relaxed);
                    inner(path.clone())?
                }
            };
            let json = (!value.is_null()).then(|| value.to_string());
            memo.seen.lock().unwrap().push(JsConfigValue { path, value: json });
            Ok(value)
        })
    }

    /// The values loaded this run, one per path.
    pub fn values(&self) -> Vec<JsConfigValue> {
        let mut values = std::mem::take(&mut *self.inner.seen.lock().unwrap());
        values.sort_by(|a, b| a.path.cmp(&b.path));
        values.dedup_by(|a, b| a.path == b.path);
        values
    }

    /// Whether Node evaluated a config this run.
    pub fn evaluated(&self) -> bool {
        self.inner.evaluated.load(Ordering::Relaxed)
    }

    /// What Node read this run, for `reads`, as inputs: `None` if Node can't say, or if some of
    /// it changed since the run started at `now`.
    pub fn read_by_node(&self, now: Timestamp, reads: NodeReads) -> Option<Inputs> {
        let kind = match reads {
            NodeReads::Configs => JsDependencyKind::Config,
            NodeReads::Formatting => JsDependencyKind::Format,
        };
        let read = self.dependencies.as_ref()?(kind)?;
        tracing::debug!(target: "oxfmt::cache", ?kind, files = read.files.len(), dirs = read.dirs.len(), "read by node");
        // A config's own listings aren't inputs: every new file would change them.
        let dirs = match reads {
            NodeReads::Configs => &[][..],
            NodeReads::Formatting => &read.dirs,
        };
        Recorder::reported(now, &read.files, dirs)
    }
}
