//! The staging folder a `--service` Core takes firmware from (decision 160).
//!
//! A service Core runs under its own account, so a `firmware_path` it opened
//! as given would read anything that account can, its signing key included.
//! [`read`] is the one check every route that takes a path goes through: the
//! name must be one plain file name directly under `<data dir>/staging/`, and
//! a symlink, a hardlink, a junction or any other reparse point, a nested
//! name, a `..` and an absolute path outside the folder are each refused. A
//! nested name is refused because no-follow covers only a path's last
//! component, and a folder swapped for a link after its check would be
//! followed; with one component, the bytes are read from the handle that
//! passed the check, so a link swapped in afterwards is never followed.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The staging folder's name under the data directory.
pub const STAGING_DIR: &str = "staging";

/// `<data dir>/staging`, the folder the elevated install creates.
pub fn dir() -> anyhow::Result<PathBuf> {
    Ok(crate::token_store::local_data_dir()?.join(STAGING_DIR))
}

/// Why a name was refused; the text goes to the caller as a `400`.
fn refuse(name: &str, why: &str) -> String {
    format!(
        "'{name}' is refused: {why}. A service Core takes firmware as uploaded bytes or from its staging folder only"
    )
}

/// True when `meta` is a symlink, junction or any other reparse point.
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Opens the file without following a final link.
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

/// Hardlink count of an open file.
fn link_count(file: &File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        // SAFETY: `info` is plain data the call fills in; the handle is open for the call.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(u64::from(info.nNumberOfLinks))
    }
}

/// Reads the file `name` names inside `root`, or says why it is refused.
/// `name` is relative to `root`, or absolute and inside it.
pub fn read(root: &Path, name: &str) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| {
        format!(
            "the staging folder {} is not usable ({e}); a service Core never creates it, the elevated install does",
            root.display()
        )
    })?;
    if is_link(&meta) || !meta.is_dir() {
        return Err(format!("the staging folder {} is a link or not a folder", root.display()));
    }
    let given = Path::new(name);
    let relative = if given.is_absolute() {
        given.strip_prefix(root).map_err(|_| refuse(name, "it is outside the staging folder"))?
    } else {
        given
    };
    let mut parts = relative.components();
    let (Some(Component::Normal(part)), None) = (parts.next(), parts.next()) else {
        return Err(refuse(
            name,
            "only one plain file name directly under the staging folder is taken (no folders, no `..`)",
        ));
    };
    let path = root.join(part);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("'{name}' cannot be read from the staging folder: {e}"))?;
    if is_link(&meta) {
        return Err(refuse(name, "it is a link"));
    }
    let mut file = open_nofollow(&path)
        .map_err(|e| format!("'{name}' cannot be read from the staging folder: {e}"))?;
    let opened = file.metadata().map_err(|e| format!("'{name}': {e}"))?;
    if !opened.is_file() || is_link(&opened) {
        return Err(refuse(name, "it is not a plain file"));
    }
    if link_count(&file).map_err(|e| format!("'{name}': {e}"))? > 1 {
        return Err(refuse(name, "it is a hardlink"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| format!("'{name}' cannot be read: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.hex"), b"ok").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.bin"), b"sub").unwrap();
        dir
    }

    #[test]
    fn plain_files_inside_are_read() {
        let d = staged();
        assert_eq!(read(d.path(), "app.hex").unwrap(), b"ok");
        let abs = d.path().join("app.hex");
        assert_eq!(read(d.path(), abs.to_str().unwrap()).unwrap(), b"ok");
    }

    #[test]
    fn dotdot_and_outside_absolute_are_refused() {
        let d = staged();
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(read(d.path(), "../x").unwrap_err().contains("refused"));
        assert!(read(d.path(), "sub/../app.hex").unwrap_err().contains("refused"));
        assert!(read(d.path(), outside.path().to_str().unwrap()).unwrap_err().contains("outside"));
    }

    #[test]
    fn nested_names_are_refused() {
        let d = staged();
        let nested = d.path().join("sub/a.bin");
        for name in ["sub/a.bin", "./sub/a.bin", nested.to_str().unwrap()] {
            assert!(read(d.path(), name).unwrap_err().contains("one plain file name"), "{name}");
        }
    }

    #[test]
    fn a_missing_folder_is_refused_not_created() {
        let d = staged();
        let missing = d.path().join("nope");
        assert!(read(&missing, "a").unwrap_err().contains("never creates"));
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_refused() {
        let d = staged();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), d.path().join("sym")).unwrap();
        assert!(read(d.path(), "sym").unwrap_err().contains("link"));
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), d.path().join("dirlink")).unwrap();
        assert!(read(d.path(), "dirlink/x").unwrap_err().contains("refused"));
        std::fs::hard_link(d.path().join("app.hex"), d.path().join("hard")).unwrap();
        assert!(read(d.path(), "hard").unwrap_err().contains("hardlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_staging_folder_is_refused() {
        let d = staged();
        let parent = tempfile::tempdir().unwrap();
        let via = parent.path().join("staging");
        std::os::unix::fs::symlink(d.path(), &via).unwrap();
        assert!(read(&via, "app.hex").unwrap_err().contains("link"));
    }
}
