//! What only the front and its nodes exchange: the front's answer to a
//! start, with the secrets a node holds only while the workspace runs.
//! The rest of the contract, which the web UI reads too, is
//! `workbench_model::fleet`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use workbench_model::fleet::Record;

use crate::launch::{Given, HostKey, Secrets};

/// `POST /start/<ws>`'s answer: what a node needs to start a workspace
/// the registry has on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartAnswer {
    /// For one made at run time, its record, which the node's template
    /// makes its config of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Record>,
    /// Its secrets directory and secret environment, as
    /// `write_secrets_share` copies them on a host on its own.
    pub secrets: WireSecrets,
    /// Its SSH host key, with a certificate for `<ws>.<domain>` signed
    /// now; none from a front without the CAs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<WireHostKey>,
    /// The user CA's public key, which the guest trusts for its owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ca: Option<B64>,
}

/// Bytes, as base64 in JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct B64(pub Vec<u8>);

impl TryFrom<String> for B64 {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        STANDARD.decode(s.as_bytes()).map(B64).map_err(|e| format!("base64: {e}"))
    }
}

impl From<B64> for String {
    fn from(b: B64) -> String {
        STANDARD.encode(&b.0)
    }
}

/// A secrets share: its directories, each after its parent, its files
/// and the secret environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WireSecrets {
    #[serde(default)]
    pub dirs: Vec<String>,
    #[serde(default)]
    pub files: Vec<WireFile>,
    #[serde(default)]
    pub environment: B64,
}

/// A secret file, by its path relative to the secrets directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFile {
    pub path: String,
    pub data: B64,
}

/// A guest's host key, its public key and its certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireHostKey {
    pub private: B64,
    pub public: B64,
    pub certificate: B64,
}

/// A secret's path as the answer carries it: UTF-8 only.
fn utf8(p: &std::path::Path) -> anyhow::Result<String> {
    p.to_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("a secret's name, {}, isn't UTF-8", p.display()))
}

impl TryFrom<&Secrets> for WireSecrets {
    type Error = anyhow::Error;

    fn try_from(s: &Secrets) -> anyhow::Result<Self> {
        Ok(WireSecrets {
            dirs: s.dirs.iter().map(|d| utf8(d)).collect::<anyhow::Result<_>>()?,
            files: s
                .files
                .iter()
                .map(|(path, data)| Ok(WireFile { path: utf8(path)?, data: B64(data.clone()) }))
                .collect::<anyhow::Result<_>>()?,
            environment: B64(s.environment.clone()),
        })
    }
}

impl From<&HostKey> for WireHostKey {
    fn from(k: &HostKey) -> Self {
        WireHostKey {
            private: B64(k.private.clone()),
            public: B64(k.public.clone()),
            certificate: B64(k.certificate.clone()),
        }
    }
}

impl StartAnswer {
    /// What `workbench run` shares with the guest from it.
    #[must_use]
    pub fn given(&self) -> Given {
        Given {
            secrets: Secrets {
                dirs: self.secrets.dirs.iter().map(std::path::PathBuf::from).collect(),
                files: self.secrets.files.iter().map(|f| (f.path.clone().into(), f.data.0.clone())).collect(),
                environment: self.secrets.environment.0.clone(),
            },
            host_key: self.host_key.as_ref().map(|k| HostKey {
                private: k.private.0.clone(),
                public: k.public.0.clone(),
                certificate: k.certificate.0.clone(),
            }),
            user_ca: self.user_ca.as_ref().map(|c| c.0.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_round_trip() {
        let secrets = Secrets {
            dirs: vec!["git".into()],
            files: vec![("git/credentials".into(), b"https://u:p@git\0\xff".to_vec())],
            environment: b"A=1\n".to_vec(),
        };
        let key = HostKey { private: b"k".to_vec(), public: b"p".to_vec(), certificate: b"c".to_vec() };
        let a = StartAnswer {
            record: None,
            secrets: (&secrets).try_into().unwrap(),
            host_key: Some((&key).into()),
            user_ca: Some(B64(b"ssh-ed25519 AAAA ca".to_vec())),
        };
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["secrets"]["environment"], "QT0xCg==");
        let back: StartAnswer = serde_json::from_value(json).unwrap();
        let given = back.given();
        assert_eq!((given.secrets, given.host_key), (secrets, Some(key)));
        assert!(serde_json::from_str::<B64>("\"not base64!\"").is_err());
    }
}
