//! Desktop round trips over a real https remote (self-signed TLS, git http-backend).

mod common;

use common::{commit_file, fixture, git, register_once, seed, UA_PREFIX};
use git2::build::RepoBuilder;
use git2::{Oid, Repository};

fn clone(url: &str) -> Result<(tempfile::TempDir, Repository), git2::Error> {
    let dir = tempfile::tempdir().expect("temp clone dir");
    let repo = RepoBuilder::new().clone(url, dir.path())?;
    Ok((dir, repo))
}

#[test]
fn clone_over_https() {
    register_once();
    let fx = fixture();
    seed("clone.git");
    let (dir, repo) = clone(&fx.url("clone.git")).expect("clone");
    assert!(dir.path().join("README.md").is_file());
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
    assert!(fx.user_agents().iter().any(|ua| ua.starts_with(UA_PREFIX)));
}

#[test]
fn fetch_over_https() {
    register_once();
    let fx = fixture();
    let seeded = seed("fetch.git");
    let (_dir, repo) = clone(&fx.url("fetch.git")).expect("clone");

    commit_file(&seeded.work, "NEW.md", "two\n", "second");
    git(&seeded.work, &["push", "-q", "origin", "main"]);
    let expected: Oid = Repository::open(&seeded.work)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap();

    let mut remote = repo.find_remote("origin").unwrap();
    remote
        .fetch(&["refs/heads/main:refs/remotes/origin/main"], None, None)
        .expect("fetch over https");
    assert_eq!(repo.refname_to_id("refs/remotes/origin/main").unwrap(), expected);
}

#[test]
fn push_over_https() {
    register_once();
    let fx = fixture();
    let seeded = seed("push.git");
    let (dir, repo) = clone(&fx.url("push.git")).expect("clone");

    commit_file(dir.path(), "PUSHED.md", "pushed\n", "push over https");
    let local: Oid = repo.head().unwrap().target().unwrap();
    let mut remote = repo.find_remote("origin").unwrap();
    remote
        .push(&["refs/heads/main:refs/heads/main"], None)
        .expect("push over https");

    let remote_head = Repository::open(&seeded.bare)
        .unwrap()
        .refname_to_id("refs/heads/main")
        .unwrap();
    assert_eq!(remote_head, local);
}

#[test]
fn basic_auth_401_is_retried_with_callback_credentials() {
    register_once();
    let fx = fixture();
    seed("secret.git");
    let before = fx.unauthorized_count();
    let (_dir, repo) = clone(&fx.url("private/secret.git")).expect("clone after 401 retry");
    assert!(repo.head().is_ok());
    assert!(fx.unauthorized_count() > before, "server never sent a 401");
}

#[test]
fn basic_auth_401_without_callback_credentials_fails() {
    register_once();
    let fx = fixture();
    seed("none.git");
    let err = clone(&fx.url("nocreds/none.git")).err().expect("clone must fail");
    assert!(err.message().contains("credential callback"), "got: {err}");
}

#[test]
fn basic_auth_wrong_credentials_fail() {
    register_once();
    let fx = fixture();
    seed("bad.git");
    let err = clone(&fx.url("badauth/bad.git")).err().expect("clone must fail");
    assert!(err.message().contains("401"), "got: {err}");
}

#[test]
fn info_refs_redirect_is_followed() {
    register_once();
    let fx = fixture();
    seed("moved.git");
    let (dir, _repo) = clone(&fx.url("old/moved.git")).expect("clone through info/refs redirect");
    assert!(dir.path().join("README.md").is_file());
}

#[test]
fn post_redirect_is_refused() {
    register_once();
    let fx = fixture();
    seed("redir.git");
    let err = clone(&fx.url("redir/redir.git")).err().expect("POST redirect must be refused");
    assert!(err.message().contains("redirect"), "got: {err}");
}
