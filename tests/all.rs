extern crate async_tar_rs;
extern crate filetime;
extern crate tempfile;
#[cfg(all(unix, feature = "xattr"))]
extern crate xattr;

#[cfg(feature = "async-std")]
use async_std::{
    self as runtime,
    fs::{self, File},
    io::{self, Read, ReadExt as _, Write, WriteExt as _, Seek},
};
use std::iter::repeat;
use std::path::{Path, PathBuf};
#[cfg(feature = "tokio")]
use tokio::{
    self as runtime,
    fs::{self, File},
    io::{self, AsyncRead as Read, AsyncReadExt as _, AsyncWrite as Write, AsyncWriteExt as _, AsyncSeekExt as Seek},
};

use async_tar_rs::{Archive, ArchiveBuilder, Builder, Entries, EntryType, Header};
use filetime::FileTime;
use futures::{StreamExt, TryStreamExt};
use tempfile::{Builder as TempBuilder, TempDir};

macro_rules! t {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => panic!("{} returned {}", stringify!($e), e),
        }
    };
}

macro_rules! tar {
    ($e:expr) => {
        &include_bytes!(concat!("archives/", $e))[..]
    };
}

mod header;

/// test that we can concatenate the simple.tar archive and extract the same entries twice when we
/// use the ignore_zeros option.
#[runtime::test]
async fn simple_concat() {
    let bytes = tar!("simple.tar");
    let mut archive_bytes = Vec::from(bytes);

    let original_names: Vec<String> = decode_names(Archive::new(archive_bytes.as_slice())).await;
    let expected: Vec<&str> = original_names.iter().map(|n| n.as_str()).collect();

    // concat two archives (with null in-between);
    archive_bytes.extend(bytes);

    // test now that when we read the archive, it stops processing at the first zero header.
    let actual = decode_names(Archive::new(archive_bytes.as_slice())).await;
    assert_eq!(expected, actual);

    // extend expected by itself.
    let expected: Vec<&str> = {
        let mut o = Vec::new();
        o.extend(&expected);
        o.extend(&expected);
        o
    };

    let builder = ArchiveBuilder::new(archive_bytes.as_slice()).set_ignore_zeros(true);
    let ar = builder.build();

    let actual = decode_names(ar).await;
    assert_eq!(expected, actual);

    async fn decode_names<R>(ar: Archive<R>) -> Vec<String>
    where
        R: Read + Unpin + Sync + Send,
    {
        let mut names = Vec::new();
        let mut entries = t!(ar.entries());

        while let Ok(Some(entry)) = entries.try_next().await {
            names.push(t!(::std::str::from_utf8(&entry.path_bytes())).to_string());
        }

        names
    }
}

#[runtime::test]
async fn header_impls() {
    let ar = Archive::new(tar!("simple.tar"));
    let hn = Header::new_old();
    let hnb = hn.as_bytes();
    let mut entries = t!(ar.entries());
    while let Ok(Some(file)) = entries.try_next().await {
        let h1 = file.header();
        let h1b = h1.as_bytes();
        let h2 = h1.clone();
        let h2b = h2.as_bytes();
        assert!(h1b[..] == h2b[..] && h2b[..] != hnb[..])
    }
}

#[runtime::test]
async fn header_impls_missing_last_header() {
    let ar = Archive::new(tar!("simple_missing_last_header.tar"));
    let hn = Header::new_old();
    let hnb = hn.as_bytes();
    let mut entries = t!(ar.entries());

    while let Ok(Some(file)) = entries.try_next().await {
        let h1 = file.header();
        let h1b = h1.as_bytes();
        let h2 = h1.clone();
        let h2b = h2.as_bytes();
        assert!(h1b[..] == h2b[..] && h2b[..] != hnb[..])
    }
}

#[runtime::test]
async fn reading_files() {
    let rdr = tar!("reading_files.tar");
    let ar = Archive::new(rdr);
    let mut entries = t!(ar.entries());

    let mut a = t!(entries.try_next().await).unwrap();
    assert_eq!(&*a.header().path_bytes(), b"a");
    let mut s = String::new();
    t!(a.read_to_string(&mut s).await);
    assert_eq!(s, "a\na\na\na\na\na\na\na\na\na\na\n");

    let mut b = t!(entries.try_next().await).unwrap();
    assert_eq!(&*b.header().path_bytes(), b"b");
    s.truncate(0);
    t!(b.read_to_string(&mut s).await);
    assert_eq!(s, "b\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\n");

    assert!(entries.try_next().await.unwrap().is_none());
}

#[runtime::test]
async fn writing_files() {
    let mut ar = Builder::new(Vec::new());
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let path = td.path().join("test");
    t!(t!(File::create(&path).await).write_all(b"test").await);

    t!(ar
        .append_file("test2", &mut t!(File::open(&path).await))
        .await);

    let data = t!(ar.into_inner().await);
    let ar = Archive::new(data.as_slice());
    let mut entries = t!(ar.entries());
    let mut f = t!(entries.try_next().await).unwrap();

    assert_eq!(&*f.header().path_bytes(), b"test2");
    assert_eq!(f.header().size().unwrap(), 4);
    let mut s = String::new();
    t!(f.read_to_string(&mut s).await);
    assert_eq!(s, "test");

    assert!(entries.try_next().await.unwrap().is_none());
}

#[runtime::test]
async fn large_filename() {
    let mut ar = Builder::new(Vec::new());
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let path = td.path().join("test");
    t!(t!(File::create(&path).await).write_all(b"test").await);

    let filename = "abcd/".repeat(50);
    let mut header = Header::new_ustar();
    header.set_path(&filename).unwrap();
    header.set_metadata(&t!(fs::metadata(&path).await));
    header.set_cksum();
    t!(ar.append(&header, &b"test"[..]).await);
    let too_long = "abcd".repeat(200);
    t!(ar
        .append_file(&too_long, &mut t!(File::open(&path).await))
        .await);
    t!(ar.append_data(&mut header, &too_long, &b"test"[..]).await);

    let rd = t!(ar.into_inner().await);
    let ar = Archive::new(rd.as_slice());
    let mut entries = t!(ar.entries());

    // The short entry added with `append`
    let mut f = entries.try_next().await.unwrap().unwrap();
    assert_eq!(&*f.header().path_bytes(), filename.as_bytes());
    assert_eq!(f.header().size().unwrap(), 4);
    let mut s = String::new();
    t!(f.read_to_string(&mut s).await);
    assert_eq!(s, "test");

    // The long entry added with `append_file`
    let mut f = entries.try_next().await.unwrap().unwrap();
    assert_eq!(&*f.path_bytes(), too_long.as_bytes());
    assert_eq!(f.header().size().unwrap(), 4);
    let mut s = String::new();
    t!(f.read_to_string(&mut s).await);
    assert_eq!(s, "test");

    // The long entry added with `append_data`
    let mut f = entries.try_next().await.unwrap().unwrap();
    assert!(f.header().path_bytes().len() < too_long.len());
    assert_eq!(&*f.path_bytes(), too_long.as_bytes());
    assert_eq!(f.header().size().unwrap(), 4);
    let mut s = String::new();
    t!(f.read_to_string(&mut s).await);
    assert_eq!(s, "test");

    assert!(entries.try_next().await.unwrap().is_none());
}

