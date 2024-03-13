use futures_core::Stream;
use pin_project::pin_project;
use std::io::SeekFrom;
use std::path::Path;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::{cmp, marker};
use tokio::fs;
use tokio::io::{self, AsyncRead as Read, AsyncReadExt, AsyncSeek as Seek, ReadBuf};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

use crate::entry::{EntryFields, EntryIo};
use crate::error::TarError;
use crate::other;
use crate::pax::*;
use crate::{Entry, GnuExtSparseHeader, GnuSparseHeader, Header};

/// A top-level representation of an archive file.
///
/// This archive can have an entry added to it and it can be iterated over.
pub struct Archive<R: ?Sized + Read + Send> {
    inner: ArchiveInner<R>,
}

pub struct ArchiveInner<R: ?Sized + Send> {
    pos: AtomicU64,
    mask: u32,
    unpack_xattrs: bool,
    preserve_permissions: bool,
    preserve_ownerships: bool,
    preserve_mtime: bool,
    overwrite: bool,
    ignore_zeros: bool,
    obj: Mutex<R>,
}

/// An iterator over the entries of an archive.
pub struct Entries<'a, R: 'a + Read + Send> {
    fields: EntriesFields<'a>,
    _ignored: marker::PhantomData<&'a Archive<R>>,
}

trait SeekRead: Read + Seek + Send + Unpin {}
impl<R: Read + Seek + Send + Unpin> SeekRead for R {}

#[pin_project]
struct EntriesFields<'a> {
    archive: &'a Archive<dyn Read + Send + Unpin + 'a>,
    seekable_archive: Option<&'a Archive<dyn SeekRead + 'a>>,
    next: u64,
    done: bool,
    raw: bool,
    #[pin]
    fields: Option<EntryFields<'a>>,
    gnu_longname: Option<Vec<u8>>,
    gnu_longlink: Option<Vec<u8>>,
    pax_extensions: Option<Vec<u8>>,
}

impl<R: Read + Send + Unpin> Archive<R> {
    /// Create a new archive with the underlying object as the reader.
    pub fn new(obj: R) -> Archive<R> {
        Archive {
            inner: ArchiveInner {
                mask: u32::MIN,
                unpack_xattrs: false,
                preserve_permissions: false,
                preserve_ownerships: false,
                preserve_mtime: true,
                overwrite: true,
                ignore_zeros: false,
                obj: Mutex::new(obj),
                pos: AtomicU64::new(0),
            },
        }
    }

    /// Unwrap this archive, returning the underlying object.
    pub fn into_inner(self) -> R {
        self.inner.obj.into_inner()
    }

    /// Construct an iterator over the entries in this archive.
    ///
    /// Note that care must be taken to consider each entry within an archive in
    /// sequence. If entries are processed out of sequence (from what the
    /// iterator returns), then the contents read for each entry may be
    /// corrupted.
    pub fn entries(&mut self) -> io::Result<Entries<R>> {
        let me: &mut Archive<dyn Read + Send + Unpin> = self;
        me._entries(None).map(|fields| Entries {
            fields,
            _ignored: marker::PhantomData,
        })
    }

    /// Unpacks the contents tarball into the specified `dst`.
    ///
    /// This function will iterate over the entire contents of this tarball,
    /// extracting each file in turn to the location specified by the entry's
    /// path name.
    ///
    /// This operation is relatively sensitive in that it will not write files
    /// outside of the path specified by `dst`. Files in the archive which have
    /// a '..' in their path are skipped during the unpacking process.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_tar_rs::Archive;
    /// use tokio::fs::File;
    ///
    /// # tokio_test::block_on(async {
    /// let mut ar = Archive::new(File::open("foo.tar").await.unwrap());
    /// ar.unpack("foo").await.unwrap();
    /// # })
    /// ```
    pub async fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<()> {
        let me: &mut Archive<dyn Read + Send + Unpin> = self;
        me._unpack(dst.as_ref()).await
    }

    /// Set the mask of the permission bits when unpacking this entry.
    ///
    /// The mask will be inverted when applying against a mode, similar to how
    /// `umask` works on Unix. In logical notation it looks like:
    ///
    /// ```text
    /// new_mode = old_mode & (~mask)
    /// ```
    ///
    /// The mask is 0 by default and is currently only implemented on Unix.
    pub fn set_mask(&mut self, mask: u32) {
        self.inner.mask = mask;
    }

