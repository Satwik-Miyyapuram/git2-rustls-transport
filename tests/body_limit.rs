//! The request-body cap passed to `register`. Runs in its own process because
//! registration is process-global and sets the cap for every test in it.

mod common;

use common::{fixture, git, register_once_with_limit, seed};
use git2::build::RepoBuilder;

const LIMIT: usize = 4096;

#[test]
fn push_over_the_body_limit_fails_clearly() {
    register_once_with_limit(LIMIT);
    let fx = fixture();
    seed("limit.git");
    let dir = tempfile::tempdir().expect("temp clone dir");
    // A clone's POST bodies (want/have lines) fit well under the cap.
    let repo = RepoBuilder::new()
        .clone(&fx.url("limit.git"), dir.path())
        .expect("clone under the limit");

    // 64 KiB of pseudo-random bytes do not compress, so the pack is far over 4 KiB.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let noise: Vec<u8> = (0..64 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    std::fs::write(dir.path().join("noise.bin"), noise).expect("write noise");
    git(dir.path(), &["add", "noise.bin"]);
    git(
        dir.path(),
        &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-q", "-m", "noise"],
    );

    let err = repo
        .find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/main"], None)
        .expect_err("push over the limit must fail");
    assert!(err.message().contains("4096-byte limit"), "got: {err}");
    let posts: Vec<_> = fx
        .seen("/limit.git/git-receive-pack")
        .into_iter()
        .filter(|s| s.method == "POST")
        .collect();
    assert!(posts.is_empty(), "an over-limit body was sent: {posts:?}");
}
