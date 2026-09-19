//! Which directory `tria open` means, and what the project for it is called.
//!
//! A project on the server is a directory, known by the path of it. The server takes a
//! path as given beyond expanding `~`, making it absolute and resolving `.` and `..` by
//! name — it does not follow symlinks — so the same is done here, and a project is found
//! by matching the two strings.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// The directory a `tria open` names, as the server would write it. `None` is the
/// directory tria was run in.
pub fn root(arg: Option<&str>) -> Result<String> {
    let here = std::env::current_dir().context("no working directory")?;
    let path = match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        None => here,
        Some(arg) => here.join(expand_home(arg)?),
    };
    let path = resolve(&path);
    let shown = path.display().to_string();
    let found = std::fs::metadata(&path).with_context(|| format!("no directory {shown}"))?;
    anyhow::ensure!(found.is_dir(), "{shown} is not a directory");
    Ok(shown)
}

/// `~` and `~/…`, which a shell would have expanded had it not been quoted.
fn expand_home(arg: &str) -> Result<PathBuf> {
    let rest = match arg.strip_prefix('~') {
        None => return Ok(PathBuf::from(arg)),
        Some(rest) => rest.strip_prefix('/').unwrap_or(rest),
    };
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(rest))
}

/// `.` and `..` taken out by name rather than by asking the disk, which is what leaves a
/// path through a symlink saying what it was given rather than where it came out. The
/// path is an absolute one: it has been joined onto the working directory first.
fn resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            // Above the root is the root again, which is where the server's own
            // resolving of a path leaves it.
            Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The top of the checkout a directory is in, where it is in one. A project is a whole
/// checkout far more often than it is one directory inside one, so a directory with no
/// project of its own is opened as the checkout it belongs to.
pub fn checkout_root(path: &str) -> Option<String> {
    let mut at = Path::new(path);
    loop {
        // A worktree's `.git` is a file rather than a directory, and is still the top.
        if at.join(".git").exists() {
            return Some(at.display().to_string());
        }
        at = at.parent()?;
    }
}

/// What a project for this directory is called: the name of it, as the server names one.
pub fn title(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        "project".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path is made absolute and tidied by name. Following symlinks would be tidier
    /// still and would be wrong: the server does not, and a project is found by the path
    /// string the two of them agree on.
    #[test]
    fn a_path_is_resolved_the_way_the_server_resolves_one() {
        assert_eq!(resolve(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(resolve(Path::new("/../a")), PathBuf::from("/a"));
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/src").unwrap(), home.join("src"));
        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("/tmp/x").unwrap(), PathBuf::from("/tmp/x"));
    }

    /// A relative path is relative to where tria was run, and a directory that is not
    /// there is said so before the screen is taken over.
    #[test]
    fn the_directory_has_to_be_one() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(root(None).unwrap(), here.display().to_string());
        assert_eq!(root(Some(" . ")).unwrap(), here.display().to_string());
        assert_eq!(root(Some("/")).unwrap(), "/");
        let said = root(Some("/no/such/place")).unwrap_err().to_string();
        assert!(said.contains("/no/such/place"), "{said}");
        let file = here.join("Cargo.toml");
        let said = root(Some(file.to_str().unwrap())).unwrap_err().to_string();
        assert!(said.contains("not a directory"), "{said}");
    }

    /// A directory inside a checkout is opened as the checkout: the top of this one is
    /// the repository tria itself is in.
    #[test]
    fn the_checkout_a_directory_belongs_to_is_the_top_of_it() {
        let here = std::env::current_dir().unwrap();
        let src = here.join("src");
        assert_eq!(
            checkout_root(src.to_str().unwrap()),
            Some(here.display().to_string())
        );
        assert_eq!(checkout_root("/"), None);
        assert_eq!(title("/a/b/tria"), "tria");
        assert_eq!(title("/"), "project");
    }
}
