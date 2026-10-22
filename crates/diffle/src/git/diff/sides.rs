//! Each side of a comparison as git sees it: a tree's entries, or the worktree through the index,
//! whose stat cache spares re-reading files that have not changed since they were staged.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
use gix::index::entry::{Flags, Mode, Stage, Stat};

use crate::git::BIG_FILE_THRESHOLD;

pub const FILE: u32 = 0o100644;
pub const EXECUTABLE: u32 = 0o100755;
pub const SYMLINK: u32 = 0o120000;
pub const GITLINK: u32 = 0o160000;

/// One path on one side.
#[derive(Clone, Debug)]
pub struct Item {
    pub path: BString,
    pub mode: u32,
    pub id: ObjectId,
    /// Read from the worktree rather than the object database.
    pub worktree: bool,
    pub size: Option<u64>,
    /// In the worktree but not the index.
    pub untracked: bool,
}

/// What a path holds, as git's file-type bits tell it: a typechange is a change between these.
pub fn kind(mode: u32) -> u32 {
    mode & 0o170000
}

/// Every entry of `tree`, sorted by path.
pub fn tree_items(repo: &gix::Repository, tree: ObjectId) -> anyhow::Result<Vec<Item>> {
    let index = repo.index_from_tree(&tree)?;
    Ok(index
        .entries()
        .iter()
        .map(|e| Item {
            path: e.path(&index).to_owned(),
            mode: e.mode.bits(),
            id: e.id,
            worktree: false,
            size: None,
            untracked: false,
        })
        .collect())
}

/// The worktree's tracked paths, sorted, each as git would store it now; a path that is gone (or
/// became a directory) is absent. The second list names the paths with unresolved conflicts.
pub fn worktree_items(repo: &gix::Repository, root: &Path) -> anyhow::Result<(Vec<Item>, Vec<BString>)> {
    let (mut pipeline, index) = repo.filter_pipeline(None)?;
    let caps = repo.filesystem_options()?;
    let stat_options = repo.stat_options()?;
    let timestamp = index.timestamp();
    let (mut items, mut conflicts) = (Vec::new(), Vec::new());
    let entries = index.entries();
    let mut i = 0;
    while i < entries.len() {
        let entry = &entries[i];
        let path = entry.path(&index);
        let mut next = i + 1;
        while next < entries.len() && entries[next].path(&index) == path {
            next += 1;
        }
        let stages = &entries[i..next];
        i = next;
        let staged = stages.iter().find(|e| e.stage() == Stage::Unconflicted);
        if staged.is_none() {
            conflicts.push(path.to_owned());
        }
        let entry = staged.unwrap_or(&stages[stages.len() - 1]);
        let mut item = Item {
            path: path.to_owned(),
            mode: entry.mode.bits(),
            id: entry.id,
            worktree: false,
            size: None,
            untracked: false,
        };
        if entry.mode.is_sparse() || entry.flags.contains(Flags::SKIP_WORKTREE) {
            items.push(item);
            continue;
        }
        let Some(abs) = native(path).map(|p| root.join(p)) else { continue };
        if entry.mode.is_submodule() {
            // An unpopulated submodule is unchanged, as git sees it.
            if let Some(head) = submodule_head(&abs) {
                item.id = head;
            }
            items.push(item);
            continue;
        }
        let Ok(meta) = gix::index::fs::Metadata::from_path_no_follow(&abs) else { continue };
        if meta.is_dir() {
            continue;
        }
        item.mode = if meta.is_symlink() {
            SYMLINK
        } else if !caps.executable_bit {
            if entry.mode == Mode::FILE_EXECUTABLE { EXECUTABLE } else { FILE }
        } else if meta.is_executable() {
            EXECUTABLE
        } else {
            FILE
        };
        item.size = Some(meta.len());
        let same_kind = kind(item.mode) == kind(entry.mode.bits());
        let clean = staged.is_some()
            && same_kind
            && !entry.flags.contains(Flags::INTENT_TO_ADD)
            && Stat::from_fs(&meta).is_ok_and(|stat| entry.stat.matches(&stat, stat_options))
            && !entry.stat.is_racy(timestamp, stat_options);
        if !clean {
            item.worktree = true;
            match hash_path(repo, &mut pipeline, &index, &item.path, &abs, item.mode) {
                Some(id) => item.id = id,
                None => continue,
            }
        }
        items.push(item);
    }
    Ok((items, conflicts))
}

/// Untracked files as worktree items; their contents are hashed, never kept.
pub fn untracked_items(repo: &gix::Repository, root: &Path, paths: &[String]) -> anyhow::Result<Vec<Item>> {
    let (mut pipeline, index) = repo.filter_pipeline(None)?;
    let caps = repo.filesystem_options()?;
    Ok(paths
        .iter()
        .filter_map(|path| {
            let abs = root.join(path);
            let meta = std::fs::symlink_metadata(&abs).ok()?;
            let mode = if meta.is_symlink() {
                SYMLINK
            } else if caps.executable_bit && is_executable(&meta) {
                EXECUTABLE
            } else {
                FILE
            };
            let path = BString::from(path.as_str());
            // Unreadable (permissions) counts as empty: one bad path must not fail the snapshot.
            let id = hash_path(repo, &mut pipeline, &index, &path, &abs, mode).unwrap_or_else(|| empty_blob(repo));
            Some(Item { path, mode, id, worktree: true, size: Some(meta.len()), untracked: true })
        })
        .collect())
}

