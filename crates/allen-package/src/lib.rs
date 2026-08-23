#![forbid(unsafe_code)]

//! Canonical ALLEN package manifests, local dependency graphs, and lockfiles.

mod error;
mod loader;
mod lockfile;
mod manifest;

pub use error::{PackageError, PackageErrorCode};
pub use loader::{
    LoadLimits, LoadedPackage, PackageId, ResolvedDependency, ResolvedPackage, SourceModule,
    generate_lock, load_verified_package, load_verified_root_package,
};
pub use lockfile::{
    LOCK_VERSION, LockedDependency, LockedPackage, Lockfile, canonical_lockfile, parse_lockfile,
};
pub use manifest::{
    Capabilities, Dependency, Entry, HttpGetNetwork, Manifest, ManifestLimits, Network, Package,
    SUPPORTED_LANGUAGE, ToolRequirement, Tools, canonical_https_origin, parse_manifest,
};
