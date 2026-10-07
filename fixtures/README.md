# Test certificate fixtures

Copied verbatim from the TLSNotary repository's server-fixture crate
(`crates/server-fixture/certs/src/tls/` at tlsnotary/tlsn
`4415391333bd67bd2b3f62082f0b6c3d0261aa05`, MIT OR Apache-2.0), so this repo
is self-contained for its TLS test server and integration tests.

- `root_ca_cert.der` / `root_ca.crt` — test CA (DER / PEM).
- `test_server_cert.der`, `test_server_private_key.der` — leaf cert for
  `test-server.io` and its key.

These are TEST-ONLY credentials for a name that is never publicly resolvable.
Do not use them for anything else.
