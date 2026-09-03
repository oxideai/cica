use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

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
}
