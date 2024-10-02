pub mod std;
#[cfg(test)]
pub mod test;

use ::std::{
    error,
    ffi::OsString,
    fmt::Debug,
    fs::{self},
    path::{Path, PathBuf},
};
use strum::Display;

/// Dependencies that [`Cache`] has on the file system.
pub trait Fs {
    /// Error type for methods.
    type Error: error::Error;

    /// Return a random u64. This is used for creating unique path names.
    fn rand_u64(&self) -> u64;

    /// Get the metadata of the file at `path`. If `path` doesn't exist, return `Ok(None)`. If the
    /// last component is a symlink, return the metadata about the symlink instead of trying to
    /// resolve it. An error may be returned if there is an error attempting to resolve `path`.
    /// For example, if `path` contains a dangling symlink, or if an intermediate component of the
    /// path is a file.
    fn metadata(&self, path: &Path) -> Result<Option<Metadata>, Self::Error>;

    /// Return and iterator that will yield all of the children of a directory, excluding "." and
    /// "..". There must be a directory at `path`, or an error will be returned.
    fn read_dir(
        &self,
        path: &Path,
    ) -> Result<impl Iterator<Item = Result<(OsString, Metadata), Self::Error>>, Self::Error>;

    /// Create a file with given `path` and `contents`. There must not be any file or directory at
    /// `path`, but its parent directory must be exist.
    fn create_file(&self, path: &Path, contents: &[u8]) -> Result<(), Self::Error>;

    /// Create a symlink at `link` that points to `target`. There must not be any file or directory
    /// at `link`, but its parent directory must exist.
    fn symlink(&self, target: &Path, link: &Path) -> Result<(), Self::Error>;

    /// Create an empty directory at `path`. There must not be any file or directory at `path`, but
    /// its parent directory must exist.
    fn mkdir(&self, path: &Path) -> Result<(), Self::Error>;

    /// Attempt to create a directory at `path` by creating any missing ancestor directories. Don't
    /// error if there is already a directory at `path`.
    fn mkdir_recursively(&self, path: &Path) -> Result<(), Self::Error>;

    /// Remove an existing, non-directory entry at `path`.
    fn remove(&self, path: &Path) -> Result<(), Self::Error>;

    /// Remove an existing directory, and all of its descendants, on a background thread. `path`
    /// must exist, and it must be a directory. This function will return an error immediately if
    /// `path` doesn't exist, can't be resolved, or doesn't point to a directory. Otherwise, the
    /// removal will happen in the background on another thread. If an error occurs there, the
    /// calling function won't be notified.
    fn rmdir_recursively_on_thread(&self, path: PathBuf) -> Result<(), Self::Error>;

    /// Rename `source` to `destination`. There are a bunch of rules that must be satisfied for
    /// this to succeed:
    ///   - `source` must exist.
    ///   - Either `destination` exists, or its parent exists and is a directory.
    ///   - If `destination` exists and `source` is not a directory, then `destination` must also
    ///     no be a directory. It will be removed as part of the rename.
    ///   - If `destination` exists and `source` is a directory, then `destination` must be an
    ///     empty directory.
    ///   - If `source` is a directory, it cannot be moved into one of its descendants, as this
    ///     would create a disconnected cycle.
    fn rename(&self, source: &Path, destination: &Path) -> Result<(), Self::Error>;

    /// The type returned by the [`Self::temp_file`] method. Some implementations may make this
    /// type [`Drop`] so that the temporary file can be cleaned up when it is closed.
    type TempFile: TempFile;

    /// Create a new temporary file in the directory `parent`.
    fn temp_file(&self, parent: &Path) -> Result<Self::TempFile, Self::Error>;

    /// Rename `temp_file` to `target` while consuming `temp_file`. This is different than the
    /// caller just doing the rename itself in that it consumes `temp_file` without dropping it.
    fn persist_temp_file(
        &self,
        temp_file: Self::TempFile,
        target: &Path,
    ) -> Result<(), Self::Error>;

    /// The type returned by the [`Self::temp_dir`] method. Some implementations may make this
    /// type [`Drop`] so that the temporary directory can be cleaned up when it is closed.
    type TempDir: TempDir;

    /// Create a new temporary directory in the directory `parent`.
    fn temp_dir(&self, parent: &Path) -> Result<Self::TempDir, Self::Error>;

    /// Rename `temp_dir` to `target` while consuming `temp_dir`. This is different than the
    /// caller just doing the rename itself in that it consumes `temp_dir` without dropping it.
    fn persist_temp_dir(&self, temp_dir: Self::TempDir, target: &Path) -> Result<(), Self::Error>;
}

/// A type used to represent a temporary file. The assumption is that the implementer may want to
/// make the type [`Drop`] so that the temporary file is cleaned up if it isn't consumed.
pub trait TempFile: Debug {
    /// Return the path to the temporary file. Can be used to open the file to write into it.
    fn path(&self) -> &Path;
}

/// A type used to represent a temporary directory. The assumption is that the implementer may want
/// to make the type [`Drop`] so that the temporary directory is cleaned up if it isn't consumed.
pub trait TempDir: Debug {
    /// Return the path to the temporary directory. Can be used to create files in the directory
    /// before it is made persistent.
    fn path(&self) -> &Path;
}

/// The file metadata returned from [`Fs`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metadata {
    pub type_: FileType,
    pub size: u64,
}

impl Metadata {
    /// Create a new [`Metadata`] for a directory of size `size`.
    pub fn directory(size: u64) -> Self {
        Self {
            type_: FileType::Directory,
            size,
        }
    }

    /// Create a new [`Metadata`] for a file of size `size`.
    pub fn file(size: u64) -> Self {
        Self {
            type_: FileType::File,
            size,
        }
    }

    /// Create a new [`Metadata`] for a symlink of size `size`.
    pub fn symlink(size: u64) -> Self {
        Self {
            type_: FileType::Symlink,
            size,
        }
    }
}

impl From<fs::Metadata> for Metadata {
    fn from(metadata: fs::Metadata) -> Self {
        Self {
            type_: metadata.file_type().into(),
            size: metadata.len(),
        }
    }
}

/// The file type returned from [`Fs`].
#[derive(Clone, Copy, Debug, Display, PartialEq)]
pub enum FileType {
    Directory,
    File,
    Symlink,
    Other,
}

impl From<fs::FileType> for FileType {
    fn from(file_type: fs::FileType) -> Self {
        if file_type.is_dir() {
            FileType::Directory
        } else if file_type.is_file() {
            FileType::File
        } else if file_type.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Other
        }
    }
}
