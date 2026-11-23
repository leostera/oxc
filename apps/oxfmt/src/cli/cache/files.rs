//! The per-file side of the cache, shared by the formatting threads: a [`Cache`] of the files
//! known formatted, keyed by the build and each file's options, and for a file formatted by way
//! of Node, by what Node read.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use oxc_cache::{Cache, Digest, Stat};

use super::state::Verdict;
use crate::core::{FormatPlan, oxfmtrc::FormatConfig};

/// What the cache keeps: a file is formatted. An unformatted file isn't put: it's formatted
/// again, as printing or writing it needs anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Formatted;

/// A file formatted by way of Node, to put once what Node read is known.
struct FormattedWithNode {
    /// Relative to the current directory.
    path: PathBuf,
    /// The build and its options, without what Node read.
    key: Digest,
    /// Its `stat` before it was read.
    stat: Stat,
    /// The digest of what was read.
    content: Digest,
}

/// The verdicts of a run shape, shared by the formatting threads.
pub struct FileCache {
    /// The current directory: verdicts are kept by paths relative to it.
    cwd: PathBuf,
    /// The running oxfmt: its version and its binary.
    build: Digest,
    /// The files known formatted.
    cache: Cache<Formatted>,
    /// The fingerprint of what Node read for the last runs, if it reads the same.
    node: Option<Digest>,
    /// The kept walk's files, in its order: a run reports them in it. `None` if it isn't kept.
    walk: Option<Vec<Box<str>>>,
    /// This run's files, by their place in the walk, if it walks.
    walked: Mutex<Vec<(usize, Box<str>)>>,
    /// Files formatted by way of Node this run: put once what Node read is known.
    with_node: Mutex<Vec<FormattedWithNode>>,
    /// Options digests by the `FormatConfig` they were computed for, held so its address stays unique.
    options: Mutex<FxHashMap<usize, (Arc<FormatConfig>, Digest)>>,
}

/// The files of the last run whose walk is kept: those whose `stat` says they're formatted,
/// counted, and the others, to format.
pub struct Replay {
    /// How many are known formatted by their `stat`: reported unchanged, never read.
    pub formatted: usize,
    /// In the walk's order, so they're reported in it.
    pub stale: Vec<PathBuf>,
    /// Whether the walk found any config, for the "No config found" hint.
    pub any_config_found: bool,
}

/// A file the format service is about to read.
pub struct CachedFile<'a> {
    /// The run's verdicts.
    files: &'a FileCache,
    /// Its path, relative to the current directory.
    path: PathBuf,
    /// The build and its options.
    key: Digest,
    /// Its `stat` when the format service received it, before reading it.
    stat: Stat,
}

