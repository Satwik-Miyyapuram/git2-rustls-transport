//! Desktop round trips over a real https remote (self-signed TLS, git http-backend).

mod common;

use common::{
    commit_file, fixture, git, register_once, seed, TestResult, PASSWORD, UA_PREFIX, USER,
};
use git2::build::RepoBuilder;
use git2::{Oid, Repository};
use std::error::Error;

fn clone(url: &str) -> Result<(tempfile::TempDir, Repository), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let repo = RepoBuilder::new().clone(url, dir.path())?;
    Ok((dir, repo))
}

/// Runs a clone that must fail and returns its error.
fn clone_error(url: &str) -> Result<git2::Error, Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    match RepoBuilder::new().clone(url, dir.path()) {
        Ok(_) => Err("clone unexpectedly succeeded".into()),
        Err(err) => Ok(err),
    }
}

#[test]
fn clone_over_https() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("clone.git")?;
    let (dir, repo) = clone(&fx.url("clone.git"))?;
    assert!(dir.path().join("README.md").is_file());
    assert_eq!(repo.head()?.shorthand()?, "main");
    assert!(fx.user_agents().iter().any(|ua| ua.starts_with(UA_PREFIX)));
    Ok(())
}

#[test]
fn fetch_over_https() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    let seeded = seed("fetch.git")?;
    let (_dir, repo) = clone(&fx.url("fetch.git"))?;

    commit_file(&seeded.work, "NEW.md", "two\n", "second")?;
    git(&seeded.work, &["push", "-q", "origin", "main"])?;
    let expected: Oid = Repository::open(&seeded.work)?
        .head()?
        .target()
        .ok_or("no HEAD target")?;

    let mut remote = repo.find_remote("origin")?;
    remote.fetch(&["refs/heads/main:refs/remotes/origin/main"], None, None)?;
    assert_eq!(repo.refname_to_id("refs/remotes/origin/main")?, expected);
    Ok(())
}

#[test]
fn push_over_https() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    let seeded = seed("push.git")?;
    let (dir, repo) = clone(&fx.url("push.git"))?;

    commit_file(dir.path(), "PUSHED.md", "pushed\n", "push over https")?;
    let local: Oid = repo.head()?.target().ok_or("no HEAD target")?;
    let mut remote = repo.find_remote("origin")?;
    remote.push(&["refs/heads/main:refs/heads/main"], None)?;

    let remote_head = Repository::open(&seeded.bare)?.refname_to_id("refs/heads/main")?;
    assert_eq!(remote_head, local);
    Ok(())
}

#[test]
fn basic_auth_401_is_retried_with_callback_credentials() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("secret.git")?;
    let before = fx.unauthorized_count();
    let (_dir, repo) = clone(&fx.url("private/secret.git"))?;
    assert!(repo.head().is_ok());
    assert!(fx.unauthorized_count() > before, "server never sent a 401");
    Ok(())
}

#[test]
fn basic_auth_401_without_callback_credentials_fails() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("none.git")?;
    let err = clone_error(&fx.url("nocreds/none.git"))?;
    assert!(err.message().contains("credential callback"), "got: {err}");
    Ok(())
}

#[test]
fn basic_auth_wrong_credentials_fail() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("bad.git")?;
    let err = clone_error(&fx.url("badauth/bad.git"))?;
    assert!(err.message().contains("401"), "got: {err}");
    Ok(())
}

#[test]
fn info_refs_redirect_is_followed() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("moved.git")?;
    let (dir, _repo) = clone(&fx.url("old/moved.git"))?;
    assert!(dir.path().join("README.md").is_file());
    Ok(())
}

#[test]
fn post_redirect_is_refused() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("redir.git")?;
    let err = clone_error(&fx.url("redir/redir.git"))?;
    assert!(err.message().contains("redirect"), "got: {err}");
    Ok(())
}

/// A clone whose info/refs is redirected to another origin must fail before
/// any POST: the wants/haves (and a push's pack) never reach that origin, and
/// the GET that reached it carried no credentials.
fn assert_cross_origin_refused(path: &str, leak_prefix: &str) -> TestResult {
    let fx = fixture()?;
    let err = clone_error(&fx.url(path))?;
    assert!(err.message().contains("another origin"), "got: {err}");
    let leaked = fx.seen(leak_prefix);
    assert!(
        leaked.is_empty(),
        "request sent to another origin: {leaked:?}"
    );
    Ok(())
}

