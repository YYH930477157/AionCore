//! System-git backend tests. Parsing is tested on literal `git status` output;
//! the provider operations run the real `git` executable over temporary
//! repositories (set up with git2, like the libgit2 suite).
//!
//! The libgit2 suite in `git_provider_test.rs` also runs against this backend when
//! `AIONUI_SCM_GIT_BACKEND=cli` is set, which is how behavior parity is checked
//! across every scenario it covers.

use std::sync::{Arc, Mutex};

use git2::Repository;
use tempfile::TempDir;

use super::super::GitScmProvider;
use super::*;
use crate::scm::provider::IScmProvider;
use crate::scm::types::{RepoRef, ResolvedRoot, ScmStatus};

fn init_repo(dir: &Path) -> Repository {
    let repo = Repository::init(dir).expect("init repo");
    let mut cfg = repo.config().expect("config");
    cfg.set_str("user.name", "scm test").expect("set name");
    cfg.set_str("user.email", "scm@test.local").expect("set email");
    repo
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write file");
}

fn commit_all(repo: &Repository, message: &str) {
    let mut index = repo.index().expect("index");
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add all");
    index.write().expect("write index");
    let tree = repo.find_tree(index.write_tree().expect("write tree")).expect("tree");
    let sig = repo.signature().expect("signature");
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .expect("commit");
}

fn root(dir: &Path, discover_children: bool) -> ResolvedRoot {
    ResolvedRoot {
        pe_id: "pe1".to_owned(),
        absolute_path: dir.to_string_lossy().into_owned(),
        label: "fixture".to_owned(),
        pe_name: None,
        discover_children,
    }
}

#[derive(Default)]
struct RecordingTrash(Mutex<Vec<PathBuf>>);

impl TrashSink for RecordingTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        std::fs::remove_file(path).map_err(|err| err.to_string())?;
        self.0.lock().expect("trash log").push(path.to_path_buf());
        Ok(())
    }
}

fn system_git(trash: Arc<dyn TrashSink>) -> GitScmProvider {
    GitScmProvider::with_backend(trash, Some(GitCli::system()))
}

async fn discovered(provider: &GitScmProvider, dir: &Path) -> RepoRef {
    let mut repos = provider.discover(&root(dir, false)).await.expect("discover ok");
    assert_eq!(repos.len(), 1, "an attached root surfaces exactly one repo");
    RepoRef {
        repo_id: repos.remove(0).repo_id,
    }
}

fn rows(status: &ScmStatus) -> Vec<(String, ScmResourceState, Option<bool>, Option<String>)> {
    let mut rows: Vec<_> = status
        .resources
        .iter()
        .map(|r| (r.repo_relative_path.clone(), r.state, r.staged, r.rename_from.clone()))
        .collect();
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    rows
}

fn file(rel: &str) -> FileRef {
    FileRef {
        pe_id: "pe1".to_owned(),
        relative_path: rel.to_owned(),
    }
}

#[test]
fn parses_porcelain_v2_records_and_head() {
    let output = b"# branch.oid 1234\0# branch.head main\0\
1 .M N... 100644 100644 100644 aa bb src/a b.c\0\
2 R. N... 100644 100644 100644 aa bb R100 new name.txt\0old name.txt\0\
u UU N... 100644 100644 100644 100644 aa bb cc conflict.txt\0\
? untracked dir/file.txt\0";
    let scan = parse_status(output);
    let summary: Vec<_> = scan
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.flags, e.rename_from.as_deref()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("src/a b.c", Status::WT_MODIFIED, None),
            ("new name.txt", Status::INDEX_RENAMED, Some("old name.txt")),
            ("conflict.txt", Status::CONFLICTED, None),
            ("untracked dir/file.txt", Status::WT_NEW, None),
        ]
    );
    assert_eq!(
        scan.head,
        Some(ScmHead {
            name: Some("main".to_owned()),
            detached: None
        })
    );
}

