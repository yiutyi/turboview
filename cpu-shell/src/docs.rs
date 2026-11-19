//! Directory scanning and ZIP browsing.
//!
//! The Windows build can keep its `FindFirstFileExW` fast path behind the
//! same `list_images` signature; here we use std::fs so macOS and Linux work
//! with zero platform code.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff", "ico", "tga", "pnm", "qoi",
];
pub const ARCHIVE_EXTS: &[&str] = &["zip"];

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_image(path: &Path) -> bool {
    IMAGE_EXTS.contains(&ext_of(path).as_str())
}

pub fn is_archive(path: &Path) -> bool {
    ARCHIVE_EXTS.contains(&ext_of(path).as_str())
}

pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
}

pub fn list_images(dir: &Path) -> Vec<FileEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<FileEntry> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() || !is_image(&path) {
                return None;
            }
            let name = e.file_name().to_string_lossy().to_string();
            Some(FileEntry { path, name })
        })
        .collect();
    out.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    out
}

pub struct ZipBook {
    pub path: PathBuf,
    pub names: Vec<String>,
}

impl ZipBook {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).context("打开 zip 失败")?;
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
            .context("解析 zip 失败")?;
        let mut names = Vec::new();
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index(i) {
                if entry.is_file() && is_image(Path::new(entry.name())) {
                    names.push(entry.name().to_string());
                }
            }
        }
        names.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
        Ok(Self {
            path: path.to_path_buf(),
            names,
        })
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let file = std::fs::File::open(&self.path).context("打开 zip 失败")?;
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
            .context("解析 zip 失败")?;
        let mut entry = archive.by_name(name).context("zip 条目不存在")?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        std::io::Read::read_to_end(&mut entry, &mut buf).context("读取 zip 条目失败")?;
        Ok(buf)
    }
}

#[derive(Clone)]
pub enum SourceKind {
    Dir,
    Zip,
    Single,
}

pub fn classify(path: &Path) -> SourceKind {
    if path.is_dir() {
        SourceKind::Dir
    } else if is_archive(path) {
        SourceKind::Zip
    } else {
        SourceKind::Single
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tv-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recognises_extensions() {
        assert!(is_image(Path::new("a.JPG")));
        assert!(is_image(Path::new("b.png")));
        assert!(!is_image(Path::new("c.txt")));
        assert!(is_archive(Path::new("d.zip")));
        assert!(!is_archive(Path::new("d.jpg")));
    }

    #[test]
    fn lists_only_images_sorted() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("b.jpg"), b"x").unwrap();
        std::fs::write(dir.join("A.png"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let files = list_images(&dir);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "A.png");
        assert_eq!(files[1].name, "b.jpg");
    }

    #[test]
    fn opens_zip_and_lists_image_entries() {
        let dir = temp_dir("zip");
        let zip_path = dir.join("pack.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("one.jpg", opts).unwrap();
            writer.write_all(b"not really a jpeg").unwrap();
            writer.start_file("two.png", opts).unwrap();
            writer.write_all(b"not really a png").unwrap();
            writer.start_file("readme.txt", opts).unwrap();
            writer.write_all(b"ignore me").unwrap();
            writer.finish().unwrap();
        }
        let book = ZipBook::open(&zip_path).expect("open zip");
        assert_eq!(book.names, vec!["one.jpg".to_string(), "two.png".to_string()]);
        assert_eq!(book.read("one.jpg").unwrap(), b"not really a jpeg");
    }
}
