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
//! | **実体があるか確認できない** | 記録があれば Conflict、無ければ Unmanaged。**どちらも
//!   許可では通さない** | 同上 |
//!
//! 最後の行は表の 5 番目の状況。「読めなかった」「リンクだった」「同じ実体を指す名前が
//! 複数ある」など、状態を確定できなかった場合を指す。**確定できないものに例外を作らない** —
//! Conflict の上書き許可が効くのは、ハッシュ不一致を確かに突き止めたときだけ。
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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    // 項目そのものがリンクなら、中を見る前に断る。**ディレクトリ種別でも同じ** —
    // `read_dir` はリンクを辿るので、ここで止めないとリンク先の中身でハッシュが決まる。
    refuse_symlink(path)?;

    let mut hasher = Sha256::new();
    if kind.is_dir_unit() {
        let mut entries = Vec::new();
        collect(path, path, &mut entries)?;
        // 相対パスのバイト列で並べ替えて、`read_dir` の順序から切り離す。
        entries.sort();
        for (rel, node, full) in entries {
            // 種別タグを混ぜる。これが無いと、空ディレクトリと空ファイルが同じ名前で
            // 同じ値になる。
            mix(&mut hasher, node.tag());
            mix(&mut hasher, &rel);
            if node == Node::File {
                let bytes = fs::read(&full).map_err(|e| SyncError::Io(full.clone(), e))?;
                mix(&mut hasher, &bytes);
            }
        }
    } else {
        let bytes = fs::read(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
        mix(&mut hasher, Node::File.tag());
        mix(&mut hasher, &bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 長さ前置で混ぜる。これが無いと「`a/b` + `c`」と「`a` + `b/c`」が同じ入力になる。
fn mix(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Dir,
    File,
}

impl Node {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Dir => b"d",
            Self::File => b"f",
        }
    }
}

/// `root` 配下をすべて集める。**空ディレクトリも含める** — 中身が空でも、その存在は
/// 実体の一部。落とすと手編集がハッシュに出ない。
///
/// 相対パスは `OsStr` のバイト列のまま持つ。`to_string_lossy` を通すと非 UTF-8 名や
/// `\` を含む名前が別物に潰れ、**内容の違う実体が同じハッシュになる**。
fn collect(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(Vec<u8>, Node, PathBuf)>,
) -> Result<(), SyncError> {
    let entries = fs::read_dir(dir).map_err(|e| SyncError::Io(dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| SyncError::Io(dir.to_path_buf(), e))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| SyncError::Io(path.clone(), e))?;

        if meta.file_type().is_symlink() {
            return Err(SyncError::SymlinkRefused(path));
        }

        let rel = path
            .strip_prefix(root)
            .map_err(|_| SyncError::OutsideTarget(path.clone()))?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();

        if meta.is_dir() {
            out.push((rel, Node::Dir, path.clone()));
            collect(root, &path, out)?;
        } else {
            out.push((rel, Node::File, path));
        }
    }
    Ok(())
}

/// 対象自体がシンボリックリンクなら拒否する（設計原則 4）。
fn refuse_symlink(path: &Path) -> Result<(), SyncError> {
    let meta = fs::symlink_metadata(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))?;
    if meta.file_type().is_symlink() {
        return Err(SyncError::SymlinkRefused(path.to_path_buf()));
    }
    Ok(())
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
    /// 一覧を作った時点でターゲット側にあったものの実際のハッシュ。
    /// **これが適用直前の再判定の基準**（ゲート 3）— 計画を作ったときに見たものが、
    /// 実行する瞬間もそこにあるか。
    pub observed_hash: Option<String>,
    /// 状態を確定できなかった理由（読めない、リンクだった等）。**入っているときは
    /// 状態を Conflict に倒してある** — 判定できないものを触ってよい側に倒さない。
    pub problem: Option<String>,
}

/// ソース・ターゲット・マニフェストを突き合わせて、各項目の状態を決める。
///
/// **ここがゲート 1・2**（記録の有無と、記録したハッシュとの一致）。判定の結果は
/// `ItemState` に落ち、`is_locked()` が「触ってよいか」の唯一の判断点になる。
///
/// 並び順は Unmanaged が先頭（企画 §7）。管理外の実体が見えていること自体が import の入口。
///
/// **失敗しない。** 判定できない項目があってもそこだけロック側（Conflict + `problem`）に
/// 倒し、一覧そのものは必ず返す。1 つ壊れているだけで全部見えなくなると、直す入口まで
/// 失われる。
pub fn resolve(
    source: &[DraftItem],
    target: &[DraftItem],
    manifest: &Manifest,
    target_dir: &Path,
) -> Vec<ListedItem> {
    let mut names: Vec<(ItemKind, String)> = Vec::new();
    let mut note = |kind: ItemKind, name: &str| {
        if !names.iter().any(|(k, n)| *k == kind && n == name) {
            names.push((kind, name.to_string()));
        }
    };

    for s in source {
        note(s.kind, &s.name);
    }
    // ソースから消えていても、インストール済みなら一覧に残す。でないと外す手段が無くなる。
    for t in target {
        note(t.kind, &t.name);
    }
    // ソースにもターゲットにも無い記録。掃除できるよう一覧に出す。
    // **マニフェストはユーザーが手で編集できる。** 名前を検証せずにパスへ組み立てると
    // そこが脱出経路になるので、走査と同じ規則で弾く（設計原則 4）。
    for e in &manifest.entries {
        if crate::scan::is_valid_name(&e.name) {
            note(e.kind, &e.name);
        }
    }

    let mut items: Vec<ListedItem> = names
        .into_iter()
        .map(|(kind, name)| {
            let install_path = kind.path_in(target_dir, &name);
            let record = manifest.find(kind, &name);
            let probe = state_for(kind, &install_path, record.map(|r| r.content_hash.as_str()));
            ListedItem {
                kind,
                name: name.clone(),
                state: probe.state,
                source_path: find(source, kind, &name).map(|s| s.path.clone()),
                // **観測できたときだけ `Some`。** 「入っているはず」という推測を混ぜない。
                target_path: probe.exists.then(|| install_path.clone()),
                install_path,
                recorded: record.is_some(),
                observed_hash: probe.observed_hash,
                problem: probe.problem,
            }
        })
        .collect();

    lock_colliding_names(&mut items);

    items.sort_by(|a, b| {
        a.state
            .sort_key()
            .cmp(&b.state.sort_key())
            .then_with(|| a.kind.list_order().cmp(&b.kind.list_order()))
            .then_with(|| a.name.cmp(&b.name))
    });
    items
}

