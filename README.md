# git2-rustls-transport

A libgit2 transport for `https://` remotes, written in Rust. It plugs into
[`git2`](https://crates.io/crates/git2) and performs the smart-HTTP requests
with blocking [`reqwest`](https://crates.io/crates/reqwest) and rustls, so
libgit2 can clone, fetch and push over https without OpenSSL. This is mainly
useful on Android, where OpenSSL is awkward to build.

## Usage

```rust,no_run
use git2_rustls_transport::{register, Credentials};

// Build the client yourself. Redirects must be disabled: the transport follows
// info/refs redirects itself and refuses redirects on POST.
let client = reqwest::blocking::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .build()
    .expect("client");

// Called on 401. Return None to fail the request.
register(client, |url| {
    Some(Credentials {
        username: "user".into(),
        password: "token".into(),
    })
})
.expect("register transport");

let _repo = git2::build::RepoBuilder::new()
    .clone("https://example.com/org/repo.git", std::path::Path::new("repo"))
    .expect("clone");
```

## Notes

- `register` installs the transport process-wide and is `unsafe` in `git2`. Call
  it once per process, early, before any https clone or fetch.
- The scheme `"https"` is matched as a prefix, so every https URL goes through
  this transport once it is registered.
- Authentication is HTTP basic auth only. A 401 is retried once with the
  callback's credentials. A 401 on a POST is not retried.
- The crate enables no TLS feature itself. TLS settings (trust roots, client
  certificates, proxy) come from the `reqwest` client you pass in.
- On Android, `reqwest`'s platform verifier needs a JVM and JNI setup. Use
  explicit root certificates if you cannot provide one.

## Platform status

- Windows (desktop, verified locally): clone, fetch, push and the 401 basic-auth
  retry pass against a local `git http-backend` behind a self-signed TLS server.
  Linux is covered by CI only (not run locally yet).
- Android (`aarch64-linux-android`, `x86_64-linux-android`): the library builds
  with the NDK. No emulator run has been done. See the CI workflow for the
  build job.

## Building and testing

```sh
cargo test
```

The tests need `git` on `PATH`.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option.
