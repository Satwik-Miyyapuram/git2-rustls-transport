//! Q1 (Windows only): without registration, libgit2's built-in https transport
//! (WinHTTP, enabled by the `https` feature) handles the URL. It rejects our
//! self-signed certificate, and the server never sees a request from us.

#![cfg(windows)]

mod common;

use common::{fixture, seed, UA_PREFIX};
use git2::build::RepoBuilder;

#[test]
fn builtin_https_is_used_when_nothing_is_registered() {
    let fx = fixture();
    seed("builtin.git");
    let dir = tempfile::tempdir().unwrap();
    let err = RepoBuilder::new()
        .clone(&fx.url("builtin.git"), dir.path())
        .err().expect("WinHTTP must reject the self-signed certificate");
    eprintln!("builtin clone error: {err}");
    assert!(fx.user_agents().iter().all(|ua| !ua.starts_with(UA_PREFIX)));
}
