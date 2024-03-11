use std::{cmp, marker, pin::Pin, sync::{Arc, Mutex}};
use std::io::SeekFrom;
#[cfg(feature = "async-std")]
use async_std::{
    fs,
    io::{self, Read, ReadExt, Seek},
};
#[cfg(feature = "tokio")]
use tokio::{
    fs,
    io::{self, AsyncRead as Read, AsyncReadExt, ReadBuf, AsyncSeek as Seek},
};

use crate::{
    entry::{EntryFields, EntryIo},
    error::TarError,
    other, Entry, GnuExtSparseHeader, GnuSparseHeader, Header,
};
use futures::{Stream, StreamExt};
use pin_project::pin_project;
use std::path::Path;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{ready, Context, Poll};
use crate::pax::{pax_extensions_value, PAX_GID, PAX_SIZE, PAX_UID};

/// A top-level representation of an archive file.
///
/// This archive can have an entry added to it and it can be iterated over.
#[derive(Debug)]
pub struct Archive<R: Read + Unpin + ?Sized> {
    inner: Arc<Mutex<ArchiveInner<R>>>,
}

impl<R: Read + Unpin> Clone for Archive<R> {
    fn clone(&self) -> Self {
        Archive {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ArchiveInner<R: Read + Unpin + ?Sized> {
    pos: AtomicU64,
    mask: u32,
    unpack_xattrs: bool,
    preserve_permissions: bool,
    preserve_ownerships: bool,
    preserve_mtime: bool,
    overwrite: bool,
    ignore_zeros: bool,
    buf: Mutex<Vec<u8>>,
    obj: Mutex<R>,
}

/// An iterator over the entries of an archive.
pub struct Entries<'a, R: 'a + Read + Unpin + Send> {
    raw: bool,
    fields: EntriesFields<'a>,
    _ignored: marker::PhantomData<&'a Archive<R>>,
}

trait SeekRead: Read + Seek {}
impl<R: Read + Seek> SeekRead for R {}

#[pin_project]
struct EntriesFields<'a> {
    archive: &'a Archive<dyn Read + Unpin + 'a>,
    seekable_archive: Option<&'a Archive<dyn SeekRead + Unpin + 'a>>,
    next: u64,
    done: bool,
    raw: bool,
    #[pin]
    fields: Option<EntryFields<'a>>,
    gnu_longname: Option<Vec<u8>>,
    gnu_longlink: Option<Vec<u8>>,
    pax_extensions: Option<Vec<u8>>,
}

/// Configure the archive.
pub struct ArchiveBuilder<R: Read + Unpin> {
    obj: R,
    unpack_xattrs: bool,
    preserve_permissions: bool,
    preserve_ownerships: bool,
    preserve_mtime: bool,
    overwrite: bool,
    ignore_zeros: bool,
}

impl<R: Read + Unpin> ArchiveBuilder<R> {
    /// Create a new builder.
    pub fn new(obj: R) -> Self {
        ArchiveBuilder {
            unpack_xattrs: false,
            preserve_permissions: false,
            preserve_ownerships: false,
            preserve_mtime: true,
            ignore_zeros: false,
            overwrite: true,
            obj,
        }
    }

    /// Indicate whether extended file attributes (xattrs on Unix) are preserved
    /// when unpacking this archive.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix using xattr support. This may eventually be implemented for
    /// Windows, however, if other archive implementations are found which do
    /// this as well.
    pub fn set_unpack_xattrs(mut self, unpack_xattrs: bool) -> Self {
        self.unpack_xattrs = unpack_xattrs;
        self
    }

    /// Indicate whether extended permissions (like suid on Unix) are preserved
    /// when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix.
    pub fn set_preserve_permissions(mut self, preserve: bool) -> Self {
        self.preserve_permissions = preserve;
        self
    }

    /// Indicate whether numeric ownership ids (like uid and gid on Unix)
    /// are preserved when unpacking this entry.
    ///
    /// This flag is disabled by default and is currently only implemented on
    /// Unix.
    pub fn set_preserve_ownerships(mut self, preserve: bool) -> Self {
        self.preserve_ownerships = preserve;
        self
    }

    /// Indicate whether access time information is preserved when unpacking
    /// this entry.
    ///
    /// This flag is enabled by default.
    pub fn set_preserve_mtime(mut self, preserve: bool) -> Self {
        self.preserve_mtime = preserve;
        self
    }

    /// Ignore zeroed headers, which would otherwise indicate to the archive that it has no more
    /// entries.
    ///
    /// This can be used in case multiple tar archives have been concatenated together.
    pub fn set_ignore_zeros(mut self, ignore_zeros: bool) -> Self {
        self.ignore_zeros = ignore_zeros;
        self
    }

    /// Construct the archive, ready to accept inputs.
    pub fn build(self) -> Archive<R> {
        let Self {
            unpack_xattrs,
            preserve_permissions,
            preserve_ownerships,
            preserve_mtime,
            ignore_zeros,
            overwrite,
            obj,
        } = self;

        Archive {
            inner: Arc::new(Mutex::new(ArchiveInner {
                mask: u32::MIN,
                unpack_xattrs,
                preserve_permissions,
                preserve_ownerships,
                preserve_mtime,
                ignore_zeros,
                overwrite,
                obj: Mutex::new(obj),
                buf: Mutex::new(Vec::new()),
                pos: AtomicU64::new(0),
            })),
        }
    }
}

impl<R: Read + Unpin + Send> Archive<R> {
    /// Create a new archive with the underlying object as the reader.
    pub fn new(obj: R) -> Archive<R> {
        Archive {
            inner: Arc::new(Mutex::new(ArchiveInner {
                mask: u32::MIN,
                unpack_xattrs: false,
                preserve_permissions: false,
                preserve_ownerships: false,
                preserve_mtime: true,
                ignore_zeros: false,
                overwrite: true,
                obj: Mutex::new(obj),
                buf: Mutex::new(Vec::new()),
                pos: AtomicU64::new(0),
            })),
        }
    }

    /// Unwrap this archive, returning the underlying object.
    pub fn into_inner(self) -> Result<R, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Ok(inner.into_inner().unwrap().obj),
            Err(inner) => Err(Self { inner }),
        }
    }

