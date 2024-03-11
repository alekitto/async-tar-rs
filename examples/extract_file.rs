//! An example of extracting a file in an archive.
//!
//! Takes a tarball on standard input, looks for an entry with a listed file
//! name as the first argument provided, and then prints the contents of that
//! file to stdout.

extern crate async_tar_rs;

#[cfg(feature = "async-std")]
use async_std::io::{copy, stdin, stdout};
#[cfg(feature = "tokio")]
use tokio::io::{copy, stdin, stdout};

use async_tar_rs::Archive;
use futures::TryStreamExt;
use std::env::args_os;
use std::path::Path;

fn main() {
    async_std::task::block_on(async {
        let first_arg = args_os().nth(1).unwrap();
        let filename = Path::new(&first_arg);
        let ar = Archive::new(stdin());
        let mut entries = ar.entries().unwrap();
        while let Ok(Some(mut file)) = entries.try_next().await {
            if file.path().unwrap() == filename {
                copy(&mut file, &mut stdout()).await.unwrap();
            }
        }
    });
}
