//! Writing the files that hold server credentials.
//!
//! `installed/<id>/manifest.json` is dmcp's per-server credential store: the
//! `config` block in it becomes the server's environment at spawn
//! (`run::config_to_env`), so an API token sits there in plaintext. Two
//! properties every writer needs, and neither of which `fs::write` provides:
//!
//! - **Owner-only, from the first byte.** On unix a user-scope manifest is
//!   created mode 0600 *before* any content reaches it, and its directories are
//!   created 0700, so a secret is never world-readable — not even for the width
//!   of a later chmod. System-scope files keep today's modes: that tree is
//!   documented as visible to every user on the machine and `dmcp list` reads
//!   its manifests unprivileged.
//! - **Whole file or nothing.** Content goes to a temp file beside the target
//!   and is renamed over it, so a failed or interrupted write leaves the
//!   previous manifest intact instead of a truncated one — which for a manifest
//!   holding the only copy of a token is the difference between a retry and a
//!   lost credential.
//!
//! Modes are applied where dmcp *creates* things. Nothing here re-chmods a
//! directory that already exists: a mode someone deliberately widened is theirs
//! to keep.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::discovery::Scope;
use crate::paths::Paths;

/// Who may read what dmcp writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readers {
    /// The owning user only. User scope: dmcp owns the whole path, and the
    /// manifest there is a credential store with no second reader.
    OwnerOnly,
    /// Every local account, exactly as before this module existed. The system
    /// tree is documented as "visible to every user on the machine" and the
    /// unprivileged read paths (`list`, `info`, `tools`) load its manifests, so
    /// narrowing it would hide every system-scope server from everyone but root.
    Everyone,
}

impl Readers {
    /// The audience a scope implies.
    pub fn for_scope(scope: Scope) -> Self {
        match scope {
            Scope::User => Readers::OwnerOnly,
            Scope::System => Readers::Everyone,
        }
    }

    /// The audience for a manifest identified by path rather than by scope —
    /// the shape `config set` and `dmcp setup` are in, having resolved an id
    /// through the index without learning which tree it landed in.
    pub fn for_manifest_path(paths: &Paths, manifest_path: &Path) -> Self {
        if manifest_path.starts_with(paths.system_install_dir()) {
            Readers::Everyone
        } else {
            Readers::OwnerOnly
        }
    }
}

/// Write `contents` to `path` as one indivisible replacement: into a temp file
/// in the same directory (so the rename cannot cross a filesystem boundary),
/// with the mode set at creation, then renamed over the target. On any failure
/// the temp file is removed and the error propagates with `path` untouched.
pub fn write_manifest_atomic(path: &Path, contents: &[u8], readers: Readers) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = dir.join(temp_name(path));

    match write_temp(&temp, contents, readers).and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave a copy of the secret behind under a name nothing
            // reads and nothing cleans up.
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Create an install directory and any missing parent, with the mode `readers`
/// implies. An existing directory is left exactly as it is.
#[cfg(unix)]
pub fn create_install_dir(dir: &Path, readers: Readers) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    if readers == Readers::OwnerOnly {
        // Applies to every directory this call creates — the per-server dir and
        // any missing ancestor, `installed/` included — and to none it finds.
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(not(unix))]
pub fn create_install_dir(dir: &Path, _readers: Readers) -> io::Result<()> {
    // No mode to set: a directory under the user profile inherits ACLs that
    // already scope it to that user. Anything stricter is an installer concern.
    std::fs::create_dir_all(dir)
}

fn write_temp(temp: &Path, contents: &[u8], readers: Readers) -> io::Result<()> {
    use std::io::Write;

    let mut file = create_temp_file(temp, readers)?;
    file.write_all(contents)?;
    // The rename only publishes what reached the disk: without this a crash
    // between the two can leave the target renamed onto empty content.
    file.sync_all()
}

#[cfg(unix)]
fn create_temp_file(temp: &Path, readers: Readers) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let private = readers == Readers::OwnerOnly;
    let mode = if private { 0o600 } else { 0o666 };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(temp)?;
    if private {
        // O_CREAT's mode is masked by the umask, so the open above guarantees
        // only "no wider than 0600". Set it explicitly so the guarantee is the
        // mode itself. The shared case keeps taking the umask, so system-scope
        // files land exactly where `fs::write` used to put them.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    Ok(file)
}

#[cfg(not(unix))]
fn create_temp_file(temp: &Path, _readers: Readers) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
}

