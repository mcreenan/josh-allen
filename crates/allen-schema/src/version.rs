use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExactVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ExactVersion {
    /// Parse one canonical semantic version without pre-release or build text.
    ///
    /// # Errors
    ///
    /// Returns an error unless `input` is exactly `M.m.p` with canonical
    /// unsigned decimal components.
    pub fn parse(input: &str) -> Result<Self, VersionError> {
        let mut parts = input.split('.');
        let major = parse_component(parts.next().ok_or(VersionError)?)?;
        let minor = parse_component(parts.next().ok_or(VersionError)?)?;
        let patch = parse_component(parts.next().ok_or(VersionError)?)?;
        if parts.next().is_some() {
            return Err(VersionError);
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for ExactVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ExactVersion {
    type Err = VersionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionRange {
    pub lower: ExactVersion,
    pub upper: ExactVersion,
}

impl VersionRange {
    /// Parse the exact bounded form `>=M.m.p, <M.m.p`.
    ///
    /// # Errors
    ///
    /// Returns an error for noncanonical, unbounded, empty, or reversed ranges.
    pub fn parse(input: &str) -> Result<Self, VersionError> {
        let (lower, upper) = input.split_once(", ").ok_or(VersionError)?;
        let lower = ExactVersion::parse(lower.strip_prefix(">=").ok_or(VersionError)?)?;
        let upper = ExactVersion::parse(upper.strip_prefix('<').ok_or(VersionError)?)?;
        if lower >= upper {
            return Err(VersionError);
        }
        let range = Self { lower, upper };
        if range.to_string() != input {
            return Err(VersionError);
        }
        Ok(range)
    }

    #[must_use]
    pub fn contains(self, version: ExactVersion) -> bool {
        self.lower <= version && version < self.upper
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, ">={}, <{}", self.lower, self.upper)
    }
}

impl FromStr for VersionRange {
    type Err = VersionError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionError;

impl fmt::Display for VersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("version is not canonical")
    }
}

impl std::error::Error for VersionError {}

fn parse_component(input: &str) -> Result<u64, VersionError> {
    if input.is_empty()
        || !input.bytes().all(|byte| byte.is_ascii_digit())
        || input.len() > 1 && input.starts_with('0')
    {
        return Err(VersionError);
    }
    input.parse().map_err(|_| VersionError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_bounded_ranges_are_exact() {
        let range = VersionRange::parse(">=2.0.0, <3.0.0").unwrap();
        assert!(range.contains(ExactVersion::parse("2.9.1").unwrap()));
        assert!(!range.contains(ExactVersion::parse("3.0.0").unwrap()));
        for invalid in ["1.0", "01.0.0", "1.0.0-alpha", "1.0.0+build", " 1.0.0"] {
            assert!(ExactVersion::parse(invalid).is_err(), "{invalid}");
        }
        for invalid in [
            ">=1.0.0",
            ">=1.0.0,<2.0.0",
            ">1.0.0, <2.0.0",
            ">=2.0.0, <2.0.0",
        ] {
            assert!(VersionRange::parse(invalid).is_err(), "{invalid}");
        }
    }
}