/// `//!` の状態表そのもの。
///
/// **判定の基準は「ファイルシステム上で書き込み先が空いているか」であって、走査が項目として
/// 認識したかどうかではない。** `scan` は必須ファイルの無いディレクトリを捨てるので、
/// 走査結果だけを見ると「まだ `SKILL.md` を書いていない作業中のディレクトリ」が
/// 見えず、その上へコピーしてしまう。大文字小文字を区別しないファイルシステムで
/// `Foo` と `foo` が同じ実体を指す場合も同様で、どちらも設計原則 1 の破れになる。
/// ファイルシステムに直接訊けば、両方まとめて塞がる。
///
/// 状態を確定できなかったときは **Conflict（ロック側）に倒す**。「読めなかった」を
/// 「存在しない」と解釈すると、記録の削除や上書きへ進んでしまう。
fn state_for(kind: ItemKind, install_path: &Path, recorded_hash: Option<&str>) -> Probe {
    match fs::symlink_metadata(install_path) {
        // 書き込み先は確かに空いている。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::off(),

        // 空いているとも埋まっているとも言えない。
        // **記録が無いなら Unmanaged。** Conflict に倒すと、drybench が入れた覚えの無い
        // 実体が「上書き許可」ひとつで対象になる。設計原則 1 に override は無い。
        Err(e) => Probe::locked(
            recorded_hash.is_some(),
            format!("{} を確認できない: {e}", install_path.display()),
        ),

        Ok(_) => {
            let Some(recorded_hash) = recorded_hash else {
                // 実体はあるが記録が無い。**触らない**（設計原則 1）。
                return Probe::plain(ItemState::Unmanaged);
            };
            match hash_item(kind, install_path) {
                Ok(h) if h == recorded_hash => Probe::plain(ItemState::On).observed(h),
                // 記録した後で誰かが書き換えた。**ここだけが上書き許可の効く Conflict** —
                // 不一致を確かに突き止めた場合（設計原則 2）。
                Ok(h) => Probe::plain(ItemState::Conflict).observed(h),
                // ハッシュが取れない項目が 1 つあっても、一覧全体を出せなくしない
                // （企画 §7: 見えていること自体が入口）。その項目だけロックする。
                Err(e) => Probe::locked(true, e.to_string()).exists(),
            }
        }
    }
}

/// `state_for` の結果。
struct Probe {
    state: ItemState,
    problem: Option<String>,
    /// 実体を**観測できた**か。確認できなかった場合は `false`（憶測を混ぜない）。
    exists: bool,
    observed_hash: Option<String>,
}

impl Probe {
    fn off() -> Self {
        Self {
            state: ItemState::Off,
            problem: None,
            exists: false,
            observed_hash: None,
        }
    }

    fn plain(state: ItemState) -> Self {
        Self {
            state,
            problem: None,
            exists: true,
            observed_hash: None,
        }
    }

    fn observed(mut self, hash: String) -> Self {
        self.observed_hash = Some(hash);
        self
    }

    /// 判定できなかった項目。記録があれば Conflict、無ければ Unmanaged。
    /// **`problem` が入っている限り、どちらも上書き許可では通らない。**
    fn locked(recorded: bool, problem: String) -> Self {
        Self {
            state: if recorded {
                ItemState::Conflict
            } else {
                ItemState::Unmanaged
            },
            problem: Some(problem),
            exists: false,
            observed_hash: None,
        }
    }

    fn exists(mut self) -> Self {
        self.exists = true;
        self
    }
}

/// 複数の名前が同じ実体を指しているなら、**どれも触らない**。
///
/// 大文字小文字を区別しない、あるいは Unicode 正規化を区別しないファイルシステムでは、
/// `Foo` と `foo` が同じディレクトリになる。状態判定はファイルシステムに訊くので両方とも
/// 正しく答えるが、一覧には 2 行出る。片方に「Unmanaged・触りません」と表示しながら
/// もう片方の削除でその実体を消せば、確認画面が事実と食い違う（設計原則 6）。
/// どちらが正しい行かを機械的に決める根拠は無いので、両方ロックしてユーザーに委ねる。
fn lock_colliding_names(items: &mut [ListedItem]) {
    let mut seen: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if item.target_path.is_none() {
            continue; // 実体が無ければ衝突しようがない。
        }
        if let Ok(real) = fs::canonicalize(&item.install_path) {
            seen.entry(real).or_default().push(i);
        }
    }

    for (real, indexes) in seen {
        if indexes.len() < 2 {
            continue;
        }
        let names: Vec<&str> = indexes.iter().map(|i| items[*i].name.as_str()).collect();
        let problem = format!(
            "{} を複数の名前が指している（{}）。どれを指すか決まらないので触らない",
            real.display(),
            names.join(", ")
        );
        for i in indexes {
            items[i].state = if items[i].recorded {
                ItemState::Conflict
            } else {
                ItemState::Unmanaged
            };
            items[i].problem = Some(problem.clone());
        }
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
    /// **いま入っているものにチェックが入った状態**から始める。UI はこれを起点にする。
    ///
    /// `Default` は「全部 OFF」＝「入っているものを全部消す」という最も破壊的な指示に
    /// なるので、画面の初期値に使ってはいけない。
    pub fn from_items(items: &[ListedItem]) -> Self {
        let mut sel = Self::default();
        for item in items {
            // Conflict も「入っている」。チェックを外せば削除の指示になるが、
            // Conflict は表のとおり削除されないので、実害なく現状を映せる。
            if matches!(item.state, ItemState::On | ItemState::Conflict) {
                sel.turn_on(item.kind, &item.name);
            }
        }
        sel
    }

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
///
/// **どの手も `expect` を持つ。** これは「計画を作ったとき、対象の場所に何があったか」で、
/// 適用直前にもう一度確かめる基準になる（ゲート 3）。計画は過去の観測でしかないので、
/// これが無いと「見たときの状態」で実行してしまう。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// ソースからターゲットへ入れる（新規・更新の両方）。
    Copy {
        kind: ItemKind,
        name: String,
        from: PathBuf,
        to: PathBuf,
        expect: Expect,
    },
    /// ターゲットから外す。記録も消す。
    Delete {
        kind: ItemKind,
        name: String,
        path: PathBuf,
        expect: Expect,
    },
    /// 実体はもう無いが記録だけ残っている。記録を掃除する。
    /// 計画を作った後に実体が戻っていたら、掃除してはいけない。
    Prune {
        kind: ItemKind,
        name: String,
        path: PathBuf,
        expect: Expect,
    },
}

impl Action {
    pub fn kind(&self) -> ItemKind {
        match self {
            Self::Copy { kind, .. } | Self::Delete { kind, .. } | Self::Prune { kind, .. } => *kind,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Copy { name, .. } | Self::Delete { name, .. } | Self::Prune { name, .. } => name,
        }
    }

    /// 実行の対象になるターゲット側のパス。
    pub fn path(&self) -> &Path {
        match self {
            Self::Copy { to, .. } => to,
            Self::Delete { path, .. } | Self::Prune { path, .. } => path,
        }
    }

    fn expect(&self) -> &Expect {
        match self {
            Self::Copy { expect, .. }
            | Self::Delete { expect, .. }
            | Self::Prune { expect, .. } => expect,
        }
    }
}

