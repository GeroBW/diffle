//! Revisions, commit metadata, and the plumbing range-diffs are built from.
use std::collections::HashMap;
use std::sync::LazyLock;

use gix::bstr::ByteSlice;
use gix::objs::Kind;
use regex::Regex;

use super::exec::ExecOptions;
use super::{GitRepo, refs};
use crate::error::{GitError, Result};
use crate::protocol::{CommitInfo, LastCommitsPreview};

/// `git range-diff` pairs two commits while the diff between their patches costs less than this
/// percentage of a patch's size. Git's default of 60 leaves a reworded three-line commit unpaired,
/// since the message change alone outweighs it; at 100 the message may change as much as the patch.
const CREATION_FACTOR: &str = "--creation-factor=100";

/// Numbers are right-aligned, so rows of a range with ten or more commits start with spaces.
static RANGE_DIFF_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d+:\s+([0-9a-f]+)|-:\s+-+)\s+([=!<>])\s+(?:\d+:\s+([0-9a-f]+)|-:\s+-+)\s").unwrap()
});

/// One row of `git range-diff -s`: abbreviated hashes, none on the side a commit has no counterpart.
pub struct RangeDiffRow {
    pub old: Option<String>,
    pub new: Option<String>,
    /// `=` identical, `!` changed, `<` only in the first range, `>` only in the second.
    pub marker: char,
}

impl GitRepo {
    /// Commit sha for a user-supplied revision; a revision that names no commit is missing.
    pub async fn resolve(&self, rev: &str) -> Result<String, GitError> {
        self.peeled(rev, Kind::Commit).await
    }

