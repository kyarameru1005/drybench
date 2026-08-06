//! マニフェスト（`~/.claude/.drybench-manifest.json`）の読み書き。
//!
//! 安全保証はすべてこのファイルに帰着する。`(kind, name)` と `content_hash`（sha256）を
//! 記録し、ここに無い実体は触らない（設計原則 1）、ハッシュが一致しないものも触らない
//! （設計原則 2）。
//!
//! スキーマ:
//!
//! ```json
//! {
//!   "version": 1,
//!   "entries": [
//!     { "kind": "skill", "name": "my-skill", "source_dir": "/abs/path",
//!       "synced_at": "RFC3339", "content_hash": "sha256hex" }
//!   ],
//!   "hook_settings": [
//!     { "name": "my-hook",
//!       "entries": [{ "event": "PostToolUse", "group_hash": "sha256hex" }] }
//!   ]
//! }
//! ```
//!
//! `hook_settings` は hook 種別だけが持つ情報（`settings.json` に入れたグループへの参照）
//! なので `entries` とは別配列にする。これを持たない古いマニフェストもそのまま読める。
//!
//! TODO(migrate): 実装は `apps/proteus/src/manifest.rs` から移す。
//! 移設時に加えること: マニフェストのファイル名を `.proteus-manifest.json` から
//! `.drybench-manifest.json` へ変更する。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::ItemKind;

/// ターゲットディレクトリ直下に置くマニフェストのファイル名。
pub const MANIFEST_FILE: &str = ".drybench-manifest.json";

/// 書き出すスキーマバージョン。
pub const VERSION: u32 = 1;

/// 書き込み途中で落ちても壊れたマニフェストを残さないための一時ファイル名。
const TEMP_FILE: &str = ".drybench-manifest.json.tmp";

/// drybench が同期した実体の記録。**安全保証はすべてこの記録に帰着する** —
/// ここに無い実体は触らず（設計原則 1）、`content_hash` が一致しないものも触らない
/// （設計原則 2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// hook 種別だけが持つ、`settings.json` に入れたグループへの参照。
    /// **これを持たない古いマニフェストもそのまま読める**（`serde(default)`）。
    #[serde(default)]
    pub hook_settings: Vec<HookSettings>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
            hook_settings: Vec::new(),
        }
    }
}

/// 同期した実体 1 件。`(kind, name)` が同一性の基準。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: ItemKind,
    pub name: String,
    pub source_dir: PathBuf,
    /// RFC3339。
    pub synced_at: String,
    /// drybench が書いた時点の内容の sha256。
    pub content_hash: String,
}

/// 1 つの hook が `settings.json` に入れた登録の集合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSettings {
    pub name: String,
    pub entries: Vec<HookRegistration>,
}

/// `settings.json` の 1 イベントに入れたグループへの参照。
/// `group_hash` に一致するグループだけが除去対象になる（設計原則 5）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRegistration {
    pub event: String,
    pub group_hash: String,
}

impl Manifest {
    pub fn find(&self, kind: ItemKind, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.kind == kind && e.name == name)
    }

    /// 同じ `(kind, name)` があれば置き換える。重複は作らない。
    pub fn upsert(&mut self, entry: Entry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.kind == entry.kind && e.name == entry.name)
        {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
    }

    /// 記録を消して返す。無ければ `None`（存在しないものの除去はエラーではない）。
    pub fn remove(&mut self, kind: ItemKind, name: &str) -> Option<Entry> {
        let i = self
            .entries
            .iter()
            .position(|e| e.kind == kind && e.name == name)?;
        Some(self.entries.remove(i))
    }

    pub fn find_hook(&self, name: &str) -> Option<&HookSettings> {
        self.hook_settings.iter().find(|h| h.name == name)
    }

    pub fn upsert_hook(&mut self, hook: HookSettings) {
        match self.hook_settings.iter_mut().find(|h| h.name == hook.name) {
            Some(existing) => *existing = hook,
            None => self.hook_settings.push(hook),
        }
    }

    pub fn remove_hook(&mut self, name: &str) -> Option<HookSettings> {
        let i = self.hook_settings.iter().position(|h| h.name == name)?;
        Some(self.hook_settings.remove(i))
    }
}

