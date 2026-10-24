//! System-git backend for [`GitScmProvider`]: the same operations through the
//! `git` executable instead of in-process libgit2.
//!
//! Opt-in through `AIONUI_SCM_GIT_BACKEND=cli`, which the desktop app sets from a
//! user setting. It exists for machines where a transparent file-encryption
//! product decrypts files only for programs on its trust list: `git` is usually on
//! that list (editors depend on it) while aioncore is not, so libgit2 running in
//! aioncore reads ciphertext and cannot even open the repository.
//!
//! Repository metadata, the index, objects and work-tree content are therefore all
//! read by `git`. aioncore itself only lists directories, checks that `.git`
//! exists and moves untracked files to the trash. The one exception is
//! [`IScmProvider::original`] at the working anchor, which still reads the file
//! directly; the changes panel never requests it, because its diffs come from
//! `git diff`.
//!
//! Where the two backends differ, this one follows plain git: renames are detected
//! the way `git status` detects them (in the index only), so an unstaged move
//! shows as a delete plus a create, and `degraded` is never reported because
//! `git status` skips its optional index refresh silently.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git2::Status;

use super::{
    DIFF_CONTENT_LIMIT, OpenedRepo, STATUS_RESOURCE_LIMIT, TrashSink, canonical_key, classify, dedup_by_workdir,
    discover_child_repos_with, pe_relative_path,
};
use crate::scm::error::ScmError;
use crate::scm::types::{ContentRef, DiffContent, FileRef, ScmHead, ScmResource, ScmResourceState};

/// Environment variable that selects this backend (`cli`); anything else keeps libgit2.
pub(crate) const BACKEND_ENV: &str = "AIONUI_SCM_GIT_BACKEND";

/// Paths per command line stay under this many bytes: Windows caps a command line
/// at 32 767 UTF-16 units, and the git arguments around the paths need room too.
const PATH_ARGS_BUDGET: usize = 24_000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A finished git process.
struct Run {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The failure as a user-facing reason: git's own message, which names the path.
    fn reason(&self, command: &str) -> String {
        let message = self.stderr.trim();
        match self.code {
            Some(code) if message.is_empty() => format!("git {command} exited with {code}"),
            Some(_) => message.to_owned(),
            None => format!("git {command} was terminated"),
        }
    }

    fn error(&self, context: &'static str, command: &str) -> ScmError {
        ScmError::OperationFailed {
            context,
            message: self.reason(command),
        }
    }
}

/// One change `git status` reported.
#[derive(Debug, Clone)]
struct StatusEntry {
    path: String,
    flags: Status,
    rename_from: Option<String>,
}

/// A parsed `git status --porcelain=v2 --branch` result.
#[derive(Debug, Default)]
struct StatusScan {
    entries: Vec<StatusEntry>,
    head: Option<ScmHead>,
}

/// An index entry from `git ls-files --stage`.
struct IndexEntry {
    oid: String,
    stage: u8,
}

/// The `git` executable used for every repository access.
#[derive(Debug, Clone)]
pub(super) struct GitCli {
    program: PathBuf,
}

impl GitCli {
    /// The backend selected by [`BACKEND_ENV`], or `None` for libgit2.
    pub(super) fn from_env() -> Option<Self> {
        match std::env::var(BACKEND_ENV) {
            Ok(value) if value.trim().eq_ignore_ascii_case("cli") => Some(Self::system()),
            _ => None,
        }
    }

    /// `git` resolved from `PATH`, which aioncore prepares at startup.
    pub(super) fn system() -> Self {
        Self {
            program: PathBuf::from("git"),
        }
    }

