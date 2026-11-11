//! Resolve a launch command against `PATH` and `PATHEXT`.
//!
//! On Windows `npm`, `npx`, `uv` and `uvx` are `.cmd` shims. `CreateProcess`
//! appends `.exe` to a bare name but never `.cmd` or `.bat`, so spawning `npx`
//! fails with "program not found" on a machine where it is installed. The
//! resolution here is a pure function of the command, the two variables and a
//! file-existence probe, so it is tested on every host; only the call site in
//! `call.rs` is Windows-only.
//!
//! The result is spawned directly. Rust's `Command` runs a `.cmd`/`.bat`
//! through `cmd.exe` itself and refuses arguments it cannot escape safely, so
//! no `cmd /c` string is built here.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Extensions tried when `PATHEXT` is unset or empty.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

#[derive(Debug, PartialEq, Eq)]
pub struct NotFound {
    pub command: String,
    pub searched: Vec<PathBuf>,
    pub extensions: Vec<String>,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' was not found on PATH ({} directories searched) with extensions {}",
            self.command,
            self.searched.len(),
            self.extensions.join(";")
        )
    }
}

fn parse_extensions(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            if e.starts_with('.') {
                e.to_string()
            } else {
                format!(".{e}")
            }
        })
        .collect()
}

/// The `PATHEXT` list, or the defaults when it is unset or holds no entries.
fn extensions(pathext: Option<&OsStr>) -> Vec<String> {
    let parsed = pathext
        .map(|v| parse_extensions(&v.to_string_lossy()))
        .unwrap_or_default();
    if parsed.is_empty() {
        parse_extensions(DEFAULT_PATHEXT)
    } else {
        parsed
    }
}

/// Candidate file names for `name` in one directory, in the order tried: the
/// name itself when it already carries an extension, then each `PATHEXT` entry.
fn candidates(name: &str, exts: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if Path::new(name).extension().is_some() {
        out.push(name.to_string());
    }
    out.extend(exts.iter().map(|e| format!("{name}{e}")));
    out
}

/// Resolve `command` to the file that would run.
///
/// A command containing a path separator is looked up only at that path; a bare
/// name is looked up in each `PATH` directory in order. `exists` reports whether
/// a candidate is a file.
pub fn resolve_command(
    command: &str,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, NotFound> {
    let exts = extensions(pathext);
    let has_separator = command.contains('/') || command.contains('\\');

    let dirs: Vec<PathBuf> = if has_separator {
        vec![PathBuf::new()]
    } else {
        path_var
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default()
    };

    let mut searched = Vec::new();
    for dir in &dirs {
        // A path with separators is split into its directory and file name so
        // the same candidate rule applies to both forms.
        let (dir, name) = if has_separator {
            let p = Path::new(command);
            (
                p.parent().map(Path::to_path_buf).unwrap_or_default(),
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        } else {
            (dir.clone(), command.to_string())
        };
        for candidate in candidates(&name, &exts) {
            let full = dir.join(candidate);
            if exists(&full) {
                return Ok(full);
            }
        }
        searched.push(dir);
    }
    Err(NotFound {
        command: command.to_string(),
        searched,
        extensions: exts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;

    fn files(names: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = names.iter().map(PathBuf::from).collect();
        move |p| set.contains(p)
    }

    fn path(dirs: &[&str]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn a_bare_name_resolves_to_a_cmd_shim() {
        let p = path(&["/bin", "/node"]);
        let got = resolve_command(
            "npx",
            Some(&p),
            Some(OsStr::new(".COM;.EXE;.BAT;.CMD")),
            files(&["/node/npx.CMD"]),
        );
        assert_eq!(got, Ok(PathBuf::from("/node/npx.CMD")));
    }

    #[test]
    fn the_first_path_directory_wins() {
        let p = path(&["/a", "/b"]);
        let got = resolve_command("uv", Some(&p), None, files(&["/b/uv.EXE", "/a/uv.CMD"]));
        assert_eq!(got, Ok(PathBuf::from("/a/uv.CMD")));
    }

    #[test]
    fn pathext_order_decides_within_a_directory() {
        let p = path(&["/d"]);
        let got = resolve_command(
            "tool",
            Some(&p),
            Some(OsStr::new(".CMD;.EXE")),
            files(&["/d/tool.EXE", "/d/tool.CMD"]),
        );
        assert_eq!(got, Ok(PathBuf::from("/d/tool.CMD")));
    }

    #[test]
    fn a_name_with_an_extension_is_tried_as_given_first() {
        let p = path(&["/d"]);
        let got = resolve_command("npx.cmd", Some(&p), None, files(&["/d/npx.cmd"]));
        assert_eq!(got, Ok(PathBuf::from("/d/npx.cmd")));
    }

    #[test]
    fn an_unset_or_empty_pathext_falls_back_to_the_defaults() {
        let p = path(&["/d"]);
        for pathext in [None, Some(OsStr::new("")), Some(OsStr::new(" ; "))] {
            let got = resolve_command("npm", Some(&p), pathext, files(&["/d/npm.CMD"]));
            assert_eq!(got, Ok(PathBuf::from("/d/npm.CMD")), "{pathext:?}");
        }
    }

    #[test]
    fn a_pathext_entry_without_a_dot_is_normalised() {
        let p = path(&["/d"]);
        let got = resolve_command("x", Some(&p), Some(OsStr::new("CMD")), files(&["/d/x.CMD"]));
        assert_eq!(got, Ok(PathBuf::from("/d/x.CMD")));
    }

    #[test]
    fn a_command_with_a_separator_ignores_path() {
        let p = path(&["/d"]);
        let got = resolve_command(
            "/opt/tool/run",
            Some(&p),
            None,
            files(&["/d/run.EXE", "/opt/tool/run.CMD"]),
        );
        assert_eq!(got, Ok(PathBuf::from("/opt/tool/run.CMD")));
    }

    #[test]
    fn a_miss_names_what_was_searched() {
        let p = path(&["/a", "/b"]);
        let err = resolve_command("ghost", Some(&p), None, files(&[])).unwrap_err();
        assert_eq!(err.searched, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        let msg = err.to_string();
        assert!(
            msg.contains("'ghost'") && msg.contains("2 directories"),
            "{msg}"
        );
        assert!(msg.contains(".CMD"), "{msg}");
    }

    #[test]
    fn no_path_variable_is_a_miss_not_a_panic() {
        let err = resolve_command("npx", None, None, files(&[])).unwrap_err();
        assert!(err.searched.is_empty());
    }
}