    /// Indicate whether extended file attributes (xattrs on Unix) are preserved
    /// when unpacking this archive.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix using xattr support. This may eventually be implemented for
    /// Windows, however, if other archive implementations are found which do
    /// this as well.
    pub fn set_unpack_xattrs(&mut self, unpack_xattrs: bool) {
        self.inner.unpack_xattrs = unpack_xattrs;
    }

    /// Indicate whether extended permissions (like suid on Unix) are preserved
    /// when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix.
    pub fn set_preserve_permissions(&mut self, preserve: bool) {
        self.inner.preserve_permissions = preserve;
    }

    /// Indicate whether numeric ownership ids (like uid and gid on Unix)
    /// are preserved when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix.
    pub fn set_preserve_ownerships(&mut self, preserve: bool) {
        self.inner.preserve_ownerships = preserve;
    }

    /// Indicate whether files and symlinks should be overwritten on extraction.
    pub fn set_overwrite(&mut self, overwrite: bool) {
        self.inner.overwrite = overwrite;
    }

    /// Indicate whether access time information is preserved when unpacking
    /// this entry.
    ///
    /// This flag is enabled by default.
    pub fn set_preserve_mtime(&mut self, preserve: bool) {
        self.inner.preserve_mtime = preserve;
    }

    /// Ignore zeroed headers, which would otherwise indicate to the archive that it has no more
    /// entries.
    ///
    /// This can be used in case multiple tar archives have been concatenated together.
    pub fn set_ignore_zeros(&mut self, ignore_zeros: bool) {
        self.inner.ignore_zeros = ignore_zeros;
    }
}

impl<R: Seek + Read + Send + Unpin> Archive<R> {
    /// Construct an iterator over the entries in this archive for a seekable
    /// reader. Seek will be used to efficiently skip over file contents.
    ///
    /// Note that care must be taken to consider each entry within an archive in
    /// sequence. If entries are processed out of sequence (from what the
    /// iterator returns), then the contents read for each entry may be
    /// corrupted.
    pub fn entries_with_seek(&mut self) -> io::Result<Entries<R>> {
        let me: &Archive<dyn Read + Send + Unpin> = self;
        let me_seekable: &Archive<dyn SeekRead> = self;
        me._entries(Some(me_seekable)).map(|fields| Entries {
            fields,
            _ignored: marker::PhantomData,
        })
    }
}

impl Archive<dyn Read + Send + Unpin + '_> {
    fn _entries<'a>(
        &'a self,
        seekable_archive: Option<&'a Archive<dyn SeekRead + 'a>>,
    ) -> io::Result<EntriesFields<'a>> {
        if self.inner.pos.load(Ordering::SeqCst) != 0 {
            return Err(other(
                "cannot call entries unless archive is at \
                 position 0",
            ));
        }
        Ok(EntriesFields {
            archive: self,
            seekable_archive,
            done: false,
            next: 0,
            raw: false,
            fields: None,
            gnu_longname: None,
            gnu_longlink: None,
            pax_extensions: None,
        })
    }

    async fn _unpack(&mut self, dst: &Path) -> io::Result<()> {
        if dst.symlink_metadata().is_err() {
            fs::create_dir_all(&dst)
                .await
                .map_err(|e| TarError::new(format!("failed to create `{}`", dst.display()), e))?;
        }

        // Canonicalizing the dst directory will prepend the path with '\\?\'
        // on windows which will allow windows APIs to treat the path as an
        // extended-length path with a 32,767 character limit. Otherwise all
        // unpacked paths over 260 characters will fail on creation with a
        // NotFound exception.
        let dst = &dst.canonicalize().unwrap_or(dst.to_path_buf());

        // Delay any directory entries until the end (they will be created if needed by
        // descendants), to ensure that directory permissions do not interfer with descendant
        // extraction.
        let mut directories = Vec::new();
        let mut entries = self._entries(None)?;
        while let Some(entry) = entries.next().await {
            let mut file = entry.map_err(|e| TarError::new("failed to iterate over archive", e))?;
            if file.header().entry_type() == crate::EntryType::Directory {
                directories.push(file);
            } else {
                file.unpack_in(dst).await?;
            }
        }
        for mut dir in directories {
            dir.unpack_in(dst).await?;
        }

        Ok(())
    }
}

impl<'a, R: Read + Send> Entries<'a, R> {
    /// Indicates whether this iterator will return raw entries or not.
    ///
    /// If the raw list of entries are returned, then no preprocessing happens
    /// on account of this library, for example taking into account GNU long name
    /// or long link archive members. Raw iteration is disabled by default.
    pub fn raw(self, raw: bool) -> Entries<'a, R> {
        Entries {
            fields: EntriesFields { raw, ..self.fields },
            _ignored: marker::PhantomData,
        }
    }
}