    fn exec(
        &self,
        dir: &Path,
        context: &'static str,
        args: &[&str],
        env: &[(&str, OsString)],
    ) -> Result<Run, ScmError> {
        let mut command = Command::new(&self.program);
        command
            .current_dir(dir)
            // Raw paths, and pathspecs taken literally: a file named `*.c` must not
            // match every C file.
            .args(["-c", "core.quotepath=false", "--literal-pathspecs"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            // English messages, so "not a git repository" can be recognized.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command.output().map_err(|err| ScmError::OperationFailed {
            context,
            message: format!("could not run system git: {err}"),
        })?;
        Ok(Run {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Run git and return its stdout, or the failure.
    fn run(&self, dir: &Path, context: &'static str, args: &[&str]) -> Result<Vec<u8>, ScmError> {
        let run = self.exec(dir, context, args, &[])?;
        if run.ok() {
            Ok(run.stdout)
        } else {
            Err(run.error(context, args.first().copied().unwrap_or_default()))
        }
    }

    /// Run `args -- paths` in chunks; a failed chunk is retried one path at a time so
    /// each failure is reported against its own request index (best effort).
    fn per_path(
        &self,
        dir: &Path,
        context: &'static str,
        args: &[&str],
        paths: &[(usize, &str)],
    ) -> Vec<(usize, String)> {
        let mut failed = Vec::new();
        for chunk in chunks(paths) {
            let batch: Vec<&str> = chunk.iter().map(|(_, path)| *path).collect();
            if self.with_paths(dir, context, args, &batch).is_ok() {
                continue;
            }
            for (index, path) in chunk {
                if let Err(reason) = self.with_paths(dir, context, args, &[path]) {
                    failed.push((*index, reason));
                }
            }
        }
        failed
    }

    fn with_paths(&self, dir: &Path, context: &'static str, args: &[&str], paths: &[&str]) -> Result<Vec<u8>, String> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 1 + paths.len());
        full.extend_from_slice(args);
        full.push("--");
        full.extend_from_slice(paths);
        let run = self.exec(dir, context, &full, &[]).map_err(|err| err.to_string())?;
        if run.ok() {
            Ok(run.stdout)
        } else {
            Err(run.reason(args.first().copied().unwrap_or_default()))
        }
    }

    /// Open `path` as a repository the way `Repository::open` does: the path itself
    /// must be the work tree root, and a parent repository is never picked up.
    /// `Ok(None)` covers a folder without `.git` and a bare repository.
    pub(super) fn open(&self, path: &Path, relative_path: String) -> Result<Option<OpenedRepo>, ScmError> {
        // No `.git` entry means no repository, decided without starting git: a
        // workspace scan over many child folders stays cheap.
        if std::fs::symlink_metadata(path.join(".git")).is_err() {
            return Ok(None);
        }
        let ceiling = path
            .parent()
            .map(|parent| parent.as_os_str().to_owned())
            .unwrap_or_default();
        let run = self.exec(
            path,
            "discover",
            &[
                "rev-parse",
                "--is-bare-repository",
                "--absolute-git-dir",
                "--git-common-dir",
                "--show-toplevel",
            ],
            &[("GIT_CEILING_DIRECTORIES", ceiling)],
        )?;
        if !run.ok() {
            if run.stderr.contains("must be run in a work tree") {
                return Ok(None);
            }
            // Everything else is reported, including git's "not a git repository":
            // with `.git` present that means git could not read it — an encrypted
            // HEAD when git is not trusted either, a broken gitfile — which is
            // exactly what the user needs to see instead of "no repository here".
            return Err(run.error("discover", "rev-parse"));
        }
        let text = String::from_utf8_lossy(&run.stdout);
        let mut lines = text.lines();
        if lines.next() == Some("true") {
            tracing::debug!(?path, "scm discover: bare repository has no work tree, not surfaced");
            return Ok(None);
        }
        let (Some(git_dir), Some(common_dir), Some(workdir)) = (lines.next(), lines.next(), lines.next()) else {
            return Err(ScmError::OperationFailed {
                context: "discover",
                message: format!("unexpected git rev-parse output: {text}"),
            });
        };
        let git_dir = PathBuf::from(git_dir);
        // `--git-common-dir` is printed relative to the working directory when it
        // lies below it.
        let common_dir = match Path::new(common_dir) {
            common if common.is_absolute() => common.to_path_buf(),
            common => path.join(common),
        };
        let workdir = PathBuf::from(workdir);
        let is_worktree = canonical_key(&git_dir) != canonical_key(&common_dir);
        let head = self.head(&workdir);
        Ok(Some(OpenedRepo {
            relative_path,
            workdir,
            git_dir,
            common_dir,
            is_worktree,
            head,
        }))
    }

    /// Head as the neutral model sees it; an unborn or unreadable head is the
    /// default, the same answer libgit2 gives.
    fn head(&self, workdir: &Path) -> Option<ScmHead> {
        match self.exec(workdir, "discover", &["rev-parse", "--abbrev-ref", "HEAD"], &[]) {
            Ok(run) if run.ok() => {
                let name = String::from_utf8_lossy(&run.stdout).trim().to_owned();
                Some(if name == "HEAD" {
                    ScmHead {
                        name: Some(name),
                        detached: Some(true),
                    }
                } else {
                    ScmHead {
                        name: Some(name),
                        detached: None,
                    }
                })
            }
            _ => Some(ScmHead::default()),
        }
    }

    /// Discovery with the libgit2 backend's rules: the root, else (for a workspace
    /// root) each immediate child repository, plus linked worktrees inside the root.
    pub(super) fn discover(&self, root_path: &Path, discover_children: bool) -> Result<Vec<OpenedRepo>, ScmError> {
        let pe_root_key = canonical_key(root_path);
        let mut opened = if let Some(root_repo) = self.open(root_path, String::new())? {
            vec![root_repo]
        } else if discover_children {
            discover_child_repos_with(root_path, |path, name| self.open(path, name))
        } else {
            vec![]
        };
        let mut worktrees = vec![];
        for repo in &opened {
            if repo.is_worktree {
                continue;
            }
            worktrees.extend(self.linked_worktrees(repo, &pe_root_key));
        }
        opened.extend(worktrees);
        dedup_by_workdir(&mut opened);
        Ok(opened)
    }

    /// Linked worktrees of `repo` whose work tree lives inside the pe root. Any
    /// failure drops that worktree, never the primary's discovery.
    fn linked_worktrees(&self, repo: &OpenedRepo, pe_root_key: &Path) -> Vec<OpenedRepo> {
        // Only a repository with linked worktrees has this directory; most do not,
        // and they cost no process.
        if !repo.common_dir.join("worktrees").is_dir() {
            return vec![];
        }
        let listing = match self.run(&repo.workdir, "discover", &["worktree", "list", "--porcelain"]) {
            Ok(listing) => listing,
            Err(err) => {
                tracing::debug!(workdir = ?repo.workdir, error = %err, "scm discover: worktree list failed, skipping");
                return vec![];
            }
        };
        let listing = String::from_utf8_lossy(&listing);
        let mut out = vec![];
        // The main work tree is listed first; every later `worktree` line is linked.
        for wt_path in listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .skip(1)
        {
            let wt_path = Path::new(wt_path);
            let Some(relative_path) = pe_relative_path(pe_root_key, wt_path) else {
                tracing::debug!(
                    ?wt_path,
                    "scm discover: linked worktree lives outside the pe root, not surfaced"
                );
                continue;
            };
            match self.open(wt_path, relative_path) {
                Ok(Some(opened)) => out.push(opened),
                Ok(None) => {}
                Err(err) => tracing::debug!(?wt_path, error = %err, "scm discover: worktree open failed, skipping"),
            }
        }
        out
    }

    fn scan(&self, workdir: &Path, context: &'static str) -> Result<StatusScan, ScmError> {
        let output = self.run(
            workdir,
            context,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=all",
                "--find-renames",
            ],
        )?;
        Ok(parse_status(&output))
    }

    /// The flat change list, truncated at [`STATUS_RESOURCE_LIMIT`], with head.
    pub(super) fn status(&self, workdir: &Path) -> Result<(Vec<ScmResource>, bool, bool, Option<ScmHead>), ScmError> {
        let scan = self.scan(workdir, "status")?;
        let mut resources = Vec::new();
        let mut truncated = false;
        for entry in &scan.entries {
            for (state, staged) in classify(entry.flags) {
                if resources.len() >= STATUS_RESOURCE_LIMIT {
                    truncated = true;
                    break;
                }
                resources.push(ScmResource {
                    file: FileRef {
                        pe_id: String::new(), // filled by the caller, which owns identity
                        relative_path: entry.path.clone(),
                    },
                    repo_relative_path: entry.path.clone(),
                    state,
                    staged,
                    rename_from: matches!(state, ScmResourceState::Renamed)
                        .then(|| entry.rename_from.clone())
                        .flatten(),
                });
            }
            if truncated {
                break;
            }
        }
        Ok((resources, truncated, false, scan.head))
    }

    fn has_head(&self, workdir: &Path) -> Result<bool, ScmError> {
        Ok(self
            .exec(
                workdir,
                "original",
                &["rev-parse", "-q", "--verify", "HEAD^{commit}"],
                &[],
            )?
            .ok())
    }

    fn index_entries(&self, workdir: &Path, context: &'static str, rel: &str) -> Result<Vec<IndexEntry>, ScmError> {
        let output = self.run(workdir, context, &["ls-files", "--stage", "-z", "--", rel])?;
        let mut entries = Vec::new();
        for record in output.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
            let record = String::from_utf8_lossy(record);
            // `<mode> <oid> <stage>\t<path>`; a directory pathspec also lists the
            // files below it, which are not this file.
            let Some((meta, path)) = record.split_once('\t') else {
                continue;
            };
            if path != rel {
                continue;
            }
            let mut fields = meta.split(' ');
            let (Some(_mode), Some(oid), Some(stage)) = (fields.next(), fields.next(), fields.next()) else {
                continue;
            };
            entries.push(IndexEntry {
                oid: oid.to_owned(),
                stage: stage.parse().unwrap_or(0),
            });
        }
        Ok(entries)
    }

    fn blob(&self, workdir: &Path, context: &'static str, oid: &str) -> Result<Vec<u8>, ScmError> {
        self.run(workdir, context, &["cat-file", "blob", oid])
    }

    /// Read one file at an anchor; `None` when it has no version there.
    pub(super) fn original(&self, workdir: &Path, rel: &str, at: ContentRef) -> Result<Option<Vec<u8>>, ScmError> {
        match at {
            ContentRef::Working => match std::fs::read(workdir.join(rel)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(ScmError::Io {
                    path: rel.to_owned(),
                    message: err.to_string(),
                }),
            },
            ContentRef::Staged => {
                let entries = self.index_entries(workdir, "original", rel)?;
                if let Some(resolved) = entries.iter().find(|entry| entry.stage == 0) {
                    return self.blob(workdir, "original", &resolved.oid).map(Some);
                }
                // Only conflict sides: there is no single staged version to return.
                if entries.is_empty() {
                    Ok(None)
                } else {
                    Err(ScmError::OpaqueResource { path: rel.to_owned() })
                }
            }
            ContentRef::Committed => {
                if !self.has_head(workdir)? {
                    return Ok(None); // unborn head: nothing committed yet
                }
                let listing = self.run(workdir, "original", &["ls-tree", "-z", "HEAD", "--", rel])?;
                for record in listing.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
                    let record = String::from_utf8_lossy(record);
                    // `<mode> <type> <oid>\t<path>`
                    let Some((meta, path)) = record.split_once('\t') else {
                        continue;
                    };
                    let mut fields = meta.split(' ');
                    if path == rel
                        && let (Some(_mode), Some("blob"), Some(oid)) = (fields.next(), fields.next(), fields.next())
                    {
                        return self.blob(workdir, "original", oid).map(Some);
                    }
                }
                Ok(None)
            }
        }
    }