/// A sibling name unique per process and per call, so two writers racing on the
/// same manifest cannot pick the same temp. `create_new` turns any collision
/// that survives that — a temp orphaned by a killed process — into an error
/// rather than a clobber, and never follows a symlink planted at the name.
fn temp_name(path: &Path) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "manifest.json".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        ".{}.{}.{}{}.tmp",
        file,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        root: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "dmcp-manifest-io-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&root).unwrap();
            TempDir { root }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn siblings(dir: &Path, target: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != target)
            .collect()
    }

    #[test]
    fn a_write_replaces_the_target_and_leaves_no_sibling() {
        let dir = TempDir::new();
        let target = dir.root.join("manifest.json");
        std::fs::write(&target, b"old").unwrap();

        write_manifest_atomic(&target, b"new", Readers::OwnerOnly).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(
            siblings(&dir.root, "manifest.json").is_empty(),
            "the temp file must not outlive the rename"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_owner_only_and_a_shared_one_is_not_narrowed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let private = dir.root.join("private.json");
        let shared = dir.root.join("shared.json");

        write_manifest_atomic(&private, b"{}", Readers::OwnerOnly).unwrap();
        write_manifest_atomic(&shared, b"{}", Readers::Everyone).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&private), 0o600);
        assert_eq!(
            mode(&shared) & 0o044,
            0o044,
            "a system-scope manifest must stay readable to the users who list it"
        );
    }

    /// The failure this exists to prevent: a write that cannot be completed
    /// must not consume the file it was replacing. `fs::write` truncates first
    /// and only then discovers it cannot finish; this fails before the target
    /// is touched at all.
    #[cfg(unix)]
    #[test]
    fn a_write_that_cannot_complete_leaves_the_previous_manifest_intact() {
        use std::os::unix::fs::PermissionsExt;

        // root ignores directory permissions, so the setup below would not fail
        // and the assertion would prove nothing.
        if nix::unistd::Uid::current().is_root() {
            return;
        }

        let dir = TempDir::new();
        let target = dir.root.join("manifest.json");
        let original = br#"{"config":{"GITHUB_TOKEN":"ghp_secret"}}"#;
        std::fs::write(&target, original).unwrap();

        // Readable and traversable but not writable: the target file itself is
        // still writable, so this is exactly the case an in-place rewrite would
        // sail through and truncate.
        std::fs::set_permissions(&dir.root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let err = write_manifest_atomic(&target, b"{}", Readers::OwnerOnly).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        assert_eq!(
            std::fs::read(&target).unwrap(),
            original,
            "the credential the write could not replace must still be there"
        );

        std::fs::set_permissions(&dir.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            siblings(&dir.root, "manifest.json").is_empty(),
            "a failed write must not leave a partial copy of the secret behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_created_install_dir_is_private_and_an_existing_one_is_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let nested = dir.root.join("installed/com.example.server");
        create_install_dir(&nested, Readers::OwnerOnly).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(
            mode(&dir.root.join("installed")),
            0o700,
            "every directory the call creates gets the mode, not just the leaf"
        );

        // Deliberately widened by the user, then re-created: dmcp must not
        // fight a mode it did not set.
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_install_dir(&nested, Readers::OwnerOnly).unwrap();
        assert_eq!(mode(&nested), 0o755);
    }

    #[test]
    fn the_audience_follows_the_scope_and_the_tree() {
        assert_eq!(Readers::for_scope(Scope::User), Readers::OwnerOnly);
        assert_eq!(Readers::for_scope(Scope::System), Readers::Everyone);

        let paths = Paths {
            user_sources: std::path::PathBuf::from("/home/u/.config/mcp/sources.list"),
            user_install_dir: std::path::PathBuf::from("/home/u/.local/share/mcp/installed"),
            system_sources: std::path::PathBuf::from("/etc/mcp/sources.list"),
            system_install_dir: std::path::PathBuf::from("/usr/share/mcp/installed/"),
            vector_index_dir: std::path::PathBuf::from("/home/u/.local/share/mcp/vector_index"),
        };
        assert_eq!(
            Readers::for_manifest_path(
                &paths,
                Path::new("/home/u/.local/share/mcp/installed/x/manifest.json")
            ),
            Readers::OwnerOnly
        );
        assert_eq!(
            Readers::for_manifest_path(
                &paths,
                Path::new("/usr/share/mcp/installed/x/manifest.json")
            ),
            Readers::Everyone,
            "a trailing slash on the configured system dir must not change the answer"
        );
    }
}