    /// `resolve`, with none for a revision that names nothing.
    pub async fn try_resolve(&self, rev: &str) -> Result<Option<String>, GitError> {
        match self.resolve(rev).await {
            Ok(sha) => Ok(Some(sha)),
            Err(e) if e.is_missing() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The tree a commit points at.
    pub async fn tree(&self, rev: &str) -> Result<String, GitError> {
        self.peeled(rev, Kind::Tree).await
    }

    /// `rev` peeled to an object of `kind`, as `rev-parse --verify --quiet rev^{kind}`: any failure is missing.
    async fn peeled(&self, rev: &str, kind: Kind) -> Result<String, GitError> {
        let owned = rev.to_owned();
        let found = self
            .gix("rev-parse", move |repo| {
                let Ok(id) = repo.rev_parse_single(owned.as_str()) else { return Ok(None) };
                Ok(id.object()?.peel_to_kind(kind).ok().map(|o| o.id.to_string()))
            })
            .await?;
        found.ok_or_else(|| GitError {
            code: Some(1),
            ..GitError::refused(format!("unknown revision: {rev}"), &["rev-parse", "--verify", rev])
        })
    }

    /// Commit messages for two ancestor offsets, pinned to one HEAD.
    pub async fn last_commits_preview(&self, old_offset: u64, new_offset: u64) -> Result<LastCommitsPreview> {
        let Some(head) = self.commit_info("HEAD").await? else {
            return Ok(LastCommitsPreview { old: None, new: None });
        };
        let at = |offset: u64| {
            let head = head.clone();
            async move {
                if offset == 0 { Ok(Some(head)) } else { self.commit_info(&format!("{}~{offset}", head.sha)).await }
            }
        };
        let (old, new) = tokio::try_join!(at(old_offset), at(new_offset))?;
        Ok(LastCommitsPreview { old, new })
    }

    /// A commit's hash, abbreviation and full message; none when `rev` names no commit.
    pub async fn commit_info(&self, rev: &str) -> Result<Option<CommitInfo>, GitError> {
        let Some(sha) = self.try_resolve(rev).await? else {
            return Ok(None);
        };
        self.gix("reading a commit", move |repo| {
            let commit = repo.find_commit(gix::ObjectId::from_hex(sha.as_bytes())?)?;
            let message = commit.message_raw_sloppy().to_str_lossy().trim_end().to_owned();
            Ok(Some(CommitInfo { short: commit.id().shorten_or_id().to_string(), message, sha }))
        })
        .await
    }

    /// The empty tree under the repository's hash algorithm: the old side of an unborn branch.
    pub async fn empty_tree(&self) -> Result<String, GitError> {
        self.gix("hashing the empty tree", |repo| Ok(gix::ObjectId::empty_tree(repo.object_hash()).to_string())).await
    }

    pub async fn merge_base(&self, a: &str, b: &str) -> Result<String, GitError> {
        let (a, b) = (a.to_owned(), b.to_owned());
        self.gix("merge-base", move |repo| {
            let (a, b) = (repo.rev_parse_single(a.as_str())?, repo.rev_parse_single(b.as_str())?);
            let base = repo.merge_base(a, b)?.ok_or_else(|| anyhow::anyhow!("no merge base between {a} and {b}"))?;
            Ok(base.to_string())
        })
        .await
    }

    /// Replays what `head` changed since `base` onto `onto`, in memory: the resulting tree, and the
    /// paths the merge could not settle, which the tree holds with conflict markers. Needs git 2.38.
    pub async fn replay(&self, base: &str, onto: &str, head: &str) -> Result<(String, Vec<String>), GitError> {
        let merge_base = format!("--merge-base={base}");
        let args = ["merge-tree", "--write-tree", "-z", "--name-only", &merge_base, "--end-of-options", onto, head];
        let out = self.text_with(&args, ExecOptions { ok_codes: &[1], ..Default::default() }).await?;
        // With -z, the tree, each conflicted path and the sections end in NUL.
        let mut parts = out.split('\0');
        let tree = parts.next().unwrap_or_default().to_owned();
        let conflicts = parts.take_while(|p| !p.is_empty()).map(str::to_owned).collect();
        Ok((tree, conflicts))
    }

    /// How `git range-diff` pairs the commits of two ranges, in its output order. Summary only, no
    /// patches. Ranges are given as hashes: range-diff learnt `--end-of-options` only in git 2.44.
    pub async fn range_diff(&self, old_range: &str, new_range: &str) -> Result<Vec<RangeDiffRow>, GitError> {
        let out = self.text(&["range-diff", "-s", "--no-color", CREATION_FACTOR, old_range, new_range]).await?;
        Ok(out
            .lines()
            .filter_map(|line| RANGE_DIFF_ROW.captures(line))
            .map(|m| RangeDiffRow {
                old: m.get(1).map(|s| s.as_str().to_owned()),
                new: m.get(3).map(|s| s.as_str().to_owned()),
                marker: m[2].chars().next().unwrap_or('!'),
            })
            .collect())
    }

    /// Each commit's stable patch id, so two commits with the same change under different messages compare equal.
    pub async fn patch_ids(&self, shas: &[String]) -> Result<HashMap<String, String>, GitError> {
        let mut ids = HashMap::new();
        if shas.is_empty() {
            return Ok(ids);
        }
        let mut args = vec!["log", "-p", "--no-walk", "--no-show-signature", "--format=%H", "--end-of-options"];
        args.extend(shas.iter().map(String::as_str));
        let patches = self.exec(&args, ExecOptions::default()).await?;
        let opts = ExecOptions { input: Some(&patches), ..Default::default() };
        for line in self.text_with(&["patch-id", "--stable"], opts).await?.lines() {
            if let Some((id, sha)) = line.split_once(' ') {
                ids.insert(sha.to_owned(), id.to_owned());
            }
        }
        Ok(ids)
    }

    /// Points refs at objects so they survive `gc` and a force-push, or with none deletes them.
    /// Refs must stay inside `refs/diffle/iterations/`: nothing the user owns moves.
    pub async fn pin(&self, refs: &[(String, Option<String>)]) -> Result<(), GitError> {
        if let Some((name, _)) = refs.iter().find(|(name, _)| !name.starts_with("refs/diffle/iterations/")) {
            return Err(GitError::refused(format!("refusing to update {name}"), &["update-ref"]));
        }
        let refs = refs.to_vec();
        self.gix("update-ref", move |repo| {
            let edits = refs
                .into_iter()
                .map(|(name, sha)| match sha {
                    Some(sha) => refs::update(&name, gix::ObjectId::from_hex(sha.as_bytes())?),
                    None => refs::delete(&name),
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            if !edits.is_empty() {
                repo.edit_references(edits)?;
            }
            Ok(())
        })
        .await
    }
}