    /// A unified patch between two anchors, produced by `git diff` so the work-tree
    /// side is read by git.
    pub(super) fn diff(
        &self,
        workdir: &Path,
        rel: &str,
        from: ContentRef,
        to: ContentRef,
    ) -> Result<DiffContent, ScmError> {
        if from == to {
            return Ok(DiffContent {
                patch: Some(String::new()),
                ..DiffContent::default()
            });
        }
        let (older, newer, reverse) = if anchor_rank(from) < anchor_rank(to) {
            (from, to, false)
        } else {
            (to, from, true)
        };
        let entries = self.index_entries(workdir, "diff", rel)?;
        if (older == ContentRef::Staged || newer == ContentRef::Staged) && entries.iter().any(|entry| entry.stage != 0)
        {
            return Err(ScmError::OpaqueResource { path: rel.to_owned() });
        }

        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ];
        if reverse {
            args.push("-R");
        }
        let in_index = !entries.is_empty();
        let output = match (older, newer) {
            (ContentRef::Committed, ContentRef::Staged) => {
                args.push("--cached");
                self.with_paths(workdir, "diff", &args, &[rel])
            }
            (ContentRef::Staged, ContentRef::Working) if in_index => self.with_paths(workdir, "diff", &args, &[rel]),
            (ContentRef::Committed, ContentRef::Working) if in_index && self.has_head(workdir)? => {
                args.push("HEAD");
                self.with_paths(workdir, "diff", &args, &[rel])
            }
            // Untracked, or nothing committed yet: the older side is empty, so the
            // whole work-tree file is the change.
            (_, ContentRef::Working) => {
                if !workdir.join(rel).exists() {
                    Ok(Vec::new())
                } else {
                    self.no_index(workdir, &args, rel)
                }
            }
            _ => unreachable!("anchors are ordered and distinct"),
        }
        .map_err(|message| ScmError::OperationFailed {
            context: "diff",
            message,
        })?;

