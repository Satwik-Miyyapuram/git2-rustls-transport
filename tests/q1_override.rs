//! Q1 (all platforms): with `register("https", ...)` in place, clone goes through
//! this crate. Observable: the server sees our User-Agent on every request.
//! Runs in its own process because registration is process-global.

mod common;

use common::{fixture, register_once, seed, TestResult, UA_PREFIX};
use git2::build::RepoBuilder;

#[test]
fn registered_https_prefix_overrides_builtin_transport() -> TestResult {
    register_once()?;
    let fx = fixture()?;
    seed("q1.git")?;
    let dir = tempfile::tempdir()?;
    RepoBuilder::new().clone(&fx.url("q1.git"), dir.path())?;
    let agents = fx.user_agents();
    assert!(!agents.is_empty(), "server saw no requests");
    assert!(
        agents.iter().all(|ua| ua.starts_with(UA_PREFIX)),
        "some request bypassed the registered transport: {agents:?}"
    );
    Ok(())
}
