#[cfg(feature = "async-std")]
use async_std::{
    fs::{self, OpenOptions},
    io::{self, Error, ErrorKind, Read, ReadExt, SeekExt, SeekFrom},
    os::unix::fs::symlink as symlink_file,
};
use std::{
    borrow::Cow,
    cmp, fmt, marker,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(feature = "tokio")]
use tokio::{
    fs::{self, symlink as symlink_file, OpenOptions},
    io::{
        self, AsyncRead as Read, AsyncReadExt, AsyncSeekExt, Error, ErrorKind, ReadBuf, SeekFrom,
    },
};

use crate::{
    error::TarError, header::bytes2path, other, Archive, Header, PaxExtensions,
};
use filetime::{self, FileTime};
use pin_project::pin_project;
use std::path::{Component, Path, PathBuf};
use std::task::ready;
use crate::archive::ArchiveInner;

/// A read-only view into an entry of an archive.
///
/// This structure is a window into a portion of a borrowed archive which can
/// be inspected. It acts as a file handle by implementing the Reader trait. An
/// entry cannot be rewritten once inserted into an archive.
#[pin_project]
pub struct Entry<'a, R: Read + Unpin> {
    #[pin]
    fields: EntryFields<'a>,
    _ignored: marker::PhantomData<Archive<R>>,
}

impl<'a, R: Read + Unpin> fmt::Debug for Entry<'a, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("fields", &self.fields)
            .finish()
    }
}

// private implementation detail of `Entry`, but concrete (no type parameters)
// and also all-public to be constructed from other modules.
#[pin_project]
pub struct EntryFields<'a> {
    pub long_pathname: Option<Vec<u8>>,
    pub long_linkname: Option<Vec<u8>>,
    pub pax_extensions: Option<Vec<u8>>,
    pub mask: u32,
    pub header: Header,
    pub size: u64,
    pub header_pos: u64,
    pub file_pos: u64,
    #[pin]
    pub data: Vec<EntryIo<'a>>,
    pub unpack_xattrs: bool,
    pub preserve_permissions: bool,
    pub preserve_ownerships: bool,
    pub preserve_mtime: bool,
    pub overwrite: bool,
    #[pin]
    pub(crate) read_state: Option<EntryIo<'a>>,
}

impl<'a> fmt::Debug for EntryFields<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryFields")
            .field("long_pathname", &self.long_pathname)
            .field("long_linkname", &self.long_linkname)
            .field("pax_extensions", &self.pax_extensions)
            .field("header", &self.header)
            .field("size", &self.size)
            .field("header_pos", &self.header_pos)
            .field("file_pos", &self.file_pos)
            .field("data", &self.data)
            .field("unpack_xattrs", &self.unpack_xattrs)
            .field("preserve_permissions", &self.preserve_permissions)
            .field("preserve_ownerships", &self.preserve_ownerships)
            .field("preserve_mtime", &self.preserve_mtime)
            .field("overwrite", &self.overwrite)
            .field("read_state", &self.read_state)
            .finish()
    }
}

#[pin_project(project = EntryIoProject)]
pub enum EntryIo<'a> {
    Pad(#[pin] io::Take<io::Repeat>),
    Data(#[pin] io::Take<&'a ArchiveInner<dyn Read + 'a>>),
}

impl<'a> fmt::Debug for EntryIo<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryIo::Pad(_) => write!(f, "EntryIo::Pad"),
            EntryIo::Data(_) => write!(f, "EntryIo::Data"),
        }
    }
}

/// When unpacking items the unpacked thing is returned to allow custom
/// additional handling by users. Today the File is returned, in future
/// the enum may be extended with kinds for links, directories etc.
#[derive(Debug)]
#[non_exhaustive]
pub enum Unpacked {
    /// A file was unpacked.
    File(fs::File),
    /// A directory, hardlink, symlink, or other node was unpacked.
    #[doc(hidden)]
    __Nonexhaustive,
}

impl<'a, R: Read + Unpin + Send> Entry<'a, R> {
    /// Returns the path name for this entry.
    ///
    /// This method may fail if the pathname is not valid Unicode and this is
    /// called on a Windows platform.
    ///
    /// Note that this function will convert any `\` characters to directory
    /// separators, and it will not always return the same value as
    /// `self.header().path()` as some archive formats have support for longer
    /// path names described in separate entries.
    ///
    /// It is recommended to use this method instead of inspecting the `header`
    /// directly to ensure that various archive formats are handled correctly.
    pub fn path(&self) -> io::Result<Cow<Path>> {
        self.fields.path()
    }