        if output.len() > DIFF_CONTENT_LIMIT * 2 {
            return Ok(DiffContent {
                truncated: true,
                ..DiffContent::default()
            });
        }
        let patch = String::from_utf8_lossy(&output).into_owned();
        if patch.starts_with("Binary files ")
            || patch.contains("\nBinary files ")
            || patch.contains("\nGIT binary patch")
        {
            return Ok(DiffContent {
                binary: true,
                ..DiffContent::default()
            });
        }
        Ok(DiffContent {
            patch: Some(patch),
            ..DiffContent::default()
        })
    }

    /// `git diff --no-index` against an empty file; exit status 1 only means the
    /// two sides differ.
    fn no_index(&self, workdir: &Path, args: &[&str], rel: &str) -> Result<Vec<u8>, String> {
        let mut full = args.to_vec();
        full.extend(["--no-index", "--", "/dev/null", rel]);
        let run = self.exec(workdir, "diff", &full, &[]).map_err(|err| err.to_string())?;
        match run.code {
            Some(0 | 1) => Ok(run.stdout),
            _ => Err(run.reason("diff")),
        }
    }

    /// Refuse the whole request when any file is conflicted, before anything is
    /// touched: acting on a conflict can destroy a half-finished resolution.
    fn refuse_conflicted(&self, workdir: &Path, context: &'static str, rels: &[String]) -> Result<(), ScmError> {
        let indexed: Vec<(usize, &str)> = rels.iter().map(String::as_str).enumerate().collect();
        for chunk in chunks(&indexed) {
            let batch: Vec<&str> = chunk.iter().map(|(_, rel)| *rel).collect();
            let output = self
                .with_paths(workdir, context, &["ls-files", "--unmerged", "-z"], &batch)
                .map_err(|message| ScmError::OperationFailed { context, message })?;
            for record in output.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
                let record = String::from_utf8_lossy(record);
                if let Some((_, path)) = record.split_once('\t')
                    && let Some(rel) = rels.iter().find(|rel| rel.as_str() == path)
                {
                    return Err(ScmError::OpaqueResource { path: rel.clone() });
                }
            }
        }
        Ok(())
    }

    /// Discard work-tree changes: restore tracked files from the index, undo renames
    /// as a pair, and send untracked files to the trash.
    pub(super) fn revert(
        &self,
        workdir: &Path,
        rels: &[String],
        trash: &dyn TrashSink,
    ) -> Result<Vec<(usize, String)>, ScmError> {
        let scan = self.scan(workdir, "revert")?;
        let by_path: HashMap<&str, &StatusEntry> =
            scan.entries.iter().map(|entry| (entry.path.as_str(), entry)).collect();

        let mut tracked: Vec<(usize, &str)> = Vec::new();
        let mut to_trash: Vec<(usize, PathBuf)> = Vec::new();
        let mut renamed: Vec<(usize, String, PathBuf)> = Vec::new();
        for (index, rel) in rels.iter().enumerate() {
            let entry = by_path.get(rel.as_str()).copied();
            let flags = entry.map_or(Status::empty(), |entry| entry.flags);
            if classify(flags).first().is_some_and(|(state, _)| state.is_opaque()) {
                return Err(ScmError::OpaqueResource { path: rel.clone() });
            }
            if flags.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED)
                && let Some(from) = entry.and_then(|entry| entry.rename_from.clone())
            {
                renamed.push((index, from, workdir.join(rel)));
                continue;
            }
            if flags.contains(Status::WT_NEW) && !flags.intersects(Status::INDEX_NEW) {
                to_trash.push((index, workdir.join(rel)));
            } else if flags.intersects(Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE) {
                tracked.push((index, rel.as_str()));
            }
            // Otherwise the work tree already matches the index: nothing to discard.
        }

        // Restored from the index, not the last commit, so a staged version
        // survives — see the libgit2 backend's `revert_paths`.
        let mut failed = self.per_path(workdir, "revert", &["checkout", "-q"], &tracked);

        // Source first, so a failed restore leaves the file at its new path rather
        // than at neither. From HEAD into the work tree only: a staged rename has
        // already removed the source from the index.
        for (index, from, to) in renamed {
            if let Err(message) = self.with_paths(
                workdir,
                "revert",
                &["restore", "--source=HEAD", "--worktree"],
                &[from.as_str()],
            ) {
                failed.push((index, format!("restoring {from} failed: {message}")));
                continue;
            }
            if let Err(message) = trash.trash(&to) {
                failed.push((index, format!("move to trash failed: {message}")));
            }
        }

        for (index, path) in to_trash {
            if let Err(message) = trash.trash(&path) {
                failed.push((index, format!("move to trash failed: {message}")));
            }
        }
        Ok(failed)
    }

    /// Stage the files; `add -A` records a deleted file as a removal.
    pub(super) fn stage(&self, workdir: &Path, rels: &[String]) -> Result<Vec<(usize, String)>, ScmError> {
        self.refuse_conflicted(workdir, "stage", rels)?;
        let indexed: Vec<(usize, &str)> = rels.iter().map(String::as_str).enumerate().collect();
        Ok(self.per_path(workdir, "stage", &["add", "-A"], &indexed))
    }

    /// Unstage the files; with nothing committed yet that means dropping them from
    /// the index, as the libgit2 backend does.
    pub(super) fn unstage(&self, workdir: &Path, rels: &[String]) -> Result<Vec<(usize, String)>, ScmError> {
        self.refuse_conflicted(workdir, "unstage", rels)?;
        let indexed: Vec<(usize, &str)> = rels.iter().map(String::as_str).enumerate().collect();
        let args: &[&str] = if self.has_head(workdir)? {
            &["reset", "-q", "HEAD"]
        } else {
            &["rm", "--cached", "-q", "-f", "--ignore-unmatch"]
        };
        Ok(self.per_path(workdir, "unstage", args, &indexed))
    }
}

