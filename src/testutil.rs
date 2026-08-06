//! テスト専用のヘルパ。`#[cfg(test)]` でのみコンパイルされる。
//!
//! **すべて `std::env::temp_dir()` の下だけを使う。** 実在の `~/.claude` には構造上
//! 到達しえない（CONTRIBUTING.md の ground rules）。dev-dependencies を持たない方針なので
//! `tempfile` クレートは使わず、必要な最小限を自前で持つ。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::model::ItemKind;

/// 生成ごとに一意な名前を作るための連番。tag とプロセス ID だけでは、同じテスト内で
/// 2 つ作ったときに衝突しうる。
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// スコープを抜けると中身ごと消える一時ディレクトリ。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("drybench-test-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("一時ディレクトリを作れない");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.0.join(rel)
    }

    /// 親ディレクトリごと作ってファイルを書く。
    pub fn write(&self, rel: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    pub fn mkdir(&self, rel: impl AsRef<Path>) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// `<root>/<kind>/<name>/<必須ファイル>` を作る（skill / hook 用）。
    pub fn dir_item(&self, kind: ItemKind, name: &str) -> PathBuf {
        let dir = self.mkdir(Path::new(kind.dir_name()).join(name));
        if let Some(required) = kind.required_file() {
            fs::write(dir.join(required), b"x").unwrap();
        }
        dir
    }

    /// `<root>/agents/<name>.md` を作る。
    pub fn agent_item(&self, name: &str) -> PathBuf {
        self.write(
            Path::new(ItemKind::Agent.dir_name()).join(format!("{name}.md")),
            "x",
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