    /// Construct a stream over the entries in this archive.
    ///
    /// Note that care must be taken to consider each entry within an archive in
    /// sequence. If entries are processed out of sequence (from what the
    /// stream returns), then the contents read for each entry may be
    /// corrupted.
    pub fn entries(&mut self) -> io::Result<Entries<R>> {
        if self.inner.lock().unwrap().pos.load(Ordering::SeqCst) != 0 {
            return Err(other(
                "cannot call entries unless archive is at \
                 position 0",
            ));
        }

        Ok(Entries {
            raw: false,
            fields: EntriesFields {
                archive: self,
                seekable_archive: None,
                done: false,
                next: 0,
                raw: false,
                fields: None,
                gnu_longname: None,
                gnu_longlink: None,
                pax_extensions: None,
            },
            _ignored: marker::PhantomData,
        })
    }

    /// Construct a stream over the raw entries in this archive.
    ///
    /// Note that care must be taken to consider each entry within an archive in
    /// sequence. If entries are processed out of sequence (from what the
    /// stream returns), then the contents read for each entry may be
    /// corrupted.
    pub fn entries_raw(&mut self) -> io::Result<Entries<R>> {
        if self.inner.lock().unwrap().pos.load(Ordering::SeqCst) != 0 {
            return Err(other(
                "cannot call entries_raw unless archive is at \
                 position 0",
            ));
        }

        Ok(Entries {
            raw: true,
            fields: EntriesFields {
                archive: self,
                seekable_archive: None,
                done: false,
                next: 0,
                raw: false,
                fields: None,
                gnu_longname: None,
                gnu_longlink: None,
                pax_extensions: None,
            },
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
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    /// #
    /// use tokio::fs::File;
    /// use async_tar_rs::Archive;
    ///
    /// let mut ar = Archive::new(File::open("foo.tar").await?);
    /// ar.unpack("foo").await?;
    /// #
    /// # Ok(()) }
    /// ```
    pub async fn unpack<P: AsRef<Path>>(&mut self, dst: P) -> io::Result<()> {
        let mut entries = self.entries()?;
        let mut pinned = Pin::new(&mut entries);
        let dst = dst.as_ref();

        if dst.symlink_metadata().is_err() {
            fs::create_dir_all(&dst)
                .await
                .map_err(|e| TarError::new(&format!("failed to create `{}`", dst.display()), e))?;
        }

        // Canonicalizing the dst directory will prepend the path with '\\?\'
        // on windows which will allow windows APIs to treat the path as an
        // extended-length path with a 32,767 character limit. Otherwise all
        // unpacked paths over 260 characters will fail on creation with a
        // NotFound exception.
        let dst = &dst.canonicalize().unwrap_or_else(|_| dst.to_path_buf());

        // Delay any directory entries until the end (they will be created if needed by
        // descendants), to ensure that directory permissions do not interfer with descendant
        // extraction.
        let mut directories = Vec::new();
        while let Some(entry) = pinned.next().await {
            let mut file = entry.map_err(|e| TarError::new("failed to iterate over archive", e))?;
            if file.header().entry_type() == crate::EntryType::Directory {
                directories.push(file);
            } else {
                file.unpack_in(dst).await?;
            }
        }

        // Apply the directories.
        //
        // Note: the order of application is important to permissions. That is, we must traverse
        // the filesystem graph in topological ordering or else we risk not being able to create
        // child directories within those of more restrictive permissions. See [0] for details.
        //
        // [0]: <https://github.com/alexcrichton/tar-rs/issues/242>
        directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
        for mut dir in directories {
            dir.unpack_in(dst).await?;
        }

        Ok(())
    }
}

macro_rules! ready_opt_err {
    ($val:expr) => {
        match ::std::task::ready!($val) {
            Some(Ok(val)) => val,
            Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            None => return Poll::Ready(None),
        }
    };
}

macro_rules! ready_err {
    ($val:expr) => {
        match ::std::task::ready!($val) {
            Ok(val) => val,
            Err(err) => return Poll::Ready(Some(Err(err))),
        }
    };
}

impl<'a, R: Read + Send + Unpin> Stream for Entries<'a, R> {
    type Item = io::Result<Entry<'a, Archive<R>>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(
            futures::ready!(Pin::new(&mut self.fields).poll_next(cx))
                .map(|result| result.map(|e| EntryFields::from(e).into_entry())),
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
            futures::ready!(self.poll_skip(delta, cx))?;

            // EOF is an indicator that we are at the end of the archive.
            if !futures::ready!(
                pin!(&self.archive.inner).poll_try_read_all(cx, header.as_mut_bytes())
            )? {
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
    ) -> Poll<io::Result<Option<Entry<io::Empty>>>> {
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
                self.fields = match futures::ready!(
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

                self.gnu_longname = Some(futures::ready!(Pin::new(fields).poll_read_all(cx))?);
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

                self.gnu_longlink = Some(futures::ready!(Pin::new(fields).poll_read_all(cx))?);
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
                    Some(futures::ready!(Pin::new(fields).poll_read_all(cx))?);
                self.fields = None;
                continue;
            }

            futures::ready!(Self::poll_parse_sparse_header(
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
                    if !futures::ready!(
                        pin!(&archive.inner).poll_try_read_all(cx, ext.as_mut_bytes())
                    )? {
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
            #[cfg(feature = "tokio")]
            let _ = futures::ready!(pin!(&seekable_archive.inner).poll_complete(cx));
            let pos = SeekFrom::Current(
                i64::try_from(amt).map_err(|_| other("seek position out of bounds"))?,
            );

            #[cfg(feature = "tokio")]
            match pin!(&seekable_archive.inner).start_seek(pos) {
                Ok(_) => pin!(&seekable_archive.inner).poll_complete(cx),
                Err(e) => Poll::Ready(Err(e)),
            }

            #[cfg(feature = "async-std")]
            pin!(&seekable_archive.inner).poll_seek(cx, pos)
        } else {
            let mut rem = amt;
            let mut buf = [0u8; 4096 * 8];
            while rem > 0 {
                let n = cmp::min(amt, buf.len() as u64);
                #[cfg(feature = "tokio")]
                {
                    let mut buf = ReadBuf::new(&mut buf[..n as usize]);
                    match futures::ready!(pin!(&self.archive.inner).poll_read(cx, &mut buf)) {
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

                #[cfg(feature = "async-std")]
                {
                    let mut buf = &mut buf[..n as usize];
                    match futures::ready!(pin!(&self.archive.inner).poll_read(cx, &mut buf)) {
                        Ok(n) => {
                            if n == 0 {
                                return Poll::Ready(Err(other("unexpected EOF during skip")));
                            }

                            rem -= n as u64;
                        }
                        Err(e) => return Poll::Ready(Err(e)),
                    }
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
            match futures::ready!(self.poll_next_entry(cx)) {
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


impl<R: Read + Unpin> Read for Archive<R> {
    #[cfg(feature = "async-std")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        into: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut lock = self.inner.lock().unwrap();
        let mut inner = Pin::new(&mut *lock);
        let r = Pin::new(&mut inner.obj);

        let res = ready!(r.poll_read(cx, into));
        match res {
            Ok(i) => {
                inner.pos += i as u64;
                Poll::Ready(Ok(i))
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[cfg(feature = "tokio")]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut lock = self.inner.lock().unwrap();
        let mut inner = Pin::new(&mut *lock);
        let r = Pin::new(&mut inner.obj);

        let res = ready!(r.poll_read(cx, buf));
        match res {
            Ok(()) => {
                inner.pos += buf.filled().len() as u64;
                Poll::Ready(Ok(()))
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

/// Try to fill the buffer from the reader.
///
/// If the reader reaches its end before filling the buffer at all, returns `false`.
/// Otherwise, returns `true`.
fn poll_try_read_all<R: Read + Unpin>(
    mut source: R,
    cx: &mut Context<'_>,
    buf: &mut [u8],
    pos: &mut usize,
) -> Poll<io::Result<bool>> {
    #[cfg(feature = "async-std")]
    {
        while *pos < buf.len() {
            match ready!(Pin::new(&mut source).poll_read(cx, &mut buf[*pos..])) {
                Ok(0) => {
                    if *pos == 0 {
                        return Poll::Ready(Ok(false));
                    }

                    return Poll::Ready(Err(other("failed to read entire block")));
                }
                Ok(n) => *pos += n,
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    #[cfg(feature = "tokio")]
    {
        let mut read_buf = ReadBuf::new(buf);
        read_buf.set_filled(*pos);

        while read_buf.remaining() > 0 {
            match ready!(Pin::new(&mut source).poll_read(cx, &mut read_buf)) {
                Ok(()) => {
                    let len = read_buf.filled().len();
                    if len == 0 {
                        if *pos == 0 {
                            return Poll::Ready(Ok(false));
                        }

                        return Poll::Ready(Err(other("failed to read entire block")));
                    } else {
                        *pos += len;
                    }
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    *pos = 0;
    Poll::Ready(Ok(true))
}

/// Skip n bytes on the given source.
fn poll_skip<R: Read + Unpin>(
    mut source: R,
    cx: &mut Context<'_>,
    mut amt: u64,
) -> Poll<io::Result<()>> {
    let mut buf = [0u8; 4096 * 8];
    while amt > 0 {
        let n = cmp::min(amt, buf.len() as u64);

        #[cfg(feature = "async-std")]
        {
            match ready!(Pin::new(&mut source).poll_read(cx, &mut buf[..n as usize])) {
                Ok(0) => {
                    return Poll::Ready(Err(other("unexpected EOF during skip")));
                }
                Ok(n) => {
                    amt -= n as u64;
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        #[cfg(feature = "tokio")]
        {
            let start_pos = buf.len() - n as usize;
            let mut read_buf = ReadBuf::new(&mut buf);
            read_buf.set_filled(start_pos);

            match ready!(Pin::new(&mut source).poll_read(cx, &mut read_buf)) {
                Ok(()) => {
                    let len = read_buf.filled().len() - start_pos;
                    if len == 0 {
                        return Poll::Ready(Err(other("unexpected EOF during skip")));
                    } else {
                        amt -= len as u64;
                    }
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    Poll::Ready(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    assert_impl_all!(fs::File: Send, Sync);
    assert_impl_all!(Entries<fs::File>: Send, Sync);
    assert_impl_all!(Archive<fs::File>: Send, Sync);
    assert_impl_all!(Entry<Archive<fs::File>>: Send, Sync);
}
