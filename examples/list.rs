//! An example of listing the file names of entries in an archive.
//!
//! Takes a tarball on stdin and prints out all of the entries inside.

extern crate async_tar_rs;

#[cfg(feature = "async-std")]
use async_std::io::stdin;
use futures::TryStreamExt;
#[cfg(feature = "tokio")]
use tokio::io::stdin;

use async_tar_rs::Archive;

fn main() {
    async_std::task::block_on(async {
        let ar = Archive::new(stdin());
        let mut entries = ar.entries().unwrap();
        while let Ok(Some(f)) = entries.try_next().await {
            println!("{}", f.path().unwrap().display());
        }
    });
}