#[test]
fn reports_unborn_and_detached_heads_like_libgit2() {
    assert_eq!(
        parse_status(b"# branch.oid (initial)\0# branch.head main\0").head,
        Some(ScmHead::default())
    );
    assert_eq!(
        parse_status(b"# branch.oid 1234\0# branch.head (detached)\0").head,
        Some(ScmHead {
            name: Some("HEAD".to_owned()),
            detached: Some(true)
        })
    );
}

#[test]
fn skips_a_non_utf8_path_without_losing_the_records_after_it() {
    let mut output = b"2 R. N... 100644 100644 100644 aa bb R100 ".to_vec();
    output.extend_from_slice(&[0xff, 0xfe]);
    output.extend_from_slice(b"\0old.txt\0? kept.txt\0");
    let paths: Vec<_> = parse_status(&output).entries.into_iter().map(|e| e.path).collect();
    assert_eq!(paths, vec!["kept.txt"]);
}

#[test]
fn splits_long_path_lists_across_command_lines() {
    let long = "p".repeat(PATH_ARGS_BUDGET / 2);
    let paths = vec![(0, long.as_str()), (1, long.as_str()), (2, "short")];
    let split: Vec<Vec<usize>> = chunks(&paths)
        .into_iter()
        .map(|chunk| chunk.iter().map(|(index, _)| *index).collect())
        .collect();
    assert_eq!(split, vec![vec![0], vec![1, 2]]);
}

#[tokio::test]
async fn discovers_the_root_but_never_a_parent_repository() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    let nested = tmp.path().join("nested");
    std::fs::create_dir_all(&nested).expect("mkdir");
    let provider = system_git(Arc::new(RecordingTrash::default()));

    assert!(
        provider
            .discover(&root(&nested, false))
            .await
            .expect("discover ok")
            .is_empty()
    );
    let repos = provider.discover(&root(tmp.path(), false)).await.expect("discover ok");
    assert_eq!(repos.len(), 1);
    assert_eq!(
        repos[0].head,
        Some(ScmHead::default()),
        "an unborn head carries no name"
    );
}

#[tokio::test]
async fn workspace_discovery_surfaces_child_repositories_and_skips_bare_ones() {
    let tmp = TempDir::new().expect("tempdir");
    let app = init_repo(&tmp.path().join("app"));
    write(&tmp.path().join("app"), "main.c", "int main;\n");
    commit_all(&app, "init");
    init_repo(&tmp.path().join(".hidden"));
    Repository::init_bare(tmp.path().join("bare").join(".git")).expect("bare");
    let provider = system_git(Arc::new(RecordingTrash::default()));

    let repos = provider.discover(&root(tmp.path(), true)).await.expect("discover ok");
    let mut names: Vec<_> = repos.iter().map(|r| r.root.relative_path.clone()).collect();
    names.sort();
    assert_eq!(names, vec![".hidden".to_owned(), "app".to_owned()]);
    let app = repos.iter().find(|r| r.root.relative_path == "app").expect("app");
    let head = app.head.as_ref().expect("head");
    assert!(head.name.is_some() && head.detached.is_none(), "{head:?}");
}

#[tokio::test]
async fn reports_a_git_folder_that_git_cannot_recognize() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    // What an encrypted HEAD looks like when git itself is not trusted to decrypt it:
    // git then calls the folder "not a git repository", which must still be reported.
    std::fs::write(tmp.path().join(".git").join("HEAD"), [0x9f, 0x10, 0x00, 0x88]).expect("write");
    let provider = system_git(Arc::new(RecordingTrash::default()));

    let error = provider
        .discover(&root(tmp.path(), false))
        .await
        .expect_err("must fail");
    assert!(error.to_string().contains("not a git repository"), "{error}");
}

