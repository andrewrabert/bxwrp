use std::path::{Path, PathBuf};
use std::sync::Arc;

use bashkit::{
    DirEntry, FileSystem, FileSystemExt, FsLimits, FsUsage, Metadata, VfsSnapshot, async_trait,
    normalize_path,
};

const ENOENT: i32 = 2;

fn absent() -> bashkit::Error {
    std::io::Error::from_raw_os_error(ENOENT).into()
}

fn not_a_directory() -> bashkit::Error {
    std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory").into()
}

fn crosses_devices() -> bashkit::Error {
    std::io::Error::new(std::io::ErrorKind::CrossesDevices, "crosses mounts").into()
}

pub struct Layer {
    target: PathBuf,
    source: Option<Source>,
}

struct Source {
    fs: Arc<dyn FileSystem>,
    rebase: bool,
}

impl Layer {
    pub fn mounted(target: &Path, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            target: normalize_path(target),
            source: Some(Source { fs, rebase: true }),
        }
    }

    pub fn shared(target: &Path, fs: Arc<dyn FileSystem>) -> Self {
        Self {
            target: normalize_path(target),
            source: Some(Source { fs, rebase: false }),
        }
    }

    #[must_use]
    pub fn removed(target: &Path) -> Self {
        Self {
            target: normalize_path(target),
            source: None,
        }
    }
}

enum Resolved {
    Absent,
    At(Arc<dyn FileSystem>, PathBuf),
}

pub struct MountTable {
    root: Arc<dyn FileSystem>,
    layers: Vec<Layer>,
}

impl MountTable {
    pub fn new(root: Arc<dyn FileSystem>, layers: Vec<Layer>) -> Self {
        Self { root, layers }
    }

    fn resolve(&self, path: &Path) -> Resolved {
        let path = normalize_path(path);
        for layer in self.layers.iter().rev() {
            if !path.starts_with(&layer.target) {
                continue;
            }
            let Some(source) = &layer.source else {
                return Resolved::Absent;
            };
            let path = if source.rebase {
                Path::new("/").join(path.strip_prefix(&layer.target).unwrap_or(&path))
            } else {
                path
            };
            return Resolved::At(Arc::clone(&source.fs), path);
        }
        Resolved::At(Arc::clone(&self.root), path)
    }

    fn at(&self, path: &Path) -> bashkit::Result<(Arc<dyn FileSystem>, PathBuf)> {
        match self.resolve(path) {
            Resolved::Absent => Err(absent()),
            Resolved::At(fs, path) => Ok((fs, path)),
        }
    }

    fn targeted(&self, path: &Path) -> bool {
        self.layers.iter().any(|layer| layer.target == path)
    }

    async fn entry(&self, path: &Path, name: &str, fallback: Metadata) -> Option<DirEntry> {
        let (fs, resolved) = match self.resolve(&path.join(name)) {
            Resolved::Absent => return None,
            Resolved::At(fs, resolved) => (fs, resolved),
        };
        let metadata = if self.targeted(&path.join(name)) {
            fs.stat(&resolved).await.ok()?
        } else {
            fallback
        };
        Some(DirEntry {
            name: name.to_string(),
            metadata,
        })
    }
}

#[async_trait]
impl FileSystemExt for MountTable {
    fn usage(&self) -> FsUsage {
        self.root.usage()
    }

    async fn mkfifo(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.mkfifo(&path, mode).await
    }

    fn limits(&self) -> FsLimits {
        self.root.limits()
    }

    fn backend_kind(&self) -> &'static str {
        self.root.backend_kind()
    }

    fn vfs_snapshot(&self) -> Option<VfsSnapshot> {
        self.root.vfs_snapshot()
    }

    fn vfs_restore(&self, snapshot: &VfsSnapshot) -> bashkit::Result<()> {
        self.root.vfs_restore(snapshot)
    }
}

#[async_trait]
impl FileSystem for MountTable {
    async fn read_file(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        let (fs, path) = self.at(path)?;
        fs.read_file(&path).await
    }

