//! dmcp - MCP Manager CLI

use clap::{Parser, Subcommand};
use dmcp::call::{emit_elevation_sentinel, strip_elevation_sentinel_flag};
use dmcp::config;
use dmcp::elevation::{
    is_elevated, is_system_scope, re_exec_with_pkexec, restore_invoking_user_home,
};
use dmcp::{
    add_source, call, connect, discovery, fetch_server_from_registry, filter_servers_by_keywords,
    get_server, install, list_registry_servers, list_registry_servers_from_url, list_servers,
    list_sources, remove_source, run, run_setup, scope_from_registry_server, set_config_value,
    uninstall, Paths,
};
use dmcp::{sync_index, VectorEntry, VectorIndex};

#[derive(Parser)]
#[command(name = "dmcp")]
#[command(about = "MCP Manager - discover, manage, and invoke MCP servers")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable debug output
    #[arg(short, long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// List installed MCP servers (default: both user and system)
    List {
        /// Include user-scope servers only
        #[arg(long)]
        user: bool,

        /// Include system-scope servers only
        #[arg(long)]
        system: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show detailed info for a server
    Info {
        /// Server ID (e.g. com.example.calculator)
        id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Get or set server configuration
    Config {
        /// Server ID
        id: String,

        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage registry sources
    Sources {
        #[command(subcommand)]
        action: SourcesAction,
    },

    /// Install an MCP server from registry (by ID) or from manifest/endpoint URL
    Install {
        /// Server ID (from registry) or URL (manifest.json or SSE/WebSocket endpoint)
        id_or_url: String,

        /// Install to system scope (requires elevation)
        #[arg(long)]
        system: bool,

        /// Skip running the setup script (if defined)
        #[arg(long)]
        no_setup: bool,

        /// Install even though the registry does not vouch for this platform
        /// (use when verifying a new OS, then PR the result to the registry)
        #[arg(long)]
        ignore_platform: bool,
    },

    /// Uninstall an MCP server
    Uninstall {
        /// Server ID to uninstall
        id: String,
    },

    /// Update installed servers whose registry manifest has drifted
    ///
    /// Drift is detected by manifest hash, not version: a same-version fix or a
    /// security republish still changes the hash. Updating re-runs the install
    /// flow (overwrite, hash-verify, re-clone, re-run setup) under the same
    /// trust gates as install.
    Update {
        /// Server ID to update (omit and pass --all to update every server)
        id: Option<String>,

        /// Update all installed servers that have drifted
        #[arg(long, conflicts_with = "id")]
        all: bool,

        /// Report drift without changing anything
        #[arg(long)]
        check: bool,

        /// Output the drift report as JSON (requires --check)
        #[arg(long, requires = "check")]
        json: bool,

        /// Refresh even though the registry does not vouch for this platform
        #[arg(long)]
        ignore_platform: bool,
    },

    /// Run an MCP server (stdio: spawn and relay; SSE/WebSocket: print connection URL)
    Run {
        /// Server ID to run
        id: String,

        /// Enable verbose output (reserved for future debug mode)
        #[arg(long)]
        verbose: bool,
    },

    /// Connect to a remote server. Fetches manifest from URL if valid JSON; otherwise treats URL as raw endpoint.
    Connect {
        /// URL to manifest.json (fetched and used) or raw SSE/WebSocket endpoint (fallback)
        url: String,

        /// Override server ID
        #[arg(long)]
        id: Option<String>,

        /// Override display name
        #[arg(long)]
        name: Option<String>,

        /// Override short description
        #[arg(long)]
        summary: Option<String>,

        /// Override version string
        #[arg(long)]
        version: Option<String>,

        /// Config key=value (repeatable, overrides manifest config)
        #[arg(short, long, value_parser = parse_config)]
        config: Vec<(String, String)>,

        /// Install to system scope (requires elevation)
        #[arg(long)]
        system: bool,

        /// Skip running the setup script (if defined)
        #[arg(long)]
        no_setup: bool,

        /// Connect even though the manifest does not vouch for this platform
        /// (use when verifying a new OS, then PR the result to the registry)
        #[arg(long)]
        ignore_platform: bool,
    },

    /// Run the setup script for an installed server (e.g. after config changes)
    Setup {
        /// Server ID
        id: String,
    },

    /// List tools available on an MCP server
    Tools {
        /// Server ID
        id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Call a tool on an MCP server
    Call {
        /// Server ID
        id: String,

        /// Tool name to call
        tool: String,

        /// Tool arguments as JSON (e.g. '{"key":"value"}')
        #[arg(long)]
        args: Option<String>,

        /// Keep the server alive across calls under this session id (stateful,
        /// user-scope servers only). Routes the call through the broker so
        /// in-process state (a browser, a REPL) survives between calls.
        #[arg(long)]
        session: Option<String>,

        /// Answer questions the server asks mid-call over stdio (requires
        /// --session). Every stdout line becomes a JSON object tagged with
        /// `type`: a `prompt` to answer by writing one JSON answer line to
        /// stdin, then the final `result`. For a caller that drives dmcp as a
        /// subprocess; without it, prompts are declined.
        #[arg(long, requires = "session")]
        interactive: bool,
    },

    /// Manage live server sessions (broker-backed stateful servers)
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    /// Run the session broker in the foreground (internal; auto-started on demand)
    #[command(hide = true)]
    Broker,

    /// Run dmcp as an MCP server (for LLM integration)
    Serve,

    /// Browse servers available in registry sources (or a specific registry URL).
    /// Use --vector / --vectors for semantic search against the local vector index.
    Browse {
        /// Registry URL to browse (omit to use configured sources; ignored for vector search)
        url: Option<String>,

        /// Show user-scope sources only (ignored when URL is given)
        #[arg(long)]
        user: bool,

        /// Show system-scope sources only (ignored when URL is given)
        #[arg(long)]
        system: bool,

        /// Filter by keyword (repeatable). Matches id, name, summary, and keywords.
        #[arg(short = 'k', long = "keyword")]
        keyword: Vec<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Semantic search: query vector as a JSON array of floats
        #[arg(long, conflicts_with = "vectors")]
        vector: Option<String>,

        /// Batch semantic search: JSON array of query vectors (array-of-arrays)
        #[arg(long, conflicts_with = "vector")]
        vectors: Option<String>,

        /// Maximum results per query vector (default: 5)
        #[arg(long, default_value = "5")]
        top_k: usize,

        /// Minimum cosine similarity score 0.0–1.0 (default: 0.0)
        #[arg(long, default_value = "0.0")]
        min_score: f32,
    },

    /// Count visible MCP servers (local + reachable registries)
    Count {
        /// Output as JSON with breakdown { "total", "local", "registry" }
        #[arg(long)]
        json: bool,
    },

    /// Download and cache registry vector embeddings for semantic search
    SyncIndex,

    /// Show the embedding model specification stored in the local vector index
    EmbeddingSpec {
        /// Output as JSON (default output is already JSON)
        #[arg(long)]
        json: bool,
    },

    /// Store externally-computed embedding vectors for a server (non-registry servers)
    IndexServer {
        /// Server ID (e.g. com.example.myserver)
        server_id: String,

        /// Vectors as JSON: {"server": [...], "tools": {"tool_name": [...]}}
        #[arg(long)]
        vectors: String,

        /// Server display name (used in search results)
        #[arg(long)]
        name: Option<String>,

        /// Server description (used in search results)
        #[arg(long)]
        description: Option<String>,
    },

    /// Show resolved paths (for debugging)
    Paths,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Get config value(s)
    Get {
        /// Specific key (omit for all)
        key: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Set a config value
    Set {
        /// Config key
        key: String,

        /// Config value
        value: String,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// List live sessions (session id, server, scope, age, idle)
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Close a session's server(s). Idempotent (exit 0 even if nothing is open).
    Close {
        /// Session id to close
        session: String,

        /// Close only this server within the session (default: all)
        #[arg(long)]
        id: Option<String>,
    },

    /// Close sessions idle beyond the TTL now
    Gc,
}

#[derive(Subcommand)]
enum SourcesAction {
    /// List registry source URLs
    List {
        /// Show user-scope sources only
        #[arg(long)]
        user: bool,

        /// Show system-scope sources only
        #[arg(long)]
        system: bool,
    },

    /// Add a registry source URL
    Add {
        /// URL of the registry JSON file
        url: String,

        /// Add to user scope (default)
        #[arg(long)]
        user: bool,

        /// Add to system scope (requires elevation)
        #[arg(long)]
        system: bool,
    },

    /// Remove a registry source URL
    Remove {
        /// URL to remove
        url: String,

        /// Remove from user scope
        #[arg(long)]
        user: bool,

        /// Remove from system scope (requires elevation)
        #[arg(long)]
        system: bool,
    },
}

fn main() {
    // FIRST, before dotenvy, Paths::resolve, and any thread/runtime: a pkexec
    // re-exec lands here with HOME reset to root's; restore the invoking user's
    // HOME from PKEXEC_UID so config resolution reads their sources.list, not
    // /root's (issue #52). set_var is process-global — sound only single-threaded.
    restore_invoking_user_home();

    // A delegated elevation re-exec carries an internal flag (pkexec/sudo strip
    // the environment, so it cannot ride an env var): if present, this is the
    // now-root child announcing it is past authentication — emit the sentinel
    // the parent watches for, then strip the flag so clap never sees it (#51).
    // args_os() rather than args() so a non-UTF-8 argv element passes through
    // instead of panicking the process before clap can handle it.
    let (raw_args, delegated_authenticated) =
        strip_elevation_sentinel_flag(std::env::args_os().collect());
    if delegated_authenticated {
        emit_elevation_sentinel();
    }

    dotenvy::dotenv().ok();
    let cli = Cli::parse_from(raw_args);
    let paths = Paths::resolve();
    let debug = cli.debug;

    match cli.command {
        Commands::Paths => {
            println!("User install dir:  {}", paths.user_install_dir().display());
            println!(
                "System install dir: {}",
                paths.system_install_dir().display()
            );
            let user_index = paths.user_install_dir().join("index.json");
            let system_index = paths.system_install_dir().join("index.json");
            println!("User index exists:  {}", user_index.exists());
            println!("System index exists: {}", system_index.exists());
        }
        Commands::List { user, system, json } => {
            let include_user = user || !system;
            let include_system = system || !user;
            let servers = list_servers(&paths, include_user, include_system, debug);

            if json {
                let output = serde_json::to_string_pretty(&servers).unwrap();
                println!("{output}");
            } else {
                if servers.is_empty() {
                    println!("No MCP servers installed.");
                    return;
                }
                print_list_table(&servers);
            }
        }
        Commands::Info { id, json } => match get_server(&paths, &id) {
            Some((manifest, scope)) => {
                let scope_str = match scope {
                    dmcp::discovery::Scope::User => "user",
                    dmcp::discovery::Scope::System => "system",
                };
                if json {
                    let output = serde_json::to_string_pretty(&manifest).unwrap();
                    println!("{output}");
                } else {
                    print_info_output(&manifest, scope_str);
                }
            }
            None => {
                eprintln!("Server not found: {}", id);
                std::process::exit(1);
            }
        },
        Commands::Config { id, action } => match action {
            ConfigAction::Set { key, value } => match set_config_value(&paths, &id, &key, &value) {
                Ok(()) => println!("Set {} = {}", key, value),
                Err(config::SetConfigError::WriteFailed(_, manifest_path)) if !is_elevated() => {
                    if is_system_scope(&manifest_path, paths.system_install_dir()) {
                        re_exec_with_pkexec();
                    } else {
                        eprintln!("Error: Failed to write manifest (permission denied)");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            },
            ConfigAction::Get { key, json } => match get_server(&paths, &id) {
                Some((manifest, _)) => {
                    if let Some(k) = key {
                        match manifest.config.get(&k) {
                            Some(v) => {
                                if json {
                                    println!("{}", serde_json::to_string_pretty(v).unwrap());
                                } else {
                                    let val: String = v
                                        .as_str()
                                        .map(String::from)
                                        .unwrap_or_else(|| v.to_string());
                                    println!("{}", val);
                                }
                            }
                            None => {
                                eprintln!("Config key not found: {}", k);
                                std::process::exit(1);
                            }
                        }
                    } else {
                        if json {
                            let output = serde_json::to_string_pretty(&manifest.config).unwrap();
                            println!("{output}");
                        } else {
                            if manifest.config.is_empty() {
                                println!("No config set.");
                            } else {
                                for (k, v) in &manifest.config {
                                    let val: String = v
                                        .as_str()
                                        .map(String::from)
                                        .unwrap_or_else(|| v.to_string());
                                    println!("{} = {}", k, val);
                                }
                            }
                        }
                    }
                }
                None => {
                    eprintln!("Server not found: {}", id);
                    std::process::exit(1);
                }
            },
        },
        Commands::Sources { action } => match action {
            SourcesAction::List { user, system } => {
                let include_user = user || !system;
                let include_system = system || !user;
                let sources = list_sources(&paths, include_user, include_system);
                if sources.is_empty() {
                    println!("No registry sources configured.");
                    println!("Add URLs to ~/.config/mcp/sources.list or /etc/mcp/sources.list");
                    return;
                }
                println!("{:<8} URL", "SCOPE");
                println!("{}", "-".repeat(80));
                for (url, scope) in sources {
                    let scope_str = match scope {
                        dmcp::SourceScope::User => "user",
                        dmcp::SourceScope::System => "system",
                    };
                    println!("{:<8} {}", scope_str, url);
                }
            }
            SourcesAction::Add { url, system, .. } => {
                let scope = if system {
                    dmcp::SourceScope::System
                } else {
                    dmcp::SourceScope::User
                };
                // System scope needs root for create_dir + write; re-exec upfront
                if scope == dmcp::SourceScope::System && !is_elevated() {
                    re_exec_with_pkexec();
                }
                match add_source(&paths, &url, scope) {
                    Ok(()) => println!("Added {}", url),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            SourcesAction::Remove { url, system, .. } => {
                let scope = if system {
                    dmcp::SourceScope::System
                } else {
                    dmcp::SourceScope::User
                };
                // System scope needs root; re-exec upfront
                if scope == dmcp::SourceScope::System && !is_elevated() {
                    re_exec_with_pkexec();
                }
                match remove_source(&paths, &url, scope) {
                    Ok(()) => println!("Removed {}", url),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        },
        Commands::Install {
            id_or_url,
            system,
            no_setup,
            ignore_platform,
        } => {
            let run_setup = !no_setup;
            let is_url = id_or_url.starts_with("http://") || id_or_url.starts_with("https://");
            if is_url {
                // URL: use connect flow
                let scope = if system {
                    dmcp::discovery::Scope::System
                } else {
                    dmcp::discovery::Scope::User
                };
                if scope == dmcp::discovery::Scope::System && !is_elevated() {
                    re_exec_with_pkexec();
                }
                match connect(
                    &paths,
                    &id_or_url,
                    None,
                    None,
                    None,
                    None,
                    &[],
                    scope,
                    run_setup,
                    ignore_platform,
                ) {
                    Ok(id) => println!("Installed {}", id),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                // ID: use registry install flow
                let id = id_or_url;
                let server = match fetch_server_from_registry(&paths, &id) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                };
                // Human CLI trust gate: any tier is installable, but community
                // is a "you are trusting the submitter" warning and removed is
                // refused (TRUST-MODEL §2.1).
                match dmcp::install::cli_trust_gate(dmcp::install::trust_status(&server)) {
                    dmcp::install::TrustGate::Deny(reason) => {
                        eprintln!("Error: refusing to install {}: {}", id, reason);
                        std::process::exit(1);
                    }
                    dmcp::install::TrustGate::Warn(msg) => eprintln!("[warn] {}", msg),
                    dmcp::install::TrustGate::Allow => {}
                }
                if let Some(refusal) = install_platform_refusal(&server, ignore_platform) {
                    eprintln!("Error: refusing to install {}: {}", id, refusal);
                    std::process::exit(1);
                }
                let scope = if system {
                    dmcp::discovery::Scope::System
                } else {
                    scope_from_registry_server(&server)
                };
                if scope == dmcp::discovery::Scope::System && !is_elevated() {
                    re_exec_with_pkexec();
                }
                match install(&paths, &id, scope, Some(server), run_setup, ignore_platform) {
                    Ok(()) => println!("Installed {}", id),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::Run { id, verbose } => match run(&paths, &id, verbose) {
            Ok(()) => {}
            Err(dmcp::run::RunError::ProcessExited(code)) => std::process::exit(code),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },
        Commands::Uninstall { id } => {
            if let Some((_, _, scope)) = discovery::get_uninstall_info(&paths, &id) {
                if scope == dmcp::discovery::Scope::System && !is_elevated() {
                    re_exec_with_pkexec();
                }
            }
            match uninstall(&paths, &id) {
                Ok(()) => println!("Uninstalled {}", id),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Update {
            id,
            all,
            check,
            json,
            ignore_platform,
        } => {
            if !all && id.is_none() {
                eprintln!("Error: specify a server id or --all");
                std::process::exit(1);
            }

            let ids: Vec<String> = if all {
                list_servers(&paths, true, true, false)
                    .into_iter()
                    .map(|s| s.id)
                    .collect()
            } else {
                vec![id.clone().unwrap()]
            };

            if ids.is_empty() {
                if json {
                    println!("[]");
                } else {
                    println!("No MCP servers installed.");
                }
                return;
            }

            let assessed = match dmcp::update::assess_servers(&paths, &ids) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };

            if check {
                if json {
                    let reports: Vec<&dmcp::update::DriftReport> =
                        assessed.iter().map(|a| &a.report).collect();
                    println!("{}", serde_json::to_string_pretty(&reports).unwrap());
                } else {
                    print_drift_check(&assessed);
                }
                return;
            }

            // Apply mode.
            let mut had_error = false;
            for a in &assessed {
                let id = &a.report.id;
                if !a.in_registry {
                    println!(
                        "{}: not found in any registry; local copy left unchanged",
                        id
                    );
                    continue;
                }
                if a.revoked {
                    // A removed server is a security revocation: never refresh.
                    // Match the install gate's UX (advise, don't act silently).
                    println!(
                        "{}: REVOKED upstream (removed from registry); not refreshed",
                        id
                    );
                    println!("  run `dmcp uninstall {}` to remove it", id);
                    continue;
                }
                if !a.report.update_available {
                    println!("{}: up to date", id);
                    continue;
                }
                if a.report.unsupported_on_host && !ignore_platform {
                    // Refuse from the drift report the check already fetched,
                    // instead of letting the install gate catch it after a
                    // second registry round-trip.
                    eprintln!(
                        "Error: refusing to update {}: {}",
                        id,
                        a.report.platform_refusal()
                    );
                    had_error = true;
                    continue;
                }

                match dmcp::update::trust_gate_for_update(&a.report.trust_status) {
                    Ok(Some(msg)) => eprintln!("[warn] {}", msg),
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("Error: refusing to update {}: {}", id, e);
                        had_error = true;
                        continue;
                    }
                }

                if a.scope == dmcp::discovery::Scope::System && !is_elevated() {
                    if all {
                        // Re-execing the whole batch as root would rewrite
                        // user-scope trees with root ownership; skip and advise.
                        eprintln!(
                            "[warn] {}: system-scope update needs elevation; run `dmcp update {}`",
                            id, id
                        );
                        continue;
                    }
                    re_exec_with_pkexec();
                }

                match dmcp::update::refresh_install(&paths, id, a.scope, ignore_platform) {
                    Ok(()) => println!("{}: updated", id),
                    Err(e) => {
                        eprintln!("Error updating {}: {}", id, e);
                        had_error = true;
                    }
                }
            }
            if had_error {
                std::process::exit(1);
            }
        }
        Commands::Connect {
            url,
            id,
            name,
            summary,
            version,
            config,
            system,
            no_setup,
            ignore_platform,
        } => {
            let scope = if system {
                dmcp::discovery::Scope::System
            } else {
                dmcp::discovery::Scope::User
            };
            if scope == dmcp::discovery::Scope::System && !is_elevated() {
                re_exec_with_pkexec();
            }
            let config_ref: Vec<(String, String)> = config.to_vec();
            let run_setup = !no_setup;
            match connect(
                &paths,
                &url,
                id.as_deref(),
                name.as_deref(),
                summary.as_deref(),
                version.as_deref(),
                &config_ref,
                scope,
                run_setup,
                ignore_platform,
            ) {
                Ok(id) => println!("Connected {}", id),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Setup { id } => {
            match get_server(&paths, &id) {
                Some((manifest, _)) => {
                    let setup_script = manifest.setup_script_for_host(dmcp::host_platform());
                    match setup_script {
                        Some(script) => {
                            let install_dir = manifest
                                .install_dir
                                .as_deref()
                                .map(std::path::Path::new)
                                .filter(|p| p.is_absolute())
                                .map(|p| p.to_path_buf())
                                .or_else(|| {
                                    discovery::get_manifest_path(&paths, &id)
                                        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                                });
                            match install_dir {
                                Some(dir) => {
                                    if let Err(e) = run_setup(script, &dir, &manifest.config) {
                                        eprintln!("Error: {}", e);
                                        std::process::exit(1);
                                    }
                                    // Update manifest with setup run timestamp
                                    if let Some(manifest_path) =
                                        discovery::get_manifest_path(&paths, &id)
                                    {
                                        if let Ok(content) = std::fs::read_to_string(&manifest_path)
                                        {
                                            if let Ok(mut m) =
                                                serde_json::from_str::<serde_json::Value>(&content)
                                            {
                                                m["setupScriptPath"] = serde_json::json!(dir
                                                    .join(script)
                                                    .to_string_lossy());
                                                m["setupScriptRunAt"] = serde_json::Value::String(
                                                    dmcp::install::rfc3339_now(),
                                                );
                                                m["setupScriptVersion"] = manifest
                                                    .setup_script_version
                                                    .as_ref()
                                                    .map(|s| serde_json::Value::String(s.clone()))
                                                    .unwrap_or(serde_json::json!("1.0.0"));
                                                let _ = std::fs::write(
                                                    &manifest_path,
                                                    serde_json::to_string_pretty(&m)
                                                        .unwrap_or_default(),
                                                );
                                            }
                                        }
                                    }
                                    println!("Setup complete for {}", id);
                                }
                                None => {
                                    eprintln!("Error: Could not determine install directory");
                                    std::process::exit(1);
                                }
                            }
                        }
                        None => {
                            eprintln!("Server {} has no setup script defined", id);
                            std::process::exit(1);
                        }
                    }
                }
                None => {
                    eprintln!("Server not found: {}", id);
                    std::process::exit(1);
                }
            }
        }
        Commands::Tools { id, json } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            match rt.block_on(call::list_tools(&paths, &id)) {
                Ok(tools) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&tools).unwrap());
                    } else {
                        for t in &tools {
                            println!("{} - {}", t.name, t.description.as_deref().unwrap_or(""));
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Call {
            id,
            tool,
            args,
            session,
            interactive,
        } => {
            let args_val = args.as_deref().and_then(|s| serde_json::from_str(s).ok());
            if let Some(session_id) = session {
                // Session path: forward to the broker, which keeps the server's
                // process alive across calls. Gated on stateful + user scope; the
                // output shape (and exit-2-on-tool-error) matches the one-shot
                // path so callers can't tell the difference on success.
                // In interactive mode the caller reads a tagged JSON stream, so
                // the result is emitted in that shape too rather than as bare
                // text a reader would have to distinguish by guessing.
                let driver = interactive.then_some(dmcp::broker::StdioPromptDriver);
                let outcome = dmcp::broker::session_call_with(
                    &paths,
                    &id,
                    &tool,
                    args_val,
                    &session_id,
                    driver
                        .as_ref()
                        .map(|d| d as &dyn dmcp::broker::PromptDriver),
                );
                match outcome {
                    Ok(result) => {
                        if interactive {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "type": "result",
                                    "content": result.content,
                                    "isError": result.is_error,
                                })
                            );
                        } else {
                            println!("{}", result.content);
                        }
                        if result.is_error {
                            std::process::exit(2);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                // One-shot path (unchanged): a system-scoped stdio server's tools
                // must run as root; re-exec via pkexec/polkit before any work,
                // mirroring `dmcp run` (#33).
                call::elevate_call_for_system_scope(&paths, &id);
                let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                match rt.block_on(call::call_tool(&paths, &id, &tool, args_val)) {
                    Ok(result) => {
                        println!("{}", call::format_call_result(&result));
                        // Signal a tool-reported error via the exit code, out-of-band
                        // from the output stream, so a caller (e.g. dispatch) reads
                        // status structurally instead of sniffing the output text.
                        if call::call_is_error(&result) {
                            std::process::exit(2);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::Session { action } => match action {
            SessionAction::List { json } => match dmcp::broker::session_list(&paths) {
                Ok(sessions) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&sessions).unwrap());
                    } else if sessions.is_empty() {
                        println!("No active sessions.");
                    } else {
                        print_session_table(&sessions);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            },
            SessionAction::Close { session, id } => {
                match dmcp::broker::session_close(&paths, &session, id.as_deref()) {
                    Ok(n) => println!("Closed {} session server(s).", n),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
            SessionAction::Gc => match dmcp::broker::session_gc(&paths) {
                Ok(n) => println!("Closed {} idle session server(s).", n),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            },
        },
        Commands::Broker => {
            if let Err(e) = dmcp::broker::run_broker_foreground(&paths) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Serve => {
            if let Err(e) = dmcp::serve::run(&paths) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Browse {
            url,
            user,
            system,
            keyword,
            json,
            vector,
            vectors,
            top_k,
            min_score,
        } => {
            // Vector search mode: search the local index, bypass registry
            if let Some(ref vec_str) = vector {
                let query: Vec<f32> = match serde_json::from_str(vec_str) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Error: --vector must be a JSON array of floats: {}", e);
                        std::process::exit(1);
                    }
                };
                let index_path = paths.vector_index_path();
                let index = match VectorIndex::load(&index_path) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("Error loading vector index: {}", e);
                        std::process::exit(1);
                    }
                };
                if index.entries.is_empty() {
                    eprintln!("Vector index is empty. Run `dmcp sync-index` to populate it.");
                    std::process::exit(1);
                }
                let results = index.search(&query, top_k, min_score);
                if json {
                    println!("{}", serde_json::to_string_pretty(&results).unwrap());
                } else {
                    if results.is_empty() {
                        println!("No results above min-score {:.2}.", min_score);
                    } else {
                        print_vector_results(&results);
                    }
                }
                return;
            }

            if let Some(ref vecs_str) = vectors {
                let queries: Vec<Vec<f32>> = match serde_json::from_str(vecs_str) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "Error: --vectors must be a JSON array of float arrays: {}",
                            e
                        );
                        std::process::exit(1);
                    }
                };
                let index_path = paths.vector_index_path();
                let index = match VectorIndex::load(&index_path) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("Error loading vector index: {}", e);
                        std::process::exit(1);
                    }
                };
                if index.entries.is_empty() {
                    eprintln!("Vector index is empty. Run `dmcp sync-index` to populate it.");
                    std::process::exit(1);
                }
                let batch = index.search_batch(&queries, top_k, min_score);
                if json {
                    println!("{}", serde_json::to_string_pretty(&batch).unwrap());
                } else {
                    for (i, results) in batch.iter().enumerate() {
                        println!("=== Query {} ===", i + 1);
                        if results.is_empty() {
                            println!("No results above min-score {:.2}.", min_score);
                        } else {
                            print_vector_results(results);
                        }
                        println!();
                    }
                }
                return;
            }

            // Keyword / registry browse mode (existing behavior)
            let (mut servers, errors): (Vec<_>, Vec<_>) = if let Some(ref u) = url {
                match list_registry_servers_from_url(u) {
                    Ok(s) => (s, vec![]),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                let include_user = user || !system;
                let include_system = system || !user;
                list_registry_servers(&paths, include_user, include_system)
            };

            let (include_user, include_system) = if url.is_some() {
                (true, true)
            } else {
                (user || !system, system || !user)
            };
            let installed = list_servers(&paths, include_user, include_system, false);
            let installed_hashes: std::collections::HashMap<String, Option<String>> = installed
                .iter()
                .map(|s| {
                    (
                        s.id.clone(),
                        dmcp::update::read_installed_hash(std::path::Path::new(&s.manifest_path)),
                    )
                })
                .collect();
            let installed_ids: std::collections::HashSet<String> =
                installed.into_iter().map(|s| s.id).collect();

            for s in &mut servers {
                s.installed = installed_ids.contains(&s.id);
                // Flag drift only for installed servers whose registry entry
                // records a manifest hash to compare against.
                if s.installed {
                    if let Some(reg) = s.registry_manifest_sha256.clone() {
                        let inst = installed_hashes.get(&s.id).cloned().flatten();
                        s.update_available =
                            Some(dmcp::update::is_drifted(inst.as_deref(), Some(&reg)));
                    }
                }
            }
            servers = filter_servers_by_keywords(servers, &keyword);
            servers.sort_by(|a, b| match (a.installed, b.installed) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.id.cmp(&b.id),
            });

            for e in &errors {
                eprintln!("Warning: {}", e);
            }

            if json {
                let output = serde_json::to_string_pretty(&servers).unwrap();
                println!("{output}");
            } else {
                if servers.is_empty() && errors.is_empty() && url.is_none() {
                    println!(
                        "No registry sources configured. Add one with: dmcp sources add <url>"
                    );
                    return;
                }
                if servers.is_empty() {
                    println!("No servers found in registries.");
                    return;
                }
                print_browse_table(&servers);
            }
        }

        Commands::Count { json } => {
            let local_count = list_servers(&paths, true, true, false).len();
            let (registry_servers, _errors) = list_registry_servers(&paths, true, true);
            let registry_count = registry_servers.len();
            let total = local_count + registry_count;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "total": total,
                        "local": local_count,
                        "registry": registry_count,
                    }))
                    .unwrap()
                );
            } else {
                println!("{}", total);
            }
        }

        Commands::SyncIndex => match sync_index(&paths) {
            Ok(result) => {
                for warning in &result.errors {
                    eprintln!("{}", warning);
                }
                println!(
                    "Synced: {} server vectors, {} tool vectors indexed.",
                    result.servers_indexed, result.tools_indexed
                );
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Commands::EmbeddingSpec { json: _ } => {
            let index_path = paths.vector_index_path();
            let index = match VectorIndex::load(&index_path) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("Error loading vector index: {}", e);
                    std::process::exit(1);
                }
            };
            match index.embedding_spec {
                Some(spec) => {
                    println!("{}", serde_json::to_string_pretty(&spec).unwrap());
                }
                None => {
                    eprintln!("No embedding spec found. Run `dmcp sync-index` first.");
                    std::process::exit(1);
                }
            }
        }

        Commands::IndexServer {
            server_id,
            vectors: vectors_json,
            name,
            description,
        } => {
            let vectors_val: serde_json::Value = match serde_json::from_str(&vectors_json) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error: --vectors must be valid JSON: {}", e);
                    std::process::exit(1);
                }
            };

            let server_name = name.unwrap_or_else(|| server_id.clone());

            // When --description is omitted, fall back to @mcp.tool docstrings
            // found in the server's install directory.
            let (server_description, tool_doc_map) = if description.is_some() {
                (description, std::collections::HashMap::new())
            } else {
                let (desc, map) = resolve_doc_comment_descriptions(&paths, &server_id);
                (desc, map)
            };

            let index_path = paths.vector_index_path();
            let mut index = match VectorIndex::load(&index_path) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("Error loading vector index: {}", e);
                    std::process::exit(1);
                }
            };

            let mut new_entries: Vec<VectorEntry> = Vec::new();

            // Server-level vector
            if let Some(server_arr) = vectors_val.get("server").and_then(|v| v.as_array()) {
                let vec: Vec<f32> = server_arr
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                if !vec.is_empty() {
                    new_entries.push(VectorEntry {
                        server_id: server_id.clone(),
                        server_name: server_name.clone(),
                        server_description: server_description.clone(),
                        tool_name: None,
                        tool_description: None,
                        parameter_schema: None,
                        vector: vec,
                        source: "local".to_string(),
                        // A locally indexed server has no registry entry to
                        // vouch for it, so nothing restricts it — the same
                        // reading an absent `platforms` gets everywhere else.
                        platforms: None,
                        platforms_malformed: false,
                    });
                }
            }

            // Tool-level vectors
            if let Some(tools_obj) = vectors_val.get("tools").and_then(|v| v.as_object()) {
                for (tool_name, tool_vec_val) in tools_obj {
                    if let Some(arr) = tool_vec_val.as_array() {
                        let vec: Vec<f32> = arr
                            .iter()
                            .filter_map(|v| v.as_f64().map(|f| f as f32))
                            .collect();
                        if !vec.is_empty() {
                            // Use per-tool docstring when available
                            let tool_description = tool_doc_map.get(tool_name.as_str()).cloned();
                            new_entries.push(VectorEntry {
                                server_id: server_id.clone(),
                                server_name: server_name.clone(),
                                server_description: server_description.clone(),
                                tool_name: Some(tool_name.clone()),
                                tool_description,
                                parameter_schema: None,
                                vector: vec,
                                source: "local".to_string(),
                                platforms: None,
                                platforms_malformed: false,
                            });
                        }
                    }
                }
            }

            if new_entries.is_empty() {
                eprintln!("Error: no valid vectors found in --vectors JSON.");
                std::process::exit(1);
            }

            let n = new_entries.len();
            index.upsert_server_entries(&server_id, new_entries);

            if let Err(e) = index.save(&index_path) {
                eprintln!("Error saving vector index: {}", e);
                std::process::exit(1);
            }

            println!("Indexed {} vector(s) for {}.", n, server_id);
        }
    }
}

/// Whether `dmcp install <id>` must refuse this registry entry on this host.
///
/// Applied to the entry the CLI already fetched, before the scope is resolved
/// and therefore before any pkexec re-exec: a host the registry never vouched
/// for should not cost the operator a polkit authentication first, and the
/// refusal should not arrive from an elevated child. `install()` gates again —
/// this is a pre-elevation fast path, not a replacement for the choke point
/// library callers (`dmcp serve`, `update::refresh_install`) go through.
fn install_platform_refusal(
    server: &serde_json::Value,
    ignore_platform: bool,
) -> Option<dmcp::UnsupportedHost> {
    if ignore_platform {
        return None;
    }
    dmcp::platform::check_host(server)
}

fn parse_config(s: &str) -> Result<(String, String), String> {
    let s = s.trim();
    if let Some(eq) = s.find('=') {
        let k = s[..eq].trim().to_string();
        let v = s[eq + 1..].trim().to_string();
        if k.is_empty() {
            Err("config key cannot be empty".to_string())
        } else {
            Ok((k, v))
        }
    } else {
        Err("config must be key=value".to_string())
    }
}

fn format_tools(tools: &[serde_json::Value]) -> String {
    tools
        .iter()
        .map(|t| {
            if let Some(obj) = t.as_object() {
                obj.get("name").and_then(|n| n.as_str()).unwrap_or("?")
            } else {
                t.as_str().unwrap_or("?")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_transports(transports: &[dmcp::models::Transport]) -> String {
    transports
        .iter()
        .map(|t| match t {
            dmcp::models::Transport::Stdio { command, args, .. } => {
                let args_str = args.as_ref().map(|a| a.join(" ")).unwrap_or_default();
                format!("stdio ({command} {args_str})")
            }
            dmcp::models::Transport::Sse { url, .. } => format!("sse ({url})"),
            dmcp::models::Transport::WebSocket { ws_url, .. } => format!("websocket ({ws_url})"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn print_info_output(manifest: &dmcp::Manifest, scope_str: &str) {
    const INDENT: &str = "        ";

    println!("{}", manifest.id.as_deref().unwrap_or("?"));
    println!(
        "{}Name:        {}",
        INDENT,
        manifest.name.as_deref().unwrap_or("?")
    );
    println!(
        "{}Version:     {}",
        INDENT,
        manifest.version.as_deref().unwrap_or("?")
    );
    println!("{}Scope:       {}", INDENT, scope_str);
    if let Some(s) = manifest.summary.as_deref().filter(|x| !x.is_empty()) {
        println!("{}Summary:     {}", INDENT, s);
    }
    if let Some(d) = manifest.description.as_deref().filter(|x| !x.is_empty()) {
        println!("{}Description:", INDENT);
        for line in d.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                println!("{}{}{}", INDENT, INDENT, trimmed);
            }
        }
    }
    if let Some(a) = manifest.author.as_deref().filter(|x| !x.is_empty()) {
        println!("{}Author:      {}", INDENT, a);
    }
    if let Some(h) = manifest.homepage.as_deref().filter(|x| !x.is_empty()) {
        println!("{}Homepage:    {}", INDENT, h);
    }
    if !manifest.categories.is_empty() {
        println!("{}Categories:  {}", INDENT, manifest.categories.join(", "));
    }
    if !manifest.capabilities.is_empty() {
        println!(
            "{}Capabilities: {}",
            INDENT,
            manifest.capabilities.join(", ")
        );
    }
    if !manifest.tools.is_empty() {
        println!("{}Tools:       {}", INDENT, format_tools(&manifest.tools));
    }
    if let Some(ref t) = manifest.transports {
        println!("{}Transports:  {}", INDENT, format_transports(t));
    }
    if let Some(ref dir) = manifest.install_dir {
        println!("{}Install:     {}", INDENT, dir);
    }
    if !manifest.config.is_empty() {
        for (k, v) in &manifest.config {
            let val: String = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            println!("{}Config.{}:   {}", INDENT, k, val);
        }
    }
    if let Some(ref s) = manifest.setup_script {
        println!("{}Setup:      {}", INDENT, s);
    }
    if let Some(ref s) = manifest.setup_script_windows {
        println!("{}Setup (windows): {}", INDENT, s);
    }
}

fn print_list_table(servers: &[dmcp::ServerInfo]) {
    const INDENT: &str = "        ";

    for s in servers {
        let scope = match s.scope {
            dmcp::discovery::Scope::User => "user",
            dmcp::discovery::Scope::System => "system",
        };
        println!("{}", s.id);
        println!("{}Name:      {}", INDENT, s.name);
        println!("{}Version:   {}", INDENT, s.version);
        println!("{}Transport: {}", INDENT, s.transport_type);
        println!("{}Scope:     {}", INDENT, scope);
        println!("{}Manifest: {}", INDENT, s.manifest_path);
        println!();
    }
}

fn print_session_table(sessions: &[dmcp::broker::SessionListItem]) {
    println!(
        "{:<16} {:<8} {:>8} {:>8} SERVER",
        "SESSION", "SCOPE", "AGE(s)", "IDLE(s)"
    );
    println!("{}", "-".repeat(80));
    for s in sessions {
        println!(
            "{:<16} {:<8} {:>8} {:>8} {}",
            s.session_id, s.scope, s.age_secs, s.idle_secs, s.server_id
        );
    }
}

fn print_vector_results(results: &[dmcp::SearchResult]) {
    const INDENT: &str = "        ";
    for r in results {
        println!("{}", r.server_id);
        println!("{}Server:  {}", INDENT, r.server_name);
        if let Some(ref desc) = r.server_description {
            if !desc.is_empty() {
                println!(
                    "{}Summary: {}",
                    INDENT,
                    desc.lines().next().unwrap_or("").trim()
                );
            }
        }
        if let Some(ref tool) = r.tool_name {
            println!("{}Tool:    {}", INDENT, tool);
        }
        if let Some(ref desc) = r.tool_description {
            if !desc.is_empty() {
                println!(
                    "{}Tool desc: {}",
                    INDENT,
                    desc.lines().next().unwrap_or("").trim()
                );
            }
        }
        if let Some(ref schema) = r.parameter_schema {
            println!("{}Params:  {}", INDENT, schema);
        }
        if r.platforms_malformed {
            println!(
                "{}Platforms: unreadable declaration — UNSUPPORTED on this host ({}); \
                 install needs --ignore-platform",
                INDENT,
                dmcp::host_platform()
            );
        } else if let Some(ref platforms) = r.platforms {
            if r.unsupported_on_host {
                println!(
                    "{}Platforms: {} — UNSUPPORTED on this host ({}); install needs --ignore-platform",
                    INDENT,
                    platforms.join(", "),
                    dmcp::host_platform()
                );
            } else {
                println!("{}Platforms: {}", INDENT, platforms.join(", "));
            }
        }
        println!("{}Score:   {:.4}", INDENT, r.score);
        println!();
    }
}

fn print_drift_check(assessed: &[dmcp::AssessedServer]) {
    let mut actionable = false;
    for a in assessed {
        let id = &a.report.id;
        if !a.in_registry {
            println!("{}: not in registry", id);
        } else if a.revoked {
            println!(
                "{}: REVOKED (removed from registry) — uninstall advised",
                id
            );
            actionable = true;
        } else if a.report.update_available {
            println!("{}: update available", id);
            println!(
                "        installed: {}",
                a.report.installed_hash.as_deref().unwrap_or("(none)")
            );
            println!(
                "        registry:  {}",
                a.report.registry_hash.as_deref().unwrap_or("(none)")
            );
            actionable = true;
        } else {
            println!("{}: up to date", id);
        }
        if a.report.unsupported_on_host {
            if a.report.platforms_malformed {
                println!(
                    "        platforms: unreadable declaration — refresh needs --ignore-platform"
                );
            } else {
                println!(
                    "        platforms: {} (not {}) — refresh needs --ignore-platform",
                    a.report.platforms.as_deref().unwrap_or_default().join(", "),
                    dmcp::host_platform()
                );
            }
        }
    }
    if !actionable {
        println!("All installed servers are up to date.");
    }
}

fn print_browse_table(servers: &[dmcp::RegistryServer]) {
    const INDENT: &str = "        ";

    for s in servers {
        let status = if s.installed {
            if s.update_available == Some(true) {
                "INSTALLED (update available)"
            } else {
                "INSTALLED"
            }
        } else {
            "NOT INSTALLED"
        };
        println!("{}", s.id);
        println!("{}Name:      {}", INDENT, s.name);
        println!("{}Version:   {}", INDENT, s.version);
        println!("{}Transport: {}", INDENT, s.transport);
        println!("{}Status:    {}", INDENT, status);
        if s.platforms_malformed {
            println!(
                "{}Platforms: unreadable declaration — UNSUPPORTED on this host ({}); \
                 install needs --ignore-platform",
                INDENT,
                dmcp::host_platform()
            );
        } else if let Some(ref platforms) = s.platforms {
            if s.unsupported_on_host {
                println!(
                    "{}Platforms: {} — UNSUPPORTED on this host ({}); install needs --ignore-platform",
                    INDENT,
                    platforms.join(", "),
                    dmcp::host_platform()
                );
            } else {
                println!("{}Platforms: {}", INDENT, platforms.join(", "));
            }
        }
        if !s.summary.is_empty() {
            println!(
                "{}Summary:   {}",
                INDENT,
                s.summary.lines().next().unwrap_or("").trim()
            );
        }
        println!("{}Source:    {}", INDENT, s.source);
        println!();
    }
}

/// For the `index-server` command: when `--description` is not provided,
/// look up the server's install directory and parse `@mcp.tool` docstrings.
///
/// Returns `(server_description, tool_name -> docstring map)`.
fn resolve_doc_comment_descriptions(
    paths: &dmcp::Paths,
    server_id: &str,
) -> (Option<String>, std::collections::HashMap<String, String>) {
    let install_dir = match dmcp::get_manifest_path(paths, server_id) {
        Some(p) => match p.parent().map(|p| p.to_path_buf()) {
            Some(d) => d,
            None => return (None, std::collections::HashMap::new()),
        },
        None => return (None, std::collections::HashMap::new()),
    };

    let tool_docs = dmcp::extract_tool_docs(&install_dir);
    let server_desc = dmcp::first_description(&tool_docs);
    let map: std::collections::HashMap<String, String> = tool_docs
        .into_iter()
        .filter_map(|d| d.docstring.map(|ds| (d.tool_name, ds)))
        .collect();

    (server_desc, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A platform that is never the host, so the refusal paths are exercised
    /// the same way whichever OS the suite runs on.
    fn foreign_platform() -> &'static str {
        if dmcp::host_platform() == "linux" {
            "windows"
        } else {
            "linux"
        }
    }

    fn system_entry(platforms: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "com.example.mcp.thing",
            "scope": "system",
            "trustStatus": "official",
            "platforms": platforms,
            "transports": [{"type": "stdio", "command": "thing"}],
        })
    }

    /// The refusal is decided from the already-fetched entry, so `install`'s
    /// system scope never gets as far as asking for elevation.
    #[test]
    fn a_system_scoped_entry_is_refused_before_the_scope_is_resolved() {
        let entry = system_entry(serde_json::json!([foreign_platform()]));
        let refusal =
            install_platform_refusal(&entry, false).expect("an unvouched host must be refused");
        assert!(refusal.to_string().contains(foreign_platform()));
        assert_eq!(
            dmcp::scope_from_registry_server(&entry),
            dmcp::discovery::Scope::System,
            "the refused entry is exactly the kind that would have prompted"
        );
    }

    #[test]
    fn a_malformed_declaration_is_refused_the_same_way() {
        let entry = system_entry(serde_json::json!("windows"));
        let refusal = install_platform_refusal(&entry, false).expect("malformed must be refused");
        assert!(refusal.malformed);
    }

    #[test]
    fn a_vouched_host_and_the_override_both_pass() {
        let vouched = system_entry(serde_json::json!([dmcp::host_platform()]));
        assert!(install_platform_refusal(&vouched, false).is_none());

        let foreign = system_entry(serde_json::json!([foreign_platform()]));
        assert!(
            install_platform_refusal(&foreign, true).is_none(),
            "--ignore-platform is the way past the gate"
        );

        let undeclared = system_entry(serde_json::Value::Null);
        assert!(install_platform_refusal(&undeclared, false).is_none());
    }
}