/// The worktree's side of one path whose blob `id` is already known: its mode as git would record it.
pub fn worktree_item(root: &Path, path: &str, id: ObjectId, index: &gix::index::State, executable_bit: bool) -> Item {
    let staged = index.entry_by_path(path.as_bytes().as_bstr()).map(|e| e.mode);
    let meta = std::fs::symlink_metadata(root.join(path)).ok();
    let mode = match (&meta, staged) {
        (_, Some(mode)) if mode.is_submodule() => GITLINK,
        (Some(meta), _) if meta.is_symlink() => SYMLINK,
        (Some(meta), _) if executable_bit => {
            if is_executable(meta) {
                EXECUTABLE
            } else {
                FILE
            }
        }
        (_, Some(Mode::FILE_EXECUTABLE)) => EXECUTABLE,
        _ => FILE,
    };
    let size = meta.as_ref().map(std::fs::Metadata::len);
    Item { path: path.into(), mode, id, worktree: true, size, untracked: staged.is_none() }
}

/// Reads sides' contents: blobs from the object database, worktree files through their clean filters.
pub struct Reader<'repo> {
    repo: &'repo gix::Repository,
    root: PathBuf,
    pipeline: gix::filter::Pipeline<'repo>,
    index: gix::worktree::IndexPersistedOrInMemory,
}

impl<'repo> Reader<'repo> {
    pub fn new(repo: &'repo gix::Repository, root: &Path) -> anyhow::Result<Self> {
        let (pipeline, index) = repo.filter_pipeline(None)?;
        Ok(Self { repo, root: root.to_owned(), pipeline, index })
    }

    /// What git diffs for `item`: a gitlink reads as its `Subproject commit` line. None when it is
    /// past the big-file threshold, which git diffs as binary unread, or cannot be read.
    pub fn content(&mut self, item: &Item) -> Option<Vec<u8>> {
        if kind(item.mode) == GITLINK {
            return Some(format!("Subproject commit {}\n", item.id).into_bytes());
        }
        // Bytes git already stores (a staged, unchanged file) come from the object database.
        if let Ok(Some(object)) = self.repo.try_find_object(item.id) {
            if object.data.len() as u64 >= BIG_FILE_THRESHOLD {
                return None;
            }
            return Some(object.detach().data);
        }
        if !item.worktree || item.size.is_some_and(|size| size >= BIG_FILE_THRESHOLD) {
            return None;
        }
        let abs = self.root.join(native(item.path.as_bstr())?);
        if kind(item.mode) == SYMLINK {
            let target = std::fs::read_link(&abs).ok()?;
            return Some(gix::path::into_bstr(target).ok()?.into_owned().into());
        }
        let file = std::fs::File::open(&abs).ok()?;
        to_git(&mut self.pipeline, &self.index, &item.path, file).ok()
    }
}

/// The blob git would store for the worktree file at `abs`; none when it cannot be read.
fn hash_path(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    path: &BString,
    abs: &Path,
    mode: u32,
) -> Option<ObjectId> {
    if mode == SYMLINK {
        let target = std::fs::read_link(abs).ok()?;
        return blob_id(repo, &gix::path::into_bstr(target).ok()?).ok();
    }
    let file = std::fs::File::open(abs).ok()?;
    let len = file.metadata().ok()?.len();
    hash_to_git(repo, pipeline, index, path, file, len).ok()
}

/// The blob git stores for `src` (`len` bytes) at `path`: line endings, `ident`, encodings and
/// filter drivers applied. Unfiltered content streams through the hasher, so a large file is never held.
pub fn hash_to_git(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    path: &BString,
    src: impl Read,
    len: u64,
) -> anyhow::Result<ObjectId> {
    Ok(match pipeline.convert_to_git(src, &gix::path::from_bstr(path.as_bstr())?, index)? {
        ToGitOutcome::Unchanged(mut src) => {
            let interrupt = AtomicBool::new(false);
            let (kind, progress) = (repo.object_hash(), &mut gix::progress::Discard);
            gix::objs::compute_stream_hash(kind, gix::objs::Kind::Blob, &mut src, len, progress, &interrupt)?
        }
        ToGitOutcome::Process(mut src) => {
            let mut buf = Vec::new();
            src.read_to_end(&mut buf)?;
            blob_id(repo, &buf)?
        }
        ToGitOutcome::Buffer(converted) => blob_id(repo, converted)?,
    })
}

/// `src` as git stores it at `path`.
fn to_git(
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    path: &BString,
    src: impl Read,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    match pipeline.convert_to_git(src, &gix::path::from_bstr(path.as_bstr())?, index)? {
        ToGitOutcome::Unchanged(mut src) => src.read_to_end(&mut buf)?,
        ToGitOutcome::Process(mut src) => src.read_to_end(&mut buf)?,
        ToGitOutcome::Buffer(converted) => {
            buf.extend_from_slice(converted);
            converted.len()
        }
    };
    Ok(buf)
}

pub fn blob_id(repo: &gix::Repository, content: &[u8]) -> gix::Result<ObjectId> {
    gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, content)
}

fn empty_blob(repo: &gix::Repository) -> ObjectId {
    ObjectId::empty_blob(repo.object_hash())
}

/// A repository path as the platform spells it.
fn native(path: &gix::bstr::BStr) -> Option<PathBuf> {
    gix::path::from_bstr(path).ok().map(std::borrow::Cow::into_owned)
}

/// The commit a checked-out submodule is at; none when it is not a repository of its own.
pub fn submodule_head(dir: &Path) -> Option<ObjectId> {
    Some(gix::open(dir).ok()?.head_id().ok()?.detach())
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}
