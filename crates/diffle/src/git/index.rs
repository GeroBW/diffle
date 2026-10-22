//! What git tracks and stores: the index, a tree's paths, attributes, and the blobs git would
//! write for worktree content after its clean filters.
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::attrs::StateRef;
use gix::bstr::ByteSlice;
use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
use gix::objs::Kind;
use gix::worktree::stack::state::attributes::Source;

use super::{GitRepo, WORKTREE};
use crate::error::GitError;

impl GitRepo {
    /// Every path in the index, once.
    pub async fn ls_files(&self) -> Result<Vec<String>, GitError> {
        self.gix("ls-files", |repo| {
            let index = repo.index_or_empty()?;
            let mut paths: Vec<String> = index.entries().iter().map(|e| e.path(&index).to_string()).collect();
            // A conflicted path has an entry per stage, side by side.
            paths.dedup();
            Ok(paths)
        })
        .await
    }

    /// Whether the index tracks `path`.
    pub(super) async fn is_tracked(&self, path: &str) -> Result<bool, GitError> {
        let path = path.to_owned();
        self.gix("ls-files", move |repo| Ok(repo.index_or_empty()?.entry_by_path(path.as_bytes().as_bstr()).is_some()))
            .await
    }

    /// Every path in a commit's tree, in git's order.
    pub async fn ls_tree(&self, rev: &str) -> Result<Vec<String>, GitError> {
        let rev = rev.to_owned();
        self.gix("ls-tree", move |repo| {
            let tree = repo.rev_parse_single(rev.as_str())?.object()?.peel_to_tree()?;
            let mut paths: Vec<_> = tree
                .traverse()
                .breadthfirst
                .files()?
                .into_iter()
                .filter(|e| !e.mode.is_tree())
                .map(|e| e.filepath)
                .collect();
            paths.sort();
            Ok(paths.into_iter().map(|p| p.to_string()).collect())
        })
        .await
    }

    /// Boolean value of a gitattribute per path: `attr` or `attr=true` reads true, `-attr` or
    /// `attr=false` false; unspecified paths are absent. A revision reads `.gitattributes` from
    /// that tree, the worktree from the checkout.
    pub async fn attr(&self, name: &str, paths: &[String], rev: &str) -> Result<HashMap<String, bool>, GitError> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let (name, paths, rev) = (name.to_owned(), paths.to_vec(), rev.to_owned());
        self.gix("check-attr", move |repo| {
            let (shared, from_tree);
            let (index, source): (&gix::index::State, _) = if rev == WORKTREE {
                shared = repo.index_or_empty()?;
                (&shared, Source::WorktreeThenIdMapping)
            } else {
                let tree = repo.rev_parse_single(rev.as_str())?.object()?.peel_to_tree()?.id;
                from_tree = repo.index_from_tree(&tree)?;
                (&from_tree, Source::IdMapping)
            };
            let mut stack = repo.attributes_only(index, source)?;
            let mut outcome = stack.selected_attribute_matches([name.as_str()]);
            let mut out = HashMap::new();
            for path in paths {
                outcome.reset();
                stack.at_entry(path.as_str(), None)?.matching_attributes(&mut outcome);
                let value = outcome.iter_selected().next().and_then(|m| match m.assignment.state {
                    StateRef::Set => Some(true),
                    StateRef::Unset => Some(false),
                    StateRef::Value(v) if v.as_bstr() == "true" => Some(true),
                    StateRef::Value(v) if v.as_bstr() == "false" => Some(false),
                    _ => None,
                });
                if let Some(value) = value {
                    out.insert(path, value);
                }
            }
            Ok(out)
        })
        .await
    }

    /// What git would store for each worktree entry, in order: the file's blob after its clean
    /// filters, a symlink's target blob, a submodule's checked-out commit. A path that cannot be
    /// hashed (a broken link, permissions, vanished mid-refresh) gets an empty blob.
    pub(super) async fn hash_worktree(&self, files: &[(String, bool)]) -> Vec<String> {
        let (files, root) = (files.to_vec(), self.root.clone());
        let count = files.len();
        let hashed = self.gix("hashing the worktree", move |repo| {
            let (mut pipeline, index) = repo.filter_pipeline(None)?;
            Ok(files
                .iter()
                .map(|(path, submodule)| {
                    let abs = root.join(path);
                    if *submodule {
                        return submodule_head(&abs).unwrap_or_default();
                    }
                    let sha = match std::fs::symlink_metadata(&abs) {
                        Ok(m) if m.is_symlink() => std::fs::read_link(&abs)
                            .map_err(Into::into)
                            .and_then(|target| Ok(blob_sha(repo, target.to_string_lossy().as_bytes())?.to_string())),
                        Ok(m) if m.is_file() => std::fs::File::open(&abs)
                            .map_err(Into::into)
                            .and_then(|f| hash_to_git(repo, &mut pipeline, &index, path, f, m.len())),
                        _ => return String::new(),
                    };
                    sha.unwrap_or_default()
                })
                .collect::<Vec<_>>())
        });
        hashed.await.unwrap_or_else(|_| vec![String::new(); count])
    }

    /// What git would store for `content` at worktree `path`, its clean filters applied: equals the
    /// snapshot's blob for a worktree file whose bytes are still the snapshot's.
    pub async fn hash_object(&self, content: &[u8], path: &str) -> Result<String, GitError> {
        let (content, path) = (content.to_vec(), path.to_owned());
        self.gix("hash-object", move |repo| {
            let (mut pipeline, index) = repo.filter_pipeline(None)?;
            hash_to_git(repo, &mut pipeline, &index, &path, content.as_slice(), content.len() as u64)
        })
        .await
    }
}

/// The blob git stores for `src` (`len` bytes) at `path`: line endings, `ident`, encodings and
/// filter drivers applied. Unfiltered content streams through the hasher, so a large file is never held.
fn hash_to_git(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    path: &str,
    src: impl Read,
    len: u64,
) -> anyhow::Result<String> {
    let id = match pipeline.convert_to_git(src, Path::new(path), index)? {
        ToGitOutcome::Unchanged(mut src) => {
            let interrupt = AtomicBool::new(false);
            let (kind, progress) = (repo.object_hash(), &mut gix::progress::Discard);
            gix::objs::compute_stream_hash(kind, Kind::Blob, &mut src, len, progress, &interrupt)?
        }
        ToGitOutcome::Process(mut src) => {
            let mut buf = Vec::new();
            src.read_to_end(&mut buf)?;
            blob_sha(repo, &buf)?
        }
        ToGitOutcome::Buffer(converted) => blob_sha(repo, converted)?,
    };
    Ok(id.to_string())
}

fn blob_sha(repo: &gix::Repository, content: &[u8]) -> gix::Result<gix::ObjectId> {
    gix::objs::compute_hash(repo.object_hash(), Kind::Blob, content)
}

/// The commit a checked-out submodule is at; none when it is not a repository of its own.
fn submodule_head(dir: &Path) -> Option<String> {
    let repo = gix::open(dir).ok()?;
    Some(repo.head_id().ok()?.to_string())
}
