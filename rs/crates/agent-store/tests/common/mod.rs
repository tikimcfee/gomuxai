#![allow(dead_code)]

use std::path::PathBuf;

use agent_store::Store;

/// A temp dir under /var/tmp (/tmp is RAM on the machines this runs on).
pub fn temp_dir() -> tempfile::TempDir {
    let base = if std::path::Path::new("/var/tmp").is_dir() {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix("agent-store-test-")
        .tempdir_in(base)
        .unwrap()
}

/// A fresh store; the store is closed and its directory removed when the
/// returned guard drops (fields drop in order: store first).
pub struct TestDb {
    pub store: Store,
    pub path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TestDb {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.store
    }
}

pub fn setup_test_db() -> TestDb {
    let dir = temp_dir();
    let path = dir.path().join("gomuxai-test.db");
    let store = Store::open(&path).unwrap_or_else(|e| panic!("creating storage: {e:#}"));
    TestDb {
        store,
        path,
        _dir: dir,
    }
}
