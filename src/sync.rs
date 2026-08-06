//! ハッシュ計算、状態判定、同期計画の作成と適用。安全ゲートの実体。
//!
//! 保存時の状態判定ルール表:
//!
//! | 状況 | チェック ON | チェック OFF |
//! |---|---|---|
//! | ターゲット側に無い | コピーしてマニフェスト追加 | 何もしない |
//! | マニフェストあり・ハッシュ一致 | 再コピー（更新） | 削除してマニフェストから除去 |
//! | マニフェストあり・ハッシュ不一致 | Conflict: 既定スキップ、明示的に許可した場合のみ上書き |
//!   Conflict: 削除しない、警告表示 |
//! | マニフェストに無いが実体あり | Unmanaged: 常に上書きしない | Unmanaged: 常に削除しない |
//!
//! ゲート（設計原則 1〜4）:
//!
//! 1. マニフェストに登録があるか
//! 2. 記録した `content_hash` と現在の実体のハッシュが一致するか
//! 3. 実行直前に状態を再判定する（TOCTOU）
//! 4. 対象パスがターゲット配下であることを canonical path で確認し、対象自体が
//!    シンボリックリンクなら追わず拒否する
//!
//! TODO(migrate): 実装は `apps/proteus/src/sync.rs` から移す。
//! 移設時に加えること: `assert_within_claude_dir` を汎用名に変え、ターゲットを引数で受ける
//! （`~/.claude` ↔ プロジェクトの `.claude/` 切り替えは v0.2）。

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::manifest::Manifest;
use crate::model::{DraftItem, ItemKind, ItemState};

// ---------------------------------------------------------------- ハッシュ

