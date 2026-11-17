# Changelog

## 0.1.0

- First release. Registers a `https` smart transport for libgit2 (via `git2`
  0.21) that talks HTTP with blocking `reqwest` and rustls, so clone, fetch and
  push work without OpenSSL.
- Smart HTTP for `git-upload-pack` and `git-receive-pack` (info/refs GET, POST
  with streamed request bodies).
- On 401, calls the caller's credential callback and retries once with basic
  auth. Accepted credentials are reused for later POSTs on the same transport.
- Follows redirects on info/refs only. A redirect on a POST is refused.
- The caller builds the `reqwest::blocking::Client` (TLS roots and proxy are
  the caller's choice).
