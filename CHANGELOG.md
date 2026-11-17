# Changelog

## 0.1.0

- First release. Registers a `https` smart transport for libgit2 (via `git2`
  0.21) that talks HTTP with blocking `reqwest` and rustls, so clone, fetch and
  push work without OpenSSL.
- Smart HTTP for `git-upload-pack` and `git-receive-pack` (info/refs GET, POST
  with the request body buffered and sent with `Content-Length`).
- `register` is `unsafe`: it must not race with git2 transport creation.
- Credentials in the remote URL (`https://user:token@host/repo`) are used and
  stripped from the request URL.
- On 401, calls the caller's credential callback and retries once with basic
  auth. Accepted credentials are reused for later POSTs on the same transport.
  libgit2's `RemoteCallbacks::credentials` is not consulted.
- Follows redirects on info/refs only. A redirect on a POST or to a non-https
  URL is refused, and credentials are dropped when a redirect changes scheme,
  host or port.
- Error messages never include the URL's username or password.
- The caller builds the `reqwest::blocking::Client` (TLS roots and proxy are
  the caller's choice).
