//! cloudraid: archivio cifrato con ridondanza Reed-Solomon distribuito su più storage.

pub mod backend;
pub mod config;
pub mod crypto;
pub mod erasure;
pub mod index;
pub mod repo;

pub use config::{BackendConfig, BackendKind, ChunkConfig, Config, RcloneConfig};
pub use repo::{ReadStats, Repo, ScrubReport};
