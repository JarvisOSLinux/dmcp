//! dmcp - MCP Manager
//!
//! Discovers, manages, and invokes MCP servers at user and system scope.

pub mod broker;
pub mod browse;
pub mod call;
pub mod config;
pub mod connect;
pub mod discovery;
pub mod doc_comments;
pub mod elevation;
pub mod install;
pub mod models;
pub mod orchestrator;
pub mod paths;
pub mod platform;
pub mod run;
pub mod serve;
pub mod setup;
pub mod sources;
pub mod sync_index;
pub mod transport;
pub mod update;
pub mod vector_index;

pub use browse::{
    filter_servers_by_keywords, list_registry_servers, list_registry_servers_from_url,
    RegistryServer,
};
pub use call::{call_tool, format_call_result, list_tools};
pub use config::set_config_value;
pub use connect::connect;
pub use discovery::{get_manifest_path, get_server, list_servers, ServerInfo};
pub use doc_comments::{extract_tool_docs, first_description, ToolDoc};
pub use install::{fetch_server_from_registry, install, scope_from_registry_server, uninstall};
pub use models::{Index, Manifest};
pub use paths::Paths;
pub use platform::{host_platform, UnsupportedHost};
pub use run::run;
pub use setup::run_setup;
pub use sources::{add_source, list_sources, remove_source, SourceScope, SourcesError};
pub use sync_index::{sync_index, SyncResult};
pub use update::{assess_servers, refresh_install, AssessedServer, DriftReport, UpdateError};
pub use vector_index::{EmbeddingSpec, SearchResult, VectorEntry, VectorIndex};