    /// Returns the raw bytes listed for this entry.
    ///
    /// Note that this function will convert any `\` characters to directory
    /// separators, and it will not always return the same value as
    /// `self.header().path_bytes()` as some archive formats have support for
    /// longer path names described in separate entries.
    pub fn path_bytes(&self) -> Cow<[u8]> {
        self.fields.path_bytes()
    }

    /// Returns the link name for this entry, if any is found.
    ///
    /// This method may fail if the pathname is not valid Unicode and this is
    /// called on a Windows platform. `Ok(None)` being returned, however,
    /// indicates that the link name was not present.
    ///
    /// Note that this function will convert any `\` characters to directory
    /// separators, and it will not always return the same value as
    /// `self.header().link_name()` as some archive formats have support for
    /// longer path names described in separate entries.
    ///
    /// It is recommended to use this method instead of inspecting the `header`
    /// directly to ensure that various archive formats are handled correctly.
    pub fn link_name(&self) -> io::Result<Option<Cow<Path>>> {
        self.fields.link_name()
    }

    /// Returns the link name for this entry, in bytes, if listed.
    ///
    /// Note that this will not always return the same value as
    /// `self.header().link_name_bytes()` as some archive formats have support for
    /// longer path names described in separate entries.
    pub fn link_name_bytes(&self) -> Option<Cow<[u8]>> {
        self.fields.link_name_bytes()
    }

    /// Returns an iterator over the pax extensions contained in this entry.
    ///
    /// Pax extensions are a form of archive where extra metadata is stored in
    /// key/value pairs in entries before the entry they're intended to
    /// describe. For example this can be used to describe long file name or
    /// other metadata like atime/ctime/mtime in more precision.
    ///
    /// The returned iterator will yield key/value pairs for each extension.
    ///
    /// `None` will be returned if this entry does not indicate that it itself
    /// contains extensions, or if there were no previous extensions describing
    /// it.
    ///
    /// Note that global pax extensions are intended to be applied to all
    /// archive entries.
    ///
    /// Also note that this function will read the entire entry if the entry
    /// itself is a list of extensions.
    pub async fn pax_extensions(&mut self) -> io::Result<Option<PaxExtensions<'_>>> {
        self.fields.pax_extensions().await
    }

    /// Returns access to the header of this entry in the archive.
    ///
    /// This provides access to the metadata for this entry in the archive.
    pub fn header(&self) -> &Header {
        &self.fields.header
    }

    /// Returns the starting position, in bytes, of the header of this entry in
    /// the archive.
    ///
    /// The header is always a contiguous section of 512 bytes, so if the
    /// underlying reader implements `Seek`, then the slice from `header_pos` to
    /// `header_pos + 512` contains the raw header bytes.
    pub fn raw_header_position(&self) -> u64 {
        self.fields.header_pos
    }

    /// Returns the starting position, in bytes, of the file of this entry in
    /// the archive.
    ///
    /// If the file of this entry is continuous (e.g. not a sparse file), and
    /// if the underlying reader implements `Seek`, then the slice from
    /// `file_pos` to `file_pos + entry_size` contains the raw file bytes.
    pub fn raw_file_position(&self) -> u64 {
        self.fields.file_pos
    }

    /// Writes this file to the specified location.
    ///
    /// This function will write the entire contents of this file into the
    /// location specified by `dst`. Metadata will also be propagated to the
    /// path `dst`.
    ///
    /// This function will create a file at the path `dst`, and it is required
    /// that the intermediate directories are created. Any existing file at the
    /// location `dst` will be overwritten.
    ///
    /// > **Note**: This function does not have as many sanity checks as
    /// > `Archive::unpack` or `Entry::unpack_in`. As a result if you're
    /// > thinking of unpacking untrusted tarballs you may want to review the
    /// > implementations of the previous two functions and perhaps implement
    /// > similar logic yourself.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    /// #
    /// use futures::TryStreamExt;
    /// use tokio::fs::File;
    /// use async_tar_rs::Archive;
    ///
    /// let mut ar = Archive::new(File::open("foo.tar").await?);
    /// let mut entries = ar.entries()?;
    /// let mut i = 0;
    /// while let Ok(Some(mut file)) = entries.try_next().await {
    ///     file.unpack(format!("file-{}", i)).await?;
    ///     i += 1;
    /// }
    /// #
    /// # Ok(()) }
    /// ```
    pub async fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<Unpacked> {
        self.fields.unpack(None, dst.as_ref()).await
    }

    /// Extracts this file under the specified path, avoiding security issues.
    ///
    /// This function will write the entire contents of this file into the
    /// location obtained by appending the path of this file in the archive to
    /// `dst`, creating any intermediate directories if needed. Metadata will
    /// also be propagated to the path `dst`. Any existing file at the location
    /// `dst` will be overwritten.
    ///
    /// This function carefully avoids writing outside of `dst`. If the file has
    /// a '..' in its path, this function will skip it and return false.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    /// #
    /// use tokio::fs::File;
    /// use async_tar_rs::Archive;
    /// use futures::TryStreamExt;
    ///
    /// let mut ar = Archive::new(File::open("foo.tar").await?);
    /// let mut entries = ar.entries()?;
    /// let mut i = 0;
    /// while let Ok(Some(mut file)) = entries.try_next().await {
    ///     file.unpack_in("target").await?;
    ///     i += 1;
    /// }
    /// #
    /// # Ok(()) }
    /// ```
    pub async fn unpack_in<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<bool> {
        self.fields.unpack_in(dst.as_ref()).await
    }

    /// Indicate whether extended file attributes (xattrs on Unix) are preserved
    /// when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix using xattr support. This may eventually be implemented for
    /// Windows, however, if other archive implementations are found which do
    /// this as well.
    pub fn set_unpack_xattrs(&mut self, unpack_xattrs: bool) {
        self.fields.unpack_xattrs = unpack_xattrs;
    }

    /// Indicate whether extended permissions (like suid on Unix) are preserved
    /// when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix.
    pub fn set_preserve_permissions(&mut self, preserve: bool) {
        self.fields.preserve_permissions = preserve;
    }

    /// Indicate whether access time information is preserved when unpacking
    /// this entry.
    ///
    /// This flag is enabled by default.
    pub fn set_preserve_mtime(&mut self, preserve: bool) {
        self.fields.preserve_mtime = preserve;
    }
}

