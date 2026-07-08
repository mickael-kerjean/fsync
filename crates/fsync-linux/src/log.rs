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
        env_logger::Env::default().default_filter_or("info,fsync_linux=debug,fsync_core=debug"),
    )
    .target(env_logger::Target::Pipe(Box::new(Tee(file))))
    .write_style(env_logger::WriteStyle::Never)
    .format(|buf, record| {
        let message = record.args().to_string();
        let origin = match record.line() {
            Some(line) => format!("{}:{line}", record.target()),
            None => record.target().to_string(),
        };
        writeln!(
            buf,
            "time={} level={} origin={origin} message={message:?}",
            buf.timestamp_seconds(),
            record.level(),
        )
    })
    .init();
    Ok(())
}

struct Tee(std::fs::File);

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stdout().write_all(buf);
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stdout().flush();
        self.0.flush()
    }
}