async fn reading_entries_common<R: Read + Unpin + Send>(mut entries: Entries<R>) {
    let mut a = t!(entries.try_next().await).unwrap();
    assert_eq!(&*a.header().path_bytes(), b"a");
    let mut s = String::new();
    t!(a.read_to_string(&mut s).await);
    assert_eq!(s, "a\na\na\na\na\na\na\na\na\na\na\n");
    s.truncate(0);
    t!(a.read_to_string(&mut s).await);
    assert_eq!(s, "");

    let mut b = t!(entries.try_next().await).unwrap();
    assert_eq!(&*b.header().path_bytes(), b"b");
    s.truncate(0);
    t!(b.read_to_string(&mut s).await);
    assert_eq!(s, "b\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\n");
    assert!(entries.try_next().await.unwrap().is_none());
}

#[runtime::test]
async fn reading_entries() {
    let rdr = tar!("reading_files.tar");
    let ar = Archive::new(rdr);
    reading_entries_common(t!(ar.entries())).await;
}

// #[runtime::test]
// async fn reading_entries_with_seek() {
//     let rdr = tar!("reading_files.tar");
//     let mut ar = Archive::new(rdr);
//     reading_entries_common(ar.entries_with_seek().unwrap()).await;
// }

// #[pin_project]
// struct LoggingReader<R> {
//     #[pin]
//     inner: R,
//     read_bytes: u64,
// }
//
// impl<R> LoggingReader<R> {
//     fn new(reader: R) -> LoggingReader<R> {
//         LoggingReader {
//             inner: reader,
//             read_bytes: 0,
//         }
//     }
// }
//
// impl<T: Read> Read for LoggingReader<T> {
//     #[cfg(feature = "tokio")]
//     fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
//         let buf_filled_len = buf.filled().len();
//         let this = self.project();
//         match ready!(this.inner.poll_read(cx, buf)) {
//             Ok(_) => {
//                 let current_len = buf.filled().len();
//                 if buf_filled_len != current_len {
//                     *this.read_bytes += (current_len - buf_filled_len) as u64;
//                 }
//
//                 Poll::Ready(Ok(()))
//             }
//             Err(e) => Poll::Ready(Err(e)),
//         }
//     }
// }
//
// impl<T: Seek> Seek for LoggingReader<T> {
//     fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
//         let this = self.project();
//         this.inner.start_seek(position)
//     }
//
//     fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
//         let this = self.project();
//         this.inner.poll_complete(cx)
//     }
// }

// #[runtime::test]
// async fn skipping_entries_with_seek() {
//     let mut reader = LoggingReader::new(tar!("reading_files.tar"));
//     let mut ar_reader = Archive::new(&mut reader);
//     let entries = t!(ar_reader.entries());
//     let files: Vec<_> = entries
//         .map(|entry| entry.unwrap().path().unwrap().to_path_buf())
//         .collect().await;
//
//     let mut seekable_reader = LoggingReader::new(tar!("reading_files.tar"));
//     let mut ar_seekable_reader = Archive::new(&mut seekable_reader);
//     let files_seekable: Vec<_> = t!(ar_seekable_reader.entries_with_seek())
//         .map(|entry| entry.unwrap().path().unwrap().to_path_buf())
//         .collect();
//
//     assert!(files == files_seekable);
//     assert!(seekable_reader.read_bytes < reader.read_bytes);
// }

async fn check_dirtree(td: &TempDir) {
    let dir_a = td.path().join("a");
    let dir_b = td.path().join("a/b");
    let file_c = td.path().join("a/c");
    assert!(fs::metadata(&dir_a)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    assert!(fs::metadata(&dir_b)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    assert!(fs::metadata(&file_c)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
}

#[runtime::test]
async fn extracting_directories() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let rdr = tar!("directory.tar");
    let ar = Archive::new(rdr);
    t!(ar.unpack(td.path()).await);
    check_dirtree(&td).await;
}

#[runtime::test]
#[cfg(all(unix, feature = "xattr"))]
async fn xattrs() {
    // If /tmp is a tmpfs, xattr will fail
    // The xattr crate's unit tests also use /var/tmp for this reason
    let td = t!(TempBuilder::new()
        .prefix("async-tar")
        .tempdir_in("/var/tmp"));
    let rdr = tar!("xattrs.tar");
    let builder = ArchiveBuilder::new(rdr).set_unpack_xattrs(true);
    let ar = builder.build();
    t!(ar.unpack(td.path()).await);

    let val = xattr::get(td.path().join("a/b"), "user.pax.flags").unwrap();
    assert_eq!(val.unwrap(), b"epm");
}

#[runtime::test]
#[cfg(all(unix, feature = "xattr"))]
async fn no_xattrs() {
    // If /tmp is a tmpfs, xattr will fail
    // The xattr crate's unit tests also use /var/tmp for this reason
    let td = t!(TempBuilder::new()
        .prefix("async-tar")
        .tempdir_in("/var/tmp"));
    let rdr = tar!("xattrs.tar");
    let builder = ArchiveBuilder::new(rdr).set_unpack_xattrs(false);
    let ar = builder.build();
    t!(ar.unpack(td.path()).await);

    assert_eq!(
        xattr::get(td.path().join("a/b"), "user.pax.flags").unwrap(),
        None
    );
}

#[runtime::test]
async fn writing_and_extracting_directories() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut ar = Builder::new(Vec::new());
    let tmppath = td.path().join("tmpfile");
    t!(t!(File::create(&tmppath).await).write_all(b"c").await);
    t!(ar.append_dir("a", ".").await);
    t!(ar.append_dir("a/b", ".").await);
    t!(ar
        .append_file("a/c", &mut t!(File::open(&tmppath).await))
        .await);
    t!(ar.finish().await);

    let rdr = t!(ar.into_inner().await);
    let ar = Archive::new(rdr.as_slice());
    t!(ar.unpack(td.path()).await);
    check_dirtree(&td).await;
}

#[runtime::test]
async fn writing_and_extracting_directories_complex_permissions() {
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());

    // Archive with complex permissions which would fail to unpack if one attempted to do so
    // without reordering of entries.
    let mut ar = Builder::new(Vec::new());
    let tmppath = td.path().join("tmpfile");
    t!(t!(File::create(&tmppath).await).write_all(b"c").await);

    // Root dir with very stringent permissions
    let data: &[u8] = &[];
    let mut header = Header::new_gnu();
    header.set_mode(0o555);
    header.set_entry_type(EntryType::Directory);
    t!(header.set_path("a"));
    header.set_size(0);
    header.set_cksum();
    t!(ar.append(&header, data).await);

    // Nested dir
    header.set_mode(0o777);
    header.set_entry_type(EntryType::Directory);
    t!(header.set_path("a/b"));
    header.set_cksum();
    t!(ar.append(&header, data).await);

    // Nested file.
    t!(ar.append_file("a/c", &mut t!(File::open(&tmppath).await)).await);
    t!(ar.finish().await);

    let rdr = t!(ar.into_inner().await);
    let ar = Archive::new(rdr.as_slice());
    ar.unpack(td.path()).await.unwrap();
    check_dirtree(&td).await;
}