impl<'a, R: Read + Unpin> Read for Entry<'a, R> {
    #[cfg(feature = "async-std")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        into: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();
        Pin::new(&mut *this.fields).poll_read(cx, into)
    }

    #[cfg(feature = "tokio")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();
        Pin::new(&mut *this.fields).poll_read(cx, buf)
    }
}

impl<'a, R: Read + Unpin + Send> EntryFields<'a> {
    pub fn from(entry: Entry<R>) -> Self {
        entry.fields
    }

    pub fn into_entry(self) -> Entry<'a, R> {
        Entry {
            fields: self,
            _ignored: marker::PhantomData,
        }
    }

    pub(crate) fn poll_read_all(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Vec<u8>>> {
        // Preallocate some data but don't let ourselves get too crazy now.
        let cap = cmp::min(self.size, 128 * 1024);
        let mut buf = Vec::with_capacity(cap as usize);

        // Copied from futures::ReadToEnd
        match ready!(poll_read_all_internal(self, cx, &mut buf)) {
            Ok(_) => Poll::Ready(Ok(buf)),
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    pub async fn read_all(&mut self) -> io::Result<Vec<u8>> {
        // Preallocate some data but don't let ourselves get too crazy now.
        let cap = cmp::min(self.size, 128 * 1024);
        let mut v = Vec::with_capacity(cap as usize);
        self.read_to_end(&mut v).await.map(|_| v)
    }

    fn path(&self) -> io::Result<Cow<'_, Path>> {
        bytes2path(self.path_bytes())
    }

    fn path_bytes(&self) -> Cow<[u8]> {
        if let Some(ref bytes) = self.long_pathname {
            if let Some(&0) = bytes.last() {
                Cow::Borrowed(&bytes[..bytes.len() - 1])
            } else {
                Cow::Borrowed(bytes)
            }
        } else {
            if let Some(ref pax) = self.pax_extensions {
                let pax = PaxExtensions::new(pax)
                    .filter_map(Result::ok)
                    .find(|f| f.key_bytes() == b"path")
                    .map(|f| f.value_bytes());
                if let Some(field) = pax {
                    return Cow::Borrowed(field);
                }
            }
            self.header.path_bytes()
        }
    }

    /// Gets the path in a "lossy" way, used for error reporting ONLY.
    fn path_lossy(&self) -> String {
        String::from_utf8_lossy(&self.path_bytes()).to_string()
    }

    fn link_name(&self) -> io::Result<Option<Cow<Path>>> {
        match self.link_name_bytes() {
            Some(bytes) => bytes2path(bytes).map(Some),
            None => Ok(None),
        }
    }

    fn link_name_bytes(&self) -> Option<Cow<[u8]>> {
        match self.long_linkname {
            Some(ref bytes) => {
                if let Some(&0) = bytes.last() {
                    Some(Cow::Borrowed(&bytes[..bytes.len() - 1]))
                } else {
                    Some(Cow::Borrowed(bytes))
                }
            }
            None => self.header.link_name_bytes(),
        }
    }

    async fn pax_extensions(&mut self) -> io::Result<Option<PaxExtensions<'_>>> {
        if self.pax_extensions.is_none() {
            if !self.header.entry_type().is_pax_global_extensions()
                && !self.header.entry_type().is_pax_local_extensions()
            {
                return Ok(None);
            }
            self.pax_extensions = Some(self.read_all().await?);
        }

        Ok(Some(PaxExtensions::new(
            self.pax_extensions.as_ref().unwrap(),
        )))
    }

    async fn unpack_in(&mut self, dst: &Path) -> io::Result<bool> {
        // Notes regarding bsdtar 2.8.3 / libarchive 2.8.3:
        // * Leading '/'s are trimmed. For example, `///test` is treated as
        //   `test`.
        // * If the filename contains '..', then the file is skipped when
        //   extracting the tarball.
        // * '//' within a filename is effectively skipped. An error is
        //   logged, but otherwise the effect is as if any two or more
        //   adjacent '/'s within the filename were consolidated into one
        //   '/'.
        //
        // Most of this is handled by the `path` module of the standard
        // library, but we specially handle a few cases here as well.

        let mut file_dst = dst.to_path_buf();
        {
            let path = self.path().map_err(|e| {
                TarError::new(
                    &format!("invalid path in entry header: {}", self.path_lossy()),
                    e,
                )
            })?;
            for part in path.components() {
                match part {
                    // Leading '/' characters, root paths, and '.'
                    // components are just ignored and treated as "empty
                    // components"
                    Component::Prefix(..) | Component::RootDir | Component::CurDir => continue,

                    // If any part of the filename is '..', then skip over
                    // unpacking the file to prevent directory traversal
                    // security issues.  See, e.g.: CVE-2001-1267,
                    // CVE-2002-0399, CVE-2005-1918, CVE-2007-4131
                    Component::ParentDir => return Ok(false),

                    Component::Normal(part) => file_dst.push(part),
                }
            }
        }

        // Skip cases where only slashes or '.' parts were seen, because
        // this is effectively an empty filename.
        if *dst == *file_dst {
            return Ok(true);
        }

        // Skip entries without a parent (i.e. outside of FS root)
        let parent = match file_dst.parent() {
            Some(p) => p,
            None => return Ok(false),
        };

        self.ensure_dir_created(dst, parent)
            .await
            .map_err(|e| TarError::new(&format!("failed to create `{}`", parent.display()), e))?;

        let canon_target = self.validate_inside_dst(dst, parent).await?;

        self.unpack(Some(&canon_target), &file_dst)
            .await
            .map_err(|e| TarError::new(&format!("failed to unpack `{}`", file_dst.display()), e))?;

        Ok(true)
    }

    /// Unpack as destination directory `dst`.
    async fn unpack_dir(&mut self, dst: &Path) -> io::Result<()> {
        // If the directory already exists just let it slide
        match fs::create_dir(dst).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if err.kind() == ErrorKind::AlreadyExists {
                    let prev = fs::metadata(dst).await;
                    if prev.map(|m| m.is_dir()).unwrap_or(false) {
                        return Ok(());
                    }
                }
                Err(Error::new(
                    err.kind(),
                    format!("{} when creating dir {}", err, dst.display()),
                ))
            }
        }
    }

    /// Returns access to the header of this entry in the archive.
    async fn unpack(&mut self, target_base: Option<&Path>, dst: &Path) -> io::Result<Unpacked> {
        async fn set_perms_ownerships(
            dst: &Path,
            f: Option<&mut fs::File>,
            header: &Header,
            mask: u32,
            perms: bool,
            ownerships: bool,
        ) -> io::Result<()> {
            // ownerships need to be set first to avoid stripping SUID bits in the permissions ...
            if ownerships {
                set_ownerships(dst, &f, header.uid()?, header.gid()?)?;
            }
            // ... then set permissions, SUID bits set here is kept
            if let Ok(mode) = header.mode() {
                set_perms(dst, f, mode, mask, perms).await?;
            }

            Ok(())
        }

        let kind = self.header.entry_type();

        if kind.is_dir() {
            self.unpack_dir(dst).await?;
            set_perms_ownerships(
                dst,
                None,
                &self.header,
                self.mask,
                self.preserve_permissions,
                self.preserve_ownerships,
            ).await?;

            return Ok(Unpacked::__Nonexhaustive);
        } else if kind.is_hard_link() || kind.is_symlink() {
            let src = match self.link_name()? {
                Some(name) => name,
                None => {
                    return Err(other(&format!(
                        "hard link listed for {} but no link name found",
                        String::from_utf8_lossy(self.header.as_bytes())
                    )));
                }
            };

            if src.iter().count() == 0 {
                return Err(other(&format!(
                    "symlink destination for {} is empty",
                    String::from_utf8_lossy(self.header.as_bytes())
                )));
            }

            if kind.is_hard_link() {
                let link_src = match target_base {
                    // If we're unpacking within a directory then ensure that
                    // the destination of this hard link is both present and
                    // inside our own directory. This is needed because we want
                    // to make sure to not overwrite anything outside the root.
                    //
                    // Note that this logic is only needed for hard links
                    // currently. With symlinks the `validate_inside_dst` which
                    // happens before this method as part of `unpack_in` will
                    // use canonicalization to ensure this guarantee. For hard
                    // links though they're canonicalized to their existing path
                    // so we need to validate at this time.
                    Some(p) => {
                        let link_src = p.join(src);
                        self.validate_inside_dst(p, &link_src).await?;
                        link_src
                    }
                    None => src.into_owned(),
                };
                fs::hard_link(&link_src, dst).await.map_err(|err| {
                    Error::new(
                        err.kind(),
                        format!(
                            "{} when hard linking {} to {}",
                            err,
                            link_src.display(),
                            dst.display()
                        ),
                    )
                })?;
            } else {
                let overwrite = self.overwrite;
                match symlink(&src, dst).await {
                    Err(err_io) => {
                        if err_io.kind() == ErrorKind::AlreadyExists && overwrite {
                            // remove dest and try once more
                            let result = fs::remove_file(dst).await;
                            if result.is_ok() {
                                symlink(&src, dst).await
                            } else {
                                result
                            }
                        } else {
                            Err(err_io)
                        }
                    }
                    Ok(r) => Ok(r),
                }
                    .map_err(|err| {
                        Error::new(
                            err.kind(),
                            format!(
                                "{} when symlinking {} to {}",
                                err,
                                src.display(),
                                dst.display()
                            ),
                        )
                    })?;
                // While permissions on symlinks are meaningless on most systems, the ownership
                // of symlinks is important as it dictates the access control to the symlink
                // itself.
                if self.preserve_ownerships {
                    _set_ownerships(dst, &None, self.header.uid()?, self.header.gid()?)?;
                }
                if self.preserve_mtime {
                    if let Some(mtime) = get_mtime(&self.header) {
                        filetime::set_symlink_file_times(dst, mtime, mtime).map_err(|e| {
                            TarError::new(
                                &format!("failed to set mtime for `{}`", dst.display()),
                                e,
                            )
                        })?;
                    }
                }
            }
            return Ok(Unpacked::__Nonexhaustive);

            #[cfg(target_arch = "wasm32")]
            #[allow(unused_variables)]
            async fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::Other, "Not implemented"))
            }

            #[cfg(windows)]
            async fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
                async_std::os::windows::fs::symlink_file(src, dst).await
            }

            #[cfg(any(unix, target_os = "redox"))]
            async fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
                symlink_file(src, dst).await
            }
        } else if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            return Ok(Unpacked::__Nonexhaustive);
        };

        // Old BSD-tar compatibility.
        // Names that have a trailing slash should be treated as a directory.
        // Only applies to old headers.
        if self.header.as_ustar().is_none() && self.path_bytes().ends_with(b"/") {
            self.unpack_dir(dst).await?;
            set_perms_ownerships(
                dst,
                None,
                &self.header,
                self.mask,
                self.preserve_permissions,
                self.preserve_ownerships,
            ).await?;

            return Ok(Unpacked::__Nonexhaustive);
        }

        // Note the lack of `else` clause above. According to the FreeBSD
        // documentation:
        //
        // > A POSIX-compliant implementation must treat any unrecognized
        // > typeflag value as a regular file.
        //
        // As a result if we don't recognize the kind we just write out the file
        // as we would normally.

        // Ensure we write a new file rather than overwriting in-place which
        // is attackable; if an existing file is found unlink it.
        async fn open(dst: &Path) -> io::Result<fs::File> {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dst)
                .await
        }
        let mut f = async {
            let mut f = match open(dst).await {
                Ok(f) => Ok(f),
                Err(err) => {
                    if err.kind() == ErrorKind::AlreadyExists {
                        match fs::remove_file(dst).await {
                            Ok(()) => open(dst).await,
                            Err(ref e) if e.kind() == io::ErrorKind::NotFound => open(dst).await,
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(err)
                    }
                }
            }?;
            for io in self.data.drain(..) {
                match io {
                    EntryIo::Data(mut d) => {
                        let expected = d.limit();
                        if io::copy(&mut d, &mut f).await? != expected {
                            return Err(other("failed to write entire file"));
                        }
                    }
                    EntryIo::Pad(d) => {
                        // TODO: checked cast to i64
                        let to = SeekFrom::Current(d.limit() as i64);
                        let size = f.seek(to).await?;
                        f.set_len(size).await?;
                    }
                }
            }
            Ok::<fs::File, io::Error>(f)
        }
        .await
        .map_err(|e| {
            let header = self.header.path_bytes();
            TarError::new(
                &format!(
                    "failed to unpack `{}` into `{}`",
                    String::from_utf8_lossy(&header),
                    dst.display()
                ),
                e,
            )
        })?;

        if self.preserve_mtime {
            if let Ok(mtime) = self.header.mtime() {
                let mtime = FileTime::from_unix_time(mtime as i64, 0);
                filetime::set_file_times(dst, mtime, mtime).map_err(|e| {
                    TarError::new(&format!("failed to set mtime for `{}`", dst.display()), e)
                })?;
            }
        }

        set_perms_ownerships(
            dst,
            Some(&mut f),
            &self.header,
            self.mask,
            self.preserve_permissions,
            self.preserve_ownerships,
        ).await?;

        if self.unpack_xattrs {
            set_xattrs(self, dst).await?;
        }
        return Ok(Unpacked::File(f));

        fn get_mtime(header: &Header) -> Option<FileTime> {
            header.mtime().ok().map(|mtime| {
                // For some more information on this see the comments in
                // `Header::fill_platform_from`, but the general idea is that
                // we're trying to avoid 0-mtime files coming out of archives
                // since some tools don't ingest them well. Perhaps one day
                // when Cargo stops working with 0-mtime archives we can remove
                // this.
                let mtime = if mtime == 0 { 1 } else { mtime };
                FileTime::from_unix_time(mtime as i64, 0)
            })
        }

        fn set_ownerships(
            dst: &Path,
            f: &Option<&mut fs::File>,
            uid: u64,
            gid: u64,
        ) -> Result<(), TarError> {
            _set_ownerships(dst, f, uid, gid).map_err(|e| {
                TarError::new(
                    &format!(
                        "failed to set ownerships to uid={:?}, gid={:?} \
                         for `{}`",
                        uid,
                        gid,
                        dst.display()
                    ),
                    e,
                )
            })
        }

        #[cfg(unix)]
        fn _set_ownerships(
            dst: &Path,
            f: &Option<&mut fs::File>,
            uid: u64,
            gid: u64,
        ) -> io::Result<()> {
            use std::os::unix::prelude::*;

            let uid: libc::uid_t = uid
                .try_into()
                .map_err(|_| Error::new(ErrorKind::Other, format!("UID {} is too large!", uid)))?;
            let gid: libc::gid_t = gid
                .try_into()
                .map_err(|_| Error::new(ErrorKind::Other, format!("GID {} is too large!", gid)))?;
            match f {
                Some(f) => unsafe {
                    let fd = f.as_raw_fd();
                    if libc::fchown(fd, uid, gid) != 0 {
                        Err(Error::last_os_error())
                    } else {
                        Ok(())
                    }
                },
                None => unsafe {
                    let path = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|e| {
                        Error::new(
                            ErrorKind::Other,
                            format!("path contains null character: {:?}", e),
                        )
                    })?;
                    if libc::lchown(path.as_ptr(), uid, gid) != 0 {
                        Err(Error::last_os_error())
                    } else {
                        Ok(())
                    }
                },
            }
        }

        // Windows does not support posix numeric ownership IDs
        #[cfg(any(windows, target_arch = "wasm32"))]
        fn _set_ownerships(
            _: &Path,
            _: &Option<&mut std::fs::File>,
            _: u64,
            _: u64,
        ) -> io::Result<()> {
            Ok(())
        }

        async fn set_perms(
            dst: &Path,
            f: Option<&mut fs::File>,
            mode: u32,
            mask: u32,
            preserve: bool,
        ) -> Result<(), TarError> {
            _set_perms(dst, f, mode, mask, preserve).await.map_err(|e| {
                TarError::new(
                    &format!(
                        "failed to set permissions to {:o} \
                         for `{}`",
                        mode,
                        dst.display()
                    ),
                    e,
                )
            })
        }

        #[cfg(unix)]
        async fn _set_perms(
            dst: &Path,
            f: Option<&mut fs::File>,
            mode: u32,
            mask: u32,
            preserve: bool,
        ) -> io::Result<()> {
            use std::os::unix::prelude::*;

            let mode = if preserve { mode } else { mode & 0o777 };
            let mode = mode & !mask;
            let perm = std::fs::Permissions::from_mode(mode as _);
            match f {
                Some(f) => f.set_permissions(perm).await,
                None => fs::set_permissions(dst, perm).await,
            }
        }

        #[cfg(windows)]
        fn _set_perms(
            dst: &Path,
            f: Option<&mut fs::File>,
            mode: u32,
            _mask: u32,
            _preserve: bool,
        ) -> io::Result<()> {
            if mode & 0o200 == 0o200 {
                return Ok(());
            }
            match f {
                Some(f) => {
                    let mut perm = f.metadata()?.permissions();
                    perm.set_readonly(true);
                    f.set_permissions(perm)
                }
                None => {
                    let mut perm = fs::metadata(dst)?.permissions();
                    perm.set_readonly(true);
                    fs::set_permissions(dst, perm)
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        #[allow(unused_variables)]
        fn _set_perms(
            dst: &Path,
            f: Option<&mut fs::File>,
            mode: u32,
            mask: u32,
            _preserve: bool,
        ) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Other, "Not implemented"))
        }

        #[cfg(all(unix, feature = "xattr"))]
        async fn set_xattrs<'a>(
            me: &mut EntryFields<'a>,
            dst: &Path,
        ) -> io::Result<()> {
            use std::{ffi::OsStr, os::unix::prelude::*};

            let exts = match me.pax_extensions().await {
                Ok(Some(e)) => e,
                _ => return Ok(()),
            };
            let exts = exts
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let key = e.key_bytes();
                    let prefix = crate::pax::PAX_SCHILYXATTR.as_bytes();
                    key.strip_prefix(prefix).map(|rest| (rest, e))
                })
                .map(|(key, e)| (OsStr::from_bytes(key), e.value_bytes()));

            for (key, value) in exts {
                xattr::set(dst, key, value).map_err(|e| {
                    TarError::new(
                        &format!(
                            "failed to set extended \
                             attributes to {}. \
                             Xattrs: key={:?}, value={:?}.",
                            dst.display(),
                            key,
                            String::from_utf8_lossy(value)
                        ),
                        e,
                    )
                })?;
            }

            Ok(())
        }
        // Windows does not completely support posix xattrs
        // https://en.wikipedia.org/wiki/Extended_file_attributes#Windows_NT
        #[cfg(any(
            windows,
            target_os = "redox",
            not(feature = "xattr"),
            target_arch = "wasm32"
        ))]
        async fn set_xattrs<R: Read + Unpin>(_: &mut EntryFields<R>, _: &Path) -> io::Result<()> {
            Ok(())
        }
    }

    async fn ensure_dir_created(&self, dst: &Path, dir: &Path) -> io::Result<()> {
        let mut ancestor = dir;
        let mut dirs_to_create = Vec::new();
        while ancestor.symlink_metadata().is_err() {
            dirs_to_create.push(ancestor);
            if let Some(parent) = ancestor.parent() {
                ancestor = parent;
            } else {
                break;
            }
        }
        for ancestor in dirs_to_create.into_iter().rev() {
            if let Some(parent) = ancestor.parent() {
                self.validate_inside_dst(dst, parent).await?;
            }
            fs::create_dir(ancestor).await?;
        }
        Ok(())
    }

    async fn validate_inside_dst(&self, dst: &Path, file_dst: &Path) -> io::Result<PathBuf> {
        // Abort if target (canonical) parent is outside `dst`
        let canon_parent = file_dst.canonicalize().map_err(|err| {
            Error::new(
                err.kind(),
                format!("{} while canonicalizing {}", err, file_dst.display()),
            )
        })?;
        let canon_target = dst.canonicalize().map_err(|err| {
            Error::new(
                err.kind(),
                format!("{} while canonicalizing {}", err, dst.display()),
            )
        })?;
        if !canon_parent.starts_with(&canon_target) {
            let err = TarError::new(
                &format!(
                    "trying to unpack outside of destination path: {}",
                    canon_target.display()
                ),
                // TODO: use ErrorKind::InvalidInput here? (minor breaking change)
                Error::new(ErrorKind::Other, "Invalid argument"),
            );
            return Err(err.into());
        }
        Ok(canon_target)
    }
}

