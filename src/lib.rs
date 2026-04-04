//! dmcp - MCP Manager
//!
//! Discovers, manages, and invokes MCP servers at user and system scope.

pub mod browse;
pub mod config;
pub mod transport;
pub mod connect;
pub mod discovery;
pub mod elevation;
pub mod install;
pub mod models;
pub mod orchestrator;
pub mod paths;
pub mod call;
pub mod run;
pub mod serve;
pub mod setup;
pub mod sources;
pub mod sync_index;
pub mod vector_index;

pub use browse::{filter_servers_by_keywords, list_registry_servers, list_registry_servers_from_url, RegistryServer};
pub use connect::connect;
pub use config::set_config_value;
pub use install::{fetch_server_from_registry, install, scope_from_registry_server, uninstall};
pub use discovery::{get_manifest_path, get_server, list_servers, ServerInfo};
pub use call::{call_tool, format_call_result, list_tools};
pub use run::run;
pub use setup::run_setup;
pub use models::{Index, Manifest};
pub use paths::Paths;
pub use sources::{add_source, list_sources, remove_source, SourceScope, SourcesError};
pub use sync_index::{sync_index, SyncResult};
pub use vector_index::{EmbeddingSpec, SearchResult, VectorEntry, VectorIndex};
