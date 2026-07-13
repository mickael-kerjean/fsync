use std::io;
use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;

pub mod config;
pub mod engine;
pub mod path;
pub mod port;
pub mod scheduler;
pub mod sdk;

pub type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

pub fn byte_stream(data: impl Into<Bytes>) -> ByteStream {
    Box::pin(futures_util::stream::once(std::future::ready(Ok(
        data.into()
    ))))
}

pub(crate) async fn file_stream(path: &std::path::Path) -> io::Result<ByteStream> {
    let file = tokio::fs::File::open(path).await?;
    Ok(Box::pin(tokio_util::io::ReaderStream::new(file)))
}

pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut file = std::fs::File::create(&tmp)?;
    io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}