impl<'a> Read for EntryFields<'a> {
    #[cfg(feature = "async-std")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        into: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();
        loop {
            if this.read_state.is_none() {
                if this.data.as_ref().is_empty() {
                    *this.read_state = None;
                } else {
                    let data = &mut *this.data;
                    *this.read_state = Some(data.remove(0));
                }
            }

            if let Some(ref mut io) = &mut *this.read_state {
                let ret = Pin::new(io).poll_read(cx, into);
                match ret {
                    Poll::Ready(Ok(0)) => {
                        *this.read_state = None;
                        if this.data.as_ref().is_empty() {
                            return Poll::Ready(Ok(0));
                        }
                        continue;
                    }
                    Poll::Ready(Ok(val)) => {
                        return Poll::Ready(Ok(val));
                    }
                    Poll::Ready(Err(err)) => {
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                }
            }
            // Unable to pull another value from `data`, so we are done.
            return Poll::Ready(Ok(0));
        }
    }

    #[cfg(feature = "tokio")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();
        loop {
            if this.read_state.is_none() {
                if this.data.as_ref().is_empty() {
                    *this.read_state = None;
                } else {
                    let data = &mut *this.data;
                    *this.read_state = Some(data.remove(0));
                }
            }

            if let Some(ref mut io) = &mut *this.read_state {
                let prev_len = buf.filled().len();
                let ret = Pin::new(io).poll_read(cx, buf);
                match ret {
                    Poll::Ready(Ok(())) => {
                        let len = buf.filled().len();
                        if len == prev_len {
                            *this.read_state = None;
                            if this.data.as_ref().is_empty() {
                                return Poll::Ready(Ok(()));
                            }

                            continue;
                        } else {
                            return Poll::Ready(Ok(()));
                        }
                    }
                    Poll::Ready(Err(err)) => {
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                }
            }
            // Unable to pull another value from `data`, so we are done.
            return Poll::Ready(Ok(()));
        }
    }
}