/// `claude_dir` のマニフェストを読む。
///
/// **存在しなければ空のマニフェストを返す**（初回起動はこの状態で始まる）。
/// **壊れていれば `Err`** — 空として扱うと「全項目が Unmanaged」になり、実際には管理下に
/// あるものまで見失う。ユーザーに壊れていることを伝えるのが正しい。
pub fn load(claude_dir: &Path) -> Result<Manifest, ManifestError> {
    let path = claude_dir.join(MANIFEST_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(e) => return Err(ManifestError::Io(path, e)),
    };
    serde_json::from_str(&raw).map_err(|e| ManifestError::Parse(path, e))
}

/// マニフェストを書く。一時ファイルへ書いてから `rename` するので、途中で落ちても
/// 壊れたマニフェストは残らない。
pub fn save(claude_dir: &Path, manifest: &Manifest) -> Result<(), ManifestError> {
    fs::create_dir_all(claude_dir).map_err(|e| ManifestError::Io(claude_dir.to_path_buf(), e))?;

    let body = serde_json::to_string_pretty(manifest)
        .map_err(|e| ManifestError::Parse(claude_dir.join(MANIFEST_FILE), e))?;

    let temp = claude_dir.join(TEMP_FILE);
    fs::write(&temp, body).map_err(|e| ManifestError::Io(temp.clone(), e))?;

    let final_path = claude_dir.join(MANIFEST_FILE);
    fs::rename(&temp, &final_path).map_err(|e| {
        // rename に失敗したら一時ファイルを残さない。
        let _ = fs::remove_file(&temp);
        ManifestError::Io(final_path, e)
    })
}