/// 適用直前に、対象の場所に何があるべきか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// 何も無いはず（新規コピー、記録だけの掃除）。
    Nothing,
    /// この内容のものがあるはず（更新、削除、許可済みの上書き）。
    Content(String),
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
    /// 計画を作ってから適用までの間に、対象が変わった（ゲート 3）。
    Changed,
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

        // **ロックされた状態はここで止める。** 状態ごとの分岐より先に `is_locked()` を
        // 通すことで、状態が増えたときに「触ってよい側」へ落ちないようにする。
        if item.state.is_locked() {
            match item.state {
                // 設計原則 1 に override は無い。どちらの向きでも、どんな許可があっても。
                ItemState::Unmanaged => plan.skipped.push(skip(SkipReason::Unmanaged)),
                // 唯一の例外。上書きだけが、項目ごとの明示的な許可で通る。
                // 削除は表のとおり常に行わない（許可があっても）。
                //
                // **許可が効くのは、記録があり、かつ状態を確定できたときだけ。**
                // `problem` が入っているものは「何なのか分からなかった」ので、許可の
                // 対象にしない。分からないものへの例外は、例外ではなく穴になる。
                ItemState::Conflict
                    if wants_on
                        && item.recorded
                        && item.problem.is_none()
                        && selection.conflict_allowed(item.kind, &item.name) =>
                {
                    match &item.source_path {
                        Some(from) => plan.actions.push(copy(item, from)),
                        None => plan.skipped.push(skip(SkipReason::NoSource)),
                    }
                }
                _ => plan.skipped.push(skip(SkipReason::Conflict)),
            }
            continue;
        }

        match item.state {
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
                    expect: expect_of(item),
                });
            }

            ItemState::Off if wants_on => match &item.source_path {
                Some(from) => plan.actions.push(copy(item, from)),
                None => plan.skipped.push(skip(SkipReason::NoSource)),
            },
            // 入っていないものを OFF のままにするのは何もしないこと。ただし記録だけが
            // 残っているなら掃除する。**`Off` は「確かに空いている」と確認できた場合しか
            // 付かない**ので、読めなかっただけで記録を消すことはない。
            ItemState::Off => {
                if item.recorded {
                    plan.actions.push(Action::Prune {
                        kind: item.kind,
                        name: item.name.clone(),
                        path: item.install_path.clone(),
                        expect: expect_of(item),
                    });
                }
            }

            // ロック済みは上で処理済み。
            ItemState::Conflict | ItemState::Unmanaged => unreachable!("is_locked で処理済み"),
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
        expect: expect_of(item),
    }
}

/// 計画を作った時点で、その場所に何が見えていたか。
fn expect_of(item: &ListedItem) -> Expect {
    match &item.observed_hash {
        Some(h) => Expect::Content(h.clone()),
        None => Expect::Nothing,
    }
}

// ----------------------------------------------------------------- 適用

/// 適用の結果。確認画面と同じ粒度で、何が起きて何が起きなかったかを返す。
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub done: Vec<Action>,
    /// ゲートで止まったもの。**失敗ではない** — 触らないのが正しい判断だった場合。
    pub skipped: Vec<Skipped>,
    pub failed: Vec<Failure>,
}

#[derive(Debug)]
pub struct Failure {
    pub kind: ItemKind,
    pub name: String,
    pub error: SyncError,
}

/// 計画を実行する。**ここがゲート 3 と 4。**
///
/// 1 手ごとに、
///
/// 1. 対象の場所がいま何であるかを見直し、計画時の観測（`expect`）と食い違えば飛ばす。
///    計画は過去の観測でしかないので、その間に変わっていたら実行してはいけない。
/// 2. 書き込み先の canonical path がターゲット配下にあることを確かめ、対象自体が
///    シンボリックリンクなら追わずに拒否する。
///
/// **成功するたびにマニフェストを保存する。** 実体を置いたのに記録が残らないと、その実体は
/// 次から Unmanaged になり、drybench からは二度と外せなくなる。途中で止まっても、
/// そこまでの分は記録と対応が取れている状態にする。
///
/// 1 手が失敗しても残りは続ける。1 つの壊れた項目のために他が入らないのは、
/// ユーザーにとって復旧しにくいだけ。
pub fn apply_plan(plan: &Plan, target_dir: &Path, manifest: &mut Manifest) -> ApplyReport {
    let mut report = ApplyReport::default();

    for action in &plan.actions {
        match apply_one(action, target_dir, manifest) {
            Ok(Outcome::Done) => {
                // 実体より記録が遅れないよう、1 手ごとに保存する。
                if let Err(e) = crate::manifest::save(target_dir, manifest) {
                    report.failed.push(Failure {
                        kind: action.kind(),
                        name: action.name().to_string(),
                        error: SyncError::Io(
                            target_dir.to_path_buf(),
                            std::io::Error::other(e.to_string()),
                        ),
                    });
                    continue;
                }
                report.done.push(action.clone());
            }
            Ok(Outcome::Skipped(reason)) => report.skipped.push(Skipped {
                kind: action.kind(),
                name: action.name().to_string(),
                reason,
            }),
            Err(error) => report.failed.push(Failure {
                kind: action.kind(),
                name: action.name().to_string(),
                error,
            }),
        }
    }

    report
}

enum Outcome {
    Done,
    Skipped(SkipReason),
}

fn apply_one(
    action: &Action,
    target_dir: &Path,
    manifest: &mut Manifest,
) -> Result<Outcome, SyncError> {
    // --- ゲート 3: 実行直前の再判定 ---
    if !still_as_expected(action)? {
        return Ok(Outcome::Skipped(SkipReason::Changed));
    }

    // --- ゲート 4: 書き込み先の検証 ---
    match action {
        Action::Copy { to, .. } => assert_within(target_dir, to)?,
        Action::Delete { path, .. } => assert_within(target_dir, path)?,
        // Prune はファイルに触れないので、検証する書き込み先が無い。
        Action::Prune { .. } => {}
    }

    match action {
        Action::Copy {
            kind,
            name,
            from,
            to,
            ..
        } => {
            copy_item(*kind, from, to)?;
            let hash = hash_item(*kind, to)?;
            manifest.upsert(crate::manifest::Entry {
                kind: *kind,
                name: name.clone(),
                source_dir: from
                    .parent()
                    .and_then(|p| p.parent())
                    .unwrap_or(from)
                    .to_path_buf(),
                synced_at: chrono::Utc::now().to_rfc3339(),
                content_hash: hash,
            });
        }
        Action::Delete {
            kind, name, path, ..
        } => {
            remove_item(path)?;
            manifest.remove(*kind, name);
        }
        Action::Prune { kind, name, .. } => {
            manifest.remove(*kind, name);
        }
    }

    Ok(Outcome::Done)
}

/// 計画時に見たものが、いまもそこにあるか（ゲート 3）。
fn still_as_expected(action: &Action) -> Result<bool, SyncError> {
    let path = action.path();
    let exists = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        // 見えないなら「変わっていない」とは言えない。
        Err(e) => return Err(SyncError::Io(path.to_path_buf(), e)),
    };

    match (action.expect(), exists) {
        (Expect::Nothing, false) => Ok(true),
        (Expect::Nothing, true) => Ok(false),
        (Expect::Content(_), false) => Ok(false),
        (Expect::Content(expected), true) => Ok(&hash_item(action.kind(), path)? == expected),
    }
}