#[tokio::test]
async fn reports_an_unreadable_repository_instead_of_hiding_it() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    // What an encrypted config looks like to a program that cannot decrypt it.
    std::fs::write(
        tmp.path().join(".git").join("config"),
        [0x45, 0x2d, 0x00, 0x9f, 0x10, 0x88],
    )
    .expect("write");
    let provider = system_git(Arc::new(RecordingTrash::default()));

    let error = provider
        .discover(&root(tmp.path(), false))
        .await
        .expect_err("must fail");
    assert!(
        matches!(
            error,
            ScmError::OperationFailed {
                context: "discover",
                ..
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn status_matches_the_libgit2_backend() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "kept.txt", "kept\n");
    write(tmp.path(), "edited.txt", "one\n");
    write(tmp.path(), "removed.txt", "gone\n");
    write(
        tmp.path(),
        "moved.txt",
        "a body long enough to be recognized as the same file\n",
    );
    commit_all(&repo, "init");

    write(tmp.path(), "edited.txt", "two\n");
    std::fs::remove_file(tmp.path().join("removed.txt")).expect("rm");
    write(tmp.path(), "new dir/added.txt", "new\n");
    write(tmp.path(), "staged.txt", "staged\n");
    std::fs::rename(tmp.path().join("moved.txt"), tmp.path().join("renamed.txt")).expect("mv");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("staged.txt")).expect("add");
    index.remove_path(Path::new("moved.txt")).expect("rm moved");
    index.add_path(Path::new("renamed.txt")).expect("add renamed");
    index.write().expect("write");
    write(tmp.path(), "staged.txt", "staged then edited\n");

    let libgit2 = GitScmProvider::with_backend(Arc::new(RecordingTrash::default()), None);
    let cli = system_git(Arc::new(RecordingTrash::default()));
    let libgit2_repo = discovered(&libgit2, tmp.path()).await;
    let cli_repo = discovered(&cli, tmp.path()).await;
    let expected = libgit2.status(&libgit2_repo).await.expect("libgit2 status");
    let actual = cli.status(&cli_repo).await.expect("cli status");

    assert_eq!(rows(&actual), rows(&expected));
    assert_eq!(actual.head, expected.head);
}

#[tokio::test]
async fn diffs_come_from_git_for_each_anchor_pair() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "one\n");
    commit_all(&repo, "init");
    write(tmp.path(), "a.txt", "two\n");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("a.txt")).expect("add");
    index.write().expect("write");
    write(tmp.path(), "a.txt", "three\n");
    write(tmp.path(), "new.txt", "fresh\n");
    std::fs::write(tmp.path().join("blob.bin"), [0u8, 1, 2, 3]).expect("binary");
    let provider = system_git(Arc::new(RecordingTrash::default()));
    let repo_ref = discovered(&provider, tmp.path()).await;

    let patch = |content: DiffContent| content.patch.expect("patch");
    let staged = patch(
        provider
            .diff(&repo_ref, &file("a.txt"), ContentRef::Committed, ContentRef::Staged)
            .await
            .expect("diff"),
    );
    assert!(staged.contains("-one") && staged.contains("+two"), "{staged}");
    let unstaged = patch(
        provider
            .diff(&repo_ref, &file("a.txt"), ContentRef::Staged, ContentRef::Working)
            .await
            .expect("diff"),
    );
    assert!(unstaged.contains("-two") && unstaged.contains("+three"), "{unstaged}");
    let reversed = patch(
        provider
            .diff(&repo_ref, &file("a.txt"), ContentRef::Working, ContentRef::Committed)
            .await
            .expect("diff"),
    );
    assert!(reversed.contains("-three") && reversed.contains("+one"), "{reversed}");
    let untracked = patch(
        provider
            .diff(&repo_ref, &file("new.txt"), ContentRef::Staged, ContentRef::Working)
            .await
            .expect("diff"),
    );
    assert!(untracked.contains("+fresh"), "{untracked}");
    let binary = provider
        .diff(&repo_ref, &file("blob.bin"), ContentRef::Staged, ContentRef::Working)
        .await
        .expect("diff");
    assert!(binary.binary && binary.patch.is_none());
    let same = patch(
        provider
            .diff(&repo_ref, &file("a.txt"), ContentRef::Working, ContentRef::Working)
            .await
            .expect("diff"),
    );
    assert!(same.is_empty());
}