impl<'a, R: Read + Send> Stream for Entries<'a, R> {
    type Item = io::Result<Entry<'a, R>>;

    /// Get the next entry, if one.
    /// This replaces the iterator interface, and it's simpler than implementing Stream.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(
            match futures_core::ready!(Pin::new(&mut self.fields).poll_next(cx)) {
                Some(result) => Some(result.map(|e| EntryFields::from(e).into_entry())),
                None => None,
            },
        )
    }
}

impl<'a> EntriesFields<'a> {
    fn poll_next_entry_raw(
        &mut self,
        pax_extensions: Option<&[u8]>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Option<Entry<'a, io::Empty>>>> {
        let mut header = Header::new_old();
        let mut header_pos = self.next;
        loop {
            // Seek to the start of the next header in the archive
            let delta = self.next - self.archive.inner.pos.load(Ordering::SeqCst);
            futures_core::ready!(self.poll_skip(delta, cx))?;

            // EOF is an indicator that we are at the end of the archive.
            if !futures_core::ready!(poll_try_read_all(
                &mut &self.archive.inner,
                cx,
                &mut ReadBuf::new(header.as_mut_bytes())
            ))? {
                return Poll::Ready(Ok(None));
            }

            // If a header is not all zeros, we have another valid header.
            // Otherwise, check if we are ignoring zeros and continue, or break as if this is the
            // end of the archive.
            if !header.as_bytes().iter().all(|i| *i == 0) {
                self.next += 512;
                break;
            }

            if !self.archive.inner.ignore_zeros {
                return Poll::Ready(Ok(None));
            }
            self.next += 512;
            header_pos = self.next;
        }

        // Make sure the checksum is ok
        let sum = header.as_bytes()[..148]
            .iter()
            .chain(&header.as_bytes()[156..])
            .fold(0, |a, b| a + (*b as u32))
            + 8 * 32;
        let cksum = header.cksum()?;
        if sum != cksum {
            return Poll::Ready(Err(other("archive header checksum mismatch")));
        }

        let mut pax_size: Option<u64> = None;
        if let Some(pax_extensions_ref) = &pax_extensions {
            pax_size = pax_extensions_value(pax_extensions_ref, PAX_SIZE);

            if let Some(pax_uid) = pax_extensions_value(pax_extensions_ref, PAX_UID) {
                header.set_uid(pax_uid);
            }

            if let Some(pax_gid) = pax_extensions_value(pax_extensions_ref, PAX_GID) {
                header.set_gid(pax_gid);
            }
        }

        let file_pos = self.next;
        let mut size = header.entry_size()?;
        if size == 0 {
            if let Some(pax_size) = pax_size {
                size = pax_size;
            }
        }
        let ret = EntryFields {
            size,
            header_pos,
            file_pos,
            data: vec![EntryIo::Data((&self.archive.inner).take(size))],
            header,
            long_pathname: None,
            long_linkname: None,
            pax_extensions: None,
            mask: self.archive.inner.mask,
            unpack_xattrs: self.archive.inner.unpack_xattrs,
            preserve_permissions: self.archive.inner.preserve_permissions,
            preserve_mtime: self.archive.inner.preserve_mtime,
            overwrite: self.archive.inner.overwrite,
            preserve_ownerships: self.archive.inner.preserve_ownerships,
            read_state: None,
        };

        // Store where the next entry is, rounding up by 512 bytes (the size of
        // a header);
        let size = size
            .checked_add(511)
            .ok_or_else(|| other("size overflow"))?;
        self.next = self
            .next
            .checked_add(size & !(512 - 1))
            .ok_or_else(|| other("size overflow"))?;

        Poll::Ready(Ok(Some(ret.into_entry())))
    }

