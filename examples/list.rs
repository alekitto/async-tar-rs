//! An example of listing the file names of entries in an archive.
//!
//! Takes a tarball on stdin and prints out all of the entries inside.

#[cfg(feature = "async-std")]
use async_std::{io::stdin, stream::StreamExt};
use async_tar_rs::Archive;
#[cfg(feature = "tokio")]
use tokio::io::stdin;
#[cfg(feature = "tokio")]
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() {
    let mut ar = Archive::new(stdin());
    let mut entries = ar.entries().unwrap();
    while let Some(file) = entries.next().await {
        let f = file.unwrap();
        println!("{}", f.path().unwrap().display());
    }
}
