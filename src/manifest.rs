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
//! 読み込み時に拒否するもの:
//!
//! - **`VERSION` より新しい `version`。** `save` は全書き換えなので、知らないフィールドを
//!   落としたまま新しい version を名乗るファイルを書き戻してしまう。
//! - **`(kind, name)` の重複。** `remove` は 1 件しか消さないため、重複があると記録だけが
//!   生き残り、後からユーザーが置いた実体を「自分のもの」と誤認しうる（設計原則 1）。
//!
//! TODO(migrate): 実装は `apps/proteus/src/manifest.rs` から移す。
//! 移設時に加えること: マニフェストのファイル名を `.proteus-manifest.json` から
//! `.drybench-manifest.json` へ変更する。

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::model::ItemKind;

/// ターゲットディレクトリ直下に置くマニフェストのファイル名。
pub const MANIFEST_FILE: &str = ".drybench-manifest.json";

/// 書き出すスキーマバージョン。これより新しいマニフェストは読まない。
pub const VERSION: u32 = 1;

/// 一時ファイル名の連番。**固定名にしてはいけない** — 同じターゲットに対して 2 つの
/// drybench が同時に保存すると、片方の `save()` が成功を返しながら他方の内容を公開して
/// しまう（記録だけが失われ、その実体は恒久的に Unmanaged になる）。
static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 一時ファイル名の生成を諦めるまでの試行回数。前回のクラッシュが残した残骸と
/// 衝突しても、数回ずらせば空きが見つかる。
const TEMP_ATTEMPTS: usize = 8;

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

    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|e| ManifestError::Parse(path.clone(), e))?;

    // 自分より新しいマニフェストは読まない。`save` は全書き換えなので、知らないフィールドを
    // 落としたまま新しい version を名乗るファイルを書き戻してしまう。
    if manifest.version > VERSION {
        return Err(ManifestError::UnsupportedVersion {
            path,
            found: manifest.version,
        });
    }

    // `(kind, name)` の重複を受け入れてはいけない。`remove` は 1 件しか消さないので、
    // 重複があると記録だけが生き残る。その記録が後からユーザーが自分で置いた実体に
    // 一致してしまえば、drybench が置いていないものを「自分のもの」と誤認する
    // （設計原則 1 が破れる）。`upsert` が守る不変条件は、読み込み側でも守る必要がある。
    if let Some(dup) = first_duplicate(&manifest) {
        return Err(ManifestError::Duplicate { path, what: dup });
    }

    Ok(manifest)
}

/// 重複している記録があれば、その識別子を返す。
fn first_duplicate(manifest: &Manifest) -> Option<String> {
    for (i, e) in manifest.entries.iter().enumerate() {
        if manifest.entries[..i]
            .iter()
            .any(|prev| prev.kind == e.kind && prev.name == e.name)
        {
            return Some(format!("{:?}/{}", e.kind, e.name));
        }
    }
    for (i, h) in manifest.hook_settings.iter().enumerate() {
        if manifest.hook_settings[..i].iter().any(|p| p.name == h.name) {
            return Some(format!("hook_settings/{}", h.name));
        }
    }
    None
}