    async fn write_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.write_file(&path, content).await
    }

    async fn append_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.append_file(&path, content).await
    }

    async fn mkdir(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.mkdir(&path, recursive).await
    }

    async fn remove(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.remove(&path, recursive).await
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<Metadata> {
        let (fs, path) = self.at(path)?;
        fs.stat(&path).await
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<DirEntry>> {
        let (fs, resolved) = self.at(path)?;
        let directory = normalize_path(path);
        let listed = fs.read_dir(&resolved).await?;

        let mut entries: Vec<DirEntry> = Vec::with_capacity(listed.len());
        for entry in listed {
            if let Some(entry) = self.entry(&directory, &entry.name, entry.metadata).await {
                entries.push(entry);
            }
        }
        for layer in &self.layers {
            if layer.target.parent() != Some(directory.as_path()) {
                continue;
            }
            let Some(name) = layer.target.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if entries.iter().any(|entry| entry.name == name) {
                continue;
            }
            let (fs, resolved) = match self.resolve(&layer.target) {
                Resolved::Absent => continue,
                Resolved::At(fs, resolved) => (fs, resolved),
            };
            if let Ok(metadata) = fs.stat(&resolved).await {
                entries.push(DirEntry {
                    name: name.to_string(),
                    metadata,
                });
            }
        }
        Ok(entries)
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        match self.resolve(path) {
            Resolved::Absent => Ok(false),
            Resolved::At(fs, path) => fs.exists(&path).await,
        }
    }

    async fn rename(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        let (source, from) = self.at(from)?;
        let (target, to) = self.at(to)?;
        if !Arc::ptr_eq(&source, &target) {
            return Err(crosses_devices());
        }
        source.rename(&from, &to).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        let (source, from) = self.at(from)?;
        let (target, to) = self.at(to)?;
        if Arc::ptr_eq(&source, &target) {
            return source.copy(&from, &to).await;
        }
        let content = source.read_file(&from).await?;
        target.write_file(&to, &content).await
    }

    async fn symlink(&self, target: &Path, link: &Path) -> bashkit::Result<()> {
        let (fs, link) = self.at(link)?;
        fs.symlink(target, &link).await
    }

    async fn read_link(&self, path: &Path) -> bashkit::Result<PathBuf> {
        let (fs, path) = self.at(path)?;
        fs.read_link(&path).await
    }

    async fn chmod(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.chmod(&path, mode).await
    }

    async fn set_modified_time(
        &self,
        path: &Path,
        time: std::time::SystemTime,
    ) -> bashkit::Result<()> {
        let (fs, path) = self.at(path)?;
        fs.set_modified_time(&path, time).await
    }
}

pub struct PathFilter {
    exclude: ignore::gitignore::Gitignore,
    include: Option<ignore::gitignore::Gitignore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Hidden,
}

impl PathFilter {
    pub fn new(exclude: &[String], include: &[String]) -> anyhow::Result<Option<Self>> {
        if exclude.is_empty() && include.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            exclude: compile(exclude)?,
            include: if include.is_empty() {
                None
            } else {
                Some(compile(include)?)
            },
        }))
    }

    pub(crate) fn visibility(&self, path: &Path, entry: Entry) -> Visibility {
        if self.hidden(path, matches!(entry, Entry::Dir)) {
            Visibility::Hidden
        } else {
            Visibility::Shown
        }
    }

    pub(crate) fn own_visibility(&self, path: &Path, entry: Entry) -> Visibility {
        let is_dir = matches!(entry, Entry::Dir);
        if path != Path::new("/") && self.exclude.matched(path, is_dir).is_ignore() {
            return Visibility::Hidden;
        }
        match &self.include {
            Some(include) if !is_dir && !include.matched(path, false).is_ignore() => {
                Visibility::Hidden
            }
            _ => Visibility::Shown,
        }
    }

    fn hidden(&self, path: &Path, is_dir: bool) -> bool {
        if path == Path::new("/") {
            return false;
        }
        if self
            .exclude
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
        {
            return true;
        }
        match &self.include {
            Some(include) if !is_dir => !include.matched(path, false).is_ignore(),
            _ => false,
        }
    }
}

fn compile(patterns: &[String]) -> anyhow::Result<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new("/");
    for pattern in patterns {
        builder.add_line(None, pattern)?;
    }
    Ok(builder.build()?)
}

pub struct FilterFs {
    inner: Arc<dyn FileSystem>,
    filter: Arc<PathFilter>,
}

impl FilterFs {
    pub fn new(inner: Arc<dyn FileSystem>, filter: Arc<PathFilter>) -> Self {
        Self { inner, filter }
    }

    fn check(&self, path: &Path, is_dir: bool) -> bashkit::Result<()> {
        if self.filter.hidden(&normalize_path(path), is_dir) {
            Err(absent())
        } else {
            Ok(())
        }
    }

    async fn check_stated(&self, path: &Path) -> bashkit::Result<()> {
        let is_dir = match self.inner.stat(path).await {
            Ok(metadata) => metadata.file_type.is_dir(),
            Err(_) => false,
        };
        self.check(path, is_dir)
    }
}

#[async_trait]
impl FileSystemExt for FilterFs {
    fn usage(&self) -> FsUsage {
        self.inner.usage()
    }

    async fn mkfifo(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        self.check(path, false)?;
        self.inner.mkfifo(path, mode).await
    }

    fn limits(&self) -> FsLimits {
        self.inner.limits()
    }

    fn backend_kind(&self) -> &'static str {
        self.inner.backend_kind()
    }

    fn vfs_snapshot(&self) -> Option<VfsSnapshot> {
        self.inner.vfs_snapshot()
    }

    fn vfs_restore(&self, snapshot: &VfsSnapshot) -> bashkit::Result<()> {
        self.inner.vfs_restore(snapshot)
    }
}