impl FileCache {
    /// The verdicts in `cache`. `walk` is the kept walk's files; `node`, the fingerprint of what
    /// Node read for the last runs, if it reads the same.
    pub fn new(
        cache: Cache<Formatted>,
        cwd: &Path,
        build: Digest,
        walk: Option<Vec<Box<str>>>,
        node: Option<Digest>,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            build,
            cache,
            node,
            walk,
            walked: Mutex::default(),
            with_node: Mutex::default(),
            options: Mutex::default(),
        }
    }

    /// The kept walk's files. Sorted before the format service starts: its `par_bridge()` holds
    /// rayon's threads.
    pub fn replay(&self, any_config_found: bool) -> Replay {
        let walk = self.walk.as_deref().unwrap_or_default();
        let stale: Vec<Option<PathBuf>> =
            walk.par_iter()
                .map(|rel| {
                    let rel = Path::new(&**rel);
                    let path = self.cwd.join(rel);
                    let formatted = self.cache.entry(rel).zip(Stat::of_path(&path)).is_some_and(
                        |(entry, stat)| self.cache.get_by_stat(rel, entry.key, stat).is_some(),
                    );
                    (!formatted).then_some(path)
                })
                .collect();
        let formatted = stale.iter().filter(|path| path.is_none()).count();
        Replay { formatted, stale: stale.into_iter().flatten().collect(), any_config_found }
    }

    /// The file at `path`, the `order`-th the format service received; `None` if it's gone or
    /// its path isn't UTF-8 (such a file is never cached).
    pub fn lookup(&self, path: &Path, plan: &FormatPlan, order: usize) -> Option<CachedFile<'_>> {
        let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
        let name: Box<str> = rel.to_str()?.into();
        let stat = Stat::of_path(path)?;
        if self.walk.is_none() {
            self.walked.lock().unwrap().push((order, name));
        }
        let key = Digest::of(&(self.build, self.options(plan)));
        Some(CachedFile { files: self, path: rel.to_path_buf(), key, stat })
    }

    /// The digest of what a file's verdict depends on besides its path and content.
    fn options(&self, plan: &FormatPlan) -> Digest {
        let ptr = Arc::as_ptr(&plan.config) as usize;
        let config = self
            .options
            .lock()
            .unwrap()
            .entry(ptr)
            .or_insert_with(|| (Arc::clone(&plan.config), Digest::of(&*plan.config)))
            .1;
        Digest::of(&(format!("{:?}", plan.strategy), config))
    }

    /// Returns the files this run walked, in the walk's order (`None` if it replayed the kept
    /// walk), and whether a file was formatted by way of Node.
    pub fn finish(&self) -> (Option<Vec<Box<str>>>, bool) {
        let walked = self.walk.is_none().then(|| {
            let mut walked = std::mem::take(&mut *self.walked.lock().unwrap());
            walked.sort_unstable_by_key(|(order, _)| *order);
            walked.into_iter().map(|(_, path)| path).collect()
        });
        (walked, !self.with_node.lock().unwrap().is_empty())
    }

    /// Saves the files known formatted: those formatted by way of Node under `node`, the
    /// fingerprint of what Node read, if it can be known.
    pub fn save(&self, node: Option<Digest>) {
        let with_node = std::mem::take(&mut *self.with_node.lock().unwrap());
        if let Some(node) = node {
            for FormattedWithNode { path, key, stat, content } in with_node {
                self.cache.put(&path, Self::node_key(key, node), stat, content, Formatted);
            }
        }
        self.cache.save();
    }

    /// The key of a file formatted by way of Node: its key, and what Node read.
    fn node_key(key: Digest, node: Digest) -> Digest {
        Digest::of(&(key, node))
    }
}

impl CachedFile<'_> {
    /// The keys it may be known formatted under: by oxfmt alone, or by way of Node.
    fn keys(&self) -> impl Iterator<Item = Digest> {
        std::iter::once(self.key)
            .chain(self.files.node.map(|node| FileCache::node_key(self.key, node)))
    }

    /// Whether the file is known formatted from its `stat` alone.
    pub fn formatted_by_stat(&self) -> bool {
        let cache = &self.files.cache;
        self.keys().any(|key| cache.get_by_stat(&self.path, key, self.stat).is_some())
    }

    /// Whether the file, read and hashing to `content`, is known formatted: by the last run,
    /// or by any run of another shape.
    pub fn formatted_by_content(&self, content: Digest) -> bool {
        let cache = &self.files.cache;
        self.keys().any(|key| cache.get_by_content(&self.path, key, self.stat, content).is_some())
    }

    /// Record what formatting found.
    pub fn record(&self, content: Digest, verdict: Verdict) {
        match verdict {
            Verdict::Unformatted => {}
            Verdict::Formatted => {
                self.files.cache.put(&self.path, self.key, self.stat, content, Formatted);
            }
            Verdict::FormattedWithNode => {
                self.files.with_node.lock().unwrap().push(FormattedWithNode {
                    path: self.path.clone(),
                    key: self.key,
                    stat: self.stat,
                    content,
                });
            }
        }
    }
}
