use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use fsync_core::path::RelPath;
use fuser::Errno;

type Attrs = BTreeMap<String, Vec<u8>>;

pub struct XattrDb {
    file: PathBuf,
    map: Mutex<BTreeMap<RelPath, Attrs>>,
}

impl XattrDb {
    pub fn open(file: PathBuf) -> Self {
        let map = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                log::warn!("xattr.json is unreadable ({err}); starting empty");
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self {
            file,
            map: Mutex::new(map),
        }
    }

    fn save(&self, map: &BTreeMap<RelPath, Attrs>) {
        if let Ok(bytes) = serde_json::to_vec(map) {
            if let Err(err) = fsync_core::write_atomic(&self.file, &bytes) {
                log::error!("xattr save: {err}");
            }
        }
    }

    pub fn set(&self, path: &RelPath, name: &str, value: &[u8], flags: i32) -> Result<(), Errno> {
        let mut map = self.map.lock().unwrap();
        let exists = map.get(path).is_some_and(|attrs| attrs.contains_key(name));
        if flags & libc::XATTR_CREATE != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            return Err(Errno::ENODATA);
        }
        map.entry(path.clone())
            .or_default()
            .insert(name.to_string(), value.to_vec());
        self.save(&map);
        Ok(())
    }

    pub fn get(&self, path: &RelPath, name: &str) -> Option<Vec<u8>> {
        self.map.lock().unwrap().get(path)?.get(name).cloned()
    }

    pub fn list(&self, path: &RelPath) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(attrs) = self.map.lock().unwrap().get(path) {
            for name in attrs.keys() {
                out.extend_from_slice(name.as_bytes());
                out.push(0);
            }
        }
        out
    }

    pub fn remove(&self, path: &RelPath, name: &str) -> Result<(), Errno> {
        let mut map = self.map.lock().unwrap();
        let attrs = map.get_mut(path).ok_or(Errno::ENODATA)?;
        attrs.remove(name).ok_or(Errno::ENODATA)?;
        if attrs.is_empty() {
            map.remove(path);
        }
        self.save(&map);
        Ok(())
    }

    pub fn forget(&self, path: &RelPath) {
        let mut map = self.map.lock().unwrap();
        let before = map.len();
        map.retain(|p, _| p != path && !p.is_descendant_of(path));
        if map.len() != before {
            self.save(&map);
        }
    }

    pub fn remap(&self, from: &RelPath, to: &RelPath) {
        let mut map = self.map.lock().unwrap();
        let before = map.len();
        map.retain(|p, _| p != to && !p.is_descendant_of(to));
        let moved: Vec<RelPath> = map
            .keys()
            .filter(|p| *p == from || p.is_descendant_of(from))
            .cloned()
            .collect();
        if moved.is_empty() && map.len() == before {
            return;
        }
        for p in moved {
            let attrs = map.remove(&p).unwrap();
            let dest = RelPath::new(&p.as_str().replacen(from.as_str(), to.as_str(), 1));
            map.insert(dest, attrs);
        }
        self.save(&map);
    }
}

#[cfg(test)]
#[path = "xattr_test.rs"]
mod tests;
