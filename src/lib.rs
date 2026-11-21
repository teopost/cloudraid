//! cloudraid: archivio cifrato con ridondanza Reed-Solomon distribuito su più storage.

pub mod backend;
pub mod config;
pub mod credentials;
pub mod crypto;
pub mod erasure;
pub mod index;
pub mod mount;
pub mod repo;
pub mod setup;
pub mod web;
pub mod xml;

pub use config::{AppsConfig, BackendConfig, BackendKind, ChunkConfig, Config};
pub use repo::{ReadStats, Repo, ScrubReport};
