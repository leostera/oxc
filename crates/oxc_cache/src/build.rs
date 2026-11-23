//! Which build is running: its binary's digest, so results kept by one build are never used by
//! another (every dev build differs).
//!
//! Hashing the binary costs a read of it, so its digest is remembered by its path and `stat`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{digest::Digest, dir::CacheDir, stat::Stat};

/// How many binaries' digests are remembered, most recently hashed first. One cache directory
/// sees a few (a project's installed build, the one a task runner bundles, a dev build), and
/// alternating between them shouldn't hash them again; but a binary at a new path (a new
/// version in a package store) adds one, so the oldest are dropped. A binary forgotten is only
/// hashed again.
const MAX_BINARIES: usize = 16;

/// A binary whose digest is remembered.
#[derive(Serialize, Deserialize)]
struct Binary {
    /// Where it is, absolute.
    path: PathBuf,
    /// Its `stat` when it was hashed: a binary rebuilt in place has another.
    stat: Stat,
    /// The digest of its bytes.
    digest: Digest,
}

/// The running build: its binary's digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BuildId(Digest);

impl BuildId {
    /// The file this code was loaded from: a napi `.node` library, or the executable.
    fn binary_path() -> Option<PathBuf> {
        #[cfg(unix)]
        {
            // SAFETY: `Dl_info` is plain C data (pointers and integers), valid when zeroed.
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            let addr = (Self::binary_path as *const ()).cast::<libc::c_void>();
            // SAFETY: `dladdr` only reads the address and fills `info`, whose strings it owns.
            if unsafe { libc::dladdr(addr, &raw mut info) } == 0 || info.dli_fname.is_null() {
                return None;
            }
            // SAFETY: `dli_fname` is a NUL-terminated string valid while the library is loaded.
            let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
            std::path::absolute(Path::new(name.to_str().ok()?)).ok()
        }
        // Not found elsewhere yet: the executable may be another program that loaded this code.
        #[cfg(not(unix))]
        None
    }

    /// Returns the running build, or `None` if its binary can't be found or read (then nothing
    /// should be cached: a result can't be tied to the code that produced it). Binaries' digests
    /// are remembered in `file` of `dir`.
    pub fn current(dir: &CacheDir, file: &str) -> Option<Self> {
        let path = Self::binary_path()?;
        let stat = Stat::of(&fs::metadata(&path).ok()?);
        let mut known: Vec<Binary> = dir.read(file).unwrap_or_default();
        let digest = if let Some(binary) =
            known.iter().find(|binary| binary.path == path && binary.stat == stat)
        {
            binary.digest
        } else {
            let digest = Digest::of_bytes(&fs::read(&path).ok()?);
            known.retain(|binary| binary.path != path);
            known.insert(0, Binary { path, stat, digest });
            known.truncate(MAX_BINARIES);
            dir.write(file, &known);
            digest
        };
        Some(Self(digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_remembers_this_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = CacheDir::open(tmp.path()).unwrap();
        let first = BuildId::current(&dir, "builds").expect("the test binary is found");
        assert!(dir.read::<Vec<Binary>>("builds").is_some_and(|known| known.len() == 1));
        assert_eq!(BuildId::current(&dir, "builds"), Some(first), "the same binary, the same id");
    }
}