    fn poll_next_entry(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Option<Entry<'a, io::Empty>>>> {
        if self.raw {
            return self.poll_next_entry_raw(None, cx);
        }

        let mut processed = 0;
        loop {
            let fields = if let Some(fields) = self.fields.as_mut() {
                fields
            } else {
                processed += 1;
                let pax_extensions = self.pax_extensions.clone();
                self.fields = match futures_core::ready!(
                    self.poll_next_entry_raw(pax_extensions.as_deref(), cx)
                )? {
                    Some(entry) => Some(EntryFields::from(entry)),
                    None if processed > 1 => {
                        return Poll::Ready(Err(other(
                            "members found describing a future member but no future member found",
                        )));
                    }
                    None => return Poll::Ready(Ok(None)),
                };
                continue;
            };

            let is_recognized_header =
                fields.header.as_gnu().is_some() || fields.header.as_ustar().is_some();

            if is_recognized_header && fields.header.entry_type().is_gnu_longname() {
                if fields.long_pathname.is_some() {
                    return Poll::Ready(Err(other(
                        "two long name entries describing \
                         the same member",
                    )));
                }

                self.gnu_longname = Some(futures_core::ready!(Pin::new(fields).poll_read_all(cx))?);
                self.fields = None;
                continue;
            }

            if is_recognized_header && fields.header.entry_type().is_gnu_longlink() {
                if fields.long_linkname.is_some() {
                    return Poll::Ready(Err(other(
                        "two long name entries describing \
                         the same member",
                    )));
                }

                self.gnu_longlink = Some(futures_core::ready!(Pin::new(fields).poll_read_all(cx))?);
                self.fields = None;
                continue;
            }

            if is_recognized_header && fields.header.entry_type().is_pax_local_extensions() {
                if fields.pax_extensions.is_some() {
                    return Poll::Ready(Err(other(
                        "two pax extensions entries describing \
                         the same member",
                    )));
                }

                self.pax_extensions =
                    Some(futures_core::ready!(Pin::new(fields).poll_read_all(cx))?);
                self.fields = None;
                continue;
            }

            futures_core::ready!(Self::poll_parse_sparse_header(
                &mut self.next,
                self.archive,
                fields,
                cx
            ))?;

            fields.long_pathname = self.gnu_longname.take();
            fields.long_linkname = self.gnu_longlink.take();
            fields.pax_extensions = self.pax_extensions.take();

            return Poll::Ready(Ok(Some(self.fields.take().unwrap().into_entry())));
        }
    }

    fn poll_parse_sparse_header(
        next: &mut u64,
        archive: &'a Archive<dyn Read + Send + Unpin + 'a>,
        entry: &mut EntryFields<'a>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if !entry.header.entry_type().is_gnu_sparse() {
            return Poll::Ready(Ok(()));
        }
        let gnu = match entry.header.as_gnu() {
            Some(gnu) => gnu,
            None => return Poll::Ready(Err(other("sparse entry type listed but not GNU header"))),
        };

        // Sparse files are represented internally as a list of blocks that are
        // read. Blocks are either a bunch of 0's or they're data from the
        // underlying archive.
        //
        // Blocks of a sparse file are described by the `GnuSparseHeader`
        // structure, some of which are contained in `GnuHeader` but some of
        // which may also be contained after the first header in further
        // headers.
        //
        // We read off all the blocks here and use the `add_block` function to
        // incrementally add them to the list of I/O block (in `entry.data`).
        // The `add_block` function also validates that each chunk comes after
        // the previous, we don't overrun the end of the file, and each block is
        // aligned to a 512-byte boundary in the archive itself.
        //
        // At the end we verify that the sparse file size (`Header::size`) is
        // the same as the current offset (described by the list of blocks) as
        // well as the amount of data read equals the size of the entry
        // (`Header::entry_size`).
        entry.data.truncate(0);

        let mut cur = 0;
        let mut remaining = entry.size;
        {
            let data = &mut entry.data;
            let reader = &archive.inner;
            let size = entry.size;
            let mut add_block = |block: &GnuSparseHeader| -> io::Result<_> {
                if block.is_empty() {
                    return Ok(());
                }
                let off = block.offset()?;
                let len = block.length()?;
                if len != 0 && (size - remaining) % 512 != 0 {
                    return Err(other(
                        "previous block in sparse file was not \
                         aligned to 512-byte boundary",
                    ));
                } else if off < cur {
                    return Err(other(
                        "out of order or overlapping sparse \
                         blocks",
                    ));
                } else if cur < off {
                    let block = io::repeat(0).take(off - cur);
                    data.push(EntryIo::Pad(block));
                }
                cur = off
                    .checked_add(len)
                    .ok_or_else(|| other("more bytes listed in sparse file than u64 can hold"))?;
                remaining = remaining.checked_sub(len).ok_or_else(|| {
                    other(
                        "sparse file consumed more data than the header \
                         listed",
                    )
                })?;
                data.push(EntryIo::Data(reader.take(len)));
                Ok(())
            };
            for block in gnu.sparse.iter() {
                add_block(block)?
            }
            if gnu.is_extended() {
                let mut ext = GnuExtSparseHeader::new();
                ext.isextended[0] = 1;
                while ext.is_extended() {
                    if !futures_core::ready!(poll_try_read_all(
                        &mut &archive.inner,
                        cx,
                        &mut ReadBuf::new(ext.as_mut_bytes())
                    ))? {
                        return Poll::Ready(Err(other("failed to read extension")));
                    }

                    *next += 512;
                    for block in ext.sparse.iter() {
                        add_block(block)?;
                    }
                }
            }
        }
        if cur != gnu.real_size()? {
            return Poll::Ready(Err(other(
                "mismatch in sparse file chunks and \
                 size in header",
            )));
        }
        entry.size = cur;
        if remaining > 0 {
            return Poll::Ready(Err(other(
                "mismatch in sparse file chunks and \
                 entry size in header",
            )));
        }

