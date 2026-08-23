use serde::{Deserialize, Serialize};

use crate::message::invalid;
use crate::payload::{
    Validate, range_contains_language, validate_language_range, validate_semver,
    validate_session_id, validate_sorted_unique,
};
use crate::{FEATURES, LANGUAGE_VERSION, PROTOCOL_VERSION, ProtocolError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerInfo {
    pub name: String,
    pub version: String,
}

impl Validate for PeerInfo {
    fn validate(&self) -> Result<(), ProtocolError> {
        if !(1..=128).contains(&self.name.len()) {
            return Err(invalid("peer name must be 1 through 128 UTF-8 bytes"));
        }
        validate_semver(&self.version)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolLimits {
    pub max_frame_bytes: u64,
    pub max_active_requests: u64,
    pub max_loaded_programs: u64,
    pub max_total_executions: u64,
    pub max_catalog_tools: u64,
    pub max_catalog_bytes: u64,
}

impl Validate for ProtocolLimits {
    fn validate(&self) -> Result<(), ProtocolError> {
        if [
            self.max_frame_bytes,
            self.max_active_requests,
            self.max_loaded_programs,
            self.max_total_executions,
            self.max_catalog_tools,
            self.max_catalog_bytes,
        ]
        .contains(&0)
        {
            return Err(invalid("protocol limits must be positive integers"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeReadyParams {
    pub runtime: PeerInfo,
}

impl Validate for RuntimeReadyParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.runtime.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub host: PeerInfo,
    pub protocol_versions: Vec<String>,
    pub language_versions: Vec<String>,
    pub execution_mode: ExecutionMode,
    pub invoking_session_id: InvokingSessionId,
    pub standard_capabilities: Vec<String>,
    pub limits: ProtocolLimits,
    pub extensions: Vec<String>,
}

impl Validate for InitializeParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.host.validate()?;
        self.validate_protocol_version()?;
        if self.language_versions.is_empty()
            || !self.language_versions.iter().any(|range| {
                validate_language_range(range).is_ok() && range_contains_language(range)
            })
        {
            return Err(invalid("language_versions must contain 0.1.0"));
        }
        for range in &self.language_versions {
            validate_language_range(range)?;
        }
        validate_sorted_unique(&self.standard_capabilities, "standard_capabilities")?;
        for capability in &self.standard_capabilities {
            if !matches!(
                capability.as_str(),
                "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs"
            ) {
                return Err(invalid("standard capability is not implemented"));
            }
        }
        self.limits.validate()?;
        if !self.extensions.is_empty() {
            return Err(invalid("extensions must be empty"));
        }
        match (self.execution_mode, &self.invoking_session_id) {
            (ExecutionMode::Unattended, InvokingSessionId::Null) => {}
            (ExecutionMode::Attached, InvokingSessionId::Id(session_id)) => {
                validate_session_id(session_id)?;
            }
            _ => {
                return Err(invalid(
                    "josh/1.4 invoking_session_id does not match execution_mode",
                ));
            }
        }
        Ok(())
    }
}

impl InitializeParams {
    /// Validates that the peer offers the current protocol contract.
    ///
    /// # Errors
    ///
    /// Returns an error when the list is not exactly the current version.
    pub fn validate_protocol_version(&self) -> Result<(), ProtocolError> {
        match self.protocol_versions.as_slice() {
            [version] if version == PROTOCOL_VERSION => Ok(()),
            _ => Err(invalid(
                "protocol_versions must contain exactly the current protocol version",
            )),
        }
    }

    #[must_use]
    pub fn bound_session_id(&self) -> Option<&str> {
        match &self.invoking_session_id {
            InvokingSessionId::Id(session_id) => Some(session_id),
            InvokingSessionId::Null => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvokingSessionId {
    Null,
    Id(String),
}

impl Serialize for InvokingSessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Id(session_id) => serializer.serialize_str(session_id),
        }
    }
}

impl<'de> Deserialize<'de> for InvokingSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)
            .map(|session_id| session_id.map_or(Self::Null, Self::Id))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Unattended,
    Attached,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub runtime: PeerInfo,
    pub language_version: String,
    pub features: Vec<String>,
    pub limits: ProtocolLimits,
}

impl Validate for InitializeResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol_version != PROTOCOL_VERSION || self.language_version != LANGUAGE_VERSION {
            return Err(invalid("initialize result selected an unsupported version"));
        }
        self.runtime.validate()?;
        if self.features.iter().map(String::as_str).collect::<Vec<_>>() != FEATURES {
            return Err(invalid(
                "initialize result features do not match the current protocol",
            ));
        }
        self.limits.validate()
    }
}