/// 書き込み先がターゲット配下にあることを canonical path で確かめる（設計原則 4）。
///
/// パスそのものはまだ存在しなくてよいので、**親を解決してから** 前方一致を見る。
/// 途中にターゲット外を指すリンクがあれば、解決結果が配下から外れるので落ちる。
fn assert_within(target_dir: &Path, path: &Path) -> Result<(), SyncError> {
    let root =
        fs::canonicalize(target_dir).map_err(|e| SyncError::Io(target_dir.to_path_buf(), e))?;

    // 対象自体がリンクなら追わない。
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(SyncError::SymlinkRefused(path.to_path_buf()));
        }
    }

    let parent = path
        .parent()
        .ok_or_else(|| SyncError::OutsideTarget(path.to_path_buf()))?;
    // 親が無ければ作る。作る先も同じ検証を通す必要があるので、再帰的に確かめる。
    if !parent.exists() {
        assert_within(target_dir, parent)?;
        fs::create_dir_all(parent).map_err(|e| SyncError::Io(parent.to_path_buf(), e))?;
    }

    let real_parent =
        fs::canonicalize(parent).map_err(|e| SyncError::Io(parent.to_path_buf(), e))?;
    if !real_parent.starts_with(&root) {
        return Err(SyncError::OutsideTarget(path.to_path_buf()));
    }
    Ok(())
}

/// ソースからターゲットへ入れる。
///
/// **いったん脇に組み立ててから差し替える。** 途中で失敗しても、書きかけの実体を
/// ターゲットに残さない。ソースにリンクが混ざっていれば、1 バイトも書かずに止まる。
fn copy_item(kind: ItemKind, from: &Path, to: &Path) -> Result<(), SyncError> {
    refuse_symlink(from)?;

    let staging = staging_path(to);
    let _ = remove_any(&staging);

    let result = if kind.is_dir_unit() {
        copy_dir(from, &staging)
    } else {
        fs::copy(from, &staging)
            .map(|_| ())
            .map_err(|e| SyncError::Io(staging.clone(), e))
    };
    if let Err(e) = result {
        let _ = remove_any(&staging);
        return Err(e);
    }

    // 差し替え。既にあるものは、ゲート 3 で「計画時のまま」と確認済み。
    if let Err(e) = remove_any(to) {
        let _ = remove_any(&staging);
        return Err(e);
    }
    if let Err(e) = fs::rename(&staging, to) {
        let _ = remove_any(&staging);
        return Err(SyncError::Io(to.to_path_buf(), e));
    }
    Ok(())
}

/// 組み立て用の一時パス。同じ親の中に作るので `rename` が同一ファイルシステム内で済む。
fn staging_path(to: &Path) -> PathBuf {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unique = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    to.with_file_name(format!(
        ".drybench-staging-{name}.{}.{unique}",
        std::process::id()
    ))
}

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn copy_dir(from: &Path, to: &Path) -> Result<(), SyncError> {
    fs::create_dir_all(to).map_err(|e| SyncError::Io(to.to_path_buf(), e))?;
    let entries = fs::read_dir(from).map_err(|e| SyncError::Io(from.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| SyncError::Io(from.to_path_buf(), e))?;
        let src = entry.path();
        let meta = fs::symlink_metadata(&src).map_err(|e| SyncError::Io(src.clone(), e))?;

        // ソース側のリンクも追わない。追うとターゲット外の内容を持ち込むことになる。
        if meta.file_type().is_symlink() {
            return Err(SyncError::SymlinkRefused(src));
        }

        let dst = to.join(entry.file_name());
        if meta.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| SyncError::Io(dst.clone(), e))?;
        }
    }
    Ok(())
}

fn remove_item(path: &Path) -> Result<(), SyncError> {
    // 削除の直前にもリンクでないことを確かめる。`remove_dir_all` はリンクを追わないが、
    // ここで断っておけば「何を消したか」が曖昧にならない。
    refuse_symlink(path)?;
    remove_any(path)
}