impl<'a> Read for EntryIo<'a> {
    #[cfg(feature = "async-std")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        into: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.project() {
            EntryIoProject::Pad(io) => io.poll_read(cx, into),
            EntryIoProject::Data(io) => io.poll_read(cx, into),
        }
    }

    #[cfg(feature = "tokio")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            EntryIoProject::Pad(io) => io.poll_read(cx, buf),
            EntryIoProject::Data(io) => io.poll_read(cx, buf),
        }
    }
}

struct Guard<'a> {
    buf: &'a mut Vec<u8>,
    len: usize,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.buf.set_len(self.len);
        }
    }
}

fn poll_read_all_internal<R: Read + ?Sized>(
    mut rd: Pin<&mut R>,
    cx: &mut Context<'_>,
    buf: &mut Vec<u8>,
) -> Poll<io::Result<usize>> {
    let mut g = Guard {
        len: buf.len(),
        buf,
    };
    let ret;
    loop {
        if g.len == g.buf.len() {
            unsafe {
                g.buf.reserve(32);
                let capacity = g.buf.capacity();
                g.buf.set_len(capacity);

                let buf = &mut g.buf[g.len..];
                std::ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
            }
        }

        #[cfg(feature = "async-std")]
        {
            match ready!(rd.as_mut().poll_read(cx, &mut g.buf[g.len..])) {
                Ok(0) => {
                    ret = Poll::Ready(Ok(g.len));
                    break;
                }
                Ok(n) => g.len += n,
                Err(e) => {
                    ret = Poll::Ready(Err(e));
                    break;
                }
            }
        }

        #[cfg(feature = "tokio")]
        {
            let mut read_buf = ReadBuf::new(&mut g.buf);
            read_buf.set_filled(g.len);

            match ready!(rd.as_mut().poll_read(cx, &mut read_buf)) {
                Ok(()) => {
                    let len = read_buf.filled().len();
                    if len == g.len {
                        ret = Poll::Ready(Ok(g.len));
                        break;
                    } else {
                        g.len += len;
                    }
                }
                Err(e) => {
                    ret = Poll::Ready(Err(e));
                    break;
                }
            }
        }
    }

    ret
}