        Poll::Ready(Ok(()))
    }

    fn poll_skip(&mut self, amt: u64, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        if let Some(seekable_archive) = self.seekable_archive {
            let _ = futures_core::ready!(pin!(&seekable_archive.inner).poll_complete(cx));

            let pos = SeekFrom::Current(
                i64::try_from(amt).map_err(|_| other("seek position out of bounds"))?,
            );

            match pin!(&seekable_archive.inner).start_seek(pos) {
                Ok(_) => pin!(&seekable_archive.inner).poll_complete(cx),
                Err(e) => Poll::Ready(Err(e)),
            }
        } else {
            let mut rem = amt;
            let mut buf = [0u8; 4096 * 8];
            while rem > 0 {
                let n = cmp::min(amt, buf.len() as u64);
                let mut buf = ReadBuf::new(&mut buf[..n as usize]);
                match futures_core::ready!(pin!(&self.archive.inner).poll_read(cx, &mut buf)) {
                    Ok(_) => {
                        let n = buf.filled().len();
                        if n == 0 {
                            return Poll::Ready(Err(other("unexpected EOF during skip")));
                        }
                        rem -= n as u64;
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }

            Poll::Ready(Ok(amt))
        }
    }
}

impl<'a> Stream for EntriesFields<'a> {
    type Item = io::Result<Entry<'a, io::Empty>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            Poll::Ready(None)
        } else {
            match futures_core::ready!(self.poll_next_entry(cx)) {
                Ok(Some(e)) => Poll::Ready(Some(Ok(e))),
                Ok(None) => {
                    self.done = true;
                    Poll::Ready(None)
                }
                Err(e) => {
                    self.done = true;
                    Poll::Ready(Some(Err(e)))
                }
            }
        }
    }
}

impl<'a, R: ?Sized + Read + Send + Unpin> Read for &'a ArchiveInner<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Ok(inner) = self.obj.try_lock() else {
            return Poll::Pending;
        };

        let mut pinned = Pin::new(inner);
        let res = futures_core::ready!(pinned.as_mut().poll_read(cx, buf));
        match res {
            Ok(()) => {
                self.pos
                    .fetch_add(buf.filled().len() as u64, Ordering::SeqCst);
                Poll::Ready(Ok(()))
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl<'a, R: ?Sized + Seek + Send + Unpin> Seek for &'a ArchiveInner<R> {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let Ok(inner) = self.obj.try_lock() else {
            return Err(io::Error::other("another operation is pending"));
        };

        let mut pinned = Pin::new(inner);
        pinned.as_mut().start_seek(position)
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        if let Ok(inner) = self.obj.try_lock() {
            let mut pinned = Pin::new(inner);
            Poll::Ready(futures_core::ready!(pinned.as_mut().poll_complete(cx)))
        } else {
            Poll::Pending
        }
    }
}

/// Try to fill the buffer from the reader.
///
/// If the reader reaches its end before filling the buffer at all, returns `false`.
/// Otherwise, returns `true`.
fn poll_try_read_all<R: Read + Send + Unpin>(
    r: &mut R,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<bool>> {
    let prev_read = buf.filled().len();
    if let Err(e) = futures_core::ready!(pin!(r).poll_read(cx, buf)) {
        return Poll::Ready(Err(e));
    }

    if buf.filled().len() == prev_read {
        if prev_read == 0 {
            Poll::Ready(Ok(false))
        } else {
            Poll::Ready(Err(other("failed to read entire block")))
        }
    } else {
        Poll::Ready(Ok(true))
    }
}
