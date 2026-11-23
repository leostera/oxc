//! `--cache`: remember which files are formatted, and what the walk and the configs read,
//! so a later run reads and formats only what changed.
//!
//! Invariants:
//! - A run with the cache prints, exits and writes what the same run without it would.
//! - A file is reported unchanged without formatting only under a key covering the build,
//!   its options, its path and its content. A verdict that called into Node is kept only in the
//!   run state, and only while everything Node read for this shape's runs reads the same.
//! - A `stat` is trusted only outside its racy window (see [`oxc_cache::Observation::holds_for`]);
//!   observations are always timed at the start of the run, before anything is read.
//! - The cache never changes an answer by failing: anything unreadable is a miss,
//!   anything unwritable is skipped.

mod files;
#[cfg(feature = "napi")]
mod js;
mod state;
mod walk;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Serialize;

use oxc_cache::{BuildId, Cache, CacheDir, Digest, Inputs, Timestamp};

pub use files::{CachedFile, FileCache, Replay};
pub use state::Verdict;
pub use walk::WalkRecorder;

/// The cache's on-disk format: another version's directory is never read.
const FORMAT: &str = "v1";
/// Run shapes kept, by most recent write: their states in `runs/`, their verdicts in `files/`.
const MAX_RUNS: usize = 256;
use state::{JsConfigs, RunState, Walk};

#[cfg(feature = "napi")]
use crate::core::{JsConfigLoaderCb, JsDependenciesCb};

/// What Node read: for JS/TS configs, or in the format workers.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "napi"), expect(dead_code))]
enum NodeReads {
    /// Evaluating JS/TS configs.
    Configs,
    /// Prettier, its plugins, the Tailwind sorter.
    Formatting,
}

/// What decides a run's walk and configs, not its output mode or thread count:
/// runs of one shape share a state.
#[derive(Serialize)]
pub struct RunShape<'a> {
    /// The current directory: paths are relative to it.
    pub cwd: &'a Path,
    /// The path arguments.
    pub paths: &'a [PathBuf],
    /// `--config`.
    pub config: Option<&'a Path>,
    /// Whether nested configs are used (what `--config` and `--disable-nested-config` resolve to).
    pub use_nested_configs: bool,
    /// `--disable-nested-config`.
    pub disable_nested_config: bool,
    /// `--ignore-path`.
    pub ignore_paths: &'a [PathBuf],
    /// `--with-node-modules`.
    pub with_node_modules: bool,
    /// `VP_VERSION`, which decides how a Vite+ config is read.
    pub vp_version: Option<&'a [u8]>,
}

/// What of the last run still holds.
#[derive(Default)]
struct Kept {
    /// The JS/TS config values, if what Node loaded for them reads the same.
    js_configs: Option<JsConfigs>,
    /// The walk, if it and the configs read the same: then it isn't walked again.
    walk: Option<Walk>,
    /// What Node read formatting files, if it reads the same: its fingerprint keys the
    /// verdicts that went through Node.
    node_reads: Option<Inputs>,
}

impl Kept {
    /// What of `prev`, the last run's state, still holds at `now`.
    fn recheck(prev: Option<&RunState>, now: Timestamp) -> Self {
        let Some(prev) = prev else { return Self::default() };
        let js_configs = prev.js_configs.as_ref().and_then(|js| {
            Some(JsConfigs { values: js.values.clone(), inputs: js.inputs.recheck(now)? })
        });
        // The walk depends on the configs' values: kept only with them.
        let walk = js_configs.as_ref().and(prev.walk.as_ref()).and_then(|walk| {
            Some(Walk {
                inputs: walk.inputs.recheck(now)?,
                any_config_found: walk.any_config_found,
                files: walk.files.clone(),
            })
        });
        let node_reads = prev.node_reads.as_ref().and_then(|reads| reads.recheck(now));
        Self { js_configs, walk, node_reads }
    }
}

/// One run with `--cache`.
pub struct Session {
    /// The cache directory: `runs/` for run states, `files/` for verdicts, `store`, `builds`.
    dir: Arc<CacheDir>,
    /// This run's shape, with the build: names its state and its verdicts.
    shape: Digest,
    /// When the run started, before anything was read: every `stat` it records counts as
    /// taken then.
    now: Timestamp,
    /// The last run's state.
    prev: Option<RunState>,
    /// What of the last run's state still holds.
    kept: Kept,
    /// The verdicts, shared with the format service.
    files: Arc<FileCache>,
    /// What the next run keeps of this one, but for its files.
    next: Option<RunState>,
    /// The JS/TS config values, answered from the kept ones where they hold.
    #[cfg(feature = "napi")]
    js: js::JsConfigMemo,
}

