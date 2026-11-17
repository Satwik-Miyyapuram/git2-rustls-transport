//! The info/refs (advertisement) cap passed to `register`. Runs in its own
//! process because registration is process-global.

mod common;

use common::{fixture, register_once_with_limits, seed};
use git2::build::RepoBuilder;
use git2_rustls_transport::Limits;

const LIMIT: usize = 64;

#[test]
fn advertisement_over_the_limit_fails_clearly() {
    register_once_with_limits(Limits::default().max_advertisement(LIMIT));
    let fx = fixture();
    seed("adv.git");
    let dir = tempfile::tempdir().expect("temp clone dir");
    // The advertisement of a repo with one branch is a few hundred bytes.
    let err = RepoBuilder::new()
        .clone(&fx.url("adv.git"), dir.path())
        .err()
        .expect("clone over the advertisement limit must fail");
    assert!(err.message().contains("64-byte limit"), "got: {err}");
}