#[runtime::test]
async fn writing_directories_recursively() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let base_dir = td.path().join("base");
    t!(fs::create_dir(&base_dir).await);
    t!(t!(File::create(base_dir.join("file1")).await)
        .write_all(b"file1")
        .await);
    let sub_dir = base_dir.join("sub");
    t!(fs::create_dir(&sub_dir).await);
    t!(t!(File::create(sub_dir.join("file2")).await)
        .write_all(b"file2")
        .await);

    let mut ar = Builder::new(Vec::new());
    t!(ar.append_dir_all("foobar", base_dir).await);
    let data = t!(ar.into_inner().await);

    let ar = Archive::new(data.as_slice());
    t!(ar.unpack(td.path()).await);
    let base_dir = td.path().join("foobar");
    assert!(fs::metadata(&base_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    let file1_path = base_dir.join("file1");
    assert!(fs::metadata(&file1_path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
    let sub_dir = base_dir.join("sub");
    assert!(fs::metadata(&sub_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    let file2_path = sub_dir.join("file2");
    assert!(fs::metadata(&file2_path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
}

#[runtime::test]
async fn append_dir_all_blank_dest() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let base_dir = td.path().join("base");
    t!(fs::create_dir(&base_dir).await);
    t!(t!(File::create(base_dir.join("file1")).await)
        .write_all(b"file1")
        .await);
    let sub_dir = base_dir.join("sub");
    t!(fs::create_dir(&sub_dir).await);
    t!(t!(File::create(sub_dir.join("file2")).await)
        .write_all(b"file2")
        .await);

    let mut ar = Builder::new(Vec::new());
    t!(ar.append_dir_all("", base_dir).await);
    let data = t!(ar.into_inner().await);

    let ar = Archive::new(data.as_slice());
    t!(ar.unpack(td.path()).await);
    let base_dir = td.path();
    assert!(fs::metadata(&base_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    let file1_path = base_dir.join("file1");
    assert!(fs::metadata(&file1_path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
    let sub_dir = base_dir.join("sub");
    assert!(fs::metadata(&sub_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
    let file2_path = sub_dir.join("file2");
    assert!(fs::metadata(&file2_path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
}

#[runtime::test]
async fn append_dir_all_does_not_work_on_non_directory() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let path = td.path().join("test");
    t!(t!(File::create(&path).await).write_all(b"test").await);

    let mut ar = Builder::new(Vec::new());
    let result = ar.append_dir_all("test", path).await;
    assert!(result.is_err());
}

#[runtime::test]
async fn extracting_duplicate_dirs() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let rdr = tar!("duplicate_dirs.tar");
    let ar = Archive::new(rdr);
    t!(ar.unpack(td.path()).await);

    let some_dir = td.path().join("some_dir");
    assert!(fs::metadata(&some_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));
}

#[runtime::test]
async fn unpack_old_style_bsd_dir() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut ar = Builder::new(Vec::new());

    let mut header = Header::new_old();
    header.set_entry_type(EntryType::Regular);
    t!(header.set_path("testdir/"));
    header.set_size(0);
    header.set_cksum();
    t!(ar.append(&header, &mut io::empty()).await);

    // Extracting
    let rdr = t!(ar.into_inner().await);
    let ar = Archive::new(rdr.as_slice());
    t!(ar.clone().unpack(td.path()).await);

    // Iterating
    let rdr = ar.into_inner().map_err(|_| ()).unwrap();
    let ar = Archive::new(rdr);
    assert!(t!(ar.entries()).all(|fr| async move { fr.is_ok() }).await);

    assert!(td.path().join("testdir").is_dir());
}

#[runtime::test]
async fn handling_incorrect_file_size() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut ar = Builder::new(Vec::new());

    let path = td.path().join("tmpfile");
    t!(File::create(&path).await);
    let mut file = t!(File::open(&path).await);
    let mut header = Header::new_old();
    t!(header.set_path("somepath"));
    header.set_metadata(&t!(file.metadata().await));
    header.set_size(2048); // past the end of file null blocks
    header.set_cksum();
    t!(ar.append(&header, &mut file).await);

    // Extracting
    let rdr = t!(ar.into_inner().await);
    let ar = rdr.clone();
    let ar = Archive::new(ar.as_slice());
    assert!(ar.clone().unpack(td.path()).await.is_err());

    // Iterating
    let ar = Archive::new(rdr.as_slice());
    assert!(t!(ar.entries()).any(|fr| async move { fr.is_err() }).await);
}

#[runtime::test]
async fn extracting_malicious_tarball() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut evil_tar = Vec::new();

    {
        let mut a = Builder::new(&mut evil_tar);
        async fn append<R: Write + Unpin + Send + Sync>(a: &mut Builder<R>, path: &'static str) {
            let mut header = Header::new_gnu();
            assert!(header.set_path(path).is_err(), "was ok: {:?}", path);
            {
                let h = header.as_gnu_mut().unwrap();
                for (a, b) in h.name.iter_mut().zip(path.as_bytes()) {
                    *a = *b;
                }
            }
            header.set_size(1);
            header.set_cksum();
            t!(a.append(&header, io::repeat(1).take(1)).await);
        }

        append(&mut a, "/tmp/abs_evil.txt").await;
        append(&mut a, "//tmp/abs_evil2.txt").await;
        append(&mut a, "///tmp/abs_evil3.txt").await;
        append(&mut a, "/./tmp/abs_evil4.txt").await;
        append(&mut a, "//./tmp/abs_evil5.txt").await;
        append(&mut a, "///./tmp/abs_evil6.txt").await;
        append(&mut a, "/../tmp/rel_evil.txt").await;
        append(&mut a, "../rel_evil2.txt").await;
        append(&mut a, "./../rel_evil3.txt").await;
        append(&mut a, "some/../../rel_evil4.txt").await;
        append(&mut a, "").await;
        append(&mut a, "././//./..").await;
        append(&mut a, "..").await;
        append(&mut a, "/////////..").await;
        append(&mut a, "/////////").await;
    }

    let ar = Archive::new(&evil_tar[..]);
    t!(ar.unpack(td.path()).await);

    assert!(fs::metadata("/tmp/abs_evil.txt").await.is_err());
    assert!(fs::metadata("/tmp/abs_evil.txt2").await.is_err());
    assert!(fs::metadata("/tmp/abs_evil.txt3").await.is_err());
    assert!(fs::metadata("/tmp/abs_evil.txt4").await.is_err());
    assert!(fs::metadata("/tmp/abs_evil.txt5").await.is_err());
    assert!(fs::metadata("/tmp/abs_evil.txt6").await.is_err());
    assert!(fs::metadata("/tmp/rel_evil.txt").await.is_err());
    assert!(fs::metadata("/tmp/rel_evil.txt").await.is_err());
    assert!(fs::metadata(td.path().join("../tmp/rel_evil.txt"))
        .await
        .is_err());
    assert!(fs::metadata(td.path().join("../rel_evil2.txt"))
        .await
        .is_err());
    assert!(fs::metadata(td.path().join("../rel_evil3.txt"))
        .await
        .is_err());
    assert!(fs::metadata(td.path().join("../rel_evil4.txt"))
        .await
        .is_err());

    // The `some` subdirectory should not be created because the only
    // filename that references this has '..'.
    assert!(fs::metadata(td.path().join("some")).await.is_err());

    // The `tmp` subdirectory should be created and within this
    // subdirectory, there should be files named `abs_evil.txt` through
    // `abs_evil6.txt`.
    let tmp_root = td.path().join("tmp");

    assert!(fs::metadata(&tmp_root)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false));

    let mut entries = std::fs::read_dir(&tmp_root).unwrap();
    while let Some(entry) = entries.next() {
        let entry = entry.unwrap();
        println!("- {:?}", entry.file_name());
    }

    assert!(fs::metadata(tmp_root.join("abs_evil.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));

    // not present due to // being interpreted differently on windows
    #[cfg(not(target_os = "windows"))]
    assert!(fs::metadata(tmp_root.join("abs_evil2.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
    assert!(fs::metadata(tmp_root.join("abs_evil3.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
    assert!(fs::metadata(tmp_root.join("abs_evil4.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));

    // not present due to // being interpreted differently on windows
    #[cfg(not(target_os = "windows"))]
    assert!(fs::metadata(tmp_root.join("abs_evil5.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
    assert!(fs::metadata(tmp_root.join("abs_evil6.txt"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false));
}

#[runtime::test]
async fn octal_spaces() {
    let rdr = tar!("spaces.tar");
    let ar = Archive::new(rdr);

    let entry = ar.entries().unwrap().next().await.unwrap().unwrap();
    assert_eq!(entry.header().mode().unwrap() & 0o777, 0o777);
    assert_eq!(entry.header().uid().unwrap(), 0);
    assert_eq!(entry.header().gid().unwrap(), 0);
    assert_eq!(entry.header().size().unwrap(), 2);
    assert_eq!(entry.header().mtime().unwrap(), 0o12_440_016_664);
    assert_eq!(entry.header().cksum().unwrap(), 0o4253);
}

#[runtime::test]
async fn extracting_malformed_tar_null_blocks() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut ar = Builder::new(Vec::new());

    let path1 = td.path().join("tmpfile1");
    let path2 = td.path().join("tmpfile2");
    t!(File::create(&path1).await);
    t!(File::create(&path2).await);
    t!(ar
        .append_file("tmpfile1", &mut t!(File::open(&path1).await))
        .await);
    let mut data = t!(ar.into_inner().await);
    let amt = data.len();
    data.truncate(amt - 512);
    let mut ar = Builder::new(data);
    t!(ar
        .append_file("tmpfile2", &mut t!(File::open(&path2).await))
        .await);
    t!(ar.finish().await);

    let data = t!(ar.into_inner().await);
    let ar = Archive::new(&data[..]);
    assert!(ar.unpack(td.path()).await.is_ok());
}

#[runtime::test]
async fn empty_filename() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let rdr = tar!("empty_filename.tar");
    let ar = Archive::new(rdr);
    assert!(ar.unpack(td.path()).await.is_ok());
}

#[runtime::test]
async fn file_times() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let rdr = tar!("file_times.tar");
    let ar = Archive::new(rdr);
    t!(ar.unpack(td.path()).await);

    let meta = fs::metadata(td.path().join("a")).await.unwrap();
    let mtime = FileTime::from_last_modification_time(&meta);
    let atime = FileTime::from_last_access_time(&meta);
    assert_eq!(mtime.unix_seconds(), 1_000_000_000);
    assert_eq!(mtime.nanoseconds(), 0);
    assert_eq!(atime.unix_seconds(), 1_000_000_000);
    assert_eq!(atime.nanoseconds(), 0);
}

#[runtime::test]
async fn backslash_treated_well() {
    // Insert a file into an archive with a backslash
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let mut ar = Builder::new(Vec::<u8>::new());
    t!(ar.append_dir("foo\\bar", td.path()).await);
    let inner = t!(ar.into_inner().await);
    let ar = Archive::new(inner.as_slice());
    let f = t!(t!(ar.entries()).try_next().await).unwrap();
    if cfg!(unix) {
        assert_eq!(t!(f.header().path()).to_str(), Some("foo\\bar"));
    } else {
        assert_eq!(t!(f.header().path()).to_str(), Some("foo/bar"));
    }

    // Unpack an archive with a backslash in the name
    let mut ar = Builder::new(Vec::<u8>::new());
    let mut header = Header::new_gnu();
    header.set_metadata(&t!(fs::metadata(td.path()).await));
    header.set_size(0);
    for (a, b) in header.as_old_mut().name.iter_mut().zip(b"foo\\bar\x00") {
        *a = *b;
    }
    header.set_cksum();
    t!(ar.append(&header, &mut io::empty()).await);
    let data = t!(ar.into_inner().await);
    let ar = Archive::new(&data[..]);
    let f = t!(t!(ar.entries()).next().await.unwrap());
    assert_eq!(t!(f.header().path()).to_str(), Some("foo\\bar"));

    let ar = Archive::new(&data[..]);
    t!(ar.unpack(td.path()).await);
    assert!(fs::metadata(td.path().join("foo\\bar")).await.is_ok());
}

#[cfg(unix)]
#[runtime::test]
async fn nul_bytes_in_path() {
    use std::{ffi::OsStr, os::unix::prelude::*};

    let nul_path = OsStr::from_bytes(b"foo\0");
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let mut ar = Builder::new(Vec::<u8>::new());
    let err = ar.append_dir(nul_path, td.path()).await.unwrap_err();
    assert!(err.to_string().contains("contains a nul byte"));
}

#[runtime::test]
async fn links() {
    let ar = Archive::new(tar!("link.tar"));
    let mut entries = t!(ar.entries());
    let link = t!(entries.next().await.unwrap());
    assert_eq!(
        t!(link.header().link_name()).as_ref().map(|p| &**p),
        Some(Path::new("file"))
    );
    let other = t!(entries.next().await.unwrap());
    assert!(t!(other.header().link_name()).is_none());
}

#[runtime::test]
#[cfg(unix)] // making symlinks on windows is hard
async fn unpack_links() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let ar = Archive::new(tar!("link.tar"));
    t!(ar.unpack(td.path()).await);

    let md = t!(fs::symlink_metadata(td.path().join("lnk")).await);
    assert!(md.file_type().is_symlink());
    assert_eq!(
        &*t!(std::fs::read_link(td.path().join("lnk"))),
        Path::new("file")
    );
    t!(File::open(td.path().join("lnk")).await);
}

#[runtime::test]
async fn pax_simple() {
    let ar = Archive::new(tar!("pax.tar"));
    let mut entries = t!(ar.entries());

    let mut first = t!(entries.next().await.unwrap());
    let mut attributes = t!(first.pax_extensions().await).unwrap();
    let first = t!(attributes.next().unwrap());
    let second = t!(attributes.next().unwrap());
    let third = t!(attributes.next().unwrap());
    assert!(attributes.next().is_none());

    assert_eq!(first.key(), Ok("mtime"));
    assert_eq!(first.value(), Ok("1453146164.953123768"));
    assert_eq!(second.key(), Ok("atime"));
    assert_eq!(second.value(), Ok("1453251915.24892486"));
    assert_eq!(third.key(), Ok("ctime"));
    assert_eq!(third.value(), Ok("1453146164.953123768"));
}

#[runtime::test]
async fn pax_simple_write() {
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());
    let pax_path = td.path().join("pax.tar");
    let file: File = t!(File::create(&pax_path).await);
    let mut ar: Builder<io::BufWriter<File>> = Builder::new(io::BufWriter::new(file));

    let pax_extensions = [
        ("arbitrary_pax_key", b"arbitrary_pax_value".as_slice()),
        ("SCHILY.xattr.security.selinux", b"foo_t"),
    ];

    t!(ar.append_pax_extensions(pax_extensions).await);
    t!(ar.append_file("test2", &mut t!(File::open(&pax_path).await)).await);
    t!(ar.finish().await);
    drop(ar);

    let archive_opened = Archive::new(t!(File::open(pax_path).await));
    let mut entries = t!(archive_opened.entries());
    let mut f = t!(entries.try_next().await).unwrap();
    let pax_headers = t!(f.pax_extensions().await);

    assert!(pax_headers.is_some(), "pax_headers is None");
    let mut pax_headers = pax_headers.unwrap();
    let pax_arbitrary = t!(pax_headers.next().unwrap());
    assert_eq!(pax_arbitrary.key(), Ok("arbitrary_pax_key"));
    assert_eq!(pax_arbitrary.value(), Ok("arbitrary_pax_value"));
    let xattr = t!(pax_headers.next().unwrap());
    assert_eq!(xattr.key().unwrap(), pax_extensions[1].0);
    assert_eq!(xattr.value_bytes(), pax_extensions[1].1);

    assert!(entries.try_next().await.unwrap().is_none());
}

#[runtime::test]
async fn pax_path() {
    let ar = Archive::new(tar!("pax2.tar"));
    let mut entries = t!(ar.entries());

    let first = t!(entries.next().await.unwrap());
    assert!(first.path().unwrap().ends_with("aaaaaaaaaaaaaaa"));
}

#[runtime::test]
async fn long_name_trailing_nul() {
    let mut b = Builder::new(Vec::<u8>::new());

    let mut h = Header::new_gnu();
    t!(h.set_path("././@LongLink"));
    h.set_size(4);
    h.set_entry_type(EntryType::new(b'L'));
    h.set_cksum();
    t!(b.append(&h, "foo\0".as_bytes()).await);
    let mut h = Header::new_gnu();

    t!(h.set_path("bar"));
    h.set_size(6);
    h.set_entry_type(EntryType::file());
    h.set_cksum();
    t!(b.append(&h, b"foobar" as &[u8]).await);

    let contents = t!(b.into_inner().await);
    let a = Archive::new(&contents[..]);

    let e = t!(t!(a.entries()).next().await.unwrap());
    assert_eq!(&*e.path_bytes(), b"foo");
}

#[runtime::test]
async fn long_linkname_trailing_nul() {
    let mut b = Builder::new(Vec::<u8>::new());

    let mut h = Header::new_gnu();
    t!(h.set_path("././@LongLink"));
    h.set_size(4);
    h.set_entry_type(EntryType::new(b'K'));
    h.set_cksum();
    t!(b.append(&h, "foo\0".as_bytes()).await);
    let mut h = Header::new_gnu();

    t!(h.set_path("bar"));
    h.set_size(6);
    h.set_entry_type(EntryType::file());
    h.set_cksum();
    t!(b.append(&h, b"foobar" as &[u8]).await);

    let contents = t!(b.into_inner().await);
    let a = Archive::new(&contents[..]);

    let e = t!(t!(a.entries()).next().await.unwrap());
    assert_eq!(&*e.link_name_bytes().unwrap(), b"foo");
}

#[runtime::test]
async fn long_linkname_gnu() {
    for t in [EntryType::Symlink, EntryType::Link] {
        let mut b = Builder::new(Vec::<u8>::new());
        let mut h = Header::new_gnu();
        h.set_entry_type(t);
        h.set_size(0);
        let path = "usr/lib/.build-id/05/159ed904e45ff5100f7acd3d3b99fa7e27e34f";
        let target = "../../../../usr/lib64/qt5/plugins/wayland-graphics-integration-server/libqt-wayland-compositor-xcomposite-egl.so";
        t!(b.append_link(&mut h, path, target).await);

        let contents = t!(b.into_inner().await);
        let a = Archive::new(&contents[..]);

        let e = &t!(t!(a.entries()).next().await.unwrap());
        assert_eq!(e.header().entry_type(), t);
        assert_eq!(e.path().unwrap().to_str().unwrap(), path);
        assert_eq!(e.link_name().unwrap().unwrap().to_str().unwrap(), target);
    }
}

#[runtime::test]
async fn linkname_literal() {
    for t in [EntryType::Symlink, EntryType::Link] {
        let mut b = Builder::new(Vec::<u8>::new());
        let mut h = Header::new_gnu();
        h.set_entry_type(t);
        h.set_size(0);
        let path = "usr/lib/systemd/systemd-sysv-install";
        let target = "../../..//sbin/chkconfig";
        h.set_link_name_literal(target).unwrap();
        t!(b.append_data(&mut h, path, io::empty()).await);

        let contents = t!(b.into_inner().await);
        let a = Archive::new(&contents[..]);

        let e = &t!(t!(a.entries()).next().await.unwrap());
        assert_eq!(e.header().entry_type(), t);
        assert_eq!(e.path().unwrap().to_str().unwrap(), path);
        assert_eq!(e.link_name().unwrap().unwrap().to_str().unwrap(), target);
    }
}

#[runtime::test]
async fn append_writer() {
    let mut b = Builder::new(std::io::Cursor::new(Vec::new()));

    let mut h = Header::new_gnu();
    h.set_uid(42);
    let mut writer = t!(b.append_writer(&mut h, "file1").await);
    t!(writer.write_all(b"foo").await);
    t!(writer.write_all(b"barbaz").await);
    t!(writer.finish().await);

    let mut h = Header::new_gnu();
    h.set_uid(43);
    let long_path: PathBuf = repeat("abcd").take(50).collect();
    let mut writer = t!(b.append_writer(&mut h, &long_path).await);
    let long_data = repeat(b'x').take(513).collect::<Vec<u8>>();
    t!(writer.write_all(&long_data).await);
    t!(writer.finish().await);

    let contents = t!(b.into_inner().await).into_inner();
    let ar = Archive::new(&contents[..]);
    let mut entries = t!(ar.entries());

    let e = &mut t!(entries.next().await.unwrap());
    assert_eq!(e.header().uid().unwrap(), 42);
    assert_eq!(&*e.path_bytes(), b"file1");
    let mut r = Vec::new();
    t!(e.read_to_end(&mut r).await);
    assert_eq!(&r[..], b"foobarbaz");

    let e = &mut t!(entries.next().await.unwrap());
    assert_eq!(e.header().uid().unwrap(), 43);
    assert_eq!(t!(e.path()), long_path.as_path());
    let mut r = Vec::new();
    t!(e.read_to_end(&mut r).await);
    assert_eq!(r.len(), 513);
    assert!(r.iter().all(|b| *b == b'x'));
}

#[runtime::test]
async fn encoded_long_name_has_trailing_nul() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let path = td.path().join("foo");
    t!(t!(File::create(&path).await).write_all(b"test").await);

    let mut b = Builder::new(Vec::<u8>::new());
    let long = "abcd".repeat(200);

    t!(b.append_file(&long, &mut t!(File::open(&path).await)).await);

    let contents = t!(b.into_inner().await);
    let a = Archive::new(&contents[..]);

    let mut e = t!(t!(a.entries_raw()).next().await.unwrap());
    let mut name = Vec::new();
    t!(e.read_to_end(&mut name).await);
    assert_eq!(name[name.len() - 1], 0);

    let header_name = &e.header().as_gnu().unwrap().name;
    assert!(header_name.starts_with(b"././@LongLink\x00"));
}

#[runtime::test]
async fn reading_sparse() {
    let rdr = tar!("sparse.tar");
    let ar = Archive::new(rdr);
    let mut entries = t!(ar.entries());

    let mut a = t!(entries.next().await.unwrap());
    let mut s = String::new();
    assert_eq!(&*a.header().path_bytes(), b"sparse_begin.txt");
    t!(a.read_to_string(&mut s).await);
    assert_eq!(&s[..5], "test\n");
    assert!(s[5..].chars().all(|x| x == '\u{0}'));

    let mut a = t!(entries.next().await.unwrap());
    let mut s = String::new();
    assert_eq!(&*a.header().path_bytes(), b"sparse_end.txt");
    t!(a.read_to_string(&mut s).await);
    assert_eq!(s.len(), 8105);
    assert!(s[..s.len() - 9].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[s.len() - 9..], "test_end\n");

    let mut a = t!(entries.next().await.unwrap());
    let mut s = String::new();
    assert_eq!(&*a.header().path_bytes(), b"sparse_ext.txt");
    t!(a.read_to_string(&mut s).await);
    assert!(s[..0x1000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x1000..0x1000 + 5], "text\n");
    assert!(s[0x1000 + 5..0x3000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x3000..0x3000 + 5], "text\n");
    assert!(s[0x3000 + 5..0x5000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x5000..0x5000 + 5], "text\n");
    assert!(s[0x5000 + 5..0x7000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x7000..0x7000 + 5], "text\n");
    assert!(s[0x7000 + 5..0x9000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x9000..0x9000 + 5], "text\n");
    assert!(s[0x9000 + 5..0xb000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0xb000..0xb000 + 5], "text\n");

    let mut a = t!(entries.next().await.unwrap());
    let mut s = String::new();
    assert_eq!(&*a.header().path_bytes(), b"sparse.txt");
    t!(a.read_to_string(&mut s).await);
    assert!(s[..0x1000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x1000..0x1000 + 6], "hello\n");
    assert!(s[0x1000 + 6..0x2fa0].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x2fa0..0x2fa0 + 6], "world\n");
    assert!(s[0x2fa0 + 6..0x4000].chars().all(|x| x == '\u{0}'));

    assert!(entries.next().await.is_none());
}

#[runtime::test]
async fn extract_sparse() {
    let rdr = tar!("sparse.tar");
    let ar = Archive::new(rdr);
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    t!(ar.unpack(td.path()).await);

    let mut s = String::new();
    t!(t!(File::open(td.path().join("sparse_begin.txt")).await)
        .read_to_string(&mut s)
        .await);
    assert_eq!(&s[..5], "test\n");
    assert!(s[5..].chars().all(|x| x == '\u{0}'));

    s.truncate(0);
    t!(t!(File::open(td.path().join("sparse_end.txt")).await)
        .read_to_string(&mut s)
        .await);
    assert!(s[..s.len() - 9].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[s.len() - 9..], "test_end\n");

    s.truncate(0);
    t!(t!(File::open(td.path().join("sparse_ext.txt")).await)
        .read_to_string(&mut s)
        .await);
    assert!(s[..0x1000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x1000..0x1000 + 5], "text\n");
    assert!(s[0x1000 + 5..0x3000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x3000..0x3000 + 5], "text\n");
    assert!(s[0x3000 + 5..0x5000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x5000..0x5000 + 5], "text\n");
    assert!(s[0x5000 + 5..0x7000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x7000..0x7000 + 5], "text\n");
    assert!(s[0x7000 + 5..0x9000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x9000..0x9000 + 5], "text\n");
    assert!(s[0x9000 + 5..0xb000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0xb000..0xb000 + 5], "text\n");

    s.truncate(0);
    t!(t!(File::open(td.path().join("sparse.txt")).await)
        .read_to_string(&mut s)
        .await);
    assert!(s[..0x1000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x1000..0x1000 + 6], "hello\n");
    assert!(s[0x1000 + 6..0x2fa0].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x2fa0..0x2fa0 + 6], "world\n");
    assert!(s[0x2fa0 + 6..0x4000].chars().all(|x| x == '\u{0}'));
}

#[runtime::test]
async fn large_sparse() {
    let rdr = tar!("sparse-large.tar");
    let ar = Archive::new(rdr);
    let mut entries = t!(ar.entries());
    // Only check the header info without extracting, as the file is very large,
    // and not all filesystems support sparse files.
    let a = t!(entries.next().await.unwrap());
    let h = a.header().as_gnu().unwrap();
    assert_eq!(h.real_size().unwrap(), 12626929280);
}

#[runtime::test]
async fn sparse_with_trailing() {
    let rdr = tar!("sparse-1.tar");
    let ar = Archive::new(rdr);
    let mut entries = t!(ar.entries());
    let mut a = t!(entries.next().await.unwrap());
    let mut s = String::new();
    t!(a.read_to_string(&mut s).await);
    assert_eq!(0x100_00c, s.len());
    assert_eq!(&s[..0xc], "0MB through\n");
    assert!(s[0xc..0x100_000].chars().all(|x| x == '\u{0}'));
    assert_eq!(&s[0x100_000..], "1MB through\n");
}

#[runtime::test]
async fn writing_sparse() {
    let mut ar = Builder::new(Vec::new());
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());

    let mut files = Vec::new();
    async fn append_file<T: Write + Unpin + Send + Sync>(ar: &mut Builder<T>, td: &TempDir, name: &str, chunks: &[(u64, u64)]) -> PathBuf {
        let path = td.path().join(name);
        let mut file = t!(File::create(&path).await);
        t!(file.set_len(
            chunks
                .iter()
                .map(|&(off, len)| off + len)
                .max()
                .unwrap_or(0),
        ).await);
        for (i, &(off, len)) in chunks.iter().enumerate() {
            t!(file.seek(io::SeekFrom::Start(off)).await);
            let mut data = vec![i as u8 + b'a'; len as usize];
            data.first_mut().map(|x| *x = b'[');
            data.last_mut().map(|x| *x = b']');
            t!(file.write_all(&data).await);
        }
        t!(ar.append_path_with_name(&path, path.file_name().unwrap()).await);

        path
    }

    files.push(append_file(&mut ar, &td, "empty", &[]).await);
    files.push(append_file(&mut ar, &td, "full_sparse", &[(0x20_000, 0)]).await);
    files.push(append_file(&mut ar, &td, "_x", &[(0x20_000, 0x1_000)]).await);
    files.push(append_file(&mut ar, &td, "x_", &[(0, 0x1_000), (0x20_000, 0)]).await);
    files.push(append_file(&mut ar, &td, "_x_x", &[(0x20_000, 0x1_000), (0x40_000, 0x1_000)]).await);
    files.push(append_file(&mut ar, &td, "x_x_", &[(0, 0x1_000), (0x20_000, 0x1_000), (0x40_000, 0)]).await);
    files.push(append_file(&mut ar, &td, "uneven", &[(0x20_333, 0x555), (0x40_777, 0x999)]).await);

    t!(ar.finish().await);

    let data = t!(ar.into_inner().await);

    // Without sparse support, the size of the tarball exceed 1MiB.
    #[cfg(target_os = "linux")]
    assert!(data.len() <= 37 * 1024); // ext4 (defaults to 4k block size)
    #[cfg(target_os = "freebsd")]
    assert!(data.len() <= 273 * 1024); // UFS (defaults to 32k block size, last block isn't a hole)

    let ar = Archive::new(&data[..]);
    let mut entries = t!(ar.entries());
    for path in files {
        let mut f = t!(entries.next().await.unwrap());

        let mut s = String::new();
        t!(f.read_to_string(&mut s).await);

        let expected = t!(fs::read_to_string(&path).await);

        assert_eq!(s, expected, "path: {path:?}");
    }

    assert!(entries.next().await.is_none());
}

#[runtime::test]
async fn path_separators() {
    let mut ar = Builder::new(Vec::new());
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let path = td.path().join("test");
    t!(t!(File::create(&path).await).write_all(b"test").await);

    let short_path: PathBuf = repeat("abcd").take(2).collect();
    let long_path: PathBuf = repeat("abcd").take(50).collect();

    // Make sure UStar headers normalize to Unix path separators
    let mut header = Header::new_ustar();

    t!(header.set_path(&short_path));
    assert_eq!(t!(header.path()), short_path);
    assert!(!header.path_bytes().contains(&b'\\'));

    t!(header.set_path(&long_path));
    assert_eq!(t!(header.path()), long_path);
    assert!(!header.path_bytes().contains(&b'\\'));

    // Make sure GNU headers normalize to Unix path separators,
    // including the `@LongLink` fallback used by `append_file`.
    t!(ar
        .append_file(&short_path, &mut t!(File::open(&path).await))
        .await);
    t!(ar
        .append_file(&long_path, &mut t!(File::open(&path).await))
        .await);

    let rd = t!(ar.into_inner().await);
    let ar = Archive::new(rd.as_slice());
    let mut entries = t!(ar.entries());

    let entry = t!(entries.try_next().await).unwrap();
    assert_eq!(t!(entry.path()), short_path);
    assert!(!entry.path_bytes().contains(&b'\\'));

    let entry = t!(entries.try_next().await).unwrap();
    assert_eq!(t!(entry.path()), long_path);
    assert!(!entry.path_bytes().contains(&b'\\'));

    assert!(entries.next().await.is_none());
}

#[runtime::test]
#[cfg(unix)]
async fn append_path_symlink() {
    use std::{borrow::Cow, env, os::unix::fs::symlink};

    let mut ar = Builder::new(Vec::new());
    ar.follow_symlinks(false);
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let long_linkname = "abcd".repeat(30);
    let long_pathname = "dcba".repeat(30);
    t!(env::set_current_dir(td.path()));
    // "short" path name / short link name
    t!(symlink("testdest", "test"));
    t!(ar.append_path("test").await);
    // short path name / long link name
    t!(symlink(&long_linkname, "test2"));
    t!(ar.append_path("test2").await);
    // long path name / long link name
    t!(symlink(&long_linkname, &long_pathname));
    t!(ar.append_path(&long_pathname).await);

    let rd = t!(ar.into_inner().await);
    let ar = Archive::new(rd.as_slice());
    let mut entries = t!(ar.entries());

    let entry = t!(entries.try_next().await).unwrap();
    assert_eq!(t!(entry.path()), Path::new("test"));
    assert_eq!(
        t!(entry.link_name()),
        Some(Cow::from(Path::new("testdest")))
    );
    assert_eq!(t!(entry.header().size()), 0);

    let entry = t!(entries.next().await.unwrap());
    assert_eq!(t!(entry.path()), Path::new("test2"));
    assert_eq!(
        t!(entry.link_name()),
        Some(Cow::from(Path::new(&long_linkname)))
    );
    assert_eq!(t!(entry.header().size()), 0);

    let entry = t!(entries.next().await.unwrap());
    assert_eq!(t!(entry.path()), Path::new(&long_pathname));
    assert_eq!(
        t!(entry.link_name()),
        Some(Cow::from(Path::new(&long_linkname)))
    );
    assert_eq!(t!(entry.header().size()), 0);

    assert!(entries.next().await.is_none());
}

#[runtime::test]
async fn name_with_slash_doesnt_fool_long_link_and_bsd_compat() {
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());

    let mut ar = Builder::new(Vec::new());

    let mut h = Header::new_gnu();
    t!(h.set_path("././@LongLink"));
    h.set_size(4);
    h.set_entry_type(EntryType::new(b'L'));
    h.set_cksum();
    t!(ar.append(&h, "foo\0".as_bytes()).await);

    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    t!(header.set_path("testdir/"));
    header.set_size(0);
    header.set_cksum();
    t!(ar.append(&header, &mut io::empty()).await);

    // Extracting
    let rdr = t!(ar.into_inner().await);
    let ar = Archive::new(rdr.as_slice());
    t!(ar.clone().unpack(td.path()).await);

    // Iterating
    let rdr = ar.into_inner().map_err(|_| ()).unwrap();
    let ar = Archive::new(rdr);
    assert!(t!(ar.entries()).all(|fr| async move { fr.is_ok() }).await);

    assert!(td.path().join("foo").is_file());
}

#[runtime::test]
async fn insert_local_file_different_name() {
    let mut ar = Builder::new(Vec::new());
    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let path = td.path().join("directory");
    t!(fs::create_dir(&path).await);
    ar.append_path_with_name(&path, "archive/dir")
        .await
        .unwrap();
    let path = td.path().join("file");

    {
        let mut file = t!(File::create(&path).await);
        t!(file.write_all(b"test").await);
        t!(file.flush().await);
    }

    ar.append_path_with_name(&path, "archive/dir/f")
        .await
        .unwrap();

    let rd = t!(ar.into_inner().await);
    let ar = Archive::new(rd.as_slice());
    let mut entries = t!(ar.entries());
    let entry = t!(entries.try_next().await).unwrap();
    assert_eq!(t!(entry.path()), Path::new("archive/dir"));
    let entry = t!(entries.try_next().await).unwrap();
    assert_eq!(t!(entry.path()), Path::new("archive/dir/f"));
    assert!(entries.next().await.is_none());
}

#[runtime::test]
#[cfg(unix)]
async fn tar_directory_containing_symlink_to_directory() {
    use std::os::unix::fs::symlink;

    let td = t!(TempBuilder::new().prefix("async-tar").tempdir());
    let dummy_src = t!(TempBuilder::new().prefix("dummy_src").tempdir());
    let dummy_dst = td.path().join("dummy_dst");
    let mut ar = Builder::new(Vec::new());
    t!(symlink(dummy_src.path().display().to_string(), &dummy_dst));

    assert!(dummy_dst.read_link().is_ok());
    assert!(dummy_dst.read_link().unwrap().is_dir());
    ar.append_dir_all("symlinks", td.path()).await.unwrap();
    ar.finish().await.unwrap();
}

#[runtime::test]
async fn long_path() {
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());
    let rdr = tar!("7z_long_path.tar");
    let ar = Archive::new(rdr);
    ar.unpack(td.path()).await.unwrap();
}

#[runtime::test]
async fn unpack_path_larger_than_windows_max_path() {
    let dir_name = "iamaprettylongnameandtobepreciseiam91characterslongwhichsomethinkisreallylongandothersdonot";
    // 183 character directory name
    let really_long_path = format!("{}{}", dir_name, dir_name);
    let td = t!(TempBuilder::new().prefix(&really_long_path).tempdir());
    // directory in 7z_long_path.tar is over 100 chars
    let rdr = tar!("7z_long_path.tar");
    let ar = Archive::new(rdr);
    // should unpack path greater than windows MAX_PATH length of 260 characters
    assert!(ar.unpack(td.path()).await.is_ok());
}

#[runtime::test]
async fn append_long_multibyte() {
    let mut x = Builder::new(Vec::new());
    let mut name = String::new();
    let data: &[u8] = &[];
    for _ in 0..512 {
        name.push('a');
        name.push('𑢮');
        x.append_data(&mut Header::new_gnu(), &name, data).await.unwrap();
        name.pop();
    }
}

#[runtime::test]
async fn read_only_directory_containing_files() {
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());

    let mut b = Builder::new(Vec::<u8>::new());

    let mut h = Header::new_gnu();
    t!(h.set_path("dir/"));
    h.set_size(0);
    h.set_entry_type(EntryType::dir());
    h.set_mode(0o444);
    h.set_cksum();
    t!(b.append(&h, "".as_bytes()).await);

    let mut h = Header::new_gnu();
    t!(h.set_path("dir/file"));
    h.set_size(2);
    h.set_entry_type(EntryType::file());
    h.set_cksum();
    t!(b.append(&h, "hi".as_bytes()).await);

    let contents = t!(b.into_inner().await);
    let ar = Archive::new(&contents[..]);
    assert!(ar.unpack(td.path()).await.is_ok());
}

// This test was marked linux only due to macOS CI can't handle `set_current_dir` correctly
#[runtime::test]
#[cfg(target_os = "linux")]
async fn tar_directory_containing_special_files() {
    use std::env;
    use std::ffi::CString;

    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());
    let fifo = td.path().join("fifo");

    unsafe {
        let fifo_path = t!(CString::new(fifo.to_str().unwrap()));
        let ret = libc::mknod(fifo_path.as_ptr(), libc::S_IFIFO | 0o644, 0);
        if ret != 0 {
            libc::perror(fifo_path.as_ptr());
            panic!("Failed to create a FIFO file");
        }
    }

    t!(env::set_current_dir(td.path()));
    let mut ar = Builder::new(Vec::new());
    // append_path has a different logic for processing files, so we need to test it as well
    t!(ar.append_path("fifo").await);
    t!(ar.append_dir_all("special", td.path()).await);
    t!(env::set_current_dir("/dev/"));
    // CI systems seem to have issues with creating a chr device
    t!(ar.append_path("null").await);
    t!(ar.finish().await);
}

#[runtime::test]
async fn header_size_overflow() {
    // maximal file size doesn't overflow anything
    let mut ar = Builder::new(Vec::new());
    let mut header = Header::new_gnu();
    header.set_size(u64::MAX);
    header.set_cksum();
    ar.append(&mut header, "x".as_bytes()).await.unwrap();
    let result = t!(ar.into_inner().await);
    let ar = Archive::new(&result[..]);
    let mut e = ar.entries().unwrap();
    let err = e.next().await.unwrap().err().unwrap();
    assert!(
        err.to_string().contains("size overflow"),
        "bad error: {}",
        err
    );

    // back-to-back entries that would overflow also don't panic
    let mut ar = Builder::new(Vec::new());
    let mut header = Header::new_gnu();
    header.set_size(1_000);
    header.set_cksum();
    ar.append(&mut header, &[0u8; 1_000][..]).await.unwrap();
    let mut header = Header::new_gnu();
    header.set_size(u64::MAX - 513);
    header.set_cksum();
    ar.append(&mut header, "x".as_bytes()).await.unwrap();
    let result = t!(ar.into_inner().await);
    let ar = Archive::new(&result[..]);
    let mut e = ar.entries().unwrap();
    e.next().await.unwrap().unwrap();
    let err = e.next().await.unwrap().err().unwrap();
    assert!(
        err.to_string().contains("size overflow"),
        "bad error: {}",
        err
    );
}

#[runtime::test]
#[cfg(unix)]
async fn ownership_preserving() {
    use std::os::unix::prelude::*;

    let mut rdr = Vec::new();
    let mut ar = Builder::new(&mut rdr);
    let data: &[u8] = &[];
    let mut header = Header::new_gnu();
    // file 1 with uid = 580800000, gid = 580800000
    header.set_gid(580800000);
    header.set_uid(580800000);
    t!(header.set_path("iamuid580800000"));
    header.set_size(0);
    header.set_cksum();
    t!(ar.append(&header, data).await);
    // file 2 with uid = 580800001, gid = 580800000
    header.set_uid(580800001);
    t!(header.set_path("iamuid580800001"));
    header.set_cksum();
    t!(ar.append(&header, data).await);
    // file 3 with uid = 580800002, gid = 580800002
    header.set_gid(580800002);
    header.set_uid(580800002);
    t!(header.set_path("iamuid580800002"));
    header.set_cksum();
    t!(ar.append(&header, data).await);
    // directory 1 with uid = 580800002, gid = 580800002
    header.set_entry_type(EntryType::Directory);
    header.set_gid(580800002);
    header.set_uid(580800002);
    t!(header.set_path("iamuid580800002dir"));
    header.set_cksum();
    t!(ar.append(&header, data).await);
    // symlink to file 1
    header.set_entry_type(EntryType::Symlink);
    header.set_gid(580800002);
    header.set_uid(580800002);
    t!(header.set_path("iamuid580800000symlink"));
    header.set_cksum();
    t!(ar.append_link(&mut header, "iamuid580800000symlink", "iamuid580800000").await);
    t!(ar.finish().await);

    let rdr = t!(ar.into_inner().await);
    let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());
    let ar = ArchiveBuilder::new(rdr.as_slice())
        .set_preserve_ownerships(true)
        .build();

    if unsafe { libc::getuid() } == 0 {
        ar.unpack(td.path()).await.unwrap();
        // validate against premade files
        // iamuid580800001 has this ownership: 580800001:580800000
        let meta = std::fs::symlink_metadata(td.path().join("iamuid580800000")).unwrap();
        assert_eq!(meta.uid(), 580800000);
        assert_eq!(meta.gid(), 580800000);
        let meta = std::fs::symlink_metadata(td.path().join("iamuid580800001")).unwrap();
        assert_eq!(meta.uid(), 580800001);
        assert_eq!(meta.gid(), 580800000);
        let meta = std::fs::symlink_metadata(td.path().join("iamuid580800002")).unwrap();
        assert_eq!(meta.uid(), 580800002);
        assert_eq!(meta.gid(), 580800002);
        let meta = std::fs::symlink_metadata(td.path().join("iamuid580800002dir")).unwrap();
        assert_eq!(meta.uid(), 580800002);
        assert_eq!(meta.gid(), 580800002);
        let meta = std::fs::symlink_metadata(td.path().join("iamuid580800000symlink")).unwrap();
        assert_eq!(meta.uid(), 580800002);
        assert_eq!(meta.gid(), 580800002)
    } else {
        // it's not possible to unpack tar while preserving ownership
        // without root permissions
        assert!(ar.unpack(td.path()).await.is_err());
    }
}

#[runtime::test]
#[cfg(unix)]
async fn pax_and_gnu_uid_gid() {
    let tarlist = [tar!("biguid_gnu.tar"), tar!("biguid_pax.tar")];

    for file in &tarlist {
        let td = t!(TempBuilder::new().prefix("tar-rs").tempdir());
        let rdr = *file;
        let ar = ArchiveBuilder::new(rdr)
            .set_preserve_ownerships(true)
            .build();

        if unsafe { libc::getuid() } == 0 {
            t!(ar.unpack(td.path()).await);
            let meta = fs::metadata(td.path().join("test.txt")).await.unwrap();
            let uid = std::os::unix::prelude::MetadataExt::uid(&meta);
            let gid = std::os::unix::prelude::MetadataExt::gid(&meta);
            // 4294967294 = u32::MAX - 1
            assert_eq!(uid, 4294967294);
            assert_eq!(gid, 4294967294);
        } else {
            // it's not possible to unpack tar while preserving ownership
            // without root permissions
            assert!(ar.unpack(td.path()).await.is_err());
        }
    }
}