impl Session {
    /// Open the cache at `location` for a run of `shape`. `None` if the build can't be identified
    /// or the directory can't be created: the run is then uncached.
    pub fn open(location: Option<&Path>, shape: &RunShape<'_>) -> Option<Self> {
        // Taken before anything is read: every stat recorded by this run counts as observed now.
        let now = Timestamp::now();
        let root = match location {
            Some(location) => crate::core::utils::normalize_relative_path(shape.cwd, location),
            None => shape.cwd.join("node_modules/.cache/oxfmt"),
        };
        let dir = Arc::new(CacheDir::open(&root.join(FORMAT))?);
        let build = Digest::of(&(env!("CARGO_PKG_VERSION"), BuildId::current(&dir, "builds")?));
        let shape_id = Digest::try_of(&(build, shape))?;
        // The shape's state and its verdicts are independent files: read together.
        let ((prev, kept), cache) = rayon::join(
            || {
                let prev: Option<RunState> = dir.read(&format!("runs/{shape_id}"));
                let kept = Kept::recheck(prev.as_ref(), now);
                (prev, kept)
            },
            || Cache::open(Arc::clone(&dir), format!("files/{shape_id}")).started_at(now),
        );
        tracing::debug!(
            target: "oxfmt::cache",
            state = prev.is_some(),
            js = kept.js_configs.is_some(),
            walk = kept.walk.is_some(),
            node_reads = kept.node_reads.is_some(),
            "kept"
        );
        let files = Arc::new(FileCache::new(
            cache,
            shape.cwd,
            build,
            kept.walk.as_ref().map(|walk| walk.files.clone()),
            kept.node_reads.as_ref().map(Inputs::fingerprint),
        ));
        Some(Self {
            #[cfg(feature = "napi")]
            js: js::JsConfigMemo::new(kept.js_configs.as_ref()),
            dir,
            shape: shape_id,
            now,
            prev,
            kept,
            files,
            next: None,
        })
    }

    /// The JS/TS config loader: kept values, and `inner` for the others.
    #[cfg(feature = "napi")]
    pub fn js_config_loader(&self, inner: JsConfigLoaderCb) -> JsConfigLoaderCb {
        self.js.loader(inner)
    }

    /// What Node read, for configs or in the format workers.
    #[cfg(feature = "napi")]
    pub fn set_js_dependencies(&mut self, dependencies: JsDependenciesCb) {
        self.js.dependencies = Some(dependencies);
    }

    /// What Node read, as inputs: `None` if it can't say, or if some of it changed during the run.
    #[cfg_attr(not(feature = "napi"), expect(clippy::unused_self))]
    fn read_by_node(&self, reads: NodeReads) -> Option<Inputs> {
        #[cfg(feature = "napi")]
        return self.js.read_by_node(self.now, reads);
        #[cfg(not(feature = "napi"))]
        {
            let _ = reads;
            None
        }
    }

    /// The verdicts, for the format service.
    pub fn files(&self) -> Arc<FileCache> {
        Arc::clone(&self.files)
    }

    /// The last run's files, if its walk is kept: nothing the walk or the configs read changed.
    pub fn replay(&self) -> Option<Replay> {
        self.kept.walk.as_ref().map(|walk| self.files.replay(walk.any_config_found))
    }

    /// This run's files are the last run's: what it keeps of the walk and configs is the same.
    pub fn replayed(&mut self) {
        self.next = Some(RunState {
            js_configs: self.kept.js_configs.clone(),
            walk: self.kept.walk.clone(),
            ..RunState::default()
        });
    }

    /// A recorder for a walk, with config discovery as it is for this run.
    pub fn recorder(&self, config_names: Vec<&'static str>, nested: bool) -> Arc<WalkRecorder> {
        Arc::new(WalkRecorder::new(self.now, config_names, nested))
    }

    /// This run walked, recording into `recorder`: what the next run can keep of it.
    pub fn walked(&mut self, recorder: Arc<WalkRecorder>, any_config_found: bool) {
        let js_configs = self.js_configs();
        let recorded = Arc::try_unwrap(recorder).ok().and_then(WalkRecorder::finish);
        tracing::debug!(target: "oxfmt::cache", walk = recorded.is_some(), js = js_configs.is_some(), "walked");
        // Its files are known once the format service is done with them: see `save`.
        let walk = js_configs.as_ref().and(recorded).map(|inputs| Walk {
            inputs,
            any_config_found,
            files: vec![],
        });
        self.next = Some(RunState { js_configs, walk, ..RunState::default() });
    }

    /// This run's JS/TS config values and what Node loaded for them: the kept inputs, plus what
    /// Node loaded if it evaluated anything this run.
    #[cfg_attr(
        not(feature = "napi"),
        expect(clippy::unused_self, clippy::unnecessary_wraps, reason = "no JS without napi")
    )]
    fn js_configs(&self) -> Option<JsConfigs> {
        #[cfg(feature = "napi")]
        {
            let values = self.js.values();
            let kept = self.kept.js_configs.as_ref().map_or_default(|js| js.inputs.clone());
            let inputs = if self.js.evaluated() {
                kept.merge(self.read_by_node(NodeReads::Configs)?)
            } else {
                kept
            };
            Some(JsConfigs { values, inputs })
        }
        // The pure Rust CLI evaluates no JS/TS config.
        #[cfg(not(feature = "napi"))]
        Some(JsConfigs { values: vec![], inputs: Inputs::default() })
    }

    /// Save this run: its verdicts, and its state if it differs from the last. Called once the
    /// format service is done with [`Self::files`].
    pub fn save(self) {
        let Some(mut next) = self.next.clone() else { return };
        let (walked, with_node) = self.files.finish();
        if let (Some(walk), Some(files)) = (&mut next.walk, walked) {
            walk.files = files;
        }
        next.node_reads = if with_node {
            let read = self.read_by_node(NodeReads::Formatting);
            read.map(|read| self.kept.node_reads.clone().unwrap_or_default().merge(read))
        } else {
            self.kept.node_reads.clone()
        };
        self.files.save(next.node_reads.as_ref().map(Inputs::fingerprint));
        if self.prev.as_ref() != Some(&next) {
            self.dir.write(&format!("runs/{}", self.shape), &next);
            self.dir.prune("runs", MAX_RUNS);
            self.dir.prune("files", MAX_RUNS);
        }
    }
}
