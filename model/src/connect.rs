//! Reaching a workspace from a terminal: what `workbench-connect`, on a
//! person's computer, and the web UI exchange. A person signed in to the
//! UI makes a pairing code; the client trades it once for a device
//! credential, and the credential for short-lived SSH certificates and
//! the tunnel to a workspace's SSH server. The server keeps only a hash of
//! each credential.

use serde::{Deserialize, Serialize};

/// How long a pairing code works, once.
pub const CODE_MINUTES: i64 = 10;
/// How long a device credential works, from pairing.
pub const DEVICE_DAYS: i64 = 30;
/// How long a device works without its person signing in to the UI, which
/// confirms the groups single sign-on gives them.
pub const GROUPS_DAYS: i64 = 7;
/// How long an SSH certificate works; the client gets another before it
/// ends.
pub const CERT_HOURS: i64 = 8;
/// The most devices one person has.
pub const MAX_DEVICES: usize = 32;
/// The longest label a device has.
pub const MAX_LABEL: usize = 64;

/// What starts a pairing code.
pub const CODE_PREFIX: &str = "wbp-";
/// What starts a device credential.
pub const CREDENTIAL_PREFIX: &str = "wbc-";

/// The path of the tunnel to a workspace's SSH server, on the UI's origin.
#[must_use]
pub fn tunnel_path(workspace: &str) -> String {
    format!("/connect/ssh/{workspace}")
}

/// Whether `s` looks like a pairing code: the prefix and 32 hex digits.
#[must_use]
pub fn is_code(s: &str) -> bool {
    s.strip_prefix(CODE_PREFIX).is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A device credential's id, the part before its secret, if it is one:
/// `wbc-<16 hex>-<64 hex>`.
#[must_use]
pub fn credential_id(s: &str) -> Option<&str> {
    let (id, secret) = s.strip_prefix(CREDENTIAL_PREFIX)?.split_once('-')?;
    let hex = |h: &str, n: usize| h.len() == n && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    (hex(id, 16) && hex(secret, 64)).then_some(id)
}

/// A label a device may have: what the client names it, one line.
#[must_use]
pub fn clean_label(label: &str) -> String {
    let one: String = label.chars().filter(|c| !c.is_control()).collect();
    let trimmed = one.trim();
    let label = if trimmed.is_empty() { "a computer" } else { trimmed };
    label.chars().take(MAX_LABEL).collect()
}

/// `POST /connect/pair`: a pairing code, once, for a device credential.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub code: String,
    /// What the person will see the device as, such as its host name.
    #[serde(default)]
    pub label: String,
}

/// The answer to a pairing: the credential, shown this once, and what the
/// client needs to reach the person's workspaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paired {
    pub credential: String,
    /// The person, whom the workspaces' SSH servers let in by name.
    pub user: String,
    /// Workspaces are `<name>.<domain>`.
    pub domain: String,
    /// The host CA's public key, which signs the workspaces' host keys.
    pub host_ca: String,
    /// When the credential ends, in seconds since the epoch.
    pub expires: i64,
}

/// `POST /connect/cert`, with the credential as a bearer token: an SSH
/// public key of the device's to sign.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertRequest {
    pub public_key: String,
}

/// The key's certificate, for the person.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cert {
    pub certificate: String,
    /// When it ends, in seconds since the epoch.
    pub valid_before: i64,
}

/// Why the connect endpoints refused, as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
}

/// One of a person's devices, as their page lists it, with times as the
/// server says them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRow {
    pub id: String,
    pub label: String,
    pub created: String,
    /// When it last reached a workspace or got a certificate, or empty.
    pub last_used: String,
    pub expires: String,
}

/// What the Connect page shows: how to set a computer up, with a fresh
/// code, and how to reach the app from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectView {
    /// `https://<domain>`.
    pub origin: String,
    /// `<workspace>.<domain>`.
    pub host: String,
    pub code: String,
    pub devices: Vec<DeviceRow>,
}

impl ConnectView {
    /// The command that sets a computer up with the code.
    #[must_use]
    pub fn install(&self) -> String {
        format!("curl -fsSL {}/connect/install.sh | sh -s -- {}", self.origin, self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_credentials() {
        assert!(is_code("wbp-0123456789abcdef0123456789abcdef"));
        for bad in
            ["", "wbp-", "wbp-0123", "wbc-0123456789abcdef0123456789abcdef", "wbp-0123456789abcdef0123456789abcdeg"]
        {
            assert!(!is_code(bad), "{bad}");
        }
        let cred = format!("wbc-0123456789abcdef-{}", "a".repeat(64));
        assert_eq!(credential_id(&cred), Some("0123456789abcdef"));
        for bad in [
            String::new(),
            "wbc-0123456789abcdef".into(),
            format!("wbc-0123456789abcde-{}", "a".repeat(64)),
            format!("wbc-0123456789ABCDEF-{}", "a".repeat(64)),
            format!("wbc-0123456789abcdef-{}", "a".repeat(63)),
            format!("wbp-0123456789abcdef-{}", "a".repeat(64)),
        ] {
            assert_eq!(credential_id(&bad), None, "{bad}");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(clean_label("  laptop\n"), "laptop");
        assert_eq!(clean_label("\u{1b}[31mred"), "[31mred");
        assert_eq!(clean_label(""), "a computer");
        assert_eq!(clean_label(&"x".repeat(100)).len(), MAX_LABEL);
    }

    #[test]
    fn the_install_command() {
        let v = ConnectView { origin: "https://ws.test".into(), code: "wbp-x".into(), ..ConnectView::default() };
        assert_eq!(v.install(), "curl -fsSL https://ws.test/connect/install.sh | sh -s -- wbp-x");
        assert_eq!(tunnel_path("alice"), "/connect/ssh/alice");
    }
}
