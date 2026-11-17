//! Desktop round trips over a real https remote (self-signed TLS, git http-backend).

mod common;

use common::{commit_file, fixture, git, register_once, seed, PASSWORD, UA_PREFIX, USER};
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
    assert_eq!(
        repo.refname_to_id("refs/remotes/origin/main").unwrap(),
        expected
    );
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
    let err = clone(&fx.url("nocreds/none.git"))
        .err()
        .expect("clone must fail");
    assert!(err.message().contains("credential callback"), "got: {err}");
}

#[test]
fn basic_auth_wrong_credentials_fail() {
    register_once();
    let fx = fixture();
    seed("bad.git");
    let err = clone(&fx.url("badauth/bad.git"))
        .err()
        .expect("clone must fail");
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
    let err = clone(&fx.url("redir/redir.git"))
        .err()
        .expect("POST redirect must be refused");
    assert!(err.message().contains("redirect"), "got: {err}");
}

/// A clone whose info/refs is redirected to another origin must fail before
/// any POST: the wants/haves (and a push's pack) never reach that origin, and
/// the GET that reached it carried no credentials.
fn assert_cross_origin_refused(path: &str, leak_prefix: &str) {
    let fx = fixture();
    let err = clone(&fx.url(path))
        .err()
        .expect("cross-origin redirect must be refused");
    assert!(err.message().contains("another origin"), "got: {err}");
    let leaked = fx.seen(leak_prefix);
    assert!(
        leaked.is_empty(),
        "request sent to another origin: {leaked:?}"
    );
}

#[test]
fn cross_host_redirect_is_refused_without_credentials() {
    register_once();
    seed("xh.git");
    assert_cross_origin_refused("xhost/xh.git", "/leak/xh.git");
}

#[test]
fn redirect_to_http_is_refused() {
    register_once();
    let fx = fixture();
    seed("plain.git");
    let err = clone(&fx.url("tohttp/plain.git"))
        .err()
        .expect("https->http redirect must be refused");
    assert!(err.message().contains("non-https"), "got: {err}");
}

#[test]
fn userinfo_url_clones_and_pushes_without_callback() {
    register_once();
    let fx = fixture();
    let seeded = seed("ui.git");
    // /nocreds/ needs auth and the callback returns None for it: the
    // credentials can only come from the URL.
    let url =
        fx.url("nocreds/ui.git")
            .replacen("https://", &format!("https://{USER}:{PASSWORD}@"), 1);
    let (dir, repo) = clone(&url).expect("clone with userinfo URL");

    commit_file(
        dir.path(),
        "UI.md",
        "ui
",
        "push with userinfo",
    );
    let local: Oid = repo.head().unwrap().target().unwrap();
    repo.find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/main"], None)
        .expect("push with userinfo URL");
    let remote_head = Repository::open(&seeded.bare)
        .unwrap()
        .refname_to_id("refs/heads/main")
        .unwrap();
    assert_eq!(remote_head, local);
}

#[test]
fn post_bodies_carry_content_length() {
    register_once();
    let fx = fixture();
    seed("len.git");
    clone(&fx.url("len.git")).expect("clone");
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
}

#[test]
fn port_only_redirect_is_refused_without_credentials() {
    register_once();
    seed("xpt.git");
    assert_cross_origin_refused("xport/xpt.git", "/leak/xpt.git");
}

#[test]
fn userinfo_in_a_redirect_location_is_never_sent() {
    register_once();
    let fx = fixture();
    seed("ul.git");
    let (dir, _repo) = clone(&fx.url("uiloc/ul.git")).expect("clone through same-origin redirect");
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
}

#[test]
fn post_redirect_to_another_origin_never_delivers_the_body() {
    register_once();
    let fx = fixture();
    seed("xp.git");
    let err = clone(&fx.url("xpost/xp.git"))
        .err()
        .expect("POST redirect must be refused");
    assert!(err.message().contains("redirect"), "got: {err}");
    let leaked = fx.seen("/leak/xp.git");
    assert!(
        leaked.is_empty(),
        "POST followed to another origin: {leaked:?}"
    );
}

#[test]
fn auth_at_a_redirected_origin_never_calls_the_callback() {
    register_once();
    let fx = fixture();
    seed("xa.git");
    let err = clone(&fx.url("xauth/xa.git"))
        .err()
        .expect("redirect to another origin must fail");
    assert!(err.message().contains("another origin"), "got: {err}");
    let there = fx.seen("/private/xa.git");
    assert!(
        there.is_empty(),
        "request sent to another origin: {there:?}"
    );
}

#[test]
fn redirect_that_leaves_and_returns_to_the_origin_is_refused() {
    register_once();
    let fx = fixture();
    seed("abba.git");
    let err = clone(&fx.url("abba/abba.git"))
        .err()
        .expect("A -> B -> A redirect must be refused");
    assert!(err.message().contains("another origin"), "got: {err}");
    let there = fx.seen("/back/abba.git");
    assert!(
        there.is_empty(),
        "request sent to another origin: {there:?}"
    );
}

#[test]
fn non_ascii_location_is_a_bad_location() {
    register_once();
    let fx = fixture();
    let err = clone(&fx.url("badloc/x.git"))
        .err()
        .expect("non-ASCII Location must fail");
    assert!(err.message().contains("bad Location"), "got: {err}");
}