#[async_trait]
impl FileSystem for FilterFs {
    async fn read_file(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        self.check(path, false)?;
        self.inner.read_file(path).await
    }

    async fn write_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        self.check(path, false)?;
        self.inner.write_file(path, content).await
    }

    async fn append_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        self.check(path, false)?;
        self.inner.append_file(path, content).await
    }

    async fn mkdir(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        self.check(path, true)?;
        self.inner.mkdir(path, recursive).await
    }

    async fn remove(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        self.check_stated(path).await?;
        self.inner.remove(path, recursive).await
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<Metadata> {
        let metadata = self.inner.stat(path).await?;
        self.check(path, metadata.file_type.is_dir())?;
        Ok(metadata)
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<DirEntry>> {
        self.check(path, true)?;
        let directory = normalize_path(path);
        let listed = self.inner.read_dir(path).await?;
        Ok(listed
            .into_iter()
            .filter(|entry| {
                !self.filter.hidden(
                    &directory.join(&entry.name),
                    entry.metadata.file_type.is_dir(),
                )
            })
            .collect())
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        match self.check_stated(path).await {
            Ok(()) => self.inner.exists(path).await,
            Err(_) => Ok(false),
        }
    }

    async fn rename(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        let is_dir = match self.inner.stat(from).await {
            Ok(metadata) => metadata.file_type.is_dir(),
            Err(_) => false,
        };
        self.check(from, is_dir)?;
        self.check(to, is_dir)?;
        self.inner.rename(from, to).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        self.check(from, false)?;
        self.check(to, false)?;
        self.inner.copy(from, to).await
    }

    async fn symlink(&self, target: &Path, link: &Path) -> bashkit::Result<()> {
        self.check(link, false)?;
        self.inner.symlink(target, link).await
    }

    async fn read_link(&self, path: &Path) -> bashkit::Result<PathBuf> {
        self.check(path, false)?;
        self.inner.read_link(path).await
    }

    async fn chmod(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        self.check_stated(path).await?;
        self.inner.chmod(path, mode).await
    }

    async fn set_modified_time(
        &self,
        path: &Path,
        time: std::time::SystemTime,
    ) -> bashkit::Result<()> {
        self.check_stated(path).await?;
        self.inner.set_modified_time(path, time).await
    }
}

pub struct SingleFile {
    parent: Arc<dyn FileSystem>,
    name: PathBuf,
}

impl SingleFile {
    pub fn new(parent: Arc<dyn FileSystem>, name: &Path) -> Self {
        Self {
            parent,
            name: Path::new("/").join(name),
        }
    }

    fn file(&self, path: &Path) -> bashkit::Result<&Path> {
        if normalize_path(path) == Path::new("/") {
            Ok(&self.name)
        } else {
            Err(absent())
        }
    }
}

#[async_trait]
impl FileSystemExt for SingleFile {
    fn usage(&self) -> FsUsage {
        self.parent.usage()
    }

    fn limits(&self) -> FsLimits {
        self.parent.limits()
    }

    fn backend_kind(&self) -> &'static str {
        self.parent.backend_kind()
    }
}

#[async_trait]
impl FileSystem for SingleFile {
    async fn read_file(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        self.parent.read_file(self.file(path)?).await
    }

    async fn write_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        self.parent.write_file(self.file(path)?, content).await
    }

    async fn append_file(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        self.parent.append_file(self.file(path)?, content).await
    }

    async fn mkdir(&self, path: &Path, _recursive: bool) -> bashkit::Result<()> {
        self.file(path)?;
        Err(not_a_directory())
    }

    async fn remove(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        self.parent.remove(self.file(path)?, recursive).await
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<Metadata> {
        self.parent.stat(self.file(path)?).await
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<DirEntry>> {
        self.file(path)?;
        Err(not_a_directory())
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        if normalize_path(path) == Path::new("/") {
            self.parent.exists(&self.name).await
        } else {
            Ok(false)
        }
    }

    async fn rename(&self, from: &Path, _to: &Path) -> bashkit::Result<()> {
        self.file(from)?;
        Err(crosses_devices())
    }

    async fn copy(&self, from: &Path, _to: &Path) -> bashkit::Result<()> {
        self.file(from)?;
        Err(crosses_devices())
    }

    async fn symlink(&self, _target: &Path, link: &Path) -> bashkit::Result<()> {
        self.file(link)?;
        Err(not_a_directory())
    }

    async fn read_link(&self, path: &Path) -> bashkit::Result<PathBuf> {
        self.parent.read_link(self.file(path)?).await
    }

    async fn chmod(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        self.parent.chmod(self.file(path)?, mode).await
    }

    async fn set_modified_time(
        &self,
        path: &Path,
        time: std::time::SystemTime,
    ) -> bashkit::Result<()> {
        self.parent.set_modified_time(self.file(path)?, time).await
    }
}
