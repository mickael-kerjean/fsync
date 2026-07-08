use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fsync_core::path::RelPath;
use windows::core::Interface;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};

pub fn spawn(
    root: &Path,
    on_view: tokio::sync::mpsc::UnboundedSender<(RelPath, bool)>,
) -> io::Result<()> {
    let root = root.to_path_buf();
    std::thread::Builder::new()
        .name("fsync-viewer".into())
        .spawn(move || view_loop(root, on_view))?;
    Ok(())
}

fn view_loop(root: PathBuf, on_view: tokio::sync::mpsc::UnboundedSender<(RelPath, bool)>) {
    if let Err(err) = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
        log::error!("viewer CoInitializeEx: {err}");
        return;
    }
    let mut last: BTreeSet<RelPath> = BTreeSet::new();
    loop {
        let current = viewed_dirs(&root).unwrap_or_default();
        for dir in &current {
            if on_view.send((dir.clone(), !last.contains(dir))).is_err() {
                return;
            }
        }
        last = current;
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn viewed_dirs(root: &Path) -> Option<BTreeSet<RelPath>> {
    let mut out = BTreeSet::new();
    unsafe {
        let windows: IShellWindows =
            CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER).ok()?;
        let count = windows.Count().ok()?;
        for i in 0..count {
            let Ok(disp) = windows.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(browser) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let Ok(url) = browser.LocationURL() else {
                continue;
            };
            if let Some(dir) = dir_from_url(&url.to_string(), root) {
                out.insert(dir);
            }
        }
    }
    Some(out)
}

fn dir_from_url(url: &str, root: &Path) -> Option<RelPath> {
    let path = url.strip_prefix("file:///")?;
    let abs = PathBuf::from(percent_decode(path).replace('/', "\\"));
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_str()?.replace('\\', "/");
    Some(if s.is_empty() {
        RelPath::root()
    } else {
        RelPath::new(&s)
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn percent_decoding() {
        assert_eq!(super::percent_decode("My%20Docs"), "My Docs");
        assert_eq!(super::percent_decode("plain"), "plain");
        assert_eq!(super::percent_decode("bad%2"), "bad%2");
        assert_eq!(super::percent_decode("caf%C3%A9"), "café");
    }
}
