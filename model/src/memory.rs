//! How far a VM's memory may grow.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The most memory a VM grows to: a size, in the unit of the field that
/// holds it, or `auto`, as far as its host has memory. JSON has it as a
/// number or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ceiling {
    Size(i64),
    Auto,
}

impl Default for Ceiling {
    fn default() -> Self {
        Ceiling::Size(0)
    }
}

impl Ceiling {
    /// The same ceiling in another unit.
    #[must_use]
    pub fn map(self, f: impl FnOnce(i64) -> i64) -> Self {
        match self {
            Ceiling::Size(n) => Ceiling::Size(f(n)),
            Ceiling::Auto => Ceiling::Auto,
        }
    }

    /// Its size, `auto` being `host`.
    #[must_use]
    pub fn or_host(self, host: i64) -> i64 {
        match self {
            Ceiling::Size(n) => n,
            Ceiling::Auto => host,
        }
    }
}

impl fmt::Display for Ceiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ceiling::Size(n) => write!(f, "{n}"),
            Ceiling::Auto => f.write_str("auto"),
        }
    }
}

/// `auto`, or a whole number, as a form sends it.
impl FromStr for Ceiling {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "auto" { Ok(Ceiling::Auto) } else { s.parse().map(Ceiling::Size) }
    }
}

impl Serialize for Ceiling {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Ceiling::Size(n) => s.serialize_i64(*n),
            Ceiling::Auto => s.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for Ceiling {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Size(i64),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Size(n) => Ok(Ceiling::Size(n)),
            Raw::Word(w) if w == "auto" => Ok(Ceiling::Auto),
            Raw::Word(w) => Err(serde::de::Error::custom(format!("{w:?} is neither a size nor auto"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_forms() {
        assert_eq!(serde_json::to_string(&Ceiling::Size(16)).unwrap(), "16");
        assert_eq!(serde_json::to_string(&Ceiling::Auto).unwrap(), r#""auto""#);
        assert_eq!(serde_json::from_str::<Ceiling>("16384").unwrap(), Ceiling::Size(16384));
        assert_eq!(serde_json::from_str::<Ceiling>(r#""auto""#).unwrap(), Ceiling::Auto);
        assert!(serde_json::from_str::<Ceiling>(r#""most""#).is_err());
        assert_eq!("auto".parse::<Ceiling>().unwrap(), Ceiling::Auto);
        assert_eq!("12".parse::<Ceiling>().unwrap(), Ceiling::Size(12));
        assert!("x".parse::<Ceiling>().is_err() && "".parse::<Ceiling>().is_err());
        assert_eq!(Ceiling::Size(8).map(|g| g * 1024), Ceiling::Size(8192));
        assert_eq!(Ceiling::Auto.map(|g| g * 1024), Ceiling::Auto);
        assert_eq!((Ceiling::Size(8).or_host(64), Ceiling::Auto.or_host(64)), (8, 64));
        assert_eq!((Ceiling::Auto.to_string(), Ceiling::Size(3).to_string()), ("auto".into(), "3".into()));
    }
}
