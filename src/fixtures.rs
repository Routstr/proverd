//! Test certificate fixtures.
//!
//! Copied from the TLSNotary server-fixture crate (see `fixtures/README.md`).
//! TEST-ONLY: the leaf certificate is for `test-server.io`, a name that is
//! never publicly resolvable.

/// Test CA certificate, DER encoded.
pub const CA_CERT_DER: &[u8] = include_bytes!("../fixtures/root_ca_cert.der");
/// Test CA certificate, PEM encoded (for CLI `--root-cert-pem` usage).
pub const CA_CERT_PEM: &[u8] = include_bytes!("../fixtures/root_ca.crt");
/// Leaf server certificate for [`SERVER_DOMAIN`], DER encoded.
pub const SERVER_CERT_DER: &[u8] = include_bytes!("../fixtures/test_server_cert.der");
/// Private key for [`SERVER_CERT_DER`], DER encoded.
pub const SERVER_KEY_DER: &[u8] = include_bytes!("../fixtures/test_server_private_key.der");
/// The domain bound to the leaf certificate.
pub const SERVER_DOMAIN: &str = "test-server.io";
/// Bearer token the fixture routes demand.
pub const AUTH_TOKEN: &str = "random_auth_token";