#[test]
fn cross_host_redirect_is_refused_without_credentials() -> TestResult {
    register_once()?;
    seed("xh.git")?;
    assert_cross_origin_refused("xhost/xh.git", "/leak/xh.git")
}

#[test]
fn redirect_to_http_is_refused() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("plain.git")?;
    let err = clone_error(&fx.url("tohttp/plain.git"))?;
    assert!(err.message().contains("non-https"), "got: {err}");
    Ok(())
}

#[test]
fn userinfo_url_clones_and_pushes_without_callback() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    let seeded = seed("ui.git")?;
    // /nocreds/ needs auth and the callback returns None for it: the
    // credentials can only come from the URL.
    let url =
        fx.url("nocreds/ui.git")
            .replacen("https://", &format!("https://{USER}:{PASSWORD}@"), 1);
    let (dir, repo) = clone(&url)?;

    commit_file(dir.path(), "UI.md", "ui\n", "push with userinfo")?;
    let local: Oid = repo.head()?.target().ok_or("no HEAD target")?;
    repo.find_remote("origin")?
        .push(&["refs/heads/main:refs/heads/main"], None)?;
    let remote_head = Repository::open(&seeded.bare)?.refname_to_id("refs/heads/main")?;
    assert_eq!(remote_head, local);
    Ok(())
}

#[test]
fn post_bodies_carry_content_length() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("len.git")?;
    clone(&fx.url("len.git"))?;
    let posts: Vec<_> = fx
        .seen("/len.git")
        .into_iter()
        .filter(|s| s.method == "POST")
        .collect();
    assert!(!posts.is_empty(), "no POST seen");
    assert!(
        posts.iter().all(|s| s.content_length && !s.chunked),
        "{posts:?}"
    );
    Ok(())
}

#[test]
fn port_only_redirect_is_refused_without_credentials() -> TestResult {
    register_once()?;
    seed("xpt.git")?;
    assert_cross_origin_refused("xport/xpt.git", "/leak/xpt.git")
}

#[test]
fn userinfo_in_a_redirect_location_is_never_sent() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("ul.git")?;
    let (dir, _repo) = clone(&fx.url("uiloc/ul.git"))?;
    assert!(dir.path().join("README.md").is_file());
    let there = fx.seen("/leak/ul.git");
    assert!(
        there.iter().any(|s| s.method == "POST"),
        "no POST after the redirect: {there:?}"
    );
    assert!(
        there.iter().all(|s| !s.authorization),
        "Location userinfo became credentials: {there:?}"
    );
    Ok(())
}

#[test]
fn post_redirect_to_another_origin_never_delivers_the_body() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("xp.git")?;
    let err = clone_error(&fx.url("xpost/xp.git"))?;
    assert!(err.message().contains("redirect"), "got: {err}");
    let leaked = fx.seen("/leak/xp.git");
    assert!(
        leaked.is_empty(),
        "POST followed to another origin: {leaked:?}"
    );
    Ok(())
}

#[test]
fn auth_at_a_redirected_origin_never_calls_the_callback() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("xa.git")?;
    let err = clone_error(&fx.url("xauth/xa.git"))?;
    assert!(err.message().contains("another origin"), "got: {err}");
    let there = fx.seen("/private/xa.git");
    assert!(
        there.is_empty(),
        "request sent to another origin: {there:?}"
    );
    Ok(())
}

#[test]
fn redirect_that_leaves_and_returns_to_the_origin_is_refused() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("abba.git")?;
    let err = clone_error(&fx.url("abba/abba.git"))?;
    assert!(err.message().contains("another origin"), "got: {err}");
    let there = fx.seen("/back/abba.git");
    assert!(
        there.is_empty(),
        "request sent to another origin: {there:?}"
    );
    Ok(())
}

#[test]
fn non_ascii_location_is_a_bad_location() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    let err = clone_error(&fx.url("badloc/x.git"))?;
    assert!(err.message().contains("bad Location"), "got: {err}");
    Ok(())
}
