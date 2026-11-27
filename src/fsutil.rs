//! Small filesystem helpers shared by the snapshot and config code.

use anyhow::{Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Remove a file or a whole directory tree. A path that does not exist is not an error.
pub fn remove_any(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("Không đọc được {}", path.display())),
    };
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("Không xoá được {}", path.display())),
    }
}

/// Copy a file or a directory tree to `dst`, creating parent directories as needed.
/// Symbolic links are skipped: browser storage never contains any, and following one
/// could copy data from outside the source tree.
pub fn copy_any(src: &Path, dst: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(src).with_context(|| format!("Không đọc được {}", src.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir_all(dst).with_context(|| format!("Không tạo được {}", dst.display()))?;
        // Keep the source's permissions (Claude keeps its data owner-only on Unix).
        let _ = fs::set_permissions(dst, meta.permissions());
        let entries =
            fs::read_dir(src).with_context(|| format!("Không đọc được {}", src.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("Không đọc được {}", src.display()))?;
            copy_any(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Không tạo được {}", parent.display()))?;
        }
        fs::copy(src, dst).with_context(|| {
            format!(
                "Không sao chép được {} sang {}",
                src.display(),
                dst.display()
            )
        })?;
    }
    Ok(())
}

/// Write `bytes` to `path` by writing a sibling temp file and renaming it into place, so a
/// crash never leaves a half-written file. On Unix the file is readable by the owner only.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("Đường dẫn không hợp lệ: {}", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("Đường dẫn không hợp lệ: {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("Không tạo được {}", dir.display()))?;

    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".asc-tmp");
    let tmp = dir.join(tmp_name);

    let write = || -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("Không ghi được {}", path.display()))
    })
}

/// Create a directory (and its parents) that only the current user can enter.
pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("Không tạo được {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Read a file, treating "does not exist" as `None`.
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Không đọc được {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_remove_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/file.txt"), b"hello").unwrap();
        fs::write(src.join("top.bin"), [0u8, 1, 2]).unwrap();

        let dst = tmp.path().join("deep/er/dst");
        copy_any(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("a/b/file.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("top.bin")).unwrap(), [0u8, 1, 2]);

        remove_any(&dst).unwrap();
        assert!(!dst.exists());
        // Removing something that is already gone is fine.
        remove_any(&dst).unwrap();
        remove_any(&src.join("top.bin")).unwrap();
        assert!(!src.join("top.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"x").unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();

        let dst = tmp.path().join("dst");
        copy_any(&src, &dst).unwrap();
        assert!(!dst.join("link").exists());
    }

    #[test]
    fn atomic_write_replaces_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/state.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        // No temp file is left behind.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            read_optional(&tmp.path().join("missing"))
                .unwrap()
                .is_none()
        );
    }
}