/// マニフェストを書く。
///
/// 一時ファイルへ書き、fsync してから `rename` する。プロセスが落ちても電源が落ちても、
/// 中途半端なマニフェストは残らない。
///
/// **一時ファイルは `create_new` で作る。** これが設計原則 4（シンボリックリンクを追わない）
/// を満たす要。`fs::write` はリンクを追って書くので、`~/.claude` に細工された
/// `.tmp` へのリンクがあると、ターゲット外の任意のファイルを上書きしてしまう。
pub fn save(claude_dir: &Path, manifest: &Manifest) -> Result<(), ManifestError> {
    fs::create_dir_all(claude_dir).map_err(|e| ManifestError::Io(claude_dir.to_path_buf(), e))?;

    let final_path = claude_dir.join(MANIFEST_FILE);

    // 書き込み先自体がシンボリックリンクなら、追わずに拒否する（設計原則 4）。
    // `rename` はリンクを追わずリンク自体を置き換えるので脱出はしないが、ユーザーが
    // 張ったリンクを黙って消すことになる。
    if fs::symlink_metadata(&final_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(ManifestError::SymlinkRefused(final_path));
    }

    let body = serde_json::to_string_pretty(manifest).map_err(ManifestError::Serialize)?;

    let (temp, mut file) = create_temp(claude_dir)?;

    // ここから先の失敗では、必ず一時ファイルを片付けてから返す。
    let write_result = file
        .write_all(body.as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(ManifestError::Io(temp, e));
    }

    if let Err(e) = fs::rename(&temp, &final_path) {
        let _ = fs::remove_file(&temp);
        return Err(ManifestError::Io(final_path, e));
    }

    // rename 自体を耐久化する。ここまでやって初めて「途中で落ちても壊れない」と言える。
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(claude_dir) {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// まだ存在しない一時ファイルを作って開く。`create_new` なので、既存のファイルにも
/// シンボリックリンクにも書き込まない。
fn create_temp(claude_dir: &Path) -> Result<(PathBuf, fs::File), ManifestError> {
    let pid = std::process::id();
    let mut last = None;
    for _ in 0..TEMP_ATTEMPTS {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = claude_dir.join(format!("{MANIFEST_FILE}.{pid}.{n}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some((path, e)),
            Err(e) => return Err(ManifestError::Io(path, e)),
        }
    }
    let (path, e) = last.expect("TEMP_ATTEMPTS は 1 以上");
    Err(ManifestError::Io(path, e))
}

#[derive(Debug)]
pub enum ManifestError {
    Io(PathBuf, std::io::Error),
    /// ディスク上のマニフェストが読めない。**ユーザーには重い意味を持つ** —
    /// 記録が信用できない＝管理下のものが分からない、ということ。
    Parse(PathBuf, serde_json::Error),
    /// 書き出そうとした内容を JSON にできない。ディスク上のファイルは無傷。
    Serialize(serde_json::Error),
    Duplicate {
        path: PathBuf,
        what: String,
    },
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
    },
    SymlinkRefused(PathBuf),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::Parse(path, e) => write!(f, "{} が壊れている: {e}", path.display()),
            Self::Serialize(e) => write!(f, "マニフェストを JSON にできない: {e}"),
            Self::Duplicate { path, what } => write!(
                f,
                "{} に重複した記録がある: {what}。手で直す必要がある",
                path.display()
            ),
            Self::UnsupportedVersion { path, found } => write!(
                f,
                "{} は version {found}。この drybench が読めるのは {VERSION} まで",
                path.display()
            ),
            Self::SymlinkRefused(path) => write!(
                f,
                "{} はシンボリックリンク。追わずに中止した",
                path.display()
            ),
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
    fn kinds_are_read_as_lowercase_strings() {
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

    // ラウンドトリップだけではフィールド名の改名を検出できない（読み書き両方が同じ名前で
    // ずれるので通ってしまう）。**書き出した生 JSON を `//!` のスキーマと突き合わせる**のが、
    // 仕様どおりのマニフェストを書けていることを守る唯一の手段。
    #[test]
    fn the_written_json_matches_the_documented_schema() {
        let t = TempDir::new("manifest-schema");
        let mut m = Manifest::default();
        m.upsert(Entry {
            kind: ItemKind::Skill,
            name: "my-skill".to_string(),
            source_dir: PathBuf::from("/abs/path"),
            synced_at: "2026-08-06T00:00:00Z".to_string(),
            content_hash: "sha256hex".to_string(),
        });
        m.upsert_hook(HookSettings {
            name: "my-hook".to_string(),
            entries: vec![HookRegistration {
                event: "PostToolUse".to_string(),
                group_hash: "grouphex".to_string(),
            }],
        });
        save(t.path(), &m).unwrap();

        let raw = fs::read_to_string(t.join(MANIFEST_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        assert_eq!(v["version"], 1);

        let e = &v["entries"][0];
        assert_eq!(e["kind"], "skill");
        assert_eq!(e["name"], "my-skill");
        assert_eq!(e["source_dir"], "/abs/path");
        assert_eq!(e["synced_at"], "2026-08-06T00:00:00Z");
        assert_eq!(e["content_hash"], "sha256hex");

        // hook_settings は entries と別配列（`//!` の要件）。
        let h = &v["hook_settings"][0];
        assert_eq!(h["name"], "my-hook");
        assert_eq!(h["entries"][0]["event"], "PostToolUse");
        assert_eq!(h["entries"][0]["group_hash"], "grouphex");

        // トップレベルはこの 3 キーだけ。
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["entries", "hook_settings", "version"]);
    }

    // 重複を受け入れると `remove` 後に記録だけが生き残り、後からユーザーが置いた実体を
    // 「自分のもの」と誤認しうる（設計原則 1）。読み込みで止める。
    #[test]
    fn a_manifest_with_duplicate_records_is_rejected() {
        let t = TempDir::new("manifest-dup");
        t.write(
            MANIFEST_FILE,
            r#"{"version":1,"entries":[
                {"kind":"skill","name":"x","source_dir":"/s","synced_at":"t","content_hash":"stale"},
                {"kind":"skill","name":"x","source_dir":"/s","synced_at":"t","content_hash":"new"}
            ]}"#,
        );
        assert!(matches!(
            load(t.path()),
            Err(ManifestError::Duplicate { .. })
        ));
    }

    #[test]
    fn duplicate_hook_settings_are_rejected_too() {
        let t = TempDir::new("manifest-dup-hook");
        t.write(
            MANIFEST_FILE,
            r#"{"version":1,"entries":[],"hook_settings":[
                {"name":"h","entries":[]},
                {"name":"h","entries":[]}
            ]}"#,
        );
        assert!(matches!(
            load(t.path()),
            Err(ManifestError::Duplicate { .. })
        ));
    }

    // 同じ名前でも種別が違えば別物。重複判定が名前だけを見ていないことの確認。
    #[test]
    fn the_same_name_under_different_kinds_is_not_a_duplicate() {
        let t = TempDir::new("manifest-not-dup");
        t.write(
            MANIFEST_FILE,
            r#"{"version":1,"entries":[
                {"kind":"skill","name":"x","source_dir":"/s","synced_at":"t","content_hash":"a"},
                {"kind":"agent","name":"x","source_dir":"/s","synced_at":"t","content_hash":"b"}
            ]}"#,
        );
        assert_eq!(load(t.path()).unwrap().entries.len(), 2);
    }

    // 新しい drybench が書いたマニフェストを古いバイナリが開いて保存すると、知らない
    // フィールドを落としたまま新しい version を名乗るファイルになる。読まずに止める。
    #[test]
    fn a_newer_manifest_version_is_refused() {
        let t = TempDir::new("manifest-version");
        t.write(
            MANIFEST_FILE,
            &format!(r#"{{"version":{},"entries":[]}}"#, VERSION + 1),
        );
        assert!(matches!(
            load(t.path()),
            Err(ManifestError::UnsupportedVersion { .. })
        ));
    }

    // 読めない理由の区別は UI 上まったく意味が違う。`is_err()` だけでは入れ替わりを検出できない。
    #[test]
    fn unreadable_and_malformed_are_different_errors() {
        let t = TempDir::new("manifest-errkind");

        for broken in ["", "   ", "[]", "\"hello\"", "null", "{\"version\":1} tail"] {
            t.write(MANIFEST_FILE, broken);
            assert!(
                matches!(load(t.path()), Err(ManifestError::Parse(..))),
                "壊れた JSON は Parse であるべき: {broken:?}"
            );
        }

        // マニフェストのパスがディレクトリなら、壊れているのではなく読めない。
        let d = TempDir::new("manifest-isdir");
        d.mkdir(MANIFEST_FILE);
        assert!(matches!(load(d.path()), Err(ManifestError::Io(..))));
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

    // 設計原則 4: 書き込み先がシンボリックリンクなら追わずに拒否する。追ってしまうと
    // ターゲット外の任意のファイルを上書きできてしまう。
    #[cfg(unix)]
    #[test]
    fn save_refuses_a_symlinked_manifest_path_and_leaves_the_victim_alone() {
        let t = TempDir::new("manifest-symlink");
        let victim = t.write("victim.txt", "触ってはいけない");
        let claude_dir = t.mkdir("claude");
        std::os::unix::fs::symlink(&victim, claude_dir.join(MANIFEST_FILE)).unwrap();

        let err = save(&claude_dir, &Manifest::default()).unwrap_err();

        assert!(matches!(err, ManifestError::SymlinkRefused(..)));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "触ってはいけない");
    }

    // 一時ファイル名を固定にすると、同じターゲットへ同時に保存したとき片方の `save()` が
    // 成功を返しながら他方の内容を公開してしまう。呼ぶたびに別の名前になること。
    #[test]
    fn each_save_uses_a_fresh_temporary_path() {
        let t = TempDir::new("manifest-tempname");
        let (a, _fa) = create_temp(t.path()).unwrap();
        let (b, _fb) = create_temp(t.path()).unwrap();

        assert_ne!(a, b);
        assert!(a.is_file() && b.is_file());
    }

    // 前回のクラッシュが一時ファイルを残していても、次の保存は成功する。
    #[test]
    fn a_leftover_temporary_file_does_not_block_the_next_save() {
        let t = TempDir::new("manifest-stale-tmp");
        t.write(format!("{MANIFEST_FILE}.99999.0.tmp"), "クラッシュの残骸");

        save(t.path(), &Manifest::default()).unwrap();

        assert!(load(t.path()).is_ok());
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