#[tokio::test]
async fn reads_committed_and_staged_content_through_git() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "committed\n");
    commit_all(&repo, "init");
    write(tmp.path(), "a.txt", "staged\n");
    let mut index = repo.index().expect("index");
    index.add_path(Path::new("a.txt")).expect("add");
    index.write().expect("write");
    let provider = system_git(Arc::new(RecordingTrash::default()));
    let repo_ref = discovered(&provider, tmp.path()).await;

    let a = file("a.txt");
    let read = |at| provider.original(&repo_ref, &a, at);
    assert_eq!(
        read(ContentRef::Committed).await.expect("read"),
        Some(b"committed\n".to_vec())
    );
    assert_eq!(
        read(ContentRef::Staged).await.expect("read"),
        Some(b"staged\n".to_vec())
    );
    let missing = provider
        .original(&repo_ref, &file("none.txt"), ContentRef::Committed)
        .await
        .expect("read");
    assert_eq!(missing, None);
}

#[tokio::test]
async fn stages_unstages_and_discards_through_git() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    write(tmp.path(), "a.txt", "one\n");
    write(tmp.path(), "b.txt", "keep\n");
    commit_all(&repo, "init");
    write(tmp.path(), "a.txt", "two\n");
    std::fs::remove_file(tmp.path().join("b.txt")).expect("rm");
    write(tmp.path(), "scratch.txt", "throwaway\n");
    let trash = Arc::new(RecordingTrash::default());
    let provider = system_git(trash.clone());
    let repo_ref = discovered(&provider, tmp.path()).await;
    let staging = provider.staging().expect("staging");

    let staged = staging
        .stage(&repo_ref, &[file("a.txt"), file("b.txt")])
        .await
        .expect("stage");
    assert!(staged.is_complete(), "{staged:?}");
    let status = provider.status(&repo_ref).await.expect("status");
    assert!(rows(&status).contains(&("a.txt".to_owned(), ScmResourceState::Modified, Some(true), None)));
    assert!(rows(&status).contains(&("b.txt".to_owned(), ScmResourceState::Deleted, Some(true), None)));

    let unstaged = staging
        .unstage(&repo_ref, &[file("a.txt"), file("b.txt")])
        .await
        .expect("unstage");
    assert!(unstaged.is_complete(), "{unstaged:?}");
    let reverted = provider
        .revert(&repo_ref, &[file("a.txt"), file("b.txt"), file("scratch.txt")])
        .await
        .expect("revert");
    assert!(reverted.is_complete(), "{reverted:?}");

    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).expect("read"),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.txt")).expect("read"),
        "keep\n"
    );
    assert_eq!(*trash.0.lock().expect("log"), vec![tmp.path().join("scratch.txt")]);
    assert!(provider.status(&repo_ref).await.expect("status").resources.is_empty());
}

#[tokio::test]
async fn unstages_in_a_repository_without_commits() {
    let tmp = TempDir::new().expect("tempdir");
    init_repo(tmp.path());
    write(tmp.path(), "first.txt", "first\n");
    let provider = system_git(Arc::new(RecordingTrash::default()));
    let repo_ref = discovered(&provider, tmp.path()).await;
    let staging = provider.staging().expect("staging");

    staging.stage(&repo_ref, &[file("first.txt")]).await.expect("stage");
    let outcome = staging.unstage(&repo_ref, &[file("first.txt")]).await.expect("unstage");
    assert!(outcome.is_complete(), "{outcome:?}");
    let status = provider.status(&repo_ref).await.expect("status");
    assert_eq!(
        rows(&status),
        vec![("first.txt".to_owned(), ScmResourceState::Created, Some(false), None)]
    );
}
