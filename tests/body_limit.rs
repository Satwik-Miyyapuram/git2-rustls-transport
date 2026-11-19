//! The request-body cap passed to `register`. Runs in its own process because
//! registration is process-global and sets the cap for every test in it.

mod common;

use common::{fixture, git, register_once_with_limits, seed, TestResult};
use git2::build::RepoBuilder;

const LIMIT: usize = 4096;

#[test]
fn push_over_the_body_limit_fails_clearly() -> TestResult {
    register_once_with_limits(git2_rustls_transport::Limits::default().max_request_body(LIMIT))?;
    let fx = fixture()?;
    seed("limit.git")?;
    let dir = tempfile::tempdir()?;
    // A clone's POST bodies (want/have lines) fit well under the cap.
    let repo = RepoBuilder::new().clone(&fx.url("limit.git"), dir.path())?;

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
    std::fs::write(dir.path().join("noise.bin"), noise)?;
    git(dir.path(), &["add", "noise.bin"])?;
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            "noise",
        ],
    )?;

    let Err(err) = repo
        .find_remote("origin")?
        .push(&["refs/heads/main:refs/heads/main"], None)
    else {
        return Err("push over the limit must fail".into());
    };
    assert!(err.message().contains("4096-byte limit"), "got: {err}");
    let posts: Vec<_> = fx
        .seen("/limit.git/git-receive-pack")
        .into_iter()
        .filter(|s| s.method == "POST")
        .collect();
    assert!(posts.is_empty(), "an over-limit body was sent: {posts:?}");
    Ok(())
}