/// Older anchors first, so a pair can be expressed as one `git diff` (`-R` when
/// the caller asked for the opposite direction).
fn anchor_rank(anchor: ContentRef) -> u8 {
    match anchor {
        ContentRef::Committed => 0,
        ContentRef::Staged => 1,
        ContentRef::Working => 2,
    }
}

/// Split paths into chunks whose combined length fits one command line.
fn chunks<'a>(paths: &'a [(usize, &'a str)]) -> Vec<&'a [(usize, &'a str)]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, (_, path)) in paths.iter().enumerate() {
        if index > start && used + path.len() + 3 > PATH_ARGS_BUDGET {
            out.push(&paths[start..index]);
            start = index;
            used = 0;
        }
        used += path.len() + 3;
    }
    if start < paths.len() {
        out.push(&paths[start..]);
    }
    out
}

/// Index-side and work-tree-side flags for a porcelain v2 `XY` pair, in the
/// libgit2 vocabulary so [`classify`] applies unchanged.
fn porcelain_flags(xy: &str) -> Status {
    let bytes = xy.as_bytes();
    let index = match bytes.first() {
        Some(b'M') => Status::INDEX_MODIFIED,
        Some(b'T') => Status::INDEX_TYPECHANGE,
        Some(b'A' | b'C') => Status::INDEX_NEW,
        Some(b'D') => Status::INDEX_DELETED,
        Some(b'R') => Status::INDEX_RENAMED,
        _ => Status::empty(),
    };
    let worktree = match bytes.get(1) {
        Some(b'M') => Status::WT_MODIFIED,
        Some(b'T') => Status::WT_TYPECHANGE,
        Some(b'A') => Status::WT_NEW,
        Some(b'D') => Status::WT_DELETED,
        Some(b'R') => Status::WT_RENAMED,
        _ => Status::empty(),
    };
    index | worktree
}

/// Parse `git status --porcelain=v2 -z --branch`.
fn parse_status(output: &[u8]) -> StatusScan {
    let mut scan = StatusScan::default();
    let mut unborn = false;
    let mut branch: Option<String> = None;
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        let Some(kind) = field.first() else { continue };
        // A rename record carries its original path as the next field.
        let rename_from = if *kind == b'2' {
            fields.next().map(|from| String::from_utf8_lossy(from).into_owned())
        } else {
            None
        };
        let Ok(text) = std::str::from_utf8(field) else {
            // Skipped rather than lossily rendered: a mangled path would not round
            // trip back to a real file.
            tracing::warn!("scm status: skipping entry with non-UTF-8 path");
            continue;
        };
        let (flags, path) = match kind {
            b'#' => {
                if let Some(oid) = text.strip_prefix("# branch.oid ") {
                    unborn = oid == "(initial)";
                } else if let Some(name) = text.strip_prefix("# branch.head ") {
                    branch = Some(name.to_owned());
                }
                continue;
            }
            // `1 XY sub mH mI mW hH hI path`
            b'1' => {
                let parts: Vec<&str> = text.splitn(9, ' ').collect();
                let (Some(xy), Some(path)) = (parts.get(1), parts.get(8)) else {
                    continue;
                };
                (porcelain_flags(xy), *path)
            }
            // `2 XY sub mH mI mW hH hI Xscore path`
            b'2' => {
                let parts: Vec<&str> = text.splitn(10, ' ').collect();
                let (Some(xy), Some(path)) = (parts.get(1), parts.get(9)) else {
                    continue;
                };
                (porcelain_flags(xy), *path)
            }
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`
            b'u' => {
                let parts: Vec<&str> = text.splitn(11, ' ').collect();
                let Some(path) = parts.get(10) else { continue };
                (Status::CONFLICTED, *path)
            }
            b'?' => (Status::WT_NEW, text.get(2..).unwrap_or_default()),
            _ => continue,
        };
        if path.is_empty() {
            continue;
        }
        scan.entries.push(StatusEntry {
            path: path.to_owned(),
            flags,
            rename_from: rename_from.filter(|_| flags.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED)),
        });
    }
    scan.head = match branch.as_deref() {
        // Unborn: no commit yet, the same "no head" answer libgit2 gives.
        _ if unborn => Some(ScmHead::default()),
        Some("(detached)") => Some(ScmHead {
            name: Some("HEAD".to_owned()),
            detached: Some(true),
        }),
        Some(name) => Some(ScmHead {
            name: Some(name.to_owned()),
            detached: None,
        }),
        None => None,
    };
    scan
}

#[cfg(test)]
#[path = "git_cli_test.rs"]
mod git_cli_test;
