use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    accounts: BTreeMap<String, Credentials>,
}

#[must_use]
pub fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NURL_CONFIG") {
        return Some(PathBuf::from(path));
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("nurl").join("config.json"))
}

fn key(server: &Url) -> String {
    server.origin().ascii_serialization()
}

fn read(path: &Path) -> Store {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write(path: &Path, store: &Store) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(store).expect("store is serializable");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    #[cfg(unix)]
    restrict(&tmp)?;
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[must_use]
pub fn load(server: &Url) -> Option<Credentials> {
    read(&path()?).accounts.remove(&key(server))
}

pub fn save(server: &Url, credentials: Credentials) -> io::Result<PathBuf> {
    let path = path().ok_or_else(|| io::Error::other("не удалось определить домашний каталог"))?;
    let mut store = read(&path);
    store.accounts.insert(key(server), credentials);
    write(&path, &store)?;
    Ok(path)
}

pub fn remove(server: &Url) -> io::Result<bool> {
    let Some(path) = path() else { return Ok(false) };
    let mut store = read(&path);
    if store.accounts.remove(&key(server)).is_none() {
        return Ok(false);
    }
    write(&path, &store)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_per_server() {
        let dir = std::env::temp_dir().join(format!("nurl-test-{}", std::process::id()));
        let file = dir.join("config.json");
        let a = Url::parse("https://nurl.pw/").unwrap();
        let b = Url::parse("http://localhost:8080").unwrap();
        let creds = |t: &str| Credentials {
            token: t.into(),
            email: "a@b.c".into(),
            name: "A".into(),
        };

        let mut store = read(&file);
        store.accounts.insert(key(&a), creds("one"));
        store.accounts.insert(key(&b), creds("two"));
        write(&file, &store).unwrap();

        let mut back = read(&file);
        assert_eq!(back.accounts.get("https://nurl.pw"), Some(&creds("one")));
        assert_eq!(
            back.accounts.remove("http://localhost:8080"),
            Some(creds("two"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