#[derive(Debug)]
pub enum ManifestError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, serde_json::Error),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::Parse(path, e) => {
                write!(f, "{} が壊れている: {e}", path.display())
            }
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use crate::testutil::TempDir;
    use std::fs;

    fn entry(kind: ItemKind, name: &str, hash: &str) -> Entry {
        Entry {
            kind,
            name: name.to_string(),
            source_dir: PathBuf::from("/src"),
            synced_at: "2026-08-06T00:00:00Z".to_string(),
            content_hash: hash.to_string(),
        }
    }

    // --- 読み込み ---

    // 初回起動はマニフェストが無い状態で始まる。ここでエラーにすると起動できない。
    #[test]
    fn a_missing_manifest_loads_as_empty() {
        let t = TempDir::new("manifest-missing");
        let m = load(t.path()).expect("マニフェスト不在はエラーにしない");
        assert!(m.entries.is_empty());
        assert!(m.hook_settings.is_empty());
        assert_eq!(m.version, VERSION);
    }

    // 壊れたマニフェストを「空」として扱ってはいけない。空は「全項目が Unmanaged」を
    // 意味してしまい、実際には管理下にあるものまで見失う。
    #[test]
    fn a_malformed_manifest_is_an_error_not_an_empty_one() {
        let t = TempDir::new("manifest-malformed");
        t.write(MANIFEST_FILE, "{ this is not json");
        assert!(load(t.path()).is_err());
    }

    // `//!` に明記された後方互換要件。
    #[test]
    fn a_manifest_without_hook_settings_still_loads() {
        let t = TempDir::new("manifest-compat");
        t.write(
            MANIFEST_FILE,
            r#"{"version":1,"entries":[
                {"kind":"skill","name":"old","source_dir":"/src",
                 "synced_at":"2026-01-01T00:00:00Z","content_hash":"abc"}
            ]}"#,
        );

        let m = load(t.path()).expect("hook_settings 無しでも読める");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].name, "old");
        assert!(m.hook_settings.is_empty());
    }

    #[test]
    fn kinds_are_stored_as_lowercase_strings() {
        let t = TempDir::new("manifest-kind");
        t.write(
            MANIFEST_FILE,
            r#"{"version":1,"entries":[
                {"kind":"skill","name":"s","source_dir":"/src","synced_at":"t","content_hash":"h"},
                {"kind":"agent","name":"a","source_dir":"/src","synced_at":"t","content_hash":"h"},
                {"kind":"hook","name":"h","source_dir":"/src","synced_at":"t","content_hash":"h"}
            ]}"#,
        );

        let kinds: Vec<ItemKind> = load(t.path())
            .unwrap()
            .entries
            .iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![ItemKind::Skill, ItemKind::Agent, ItemKind::Hook]
        );
    }

    // --- 書き込み ---

    #[test]
    fn save_then_load_round_trips() {
        let t = TempDir::new("manifest-roundtrip");
        let mut m = Manifest::default();
        m.upsert(entry(ItemKind::Skill, "my-skill", "hash1"));
        m.upsert_hook(HookSettings {
            name: "my-hook".to_string(),
            entries: vec![HookRegistration {
                event: "PostToolUse".to_string(),
                group_hash: "ghash".to_string(),
            }],
        });

        save(t.path(), &m).unwrap();

        assert_eq!(load(t.path()).unwrap(), m);
    }

    #[test]
    fn save_creates_the_target_directory_if_it_is_missing() {
        let t = TempDir::new("manifest-mkdir");
        let target = t.join("not-created-yet");

        save(&target, &Manifest::default()).unwrap();

        assert!(target.join(MANIFEST_FILE).is_file());
    }

    // 途中で落ちても壊れたマニフェストを残さないよう、一時ファイルへ書いてから rename する。
    // 成功後に一時ファイルが残っていてはいけない。
    #[test]
    fn save_leaves_no_temporary_file_behind() {
        let t = TempDir::new("manifest-tmp");
        save(t.path(), &Manifest::default()).unwrap();

        let leftovers: Vec<String> = fs::read_dir(t.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != MANIFEST_FILE)
            .collect();
        assert!(leftovers.is_empty(), "残骸: {leftovers:?}");
    }

    #[test]
    fn overwriting_an_existing_manifest_leaves_valid_json() {
        let t = TempDir::new("manifest-overwrite");
        let mut m = Manifest::default();
        m.upsert(entry(ItemKind::Skill, "first", "h1"));
        save(t.path(), &m).unwrap();

        m.remove(ItemKind::Skill, "first");
        m.upsert(entry(ItemKind::Agent, "second", "h2"));
        save(t.path(), &m).unwrap();

        let loaded = load(t.path()).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "second");
    }

    // --- 検索・追加・除去 ---

    #[test]
    fn entries_are_identified_by_kind_and_name_together() {
        let mut m = Manifest::default();
        m.upsert(entry(ItemKind::Skill, "same", "skill-hash"));
        m.upsert(entry(ItemKind::Agent, "same", "agent-hash"));

        // 名前が同じでも種別が違えば別物。
        assert_eq!(m.entries.len(), 2);
        assert_eq!(
            m.find(ItemKind::Skill, "same")
                .map(|e| e.content_hash.as_str()),
            Some("skill-hash")
        );
        assert_eq!(
            m.find(ItemKind::Agent, "same")
                .map(|e| e.content_hash.as_str()),
            Some("agent-hash")
        );
        assert!(m.find(ItemKind::Hook, "same").is_none());
    }

    #[test]
    fn upsert_replaces_rather_than_duplicates() {
        let mut m = Manifest::default();
        m.upsert(entry(ItemKind::Skill, "s", "old"));
        m.upsert(entry(ItemKind::Skill, "s", "new"));

        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.find(ItemKind::Skill, "s").unwrap().content_hash, "new");
    }

    #[test]
    fn remove_returns_the_entry_and_forgets_it() {
        let mut m = Manifest::default();
        m.upsert(entry(ItemKind::Skill, "s", "h"));

        assert_eq!(
            m.remove(ItemKind::Skill, "s").map(|e| e.name),
            Some("s".to_string())
        );
        assert!(m.find(ItemKind::Skill, "s").is_none());
        // 二度目は None。存在しないものの除去はエラーにしない。
        assert!(m.remove(ItemKind::Skill, "s").is_none());
    }

    // --- hook_settings ---

    #[test]
    fn hook_settings_are_tracked_separately_by_name() {
        let mut m = Manifest::default();
        m.upsert_hook(HookSettings {
            name: "h".to_string(),
            entries: vec![HookRegistration {
                event: "Stop".to_string(),
                group_hash: "g1".to_string(),
            }],
        });
        m.upsert_hook(HookSettings {
            name: "h".to_string(),
            entries: vec![HookRegistration {
                event: "PostToolUse".to_string(),
                group_hash: "g2".to_string(),
            }],
        });

        assert_eq!(m.hook_settings.len(), 1);
        assert_eq!(m.find_hook("h").unwrap().entries[0].group_hash, "g2");

        assert!(m.remove_hook("h").is_some());
        assert!(m.find_hook("h").is_none());
        assert!(m.remove_hook("h").is_none());
    }
}
