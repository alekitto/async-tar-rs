//! An example of listing the file names of entries in an archive.
//!
//! Takes a tarball on stdin and prints out all of the entries inside.

extern crate tar;

use tokio::io::stdin;

use tar::Archive;

#[tokio::main]
async fn main() {
    let mut ar = Archive::new(stdin());
    let mut entries = ar.entries().unwrap();
    while let Some(file) = entries.next().await {
        let f = file.unwrap();
        println!("{}", f.path().unwrap().display());
    }
}
