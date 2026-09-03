use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use uuid::Uuid;

pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_with(path, |file| file.write_all(contents))
}

/// Create `<dir>/.<name>.<uuid>.tmp`, let `fill` write it, fsync it, rename over `path`,
/// fsync the directory. On any error the temp file is removed and `path` is untouched.
pub fn write_with(path: &Path, fill: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        fill(&mut file)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A directory created beside `dest` (same filesystem) that replaces `dest` on `commit`.
/// Dropped without `commit`, it is removed and `dest` is untouched.
pub struct Staging {
    path: PathBuf,
    dest: PathBuf,
    committed: bool,
}

impl Staging {
    pub fn beside(dest: &Path) -> io::Result<Self> {
        let parent = dest
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| io::Error::other(format!("{} has no parent", dest.display())))?;
        fs::create_dir_all(parent)?;
        let name = dest
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("dir");
        // Keep the staging directory beside the destination so rename is same-filesystem.
        let path = parent.join(format!(".{name}.tmp-{}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self {
            path,
            dest: dest.to_path_buf(),
            committed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn commit(mut self) -> io::Result<()> {
        replace_dir(&self.path, &self.dest)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Move `src` onto `dest`. If the move fails, `dest`'s previous tree is put back.
/// A process crash between renames can leave the previous tree at the sibling `.old-*` path.
pub fn replace_dir(src: &Path, dest: &Path) -> io::Result<()> {
    let parent = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dir");
    let old = parent.join(format!(".{name}.old-{}", Uuid::new_v4()));
    let had_old = fs::symlink_metadata(dest).is_ok();
    if had_old {
        fs::rename(dest, &old)?;
    }
    if let Err(error) = fs::rename(src, dest) {
        if had_old && fs::rename(&old, dest).is_err() {
            return Err(io::Error::other(format!(
                "replacing {}: {error}; previous tree left at {}",
                dest.display(),
                old.display()
            )));
        }
        return Err(error);
    }
    if had_old {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_write_replaces_file() {
        let (_temp, paths) = crate::config::test_paths();
        let path = paths.base.join("atomic.json");
        fs::write(&path, b"old").unwrap();

        write(&path, b"new").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"new");
    }

    #[test]
    fn failing_fill_leaves_original_and_no_temp_file() {
        let (_temp, paths) = crate::config::test_paths();
        let path = paths.base.join("atomic.json");
        fs::write(&path, b"good").unwrap();

        let error = write_with(&path, |file| {
            file.write_all(b"bad")?;
            Err(io::Error::other("injected failure"))
        });

        assert!(error.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"good");
        assert!(fs::read_dir(&paths.base).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn commit_replaces_and_removes_old() {
        let (_temp, paths) = crate::config::test_paths();
        let dest = paths.base.join("tree");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("old.txt"), "old").unwrap();
        let staging = Staging::beside(&dest).unwrap();
        fs::write(staging.path().join("new.txt"), "new").unwrap();

        staging.commit().unwrap();

        assert!(!dest.join("old.txt").exists());
        assert_eq!(fs::read_to_string(dest.join("new.txt")).unwrap(), "new");
        assert!(fs::read_dir(&paths.base).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tree.old-")
        }));
    }

    #[test]
    fn drop_without_commit_keeps_dest() {
        let (_temp, paths) = crate::config::test_paths();
        let dest = paths.base.join("tree");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("old.txt"), "old").unwrap();
        let staging_path = {
            let staging = Staging::beside(&dest).unwrap();
            fs::write(staging.path().join("new.txt"), "new").unwrap();
            staging.path().to_path_buf()
        };

        assert_eq!(fs::read_to_string(dest.join("old.txt")).unwrap(), "old");
        assert!(!staging_path.exists());
    }
}
