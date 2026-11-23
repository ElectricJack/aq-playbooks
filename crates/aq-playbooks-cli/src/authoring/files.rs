use super::*;
use std::io::{Read, Write};
pub(super) fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("the current directory cannot be read")?;
    if is_workspace(&cwd) {
        Ok(cwd)
    } else {
        bail!(
            "{}: this is not an authoring workspace: it has no {RECORD_FILE}. The authoring commands run only in the workspace directory itself (the one holding {SOURCE_FILE}, {RECORD_FILE}, {CATALOG_FILE}, {FACTS_FILE} and {DRAFT_FILE}), not in a subdirectory of it. Change to that directory and run the command again.",
            cwd.display()
        )
    }
}
pub(super) fn is_workspace(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(RECORD_FILE)).is_ok_and(|m| m.is_file())
}
pub(super) fn exists(root: &Path, given: &str) -> bool {
    fs::symlink_metadata(root.join(given)).is_ok()
}

/// `given` inside `root`: relative, without `..`, and through no symbolic
/// link at any depth.
pub(super) fn inside(root: &Path, given: &str) -> Result<PathBuf> {
    let refuse = || {
        anyhow!(
            "{given}: only a path inside the workspace, relative to it, is accepted (such as cases/bugfix.json); absolute paths and `..` are refused."
        )
    };
    let path = Path::new(given);
    if path.is_absolute() {
        return Err(refuse());
    }
    let mut at = root.to_path_buf();
    let mut named = false;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                at.push(name);
                named = true;
                if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!(
                        "{given}: {} is a symbolic link; the workspace accepts only its own files and directories, not links. Write a regular file instead.",
                        at.strip_prefix(root).unwrap_or(&at).display()
                    );
                }
            }
            Component::CurDir => {}
            _ => return Err(refuse()),
        }
    }
    if named { Ok(at) } else { Err(refuse()) }
}

/// A regular file of the workspace, at most `max` bytes, as UTF-8: checked
/// before it is opened, and the opened file checked again.
pub(super) fn read_inside(root: &Path, given: &str, max: u64) -> Result<String> {
    let path = inside(root, given)?;
    let checked = fs::symlink_metadata(&path)
        .map_err(|e| anyhow!("{given}: cannot be read from the workspace ({e})."))?;
    anyhow::ensure!(checked.is_file(), "{given} is not a regular file.");
    let file = fs::File::open(&path).map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    let opened = file
        .metadata()
        .map_err(|e| anyhow!("{given}: cannot be read ({e})."))?;
    anyhow::ensure!(opened.is_file(), "{given} is not a regular file.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            (opened.dev(), opened.ino()) == (checked.dev(), checked.ino()),
            "{given} was replaced while it was being read; write it once and run again."
        );
        anyhow::ensure!(
            opened.nlink() == 1,
            "{given} has other hard links; the workspace accepts only files of its own. Write it as a new file."
        );
    }
    let too_big = || anyhow!("{given} is larger than {max} bytes; keep it under that.");
    if opened.len() > max {
        return Err(too_big());
    }
    let mut bytes = vec![];
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{given} is not UTF-8 text."))
}

/// Writes `bytes` to `given` inside `root` whole: to a new file beside it,
/// then renamed over it, so a reader never sees half a file and a link at
/// the target is replaced, never followed.
pub(super) fn write_inside(root: &Path, given: &str, bytes: &[u8]) -> Result<()> {
    let path = inside(root, given)?;
    let parent = path.parent().unwrap_or(root).to_path_buf();
    let mut at = root.to_path_buf();
    for component in parent
        .strip_prefix(root)
        .unwrap_or(Path::new(""))
        .components()
    {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{given}: {} is not a directory.", at.display()),
            Err(_) => fs::create_dir(&at)
                .map_err(|e| anyhow!("{given}: cannot create {} ({e}).", at.display()))?,
        }
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{given}: names no file."))?
        .to_string_lossy()
        .to_string();
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| anyhow!("{given}: cannot be written ({e})."))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        anyhow!("{given}: cannot be written ({e}).")
    })
}
