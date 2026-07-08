use std::io::Write;
use std::path::Path;

pub use ::log::{error, info, warn};

pub fn init(data: &Path) -> std::io::Result<()> {
    let path = data.join("fsync.log");
    if std::fs::metadata(&path).is_ok_and(|md| md.len() > 5 * 1024 * 1024) {
        let old = data.join("fsync.log.1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, &old);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,fsync_windows=debug"),
    )
    .target(env_logger::Target::Pipe(Box::new(file)))
    .write_style(env_logger::WriteStyle::Never)
    .format(|buf, record| {
        let message = record.args().to_string();
        writeln!(
            buf,
            "time={} level={} origin={} message={message:?}",
            buf.timestamp_seconds(),
            record.level(),
            record.target(),
        )
    })
    .init();
    Ok(())
}
