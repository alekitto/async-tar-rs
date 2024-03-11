use async_tar_rs::Builder;
use tokio::fs::File;

#[tokio::main]
async fn main() {
    let file = File::create("foo.tar").await.unwrap();
    let mut a = Builder::new(file);

    a.append_path("README.md").await.unwrap();
    a.append_file("lib.rs", &mut File::open("src/lib.rs").await.unwrap())
        .await
        .unwrap();
}