/// 実体の内容から sha256 を計算する。**この値が設計原則 2 の全体**（記録した値と一致
/// しなければ触らない）なので、同じ内容なら必ず同じ値、違えば必ず違う値でなければならない。
///
/// ディレクトリ単位の種別では、配下の全ファイルを**相対パス順に並べてから**混ぜる。
/// `read_dir` の順序は OS 依存で、それに依存すると同じ内容が Conflict として報告される。
/// 相対パス自体も混ぜるので、中身が同じままファイル名が変わっても値は変わる。
///
/// シンボリックリンクは**追わずに拒否する**（設計原則 4）。中身を読んで混ぜてしまうと、
/// 後のコピーでターゲット外の内容を持ち込むことになる。
pub fn hash_item(kind: ItemKind, path: &Path) -> Result<String, SyncError> {
    let mut hasher = Sha256::new();
    if kind.is_dir_unit() {
        let mut files = Vec::new();
        collect_files(path, path, &mut files)?;
        // 相対パスで並べ替えて、走査順から切り離す。
        files.sort();
        for rel in files {
            let bytes = fs::read(path.join(&rel)).map_err(|e| SyncError::Io(path.join(&rel), e))?;
            // 長さ前置で「a/b + c」と「a + b/c」のような食い違いを防ぐ。
            mix(&mut hasher, rel.as_bytes());
            mix(&mut hasher, &bytes);
        }
    } else {
        let bytes = read_file_refusing_symlinks(path)?;
        mix(&mut hasher, &bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn mix(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// `root` 配下のファイルを、`root` からの相対パス（`/` 区切り）で集める。
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), SyncError> {
    let entries = fs::read_dir(dir).map_err(|e| SyncError::Io(dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| SyncError::Io(dir.to_path_buf(), e))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| SyncError::Io(path.clone(), e))?;

        if meta.file_type().is_symlink() {
            return Err(SyncError::SymlinkRefused(path));
        }
        if meta.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| SyncError::OutsideTarget(path.clone()))?;
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn read_file_refusing_symlinks(path: &Path) -> Result<Vec<u8>, SyncError> {
    let meta = fs::symlink_metadata(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
    if meta.file_type().is_symlink() {
        return Err(SyncError::SymlinkRefused(path.to_path_buf()));
    }
    fs::read(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))
}

// ------------------------------------------------------------- 状態の判定

/// 一覧に出す 1 行。`scan` の結果とマニフェストを突き合わせて作る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedItem {
    pub kind: ItemKind,
    pub name: String,
    pub state: ItemState,
    /// ソース側の実体。ソースから消えていれば `None`（インストール済みなら一覧には残す）。
    pub source_path: Option<PathBuf>,
    /// ターゲット側の実体。入っていなければ `None`。
    pub target_path: Option<PathBuf>,
    /// 入れるならここ、というターゲット側のパス。まだ存在しなくてよい。
    /// **`resolve` に渡したターゲットディレクトリからしか作らない** — 書き込み先を
    /// 呼び出し側の思い込みではなく、配置規約から一意に決めるため。
    pub install_path: PathBuf,
    /// マニフェストに記録があるか。実体が消えていても記録だけ残ることがある。
    pub recorded: bool,
}

/// ソース・ターゲット・マニフェストを突き合わせて、各項目の状態を決める。
///
/// **ここがゲート 1・2**（記録の有無と、記録したハッシュとの一致）。判定の結果は
/// `ItemState` に落ち、`is_locked()` が「触ってよいか」の唯一の判断点になる。
///
/// 並び順は Unmanaged が先頭（企画 §7）。管理外の実体が見えていること自体が import の入口。
pub fn resolve(
    source: &[DraftItem],
    target: &[DraftItem],
    manifest: &Manifest,
    target_dir: &Path,
) -> Result<Vec<ListedItem>, SyncError> {
    let mut items: Vec<ListedItem> = Vec::new();

    // ソース側にあるもの。
    for s in source {
        let installed = find(target, s.kind, &s.name);
        let record = manifest.find(s.kind, &s.name);
        items.push(ListedItem {
            kind: s.kind,
            name: s.name.clone(),
            state: state_for(s.kind, installed, record.map(|r| r.content_hash.as_str()))?,
            source_path: Some(s.path.clone()),
            target_path: installed.map(|t| t.path.clone()),
            install_path: s.kind.path_in(target_dir, &s.name),
            recorded: record.is_some(),
        });
    }

    // ターゲットにだけあるもの。ソースから消えていても、インストール済みなら
    // 一覧に残さないと外す手段が無くなる。
    for t in target {
        if find(source, t.kind, &t.name).is_some() {
            continue;
        }
        let record = manifest.find(t.kind, &t.name);
        items.push(ListedItem {
            kind: t.kind,
            name: t.name.clone(),
            state: state_for(t.kind, Some(t), record.map(|r| r.content_hash.as_str()))?,
            source_path: None,
            target_path: Some(t.path.clone()),
            install_path: t.kind.path_in(target_dir, &t.name),
            recorded: record.is_some(),
        });
    }

    items.sort_by(|a, b| {
        a.state
            .sort_key()
            .cmp(&b.state.sort_key())
            .then_with(|| a.kind.dir_name().cmp(b.kind.dir_name()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(items)
}

/// `//!` の状態表そのもの。
fn state_for(
    kind: ItemKind,
    installed: Option<&DraftItem>,
    recorded_hash: Option<&str>,
) -> Result<ItemState, SyncError> {
    let Some(installed) = installed else {
        // ターゲット側に無い。記録だけ残っていても、入っていないことに変わりはない。
        return Ok(ItemState::Off);
    };
    let Some(recorded_hash) = recorded_hash else {
        // 実体はあるが記録が無い。**触らない**（設計原則 1）。
        return Ok(ItemState::Unmanaged);
    };
    if hash_item(kind, &installed.path)? == recorded_hash {
        Ok(ItemState::On)
    } else {
        // 記録した後で誰かが書き換えた（設計原則 2）。
        Ok(ItemState::Conflict)
    }
}

fn find<'a>(items: &'a [DraftItem], kind: ItemKind, name: &str) -> Option<&'a DraftItem> {
    items.iter().find(|i| i.kind == kind && i.name == name)
}

// ----------------------------------------------------------------- 計画

/// ユーザーが確認画面までに指示したこと。
#[derive(Debug, Clone, Default)]
pub struct Selection {
    on: HashSet<(ItemKind, String)>,
    /// Conflict の**上書きだけ**を項目ごとに許可する。削除には効かない。
    conflict_overrides: HashSet<(ItemKind, String)>,
}

impl Selection {
    pub fn turn_on(&mut self, kind: ItemKind, name: &str) {
        self.on.insert((kind, name.to_string()));
    }

    pub fn allow_conflict(&mut self, kind: ItemKind, name: &str) {
        self.conflict_overrides.insert((kind, name.to_string()));
    }

    pub fn is_on(&self, kind: ItemKind, name: &str) -> bool {
        self.on.contains(&(kind, name.to_string()))
    }

    pub fn conflict_allowed(&self, kind: ItemKind, name: &str) -> bool {
        self.conflict_overrides.contains(&(kind, name.to_string()))
    }
}

/// 実行する 1 手。確認画面はこの一覧をそのまま見せる（設計原則 6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// ソースからターゲットへ入れる（新規・更新の両方）。
    Copy {
        kind: ItemKind,
        name: String,
        from: PathBuf,
        to: PathBuf,
    },
    /// ターゲットから外す。記録も消す。
    Delete {
        kind: ItemKind,
        name: String,
        path: PathBuf,
    },
    /// 実体はもう無いが記録だけ残っている。記録を掃除する。
    Prune { kind: ItemKind, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub kind: ItemKind,
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 記録が無い実体。**override は存在しない**（設計原則 1）。
    Unmanaged,
    /// 記録後に書き換えられている。上書きは明示的な許可が要る（設計原則 2）。
    Conflict,
    /// ソースから消えているので、入れ直す元が無い。
    NoSource,
}

/// 何をするかの一覧。**ここではまだ何も書き込まない。**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// 状態と指示から計画を組む。`//!` の状態表をそのまま実装している。
///
/// **ロックされた状態（Conflict / Unmanaged）は、原則として計画に入らない。**
/// 唯一の例外が Conflict の上書きで、それも項目ごとの明示的な許可があるときだけ。
/// Unmanaged には例外が無い。
pub fn build_plan(items: &[ListedItem], selection: &Selection) -> Plan {
    let mut plan = Plan::default();

    for item in items {
        let wants_on = selection.is_on(item.kind, &item.name);
        let skip = |reason| Skipped {
            kind: item.kind,
            name: item.name.clone(),
            reason,
        };

        match item.state {
            // 記録が無い実体には、どちらの向きでも、どんな許可があっても触らない。
            ItemState::Unmanaged => plan.skipped.push(skip(SkipReason::Unmanaged)),

            // 上書きは明示的な許可があるときだけ。削除は表のとおり常に行わない。
            ItemState::Conflict => {
                match (wants_on, selection.conflict_allowed(item.kind, &item.name)) {
                    (true, true) => match &item.source_path {
                        Some(from) => plan.actions.push(copy(item, from)),
                        None => plan.skipped.push(skip(SkipReason::NoSource)),
                    },
                    _ => plan.skipped.push(skip(SkipReason::Conflict)),
                }
            }

            ItemState::On if wants_on => match &item.source_path {
                // ソースの編集を取り込むため、一致していても入れ直す。
                Some(from) => plan.actions.push(copy(item, from)),
                None => plan.skipped.push(skip(SkipReason::NoSource)),
            },
            ItemState::On => {
                let path = item
                    .target_path
                    .clone()
                    .expect("On はターゲットに実体がある");
                plan.actions.push(Action::Delete {
                    kind: item.kind,
                    name: item.name.clone(),
                    path,
                });
            }

            ItemState::Off if wants_on => match &item.source_path {
                Some(from) => plan.actions.push(copy(item, from)),
                None => plan.skipped.push(skip(SkipReason::NoSource)),
            },
            // 入っていないものを OFF のままにするのは何もしないこと。ただし記録だけが
            // 残っているなら掃除する。
            ItemState::Off => {
                if item.recorded {
                    plan.actions.push(Action::Prune {
                        kind: item.kind,
                        name: item.name.clone(),
                    });
                }
            }
        }
    }

    plan
}

fn copy(item: &ListedItem, from: &Path) -> Action {
    Action::Copy {
        kind: item.kind,
        name: item.name.clone(),
        from: from.to_path_buf(),
        // 既に入っている場合も含め、宛先は必ず配置規約から作る。
        to: item.install_path.clone(),
    }
}

#[derive(Debug)]
pub enum SyncError {
    Io(PathBuf, std::io::Error),
    /// シンボリックリンクは追わない（設計原則 4）。
    SymlinkRefused(PathBuf),
    /// 解決したパスが対象ディレクトリの外を指していた（設計原則 4）。
    OutsideTarget(PathBuf),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::SymlinkRefused(path) => write!(
                f,
                "{} はシンボリックリンク。追わずに中止した",
                path.display()
            ),
            Self::OutsideTarget(path) => {
                write!(f, "{} は対象ディレクトリの外を指している", path.display())
            }
        }
    }
}

impl std::error::Error for SyncError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Entry, Manifest};
    use crate::model::{ItemKind, ItemState};
    use crate::scan;
    use crate::testutil::TempDir;

    // --- 足場 ---

    struct Bench {
        t: TempDir,
        source: PathBuf,
        target: PathBuf,
        manifest: Manifest,
    }

    impl Bench {
        fn new(tag: &str) -> Self {
            let t = TempDir::new(tag);
            let source = t.mkdir("source");
            let target = t.mkdir("target");
            Self {
                t,
                source,
                target,
                manifest: Manifest::default(),
            }
        }

        /// ソース側に skill を置く。
        fn source_skill(&self, name: &str, body: &str) -> PathBuf {
            self.t.write(format!("source/skills/{name}/SKILL.md"), body);
            self.source.join("skills").join(name)
        }

        /// ターゲット側に skill を置く。
        fn target_skill(&self, name: &str, body: &str) -> PathBuf {
            self.t.write(format!("target/skills/{name}/SKILL.md"), body);
            self.target.join("skills").join(name)
        }

        /// ターゲット側の実体の現在のハッシュで、マニフェストに記録を作る（＝ On になる）。
        fn record_matching(&mut self, name: &str) {
            let path = self.target.join("skills").join(name);
            let hash = hash_item(ItemKind::Skill, &path).unwrap();
            self.record_with_hash(name, &hash);
        }

        fn record_with_hash(&mut self, name: &str, hash: &str) {
            self.manifest.upsert(Entry {
                kind: ItemKind::Skill,
                name: name.to_string(),
                source_dir: self.source.clone(),
                synced_at: "2026-08-06T00:00:00Z".to_string(),
                content_hash: hash.to_string(),
            });
        }

        fn resolve(&self) -> Vec<ListedItem> {
            let source = scan::scan(&self.source);
            let target = scan::scan(&self.target);
            resolve(&source, &target, &self.manifest, &self.target).unwrap()
        }

        fn state_of(&self, name: &str) -> ItemState {
            self.resolve()
                .into_iter()
                .find(|i| i.name == name)
                .unwrap_or_else(|| panic!("{name} が一覧に無い"))
                .state
        }
    }

    fn on(names: &[&str]) -> Selection {
        let mut s = Selection::default();
        for n in names {
            s.turn_on(ItemKind::Skill, n);
        }
        s
    }

    // --- ハッシュ ---

    #[test]
    fn the_same_content_hashes_the_same() {
        let b = Bench::new("hash-same");
        let a = b.source_skill("a", "同じ中身");
        let c = b.target_skill("a", "同じ中身");

        assert_eq!(
            hash_item(ItemKind::Skill, &a).unwrap(),
            hash_item(ItemKind::Skill, &c).unwrap()
        );
    }

    #[test]
    fn editing_a_file_changes_the_directory_hash() {
        let b = Bench::new("hash-edit");
        let dir = b.source_skill("a", "もと");
        let before = hash_item(ItemKind::Skill, &dir).unwrap();

        b.source_skill("a", "変えた");

        assert_ne!(before, hash_item(ItemKind::Skill, &dir).unwrap());
    }

    #[test]
    fn adding_or_removing_a_file_changes_the_directory_hash() {
        let b = Bench::new("hash-addremove");
        let dir = b.source_skill("a", "x");
        let base = hash_item(ItemKind::Skill, &dir).unwrap();

        let extra = b.t.write("source/skills/a/extra.md", "y");
        let with_extra = hash_item(ItemKind::Skill, &dir).unwrap();
        assert_ne!(base, with_extra, "ファイル追加でハッシュが変わらない");

        std::fs::remove_file(&extra).unwrap();
        assert_eq!(base, hash_item(ItemKind::Skill, &dir).unwrap());
    }

    // 中身が同じでもファイル名が違えば別物。パスもハッシュに含める必要がある。
    #[test]
    fn renaming_a_file_inside_changes_the_directory_hash() {
        let b = Bench::new("hash-rename");
        let dir = b.source_skill("a", "x");
        b.t.write("source/skills/a/one.md", "同じ中身");
        let before = hash_item(ItemKind::Skill, &dir).unwrap();

        std::fs::rename(dir.join("one.md"), dir.join("two.md")).unwrap();

        assert_ne!(before, hash_item(ItemKind::Skill, &dir).unwrap());
    }

    // read_dir の順序は OS 依存。作成順が違うだけでハッシュが変わってはいけない
    // （変わると、同じ中身が Conflict として報告される）。
    #[test]
    fn the_directory_hash_does_not_depend_on_creation_order() {
        let b = Bench::new("hash-order");
        b.t.write("source/skills/a/SKILL.md", "s");
        b.t.write("source/skills/a/m.md", "m");
        b.t.write("source/skills/a/z.md", "z");
        b.t.write("source/skills/a/b.md", "b");

        b.t.write("target/skills/a/z.md", "z");
        b.t.write("target/skills/a/b.md", "b");
        b.t.write("target/skills/a/SKILL.md", "s");
        b.t.write("target/skills/a/m.md", "m");

        assert_eq!(
            hash_item(ItemKind::Skill, &b.source.join("skills/a")).unwrap(),
            hash_item(ItemKind::Skill, &b.target.join("skills/a")).unwrap()
        );
    }

    #[test]
    fn nested_directories_are_included_in_the_hash() {
        let b = Bench::new("hash-nested");
        let dir = b.source_skill("a", "x");
        let before = hash_item(ItemKind::Skill, &dir).unwrap();

        b.t.write("source/skills/a/deep/inner.md", "深い");

        assert_ne!(before, hash_item(ItemKind::Skill, &dir).unwrap());
    }

    #[test]
    fn a_single_file_kind_hashes_its_own_bytes() {
        let b = Bench::new("hash-agent");
        let a = b.t.write("source/agents/x.md", "エージェント");
        let same = b.t.write("target/agents/x.md", "エージェント");
        let other = b.t.write("source/agents/y.md", "別物");

        assert_eq!(
            hash_item(ItemKind::Agent, &a).unwrap(),
            hash_item(ItemKind::Agent, &same).unwrap()
        );
        assert_ne!(
            hash_item(ItemKind::Agent, &a).unwrap(),
            hash_item(ItemKind::Agent, &other).unwrap()
        );
    }

    // 設計原則 4: リンクは追わない。中身を読んでハッシュに混ぜると、後のコピーで
    // ターゲット外の内容を持ち込むことになる。
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_an_item_is_refused_rather_than_followed() {
        let b = Bench::new("hash-symlink");
        let dir = b.source_skill("a", "x");
        let outside = b.t.write("outside.txt", "外の秘密");
        std::os::unix::fs::symlink(&outside, dir.join("link.md")).unwrap();

        assert!(matches!(
            hash_item(ItemKind::Skill, &dir),
            Err(SyncError::SymlinkRefused(..))
        ));
    }

    // --- 状態判定（4 つの状況） ---

    #[test]
    fn in_source_but_not_installed_is_off() {
        let b = Bench::new("state-off");
        b.source_skill("a", "x");
        assert_eq!(b.state_of("a"), ItemState::Off);
    }

    #[test]
    fn recorded_and_matching_is_on() {
        let mut b = Bench::new("state-on");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");
        assert_eq!(b.state_of("a"), ItemState::On);
    }

    #[test]
    fn recorded_but_edited_by_hand_is_conflict() {
        let mut b = Bench::new("state-conflict");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");
        b.target_skill("a", "手で書き換えた");

        assert_eq!(b.state_of("a"), ItemState::Conflict);
    }

    #[test]
    fn installed_without_a_record_is_unmanaged() {
        let b = Bench::new("state-unmanaged");
        b.target_skill("a", "x");
        assert_eq!(b.state_of("a"), ItemState::Unmanaged);
    }

    // 記録はあるが実体が消えている（ユーザーが手で消した）。インストールされていないので Off。
    #[test]
    fn a_record_whose_target_is_gone_is_off() {
        let mut b = Bench::new("state-stale");
        b.source_skill("a", "x");
        b.record_with_hash("a", "もう存在しない実体のハッシュ");

        assert_eq!(b.state_of("a"), ItemState::Off);
    }

    // ソースから消えても、インストール済みなら一覧に残す。でなければ外す手段が無くなる。
    #[test]
    fn something_installed_but_no_longer_in_source_still_appears() {
        let mut b = Bench::new("state-nosource");
        b.target_skill("a", "x");
        b.record_matching("a");

        let items = b.resolve();
        let a = items.iter().find(|i| i.name == "a").unwrap();
        assert_eq!(a.state, ItemState::On);
        assert!(a.source_path.is_none());
    }

    #[test]
    fn unmanaged_entries_come_first_in_the_list() {
        let mut b = Bench::new("state-order");
        b.source_skill("managed", "x");
        b.target_skill("managed", "x");
        b.record_matching("managed");
        b.target_skill("stranger", "y");

        assert_eq!(b.resolve()[0].name, "stranger");
    }

    // --- 計画（状態表の 8 セル） ---

    #[test]
    fn off_checked_on_copies_it_in() {
        let b = Bench::new("plan-off-on");
        b.source_skill("a", "x");

        let plan = build_plan(&b.resolve(), &on(&["a"]));

        assert!(matches!(plan.actions.as_slice(), [Action::Copy { name, .. }] if name == "a"));
    }

    #[test]
    fn off_left_unchecked_does_nothing() {
        let b = Bench::new("plan-off-off");
        b.source_skill("a", "x");

        assert!(build_plan(&b.resolve(), &Selection::default())
            .actions
            .is_empty());
    }

    #[test]
    fn on_checked_on_re_copies_to_pick_up_source_edits() {
        let mut b = Bench::new("plan-on-on");
        b.source_skill("a", "新しい中身");
        b.target_skill("a", "x");
        b.record_matching("a");

        let plan = build_plan(&b.resolve(), &on(&["a"]));

        assert!(matches!(plan.actions.as_slice(), [Action::Copy { .. }]));
    }

    #[test]
    fn on_unchecked_deletes_it_and_forgets_the_record() {
        let mut b = Bench::new("plan-on-off");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");

        let plan = build_plan(&b.resolve(), &Selection::default());

        assert!(matches!(plan.actions.as_slice(), [Action::Delete { name, .. }] if name == "a"));
    }

    #[test]
    fn conflict_checked_on_is_skipped_without_permission() {
        let mut b = Bench::new("plan-conflict-on");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");
        b.target_skill("a", "手で書き換えた");

        let plan = build_plan(&b.resolve(), &on(&["a"]));

        assert!(plan.actions.is_empty(), "許可なしに上書きしてはいけない");
        assert!(matches!(
            plan.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::Conflict,
                ..
            }]
        ));
    }

    #[test]
    fn conflict_checked_on_is_overwritten_only_with_explicit_permission() {
        let mut b = Bench::new("plan-conflict-allow");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");
        b.target_skill("a", "手で書き換えた");

        let mut sel = on(&["a"]);
        sel.allow_conflict(ItemKind::Skill, "a");
        let plan = build_plan(&b.resolve(), &sel);

        assert!(matches!(plan.actions.as_slice(), [Action::Copy { .. }]));
    }

    // 許可は「上書き」にだけ効く。Conflict の削除は表のとおり常に行わない。
    #[test]
    fn conflict_is_never_deleted_even_with_permission() {
        let mut b = Bench::new("plan-conflict-off");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");
        b.target_skill("a", "手で書き換えた");

        let mut sel = Selection::default();
        sel.allow_conflict(ItemKind::Skill, "a");
        let plan = build_plan(&b.resolve(), &sel);

        assert!(plan.actions.is_empty());
        assert!(matches!(
            plan.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::Conflict,
                ..
            }]
        ));
    }

    // 設計原則 1 に override は無い。どちらの向きでも、どんな許可があっても計画に現れない。
    #[test]
    fn unmanaged_never_enters_the_plan_in_either_direction() {
        let b = Bench::new("plan-unmanaged");
        b.target_skill("a", "x");

        for sel in [
            Selection::default(),
            on(&["a"]),
            {
                let mut s = on(&["a"]);
                s.allow_conflict(ItemKind::Skill, "a");
                s
            },
            {
                let mut s = Selection::default();
                s.allow_conflict(ItemKind::Skill, "a");
                s
            },
        ] {
            let plan = build_plan(&b.resolve(), &sel);
            assert!(plan.actions.is_empty(), "Unmanaged が計画に入った");
            assert!(matches!(
                plan.skipped.as_slice(),
                [Skipped {
                    reason: SkipReason::Unmanaged,
                    ..
                }]
            ));
        }
    }

    // 実体はもう無いが記録だけ残っている。OFF にしたら記録を掃除する。
    #[test]
    fn a_stale_record_is_pruned_rather_than_deleted() {
        let mut b = Bench::new("plan-prune");
        b.source_skill("a", "x");
        b.record_with_hash("a", "もう存在しない実体のハッシュ");

        let plan = build_plan(&b.resolve(), &Selection::default());

        assert!(matches!(plan.actions.as_slice(), [Action::Prune { name, .. }] if name == "a"));
    }

    // ソースが消えたものを ON のままにはできない（コピー元が無い）。
    #[test]
    fn turning_on_something_with_no_source_is_skipped() {
        let mut b = Bench::new("plan-nosource");
        b.target_skill("a", "x");
        b.record_matching("a");

        let plan = build_plan(&b.resolve(), &on(&["a"]));

        assert!(plan.actions.is_empty());
        assert!(matches!(
            plan.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::NoSource,
                ..
            }]
        ));
    }

    // 書き込み先は、ソース側のパスからでも既存の実体からでもなく、**ターゲットディレクトリと
    // 配置規約から**作る。ここがずれると、ターゲット外へ書く経路になる。
    #[test]
    fn the_copy_destination_comes_from_the_target_directory_and_the_layout() {
        let b = Bench::new("plan-dest");
        b.source_skill("a", "x");

        let plan = build_plan(&b.resolve(), &on(&["a"]));

        let Action::Copy { from, to, .. } = &plan.actions[0] else {
            panic!("Copy のはず");
        };
        assert_eq!(from, &b.source.join("skills/a"));
        assert_eq!(to, &b.target.join("skills/a"));
    }

    // agent は単一ファイルなので宛先に拡張子が付く。
    #[test]
    fn a_single_file_kind_gets_its_extension_in_the_destination() {
        let b = Bench::new("plan-dest-agent");
        b.t.write("source/agents/x.md", "エージェント");

        let mut sel = Selection::default();
        sel.turn_on(ItemKind::Agent, "x");
        let plan = build_plan(&b.resolve(), &sel);

        let Action::Copy { to, .. } = &plan.actions[0] else {
            panic!("Copy のはず");
        };
        assert_eq!(to, &b.target.join("agents/x.md"));
    }

    #[test]
    fn a_plan_that_does_nothing_is_empty() {
        let b = Bench::new("plan-empty");
        b.source_skill("a", "x");

        let plan = build_plan(&b.resolve(), &Selection::default());

        assert!(plan.is_empty());
    }
}
