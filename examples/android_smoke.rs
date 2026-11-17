//! Android smoke test: clone, fetch and push over https through this crate.
//!
//! Usage: `android_smoke <clone-dir> [push-url]`
//!
//! Clones `SMOKE_CLONE_URL` (default: this crate's public repo) into
//! `<clone-dir>`, then fetches. If `SMOKE_TOKEN` is set and `push-url` is
//! given, it also creates a one-commit repo in `<clone-dir>-push` and pushes
//! it to `push-url` as `main`. The token is used as the basic-auth password
//! (user `x-access-token`) for github.com only; it is never printed.
//!
//! TLS roots are the Mozilla set bundled by `webpki-root-certs`, so the binary
//! needs no JVM and no platform verifier.

use std::time::Instant;

use git2::{build::RepoBuilder, FetchOptions, Repository, Signature};
use git2_rustls_transport::{register, Credentials, Limits};

const DEFAULT_CLONE_URL: &str = "https://github.com/Satwik-Miyyapuram/git2-rustls-transport.git";

fn step<T>(name: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let start = Instant::now();
    let result = f();
    let ms = start.elapsed().as_millis();
    match &result {
        Ok(_) => println!("[PASS] {name} ({ms} ms)"),
        Err(e) => println!("[FAIL] {name} ({ms} ms): {e}"),
    }
    result
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let clone_dir = args.get(1).ok_or("usage: android_smoke <clone-dir> [push-url]")?;
    let push_url = args.get(2).cloned();
    let token = std::env::var("SMOKE_TOKEN").ok().filter(|t| !t.is_empty());
    let clone_url =
        std::env::var("SMOKE_CLONE_URL").unwrap_or_else(|_| DEFAULT_CLONE_URL.to_string());

    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|c| reqwest::Certificate::from_der(c.as_ref()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("loading roots: {e}"))?;
    println!("TLS verifier: rustls + webpki-root-certs ({} roots)", roots.len());

    let builder = reqwest::blocking::Client::builder()
        .tls_certs_only(roots)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300));

    let token_for_cb = token.clone();
    step("register", || {
        // SAFETY: called once, at start-up, before any git2 transport exists.
        unsafe {
            register(builder, Limits::default(), move |url| {
                let token = token_for_cb.as_ref()?;
                url.starts_with("https://github.com/").then(|| Credentials {
                    username: "x-access-token".to_string(),
                    password: token.clone(),
                })
            })
        }
        .map_err(|e| e.to_string())
    })?;

    let repo = step(&format!("clone {clone_url}"), || {
        RepoBuilder::new()
            .clone(&clone_url, std::path::Path::new(clone_dir))
            .map_err(|e| e.to_string())
    })?;

    step("fetch origin", || {
        let mut remote = repo.find_remote("origin").map_err(|e| e.to_string())?;
        let mut opts = FetchOptions::new();
        remote
            .fetch(&["refs/heads/*:refs/remotes/origin/*"], Some(&mut opts), None)
            .map_err(|e| e.to_string())
    })?;

    match (push_url, token) {
        (Some(url), Some(_)) => {
            let push_dir = format!("{clone_dir}-push");
            let pushed = step(&format!("push to {url}"), || {
                let repo = Repository::init(&push_dir).map_err(|e| e.to_string())?;
                repo.set_head("refs/heads/main").map_err(|e| e.to_string())?;
                let sig = Signature::now("wf-android-smoke", "smoke@example.invalid")
                    .map_err(|e| e.to_string())?;
                std::fs::write(
                    std::path::Path::new(&push_dir).join("smoke.txt"),
                    "hello from the Android smoke test\n",
                )
                .map_err(|e| e.to_string())?;
                let mut index = repo.index().map_err(|e| e.to_string())?;
                index
                    .add_path(std::path::Path::new("smoke.txt"))
                    .map_err(|e| e.to_string())?;
                let tree_id = index.write_tree().map_err(|e| e.to_string())?;
                let tree = repo.find_tree(tree_id).map_err(|e| e.to_string())?;
                repo.commit(Some("refs/heads/main"), &sig, &sig, "android smoke", &tree, &[])
                    .map_err(|e| e.to_string())?;
                let mut remote = repo.remote_anonymous(&url).map_err(|e| e.to_string())?;
                remote
                    .push(&["refs/heads/main:refs/heads/main"], None)
                    .map_err(|e| e.to_string())
            });
            pushed?;
        }
        _ => println!("[SKIP] push (SMOKE_TOKEN and push-url both needed)"),
    }
    Ok(())
}