fn remove_any(path: &Path) -> Result<(), SyncError> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SyncError::Io(path.to_path_buf(), e)),
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).map_err(|e| SyncError::Io(path.to_path_buf(), e))
        }
        Ok(_) => fs::remove_file(path).map_err(|e| SyncError::Io(path.to_path_buf(), e)),
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

        /// 種別を問わず、配置規約どおりに実体を置く。
        fn put(&self, root: &str, kind: ItemKind, name: &str, body: &str) -> PathBuf {
            let dir = kind.dir_name();
            match kind.required_file() {
                Some(required) => {
                    self.t
                        .write(format!("{root}/{dir}/{name}/{required}"), body);
                    self.t.join(format!("{root}/{dir}/{name}"))
                }
                None => self.t.write(format!("{root}/{dir}/{name}.md"), body),
            }
        }

        /// ターゲット側の実体の現在のハッシュで記録を作る（種別を問わない）。
        fn record_for(&mut self, kind: ItemKind, name: &str) {
            let path = kind.path_in(&self.target, name);
            let hash = hash_item(kind, &path).unwrap();
            self.manifest.upsert(Entry {
                kind,
                name: name.to_string(),
                source_dir: self.source.clone(),
                synced_at: "2026-08-06T00:00:00Z".to_string(),
                content_hash: hash,
            });
        }

        fn resolve(&self) -> Vec<ListedItem> {
            let source = scan::scan(&self.source);
            let target = scan::scan(&self.target);
            resolve(&source, &target, &self.manifest, &self.target)
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

    // --- 書き込み先が空いているかどうか ---

    // `scan` は必須ファイルの無いディレクトリを項目として認識しない。認識しないことと
    // 「そこが空いている」ことは別物で、混同すると `SKILL.md` を書く前の作業中ディレクトリを
    // 上書きする（設計原則 1）。
    #[test]
    fn a_directory_the_scan_ignores_still_occupies_the_install_path() {
        let b = Bench::new("occupied-dir");
        b.source_skill("wip", "ソース側");
        // ターゲット側は書きかけ。SKILL.md がまだ無いので走査は拾わない。
        b.t.write("target/skills/wip/draft.md", "書きかけの下書き");

        let items = b.resolve();
        assert_eq!(
            items[0].state,
            ItemState::Unmanaged,
            "空きとみなしてはいけない"
        );

        let plan = build_plan(&items, &on(&["wip"]));
        assert!(plan.actions.is_empty(), "書きかけの上にコピーしようとした");
        assert!(matches!(
            plan.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::Unmanaged,
                ..
            }]
        ));
        // 実体が残っていること。
        assert_eq!(
            fs::read_to_string(b.target.join("skills/wip/draft.md")).unwrap(),
            "書きかけの下書き"
        );
    }

    // ディレクトリのはずの場所がただのファイルでも同じ。
    #[test]
    fn a_plain_file_where_a_directory_belongs_also_occupies_it() {
        let b = Bench::new("occupied-file");
        b.source_skill("x", "ソース側");
        b.t.write("target/skills/x", "ユーザーのファイル");

        assert_eq!(b.state_of("x"), ItemState::Unmanaged);
        assert!(build_plan(&b.resolve(), &on(&["x"])).actions.is_empty());
    }

    // macOS の既定 (APFS) は大文字小文字を区別しない。`Foo` と `foo` は同じ実体を指すので、
    // 名前の文字列比較だけで別項目とみなすと、Unmanaged と判定した当の実体に
    // 別項目として着弾する（設計原則 1）。判定をファイルシステムに訊けば塞がる。
    #[test]
    fn a_case_only_difference_does_not_open_a_second_slot() {
        let b = Bench::new("case-collide");
        b.t.mkdir("probe/AAA");
        if !b.t.join("probe/aaa").exists() {
            return; // 大文字小文字を区別するファイルシステム。この危険は無い。
        }

        b.target_skill("foo", "ユーザーの実体");
        b.source_skill("Foo", "ソース側");

        let plan = build_plan(&b.resolve(), &on(&["Foo"]));

        assert!(
            plan.actions.is_empty(),
            "Unmanaged な実体に別名でコピーしようとした"
        );
        assert_eq!(
            fs::read_to_string(b.target.join("skills/foo/SKILL.md")).unwrap(),
            "ユーザーの実体"
        );
    }

    // --- ハッシュが同値性を保証すること ---

    // 相対パスを文字列に落とすと、`\` を含む名前が区切りに化けて別物と混ざる。
    #[cfg(unix)]
    #[test]
    fn a_backslash_in_a_file_name_does_not_collide_with_a_nested_path() {
        let b = Bench::new("hash-backslash");
        b.t.write("source/skills/x/SKILL.md", "s");
        b.t.write("source/skills/x/a/b", "入れ子");
        b.t.write("source/skills/x/a\\b", "中身その一");

        b.t.write("source/skills/y/SKILL.md", "s");
        b.t.write("source/skills/y/a/b", "入れ子");
        b.t.write("source/skills/y/a\\b", "中身その二");

        assert_ne!(
            hash_item(ItemKind::Skill, &b.source.join("skills/x")).unwrap(),
            hash_item(ItemKind::Skill, &b.source.join("skills/y")).unwrap(),
            "中身が違うのに同じハッシュになった"
        );
    }

    // 空のディレクトリも実体の一部。落とすと手編集がハッシュに出ない（設計原則 2）。
    #[test]
    fn an_empty_directory_is_part_of_the_hash() {
        let b = Bench::new("hash-emptydir");
        let dir = b.source_skill("x", "s");
        let before = hash_item(ItemKind::Skill, &dir).unwrap();

        b.t.mkdir("source/skills/x/templates");

        assert_ne!(before, hash_item(ItemKind::Skill, &dir).unwrap());
    }

    // 設計原則 4。ディレクトリ種別でも、項目そのものがリンクなら追わない。
    #[cfg(unix)]
    #[test]
    fn an_item_whose_root_is_a_symlink_is_refused() {
        let b = Bench::new("hash-rootlink");
        let real = b.source_skill("real", "中身");
        let link = b.target.join("skills/linked");
        fs::create_dir_all(b.target.join("skills")).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(matches!(
            hash_item(ItemKind::Skill, &link),
            Err(SyncError::SymlinkRefused(..))
        ));
    }

    // --- 判定できないときの倒し方 ---

    // 項目が 1 つ壊れていても、他が一覧に出ないと import の入口が塞がる（企画 §7）。
    #[cfg(unix)]
    #[test]
    fn one_unhashable_item_does_not_hide_the_whole_list() {
        let mut b = Bench::new("resolve-degrade");
        b.source_skill("good", "x");
        b.target_skill("good", "x");
        b.record_matching("good");

        b.target_skill("bad", "x");
        b.record_matching("bad");
        let outside = b.t.write("outside.txt", "外");
        std::os::unix::fs::symlink(&outside, b.target.join("skills/bad/link.md")).unwrap();

        let items = b.resolve();

        let good = items.iter().find(|i| i.name == "good").unwrap();
        assert_eq!(good.state, ItemState::On, "正常な項目まで見えなくなった");

        let bad = items.iter().find(|i| i.name == "bad").unwrap();
        assert_eq!(
            bad.state,
            ItemState::Conflict,
            "判定できないものはロック側へ"
        );
        assert!(bad.problem.is_some());
    }

    // 「読めなかった」を「無い」と解釈すると、実体が無事なのに記録だけ消える。
    // 記録の消えた実体は以後 Unmanaged となり、二度と外せなくなる。
    #[cfg(unix)]
    #[test]
    fn an_unreadable_target_does_not_prune_the_records() {
        use std::os::unix::fs::PermissionsExt;

        let mut b = Bench::new("resolve-unreadable");
        b.source_skill("a", "x");
        b.target_skill("a", "x");
        b.record_matching("a");

        let dir = b.target.join("skills");
        let original = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();

        let plan = build_plan(&b.resolve(), &Selection::default());

        fs::set_permissions(&dir, original).unwrap();

        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Prune { .. })),
            "読めなかっただけで記録を消した"
        );
    }

    // --- 一覧の並び ---

    #[test]
    fn the_list_order_is_unmanaged_then_conflict_then_on_then_off() {
        let mut b = Bench::new("order-full");
        b.source_skill("d-off", "x");

        b.source_skill("c-on", "x");
        b.target_skill("c-on", "x");
        b.record_matching("c-on");

        b.source_skill("b-conflict", "x");
        b.target_skill("b-conflict", "x");
        b.record_matching("b-conflict");
        b.target_skill("b-conflict", "手で書き換えた");

        b.target_skill("a-unmanaged", "x");

        let states: Vec<ItemState> = b.resolve().iter().map(|i| i.state).collect();
        assert_eq!(
            states,
            vec![
                ItemState::Unmanaged,
                ItemState::Conflict,
                ItemState::On,
                ItemState::Off
            ]
        );
    }

    #[test]
    fn kinds_are_listed_in_the_declared_order() {
        let b = Bench::new("order-kind");
        for kind in ItemKind::ALL {
            b.put("source", kind, "same-name", "x");
        }

        let kinds: Vec<ItemKind> = b.resolve().iter().map(|i| i.kind).collect();
        assert_eq!(kinds, ItemKind::ALL.to_vec());
    }

    // --- 状態表を全種別で回す ---

    // 表は種別に依らず成立しなければならない。Skill だけで確かめても足りない。
    #[test]
    fn the_state_table_holds_for_every_kind() {
        for kind in ItemKind::ALL {
            let tag = format!("table-{}", kind.dir_name());

            // ターゲットに無い + ON → コピー
            let b = Bench::new(&tag);
            b.put("source", kind, "x", "x");
            let mut sel = Selection::default();
            sel.turn_on(kind, "x");
            let plan = build_plan(&b.resolve(), &sel);
            assert!(
                matches!(plan.actions.as_slice(), [Action::Copy { to, .. }] if to == &kind.path_in(&b.target, "x")),
                "{kind:?}: Off + ON がコピーにならない"
            );

            // ターゲットに無い + OFF → 何もしない
            let plan = build_plan(&b.resolve(), &Selection::default());
            assert!(plan.is_empty(), "{kind:?}: Off + OFF で何かした");

            // 記録あり・一致 + OFF → 削除、+ ON → 再コピー
            let mut b = Bench::new(&tag);
            b.put("source", kind, "x", "x");
            b.put("target", kind, "x", "x");
            b.record_for(kind, "x");
            assert_eq!(b.resolve()[0].state, ItemState::On, "{kind:?}");
            let plan = build_plan(&b.resolve(), &Selection::default());
            assert!(
                matches!(plan.actions.as_slice(), [Action::Delete { .. }]),
                "{kind:?}: On + OFF が削除にならない"
            );
            let mut sel = Selection::default();
            sel.turn_on(kind, "x");
            assert!(
                matches!(
                    build_plan(&b.resolve(), &sel).actions.as_slice(),
                    [Action::Copy { .. }]
                ),
                "{kind:?}: On + ON が再コピーにならない"
            );

            // 記録あり・不一致 → 許可が無ければどちらの向きでも触らない
            let mut b = Bench::new(&tag);
            b.put("source", kind, "x", "x");
            b.put("target", kind, "x", "x");
            b.record_for(kind, "x");
            b.put("target", kind, "x", "手で書き換えた");
            assert_eq!(b.resolve()[0].state, ItemState::Conflict, "{kind:?}");
            let mut sel = Selection::default();
            sel.turn_on(kind, "x");
            assert!(
                build_plan(&b.resolve(), &sel).actions.is_empty(),
                "{kind:?}: Conflict を許可なく上書きした"
            );
            // 許可があっても削除はしない。
            let mut sel = Selection::default();
            sel.allow_conflict(kind, "x");
            assert!(
                build_plan(&b.resolve(), &sel).actions.is_empty(),
                "{kind:?}: Conflict を削除した"
            );
            // ON かつ許可があるときだけ上書きが通る。
            let mut sel = Selection::default();
            sel.turn_on(kind, "x");
            sel.allow_conflict(kind, "x");
            assert!(
                matches!(
                    build_plan(&b.resolve(), &sel).actions.as_slice(),
                    [Action::Copy { .. }]
                ),
                "{kind:?}: 許可があるのに上書きされない"
            );

            // 記録なし・実体あり → どちらの向きでも、どんな許可があっても触らない
            let b = Bench::new(&tag);
            b.put("target", kind, "x", "x");
            assert_eq!(b.resolve()[0].state, ItemState::Unmanaged, "{kind:?}");
            for sel in selections(kind, "x") {
                assert!(
                    build_plan(&b.resolve(), &sel).actions.is_empty(),
                    "{kind:?}: Unmanaged が計画に入った"
                );
            }
        }
    }

    /// ON / 許可の 4 通りすべて。
    fn selections(kind: ItemKind, name: &str) -> Vec<Selection> {
        let mut all = Vec::new();
        for turn_on in [false, true] {
            for allow in [false, true] {
                let mut s = Selection::default();
                if turn_on {
                    s.turn_on(kind, name);
                }
                if allow {
                    s.allow_conflict(kind, name);
                }
                all.push(s);
            }
        }
        all
    }

    // --- 初期選択 ---

    // `Default` は「入っているものを全部消す」という最も破壊的な指示になる。
    // 画面の初期値はこちらから作る。
    #[test]
    fn the_initial_selection_reflects_what_is_installed() {
        let mut b = Bench::new("selection-init");
        b.source_skill("installed", "x");
        b.target_skill("installed", "x");
        b.record_matching("installed");
        b.source_skill("not-installed", "x");
        b.target_skill("stranger", "x");

        let items = b.resolve();
        let sel = Selection::from_items(&items);

        assert!(sel.is_on(ItemKind::Skill, "installed"));
        assert!(!sel.is_on(ItemKind::Skill, "not-installed"));

        // 初期選択のまま保存しても、入っているものが消えない。
        // （表のとおり ON + 一致は再コピーになるので、計画が空にはならない。）
        let plan = build_plan(&items, &sel);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, Action::Delete { .. } | Action::Prune { .. })),
            "初期選択が削除を含んでいる: {plan:?}"
        );

        // 対して Default は、入っているものを消す指示になる。
        let destructive = build_plan(&items, &Selection::default());
        assert!(destructive
            .actions
            .iter()
            .any(|a| matches!(a, Action::Delete { .. })));
    }

    // --- 記録だけが残っている場合 ---

    #[test]
    fn a_record_with_nothing_left_anywhere_is_still_listed_and_prunable() {
        let mut b = Bench::new("prune-orphan");
        b.record_with_hash("gone", "もう存在しない実体のハッシュ");

        let items = b.resolve();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, ItemState::Off);

        let plan = build_plan(&items, &Selection::default());
        assert!(
            matches!(plan.actions.as_slice(), [Action::Prune { path, .. }] if path == &b.target.join("skills/gone"))
        );
    }

    // --- 確定できなかったものに例外を作らない ---

    // 「確認できない」を Conflict に倒すと、drybench が入れた覚えの無い実体が
    // 上書き許可ひとつで対象になる。設計原則 1 に override は無い。
    #[cfg(unix)]
    #[test]
    fn an_entity_we_cannot_inspect_is_unmanaged_when_there_is_no_record() {
        use std::os::unix::fs::PermissionsExt;

        let b = Bench::new("undetermined-norecord");
        b.source_skill("a", "ソース側");
        b.target_skill("a", "ユーザーの実体");

        let dir = b.target.join("skills");
        let original = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();

        let items = b.resolve();
        let plans: Vec<Plan> = selections(ItemKind::Skill, "a")
            .iter()
            .map(|sel| build_plan(&items, sel))
            .collect();

        fs::set_permissions(&dir, original).unwrap();

        assert_eq!(items[0].state, ItemState::Unmanaged);
        assert!(items[0].problem.is_some());
        for plan in plans {
            assert!(
                plan.actions.is_empty(),
                "確認できない実体が計画に入った: {plan:?}"
            );
        }
    }

    // 記録があっても、状態を確定できていないなら許可は効かない。
    #[cfg(unix)]
    #[test]
    fn permission_does_not_apply_to_a_conflict_we_could_not_determine() {
        use std::os::unix::fs::PermissionsExt;

        let mut b = Bench::new("undetermined-recorded");
        b.source_skill("a", "ソース側");
        b.target_skill("a", "x");
        b.record_matching("a");

        let dir = b.target.join("skills");
        let original = fs::metadata(&dir).unwrap().permissions();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();

        let items = b.resolve();
        let mut sel = on(&["a"]);
        sel.allow_conflict(ItemKind::Skill, "a");
        let plan = build_plan(&items, &sel);

        fs::set_permissions(&dir, original).unwrap();

        assert_eq!(items[0].state, ItemState::Conflict);
        assert!(items[0].problem.is_some());
        assert!(plan.actions.is_empty(), "確定できていないのに上書きした");
    }

    // `hash_item` がいったん拒否したリンクを、許可で書き込み先に昇格させない。
    // ゲート 4 だけが防壁になる状態を作らない（設計原則 4）。
    #[cfg(unix)]
    #[test]
    fn permission_does_not_promote_a_refused_symlink_to_a_destination() {
        let mut b = Bench::new("undetermined-symlink");
        b.source_skill("linked", "ソース側");
        let outside = b.t.mkdir("outside");
        fs::write(outside.join("SKILL.md"), "ターゲットの外").unwrap();
        fs::create_dir_all(b.target.join("skills")).unwrap();
        std::os::unix::fs::symlink(&outside, b.target.join("skills/linked")).unwrap();
        b.record_with_hash("linked", "何であれ一致しない");

        let items = b.resolve();
        let mut sel = on(&["linked"]);
        sel.allow_conflict(ItemKind::Skill, "linked");

        assert!(items[0].problem.is_some());
        assert!(
            build_plan(&items, &sel).actions.is_empty(),
            "拒否したリンクが書き込み先になった"
        );
    }

    // 設計原則 1 の文言そのもの: 記録の無い実体は、どんな選択でも計画に入らない。
    #[test]
    fn nothing_without_a_record_ever_enters_the_plan() {
        let mut b = Bench::new("norecord-property");
        b.target_skill("stranger", "x");
        b.source_skill("fresh", "x");
        b.source_skill("known", "x");
        b.target_skill("known", "x");
        b.record_matching("known");

        let items = b.resolve();
        for kind in ItemKind::ALL {
            for name in ["stranger", "fresh", "known"] {
                for sel in selections(kind, name) {
                    let plan = build_plan(&items, &sel);
                    for action in &plan.actions {
                        let acted = match action {
                            Action::Copy { name, .. }
                            | Action::Delete { name, .. }
                            | Action::Prune { name, .. } => name,
                        };
                        let item = items.iter().find(|i| &i.name == acted).unwrap();
                        // 記録の無いものに対する唯一の正当な action は、新規コピー
                        // （書き込み先が空いていることを確認済み）。
                        assert!(
                            item.recorded || matches!(action, Action::Copy { .. }),
                            "記録の無い実体を触ろうとした: {action:?}"
                        );
                        if !item.recorded {
                            assert_eq!(item.state, ItemState::Off, "空き以外へ新規コピーした");
                        }
                    }
                }
            }
        }
    }

    // 同じ実体を 2 行として見せ、片方に「触りません」と表示しながらもう片方で消すと、
    // 確認画面が事実と食い違う（設計原則 6）。どちらの行もロックする。
    #[test]
    fn two_names_for_one_entity_lock_each_other() {
        let mut b = Bench::new("collision-lock");
        b.t.mkdir("probe/AAA");
        if !b.t.join("probe/aaa").exists() {
            return; // 大文字小文字を区別するファイルシステム。
        }

        b.target_skill("foo", "ユーザーの実体");
        b.record_matching("Foo");
        b.source_skill("Foo", "ソース側");

        let items = b.resolve();
        assert_eq!(items.len(), 2);
        assert!(
            items.iter().all(|i| i.problem.is_some()),
            "衝突が知らされていない"
        );

        for sel in selections(ItemKind::Skill, "Foo") {
            assert!(build_plan(&items, &sel).actions.is_empty());
        }
    }

    // `target_path` は観測の記録であって主張ではない。
    #[test]
    fn the_target_path_is_none_when_nothing_was_observed() {
        let mut b = Bench::new("targetpath-observed");
        b.source_skill("a", "x");
        b.record_with_hash("a", "もう存在しない実体のハッシュ");

        assert!(b.resolve()[0].target_path.is_none());
    }

    // マニフェストは手で編集できる。名前を検証せずにパスへ組み立てれば脱出経路になる。
    #[test]
    fn a_hand_written_record_with_a_traversing_name_is_ignored() {
        let mut b = Bench::new("prune-traversal");
        b.manifest.upsert(Entry {
            kind: ItemKind::Skill,
            name: "../../escape".to_string(),
            source_dir: b.source.clone(),
            synced_at: "t".to_string(),
            content_hash: "h".to_string(),
        });

        assert!(b.resolve().is_empty());
    }

    // ================================================================ 適用

    impl Bench {
        /// 一覧 → 計画 → 適用まで通す。
        fn apply(&mut self, sel: &Selection) -> ApplyReport {
            let plan = build_plan(&self.resolve(), sel);
            apply_plan(&plan, &self.target, &mut self.manifest)
        }

        fn target_body(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.target.join(format!("skills/{name}/SKILL.md"))).ok()
        }
    }

    // --- 期待どおりに動く場合 ---

    #[test]
    fn a_new_item_is_copied_in_and_recorded() {
        let mut b = Bench::new("apply-new");
        b.source_skill("a", "中身");

        let report = b.apply(&on(&["a"]));

        assert_eq!(report.done.len(), 1);
        assert!(report.skipped.is_empty() && report.failed.is_empty());
        assert_eq!(b.target_body("a").as_deref(), Some("中身"));
        // 記録が入り、次の一覧では On になる。
        assert_eq!(b.state_of("a"), ItemState::On);
    }

    #[test]
    fn nested_content_is_copied_whole() {
        let mut b = Bench::new("apply-nested");
        b.source_skill("a", "s");
        b.t.write("source/skills/a/deep/inner.md", "奥");
        b.t.mkdir("source/skills/a/empty");

        b.apply(&on(&["a"]));

        assert_eq!(
            fs::read_to_string(b.target.join("skills/a/deep/inner.md")).unwrap(),
            "奥"
        );
        assert!(b.target.join("skills/a/empty").is_dir());
        assert_eq!(b.state_of("a"), ItemState::On);
    }

    #[test]
    fn a_single_file_kind_is_copied_as_a_file() {
        let mut b = Bench::new("apply-agent");
        b.t.write("source/agents/x.md", "エージェント");

        let mut sel = Selection::default();
        sel.turn_on(ItemKind::Agent, "x");
        b.apply(&sel);

        assert_eq!(
            fs::read_to_string(b.target.join("agents/x.md")).unwrap(),
            "エージェント"
        );
    }

    #[test]
    fn re_copying_replaces_what_was_there_and_updates_the_record() {
        let mut b = Bench::new("apply-update");
        b.source_skill("a", "はじめの中身");
        b.apply(&on(&["a"]));

        b.source_skill("a", "書き直した中身");
        b.t.write("source/skills/a/added.md", "足したファイル");
        b.apply(&on(&["a"]));

        assert_eq!(b.target_body("a").as_deref(), Some("書き直した中身"));
        assert!(b.target.join("skills/a/added.md").is_file());
        assert_eq!(b.state_of("a"), ItemState::On, "記録が更新されていない");
    }

    // 更新では、ソースから消えたファイルがターゲットに残ってはいけない。
    #[test]
    fn re_copying_does_not_leave_files_the_source_no_longer_has() {
        let mut b = Bench::new("apply-stale-file");
        b.source_skill("a", "s");
        b.t.write("source/skills/a/old.md", "消える予定");
        b.apply(&on(&["a"]));

        fs::remove_file(b.source.join("skills/a/old.md")).unwrap();
        b.apply(&on(&["a"]));

        assert!(!b.target.join("skills/a/old.md").exists());
        assert_eq!(b.state_of("a"), ItemState::On);
    }

    #[test]
    fn turning_something_off_removes_it_and_forgets_the_record() {
        let mut b = Bench::new("apply-delete");
        b.source_skill("a", "中身");
        b.apply(&on(&["a"]));

        let report = b.apply(&Selection::default());

        assert_eq!(report.done.len(), 1);
        assert!(!b.target.join("skills/a").exists());
        assert!(b.manifest.find(ItemKind::Skill, "a").is_none());
        assert_eq!(b.state_of("a"), ItemState::Off);
    }

    #[test]
    fn pruning_clears_the_record_without_touching_anything_else() {
        let mut b = Bench::new("apply-prune");
        b.source_skill("a", "x");
        b.record_with_hash("a", "もう存在しない実体のハッシュ");
        b.target_skill("neighbour", "隣の実体");

        b.apply(&Selection::default());

        assert!(b.manifest.find(ItemKind::Skill, "a").is_none());
        assert_eq!(
            fs::read_to_string(b.target.join("skills/neighbour/SKILL.md")).unwrap(),
            "隣の実体"
        );
    }

    // 記録はディスクに残らなければ意味が無い。実体だけが残ると Unmanaged になり、
    // 二度と外せなくなる。
    #[test]
    fn the_manifest_is_persisted_as_each_action_succeeds() {
        let mut b = Bench::new("apply-persist");
        b.source_skill("a", "中身");

        b.apply(&on(&["a"]));

        let on_disk = crate::manifest::load(&b.target).unwrap();
        let entry = on_disk
            .find(ItemKind::Skill, "a")
            .expect("記録が保存されていない");
        assert_eq!(
            entry.content_hash,
            hash_item(ItemKind::Skill, &b.target.join("skills/a")).unwrap()
        );
    }

    // --- ゲート 3: 実行直前の再判定（TOCTOU） ---

    // 計画を作った後、適用までの間に書き込み先が埋まった。計画時点の状態で実行しない。
    #[test]
    fn a_destination_occupied_after_planning_is_not_overwritten() {
        let mut b = Bench::new("gate3-appeared");
        b.source_skill("a", "ソース側");
        let plan = build_plan(&b.resolve(), &on(&["a"]));

        // ここで誰かが置いた。
        b.t.write("target/skills/a/SKILL.md", "後から現れた実体");

        let report = apply_plan(&plan, &b.target, &mut b.manifest);

        assert!(report.done.is_empty());
        assert!(matches!(
            report.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::Changed,
                ..
            }]
        ));
        assert_eq!(b.target_body("a").as_deref(), Some("後から現れた実体"));
    }

    // 削除の計画を作った後に、対象が書き換えられた。もう「自分が置いたもの」ではない。
    #[test]
    fn a_target_edited_after_planning_is_not_deleted() {
        let mut b = Bench::new("gate3-edited");
        b.source_skill("a", "中身");
        b.apply(&on(&["a"]));
        let plan = build_plan(&b.resolve(), &Selection::default());

        b.t.write("target/skills/a/SKILL.md", "適用直前に手で書き換えた");

        let report = apply_plan(&plan, &b.target, &mut b.manifest);

        assert!(report.done.is_empty());
        assert!(matches!(
            report.skipped.as_slice(),
            [Skipped {
                reason: SkipReason::Changed,
                ..
            }]
        ));
        assert!(
            b.target.join("skills/a").exists(),
            "書き換えられたものを消した"
        );
        assert!(b.manifest.find(ItemKind::Skill, "a").is_some());
    }

    // 更新の計画を作った後に、ターゲットが書き換えられた。上書きは Conflict の許可が要る。
    #[test]
    fn an_update_whose_target_changed_after_planning_is_skipped() {
        let mut b = Bench::new("gate3-update");
        b.source_skill("a", "はじめ");
        b.apply(&on(&["a"]));
        b.source_skill("a", "新しいソース");
        let plan = build_plan(&b.resolve(), &on(&["a"]));

        b.t.write("target/skills/a/SKILL.md", "適用直前の手編集");

        let report = apply_plan(&plan, &b.target, &mut b.manifest);

        assert!(report.done.is_empty());
        assert_eq!(b.target_body("a").as_deref(), Some("適用直前の手編集"));
    }

    // 掃除するはずの記録の実体が、適用までに戻ってきた。掃除してはいけない。
    #[test]
    fn a_record_whose_entity_came_back_is_not_pruned() {
        let mut b = Bench::new("gate3-prune");
        b.source_skill("a", "x");
        b.record_with_hash("a", "計画時には存在しなかった");
        let plan = build_plan(&b.resolve(), &Selection::default());

        b.target_skill("a", "戻ってきた");

        let report = apply_plan(&plan, &b.target, &mut b.manifest);

        assert!(report.done.is_empty());
        assert!(b.manifest.find(ItemKind::Skill, "a").is_some());
    }

    // 同じ計画を二度適用しても、二度目は再判定で止まる。
    #[test]
    fn applying_the_same_plan_twice_is_stopped_by_the_recheck() {
        let mut b = Bench::new("gate3-twice");
        b.source_skill("a", "中身");
        let plan = build_plan(&b.resolve(), &on(&["a"]));

        let first = apply_plan(&plan, &b.target, &mut b.manifest);
        let second = apply_plan(&plan, &b.target, &mut b.manifest);

        assert_eq!(first.done.len(), 1);
        assert!(second.done.is_empty(), "二度目が素通りした");
    }

    // --- ゲート 4: 書き込み先の検証 ---

    // 親ディレクトリがターゲット外へのリンク。canonical path で見れば外を指している。
    #[cfg(unix)]
    #[test]
    fn a_destination_reached_through_a_symlinked_parent_is_refused() {
        let mut b = Bench::new("gate4-parent");
        b.source_skill("a", "ソース側");
        let outside = b.t.mkdir("outside");
        fs::write(outside.join("keep.txt"), "外にあるもの").unwrap();
        std::os::unix::fs::symlink(&outside, b.target.join("skills")).unwrap();

        let report = b.apply(&on(&["a"]));

        assert!(report.done.is_empty());
        assert!(!outside.join("a").exists(), "ターゲットの外に書いた");
        assert_eq!(
            fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "外にあるもの"
        );
    }

    // 書き込み先そのものがリンク。追わずに拒否する。
    #[cfg(unix)]
    #[test]
    fn a_destination_that_is_itself_a_symlink_is_refused() {
        let mut b = Bench::new("gate4-self");
        b.source_skill("a", "ソース側");
        let outside = b.t.mkdir("outside");
        fs::write(outside.join("SKILL.md"), "外の中身").unwrap();
        fs::create_dir_all(b.target.join("skills")).unwrap();
        std::os::unix::fs::symlink(&outside, b.target.join("skills/a")).unwrap();
        // 記録を入れて Conflict にし、許可まで与えても通らないことを見る。
        b.record_with_hash("a", "一致しない");

        let mut sel = on(&["a"]);
        sel.allow_conflict(ItemKind::Skill, "a");
        let report = b.apply(&sel);

        assert!(report.done.is_empty());
        assert_eq!(
            fs::read_to_string(outside.join("SKILL.md")).unwrap(),
            "外の中身"
        );
    }

    // --- コピー元の検証 ---

    // ソースにリンクが混ざっていたら、ターゲット外の内容を持ち込むことになる。
    // 途中まで書いた状態も残さない。
    #[cfg(unix)]
    #[test]
    fn a_source_containing_a_symlink_is_refused_without_a_partial_copy() {
        let mut b = Bench::new("apply-srclink");
        let dir = b.source_skill("a", "中身");
        let outside = b.t.write("outside.txt", "外の秘密");
        std::os::unix::fs::symlink(&outside, dir.join("link.md")).unwrap();

        let report = b.apply(&on(&["a"]));

        assert!(report.done.is_empty());
        assert!(!report.failed.is_empty());
        assert!(
            !b.target.join("skills/a").exists(),
            "途中まで書いたものが残った"
        );
    }

    // 1 件失敗しても、他の項目は処理される。
    #[cfg(unix)]
    #[test]
    fn one_failure_does_not_stop_the_rest() {
        let mut b = Bench::new("apply-continue");
        let bad = b.source_skill("bad", "x");
        let outside = b.t.write("outside.txt", "外");
        std::os::unix::fs::symlink(&outside, bad.join("link.md")).unwrap();
        b.source_skill("good", "通るはず");

        let report = b.apply(&on(&["bad", "good"]));

        assert_eq!(report.done.len(), 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(b.target_body("good").as_deref(), Some("通るはず"));
    }
}
