//! `hook.json` の読み取りと、`~/.claude/settings.json` の `hooks` への登録・解除。
//!
//! hook はファイルを置くだけでは発火せず、`settings.json` への登録が必要になる。
//! ここが唯一、ユーザーの実運用設定ファイルを書き換える場所なので、次の原則を守る
//! （設計原則 5）。
//!
//! - **drybench が入れたグループだけを消す。** 挿入したグループの正規化 JSON の sha256 を
//!   マニフェストに記録し、除去時はハッシュが一致するものだけを対象にする。手で編集された
//!   グループはハッシュが変わるので触らない（実体側の Conflict と同じ思想）。
//! - **既存の設定に触らない。** `hooks` 以外のキーも、他人が書いた hook のグループも
//!   読み書きしない。
//! - **壊れた JSON なら何もしない。** 解析に失敗したら Err を返し、書き込みへ進まない。
//! - **書き込みはバックアップ＋アトミック。** 直前の内容を `settings.json.drybench-backup`
//!   へ退避し、一時ファイルへ書いてから `rename` する。キーの並び順は `serde_json` の
//!   `preserve_order` で保つ。
//! - **hook が1件も無い操作では `settings.json` を開かない。**
//! - **登録に失敗してもファイル同期は巻き戻さない。** マニフェストを先に保存し、実体との
//!   対応が取れた状態を保つ（もう一度 ON にすれば再登録される）。
//!
//! ## 書き換えで残る正規化
//!
//! 「`hooks` 以外に触らない」は値についての約束であって、バイト列についてではない。
//! ファイル全体を読み直して書き戻す以上、次は変わる。**値は変えないが、書き方は変わる。**
//!
//! - インデントは 2 スペースに、末尾には改行が付く
//! - `\uXXXX` は対応する文字に、`\/` は `/` に展開される
//! - 指数表記の符号が明示される（`1e10` → `1e+10`）
//!
//! 数値そのものは `arbitrary_precision` で書かれたまま保つ（これが無いと 2^53 超の整数が
//! 浮動小数に化け、`1e-400` が `0.0` に潰れる）。
//!
//! **重複キーだけは復元できない。** JSON の重複キーは後勝ちで畳まれるので、書き戻すと
//! 先に書かれた方が消える。直前の内容はバックアップに残るが、次の書き換えで上書きされる。
//! コメント付き JSON（JSONC）は解析できないため、そもそも何もしない。
//!
//! TODO(migrate): 実装は `apps/proteus/src/settings.rs` から移す。
//! 移設時に加えること: バックアップのファイル名を `.proteus-backup` から
//! `.drybench-backup` へ変更する。

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::manifest::HookRegistration;

/// ユーザーの実運用設定。**drybench が書き換えてよい唯一のユーザー所有ファイル。**
pub const SETTINGS_FILE: &str = "settings.json";

/// 書き換える直前の内容を退避する先。
pub const BACKUP_FILE: &str = "settings.json.drybench-backup";

/// drybench が触ってよい唯一のトップレベルキー。
const HOOKS_KEY: &str = "hooks";

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 一時ファイル名の生成を諦めるまでの試行回数。
const TEMP_ATTEMPTS: usize = 8;

// ------------------------------------------------------------ hook.json

/// hook ディレクトリの `hook.json`。形式は `skills/drybench-author/SKILL.md` に定義済み。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    #[serde(default)]
    pub description: Option<String>,
    pub events: Vec<HookEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    pub event: String,
    /// `Stop` のように matcher を取らないイベントでは省く。
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// hook ディレクトリから `hook.json` を読む。
pub fn read_spec(hook_dir: &Path) -> Result<HookSpec, SettingsError> {
    let path = hook_dir.join("hook.json");
    let raw = fs::read_to_string(&path).map_err(|e| SettingsError::Io(path.clone(), e))?;
    serde_json::from_str(&raw).map_err(|e| SettingsError::Parse(path, e))
}

// ------------------------------------------------------------- 登録・解除

/// `settings.json` の `hooks` に、この hook のグループを足す。
///
/// 返すのは入れたグループへの参照（イベント名と、正規化 JSON の sha256）。
/// **これをマニフェストに記録しておかないと、後で自分が入れたものを見分けられない。**
///
/// イベントが 1 件も無ければ `settings.json` を開かない（設計原則 5）。
pub fn register(
    claude_dir: &Path,
    spec: &HookSpec,
) -> Result<Vec<HookRegistration>, SettingsError> {
    if spec.events.is_empty() {
        return Ok(Vec::new());
    }

    let path = claude_dir.join(SETTINGS_FILE);
    let mut root = read_settings(&path)?;

    let mut registrations = Vec::new();
    let mut changed = false;
    for event in &spec.events {
        let group = build_group(event);
        let hash = group_hash(&group);
        registrations.push(HookRegistration {
            event: event.event.clone(),
            group_hash: hash.clone(),
        });

        let groups = hooks_object(&mut root)?
            .entry(event.event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                SettingsError::UnexpectedShape(format!("hooks.{} が配列でない", event.event))
            })?;

        // 同じグループが既にあるなら足さない。二重に入れると、記録は 1 件しか
        // 残らないので片方が管理外の孤児になり、消したスクリプトを呼び続ける。
        if groups.iter().any(|g| group_hash(g) == hash) {
            continue;
        }
        groups.push(group);
        changed = true;
    }

    if changed {
        write_settings(claude_dir, &path, &root)?;
    }
    Ok(registrations)
}

/// 記録したグループだけを `hooks` から取り除く。
///
/// **ハッシュが一致するものだけが対象。** 手で編集されたグループは値が変わっているので
/// 残る（実体側の Conflict と同じ思想）。他人が書いたグループも当然残る。
/// 同じ形のグループが複数あっても、記録 1 件につき 1 つしか消さない。
pub fn unregister(
    claude_dir: &Path,
    registrations: &[HookRegistration],
) -> Result<(), SettingsError> {
    if registrations.is_empty() {
        return Ok(());
    }

    let path = claude_dir.join(SETTINGS_FILE);
    if !path.exists() {
        return Ok(()); // 消す先が無い。
    }
    let mut root = read_settings(&path)?;

    let mut changed = false;
    for registration in registrations {
        let hooks = hooks_object(&mut root)?;
        let Some(groups) = hooks
            .get_mut(&registration.event)
            .and_then(|v| v.as_array_mut())
        else {
            continue;
        };
        let Some(i) = groups
            .iter()
            .position(|g| group_hash(g) == registration.group_hash)
        else {
            // 一致するものが無い。**このイベントには何もしない** — 空の配列が
            // 残っていても、それは drybench が作ったものとは限らない。
            continue;
        };
        groups.remove(i);
        changed = true;

        // 自分が入れたグループを抜いて空になったイベントだけを畳む。
        if groups.is_empty() {
            hooks.remove(&registration.event);
        }
    }

    if changed {
        // 自分が作った `hooks` を空のまま残さない。ユーザーが元から持っていた
        // `hooks` は、この時点で空になっていない限り残る。
        if let Some(hooks) = root.get(HOOKS_KEY).and_then(|v| v.as_object()) {
            if hooks.is_empty() {
                root.as_object_mut()
                    .expect("上で確認済み")
                    .remove(HOOKS_KEY);
            }
        }
        write_settings(claude_dir, &path, &root)?;
    }
    Ok(())
}

/// Claude Code が `settings.json` に期待する 1 グループを組み立てる。
fn build_group(event: &HookEvent) -> Value {
    let mut inner = Map::new();
    inner.insert("type".to_string(), Value::from("command"));
    inner.insert("command".to_string(), Value::from(event.command.clone()));
    if let Some(timeout) = event.timeout {
        inner.insert("timeout".to_string(), Value::from(timeout));
    }

    let mut group = Map::new();
    // matcher を取らないイベントでは、キーごと出さない。
    if let Some(matcher) = &event.matcher {
        group.insert("matcher".to_string(), Value::from(matcher.clone()));
    }
    group.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(inner)]),
    );
    Value::Object(group)
}

/// グループの正規化 JSON の sha256。
///
/// **キーを並べ替えてから文字列にする。** `preserve_order` を有効にしているため、
/// 読み書きの経路によってキーの並びが変わりうる。並びでハッシュが変わると、
/// 自分が入れたグループを自分で見分けられなくなる。
pub fn group_hash(group: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical(group).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let body: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", Value::from(k.as_str()), canonical(&map[*k])))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// `hooks` オブジェクトへの可変参照。無ければ作る。
/// **ここ以外のキーには一切触れない。**
///
/// `hooks` が既にあってオブジェクトでない場合（`"hooks": []` の打ち間違いなど）は
/// **エラーを返す。** 作り直せば、ユーザーが書いたものを黙って捨てることになる。
fn hooks_object(root: &mut Value) -> Result<&mut Map<String, Value>, SettingsError> {
    let obj = root.as_object_mut().ok_or_else(|| {
        SettingsError::UnexpectedShape("settings.json がオブジェクトでない".into())
    })?;

    match obj.get(HOOKS_KEY) {
        None => {
            obj.insert(HOOKS_KEY.to_string(), Value::Object(Map::new()));
        }
        Some(existing) if !existing.is_object() => {
            return Err(SettingsError::UnexpectedShape(format!(
                "settings.json の \"{HOOKS_KEY}\" がオブジェクトでない"
            )));
        }
        Some(_) => {}
    }

    Ok(obj
        .get_mut(HOOKS_KEY)
        .and_then(|v| v.as_object_mut())
        .expect("直前にオブジェクトであることを確認済み"))
}

// ------------------------------------------------------------- 読み書き

/// `settings.json` を読む。
///
/// 無ければ空のオブジェクト。**壊れていれば `Err` で、書き込みへは進まない** — 修復も
/// 再生成もしない（設計原則 5）。トップレベルがオブジェクトでないものも同様に扱う。
fn read_settings(path: &Path) -> Result<Value, SettingsError> {
    refuse_symlink(path)?;

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Object(Map::new())),
        Err(e) => return Err(SettingsError::Io(path.to_path_buf(), e)),
    };
    // 先頭の BOM を落とす。Claude Code 自身は受け付けるので、これで解析できないと
    // 「自分の設定が読めない」と言われて終わってしまう。
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);

    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let value: Value =
        serde_json::from_str(raw).map_err(|e| SettingsError::Parse(path.to_path_buf(), e))?;
    if !value.is_object() {
        return Err(SettingsError::UnexpectedShape(
            "settings.json の中身がオブジェクトではない".to_string(),
        ));
    }
    Ok(value)
}

/// バックアップを取ってから、一時ファイル経由で置き換える。
///
/// **退避するのは、解析できた内容だけ。** 壊れたファイルはここへ来ない（`read_settings`
/// が先に `Err` を返す）ので、バックアップが壊れた内容で上書きされることはない。
fn write_settings(claude_dir: &Path, path: &Path, root: &Value) -> Result<(), SettingsError> {
    fs::create_dir_all(claude_dir).map_err(|e| SettingsError::Io(claude_dir.to_path_buf(), e))?;

    let existing = fs::read(path).ok();
    // 元のファイルの権限を引き継ぐ。`settings.json` には `env` の値が入りうるので、
    // 0600 にしてある人のものを 0644 で置き換えてはいけない。
    let mode = existing
        .is_some()
        .then(|| fs::metadata(path).ok().map(|m| m.permissions()))
        .flatten();

    if let Some(existing) = existing {
        let backup = claude_dir.join(BACKUP_FILE);
        // **バックアップ先もリンクを追わない。** 固定の名前なので、ここが
        // ターゲット外へのリンクだと、無関係のファイルを切り詰めて上書きする。
        refuse_symlink(&backup)?;
        write_atomically(claude_dir, &backup, &existing, mode.clone())?;
    }

    let mut body = serde_json::to_string_pretty(root).map_err(SettingsError::Serialize)?;
    // 末尾の改行の有無は、ユーザーのファイルの見た目の一部。元に合わせる。
    body.push('\n');

    write_atomically(claude_dir, path, body.as_bytes(), mode)
}

/// 一時ファイルへ書いてから `rename` で置き換える。
///
/// `create_new` で開くので、既存のファイルにもシンボリックリンクにも書き込まない。
/// 名前は毎回変える — 固定名にすると、前回のクラッシュが残した残骸で以後ずっと
/// 書けなくなる。
fn write_atomically(
    claude_dir: &Path,
    path: &Path,
    body: &[u8],
    mode: Option<fs::Permissions>,
) -> Result<(), SettingsError> {
    let (temp, mut file) = create_temp(claude_dir)?;

    let written = file.write_all(body).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(SettingsError::Io(temp, e));
    }

    if let Some(mode) = mode {
        let _ = fs::set_permissions(&temp, mode);
    }

    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(SettingsError::Io(path.to_path_buf(), e));
    }

    // rename 自体を耐久化する。
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(claude_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// まだ存在しない一時ファイルを作って開く。前回のクラッシュが残した残骸と衝突しても、
/// 数回ずらせば空きが見つかる。
fn create_temp(claude_dir: &Path) -> Result<(PathBuf, fs::File), SettingsError> {
    let pid = std::process::id();
    let mut last = None;
    for _ in 0..TEMP_ATTEMPTS {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = claude_dir.join(format!("{SETTINGS_FILE}.{pid}.{n}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some((path, e)),
            Err(e) => return Err(SettingsError::Io(path, e)),
        }
    }
    let (path, e) = last.expect("TEMP_ATTEMPTS は 1 以上");
    Err(SettingsError::Io(path, e))
}

/// 対象自体がシンボリックリンクなら拒否する（設計原則 4）。
fn refuse_symlink(path: &Path) -> Result<(), SettingsError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(SettingsError::SymlinkRefused(path.to_path_buf()))
        }
        _ => Ok(()),
    }
}

#[derive(Debug)]
pub enum SettingsError {
    Io(PathBuf, std::io::Error),
    /// 解析できない。**この場合は何もしない。**
    Parse(PathBuf, serde_json::Error),
    Serialize(serde_json::Error),
    UnexpectedShape(String),
    SymlinkRefused(PathBuf),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, e) => write!(f, "{}: {e}", path.display()),
            Self::Parse(path, e) => write!(f, "{} を解析できない: {e}", path.display()),
            Self::Serialize(e) => write!(f, "設定を JSON にできない: {e}"),
            Self::UnexpectedShape(what) => write!(f, "想定外の形: {what}"),
            Self::SymlinkRefused(path) => write!(
                f,
                "{} はシンボリックリンク。追わずに中止した",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    const HOOK_JSON: &str = r#"{
        "description": "何かする",
        "events": [
            { "event": "PostToolUse", "matcher": "Bash",
              "command": "\"$HOME/.claude/hooks/h/run.sh\"", "timeout": 5 }
        ]
    }"#;

    fn hook_dir(t: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
        t.write(format!("hooks/{name}/hook.json"), body);
        t.join(format!("hooks/{name}"))
    }

    fn settings_of(t: &TempDir) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(t.join(SETTINGS_FILE)).unwrap()).unwrap()
    }

    // --- hook.json の読み取り ---

    #[test]
    fn a_hook_spec_is_read_from_its_directory() {
        let t = TempDir::new("hook-read");
        let dir = hook_dir(&t, "h", HOOK_JSON);

        let spec = read_spec(&dir).unwrap();

        assert_eq!(spec.events.len(), 1);
        assert_eq!(spec.events[0].event, "PostToolUse");
        assert_eq!(spec.events[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(spec.events[0].timeout, Some(5));
    }

    // `Stop` のように matcher を取らないイベントがある。
    #[test]
    fn an_event_without_a_matcher_is_allowed() {
        let t = TempDir::new("hook-nomatcher");
        let dir = hook_dir(
            &t,
            "h",
            r#"{"events":[{"event":"Stop","command":"/bin/true"}]}"#,
        );

        let spec = read_spec(&dir).unwrap();

        assert_eq!(spec.events[0].matcher, None);
        assert_eq!(spec.events[0].timeout, None);
    }

    #[test]
    fn a_malformed_or_incomplete_hook_json_is_an_error() {
        let t = TempDir::new("hook-bad");
        assert!(read_spec(&hook_dir(&t, "a", "{ not json")).is_err());
        // command が無ければ登録しようがない。
        assert!(read_spec(&hook_dir(&t, "b", r#"{"events":[{"event":"Stop"}]}"#)).is_err());
        assert!(read_spec(&t.join("hooks/missing")).is_err());
    }

    // --- 登録 ---

    #[test]
    fn registering_writes_the_group_claude_code_expects() {
        let t = TempDir::new("reg-shape");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        let v = settings_of(&t);
        let group = &v["hooks"]["PostToolUse"][0];
        assert_eq!(group["matcher"], "Bash");
        assert_eq!(group["hooks"][0]["type"], "command");
        assert_eq!(
            group["hooks"][0]["command"],
            "\"$HOME/.claude/hooks/h/run.sh\""
        );
        assert_eq!(group["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn an_event_without_a_matcher_registers_without_one() {
        let t = TempDir::new("reg-nomatcher");
        let spec = read_spec(&hook_dir(
            &t,
            "h",
            r#"{"events":[{"event":"Stop","command":"/bin/true"}]}"#,
        ))
        .unwrap();

        register(t.path(), &spec).unwrap();

        let v = settings_of(&t);
        assert!(v["hooks"]["Stop"][0].get("matcher").is_none());
        assert!(v["hooks"]["Stop"][0]["hooks"][0].get("timeout").is_none());
    }

    // 設計原則 5: `hooks` 以外のキーも、他人の hook グループも読み書きしない。
    #[test]
    fn registering_leaves_everything_else_untouched() {
        let t = TempDir::new("reg-untouched");
        t.write(
            SETTINGS_FILE,
            r#"{
  "model": "opus",
  "hooks": {
    "PostToolUse": [
      { "matcher": "Write", "hooks": [{"type":"command","command":"他人のもの"}] }
    ],
    "Stop": [
      { "hooks": [{"type":"command","command":"これも他人"}] }
    ]
  },
  "env": { "FOO": "bar" }
}"#,
        );
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        let v = settings_of(&t);
        assert_eq!(v["model"], "opus");
        assert_eq!(v["env"]["FOO"], "bar");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "これも他人");
        // 既存のグループは残り、drybench のものが後ろに足される。
        assert_eq!(v["hooks"]["PostToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], "Write");
    }

    // `serde_json` の `preserve_order` が外れたら落ちる回帰テスト。
    #[test]
    fn the_order_of_the_users_keys_is_preserved() {
        let t = TempDir::new("reg-order");
        t.write(
            SETTINGS_FILE,
            r#"{"zebra":1,"model":"opus","alpha":2,"env":{"Z":"1","A":"2"}}"#,
        );
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        let raw = fs::read_to_string(t.join(SETTINGS_FILE)).unwrap();
        let zebra = raw.find("zebra").unwrap();
        let model = raw.find("model").unwrap();
        let alpha = raw.find("alpha").unwrap();
        assert!(zebra < model && model < alpha, "並び順が変わった:\n{raw}");
        assert!(raw.find("\"Z\"").unwrap() < raw.find("\"A\"").unwrap());
    }

    #[test]
    fn a_backup_is_written_before_the_settings_change() {
        let t = TempDir::new("reg-backup");
        let before = r#"{"model":"opus"}"#;
        t.write(SETTINGS_FILE, before);
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        assert_eq!(
            fs::read_to_string(t.join(BACKUP_FILE)).unwrap(),
            before,
            "バックアップが直前の内容になっていない"
        );
    }

    // 設計原則 5: 壊れた JSON なら何もしない。修復も再生成もしない。
    #[test]
    fn a_malformed_settings_file_is_left_exactly_as_it_was() {
        let t = TempDir::new("reg-malformed");
        let broken = "{ \"model\": \"opus\", oops";
        t.write(SETTINGS_FILE, broken);
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        assert!(register(t.path(), &spec).is_err());

        assert_eq!(fs::read_to_string(t.join(SETTINGS_FILE)).unwrap(), broken);
        assert!(!t.join(BACKUP_FILE).exists(), "壊れたものを退避した");
    }

    #[test]
    fn registering_creates_the_file_when_there_is_none() {
        let t = TempDir::new("reg-create");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        assert!(settings_of(&t)["hooks"]["PostToolUse"][0]["matcher"] == "Bash");
    }

    // 設計原則 4: 追わずに拒否する。
    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_refused() {
        let t = TempDir::new("reg-symlink");
        let victim = t.write("victim.json", r#"{"secret":true}"#);
        let claude = t.mkdir("claude");
        std::os::unix::fs::symlink(&victim, claude.join(SETTINGS_FILE)).unwrap();
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        assert!(register(&claude, &spec).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), r#"{"secret":true}"#);
    }

    // hook が 1 件も無い操作では `settings.json` を開かない。
    #[test]
    fn an_operation_with_no_events_does_not_touch_the_file() {
        let t = TempDir::new("reg-empty");
        let spec = read_spec(&hook_dir(&t, "h", r#"{"events":[]}"#)).unwrap();

        assert!(register(t.path(), &spec).unwrap().is_empty());
        assert!(
            !t.join(SETTINGS_FILE).exists(),
            "空の操作でファイルを作った"
        );

        assert!(unregister(t.path(), &[]).is_ok());
        assert!(!t.join(SETTINGS_FILE).exists());
    }

    // --- 解除 ---

    #[test]
    fn unregistering_removes_only_the_group_that_was_recorded() {
        let t = TempDir::new("unreg-only-ours");
        t.write(
            SETTINGS_FILE,
            r#"{"hooks":{"PostToolUse":[
                {"matcher":"Write","hooks":[{"type":"command","command":"他人のもの"}]}
            ]}}"#,
        );
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();
        assert_eq!(
            settings_of(&t)["hooks"]["PostToolUse"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        unregister(t.path(), &registrations).unwrap();

        let v = settings_of(&t);
        let groups = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], "Write");
    }

    // 手で編集されたグループはハッシュが変わる。実体側の Conflict と同じ思想で触らない。
    #[test]
    fn a_group_edited_by_hand_is_not_removed() {
        let t = TempDir::new("unreg-edited");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        // ユーザーが timeout を変えた。
        let mut v = settings_of(&t);
        v["hooks"]["PostToolUse"][0]["hooks"][0]["timeout"] = serde_json::json!(30);
        t.write(SETTINGS_FILE, &serde_json::to_string(&v).unwrap());

        unregister(t.path(), &registrations).unwrap();

        let after = settings_of(&t);
        assert_eq!(
            after["hooks"]["PostToolUse"].as_array().unwrap().len(),
            1,
            "手で編集されたグループを消した"
        );
        assert_eq!(after["hooks"]["PostToolUse"][0]["hooks"][0]["timeout"], 30);
    }

    // 同じ内容のグループを他人も持っていた場合、消すのは 1 つだけ。
    #[test]
    fn an_identical_group_someone_else_added_is_not_also_removed() {
        let t = TempDir::new("unreg-identical");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        // 同じ形のグループがもう 1 つ現れた。
        let mut v = settings_of(&t);
        let same = v["hooks"]["PostToolUse"][0].clone();
        v["hooks"]["PostToolUse"].as_array_mut().unwrap().push(same);
        t.write(SETTINGS_FILE, &serde_json::to_string(&v).unwrap());

        unregister(t.path(), &registrations).unwrap();

        assert_eq!(
            settings_of(&t)["hooks"]["PostToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "同じ形のものを両方消した"
        );
    }

    #[test]
    fn an_event_left_with_no_groups_is_cleaned_up() {
        let t = TempDir::new("unreg-cleanup");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        unregister(t.path(), &registrations).unwrap();

        let v = settings_of(&t);
        assert!(
            v["hooks"].get("PostToolUse").is_none(),
            "空の配列が残った: {v}"
        );
    }

    #[test]
    fn unregistering_something_already_gone_is_not_an_error() {
        let t = TempDir::new("unreg-gone");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        unregister(t.path(), &registrations).unwrap();
        unregister(t.path(), &registrations).unwrap();
    }

    #[test]
    fn unregistering_leaves_a_malformed_file_alone() {
        let t = TempDir::new("unreg-malformed");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        let broken = "{ broken now";
        t.write(SETTINGS_FILE, broken);

        assert!(unregister(t.path(), &registrations).is_err());
        assert_eq!(fs::read_to_string(t.join(SETTINGS_FILE)).unwrap(), broken);
    }

    // --- ハッシュの安定性 ---

    // 記録したハッシュは、書いて読み直した後も同じでなければならない。
    // でなければ、自分が入れたグループを自分で見失う。
    #[test]
    fn the_recorded_hash_survives_a_round_trip_through_the_file() {
        let t = TempDir::new("hash-roundtrip");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        let v = settings_of(&t);
        let group = &v["hooks"]["PostToolUse"][0];
        assert_eq!(group_hash(group), registrations[0].group_hash);
    }

    // キーの並びが違うだけの同じグループは、同じハッシュになる。
    #[test]
    fn the_hash_does_not_depend_on_key_order() {
        let a = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"x"}]});
        let b = serde_json::json!({"hooks":[{"command":"x","type":"command"}],"matcher":"Bash"});

        assert_eq!(group_hash(&a), group_hash(&b));
    }

    // --- ユーザーが書いた値を変えないこと ---

    // `Value` で比較していると、書式も数値の型も見えない。**書いた値そのもの**が
    // 残っているかは、生の JSON を見ないと分からない。
    #[test]
    fn the_users_values_survive_being_rewritten() {
        let t = TempDir::new("reg-values");
        t.write(
            SETTINGS_FILE,
            r#"{"big":12345678901234567890123,"tiny":1e-400,"exp":1e10,"esc":"a\/b"}"#,
        );
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        let raw = fs::read_to_string(t.join(SETTINGS_FILE)).unwrap();
        // 値そのものが失われていないこと。書式の正規化（`1e10` → `1e+10`、
        // `a\/b` → `a/b`）は起きるが、いずれも同じ値を表す別の書き方でしかない。
        assert!(
            raw.contains("12345678901234567890123"),
            "2^53 超の整数が浮動小数に化けた:\n{raw}"
        );
        assert!(raw.contains("1e-400"), "極小の指数が 0 に潰れた:\n{raw}");
        assert!(
            raw.contains("1e10") || raw.contains("1e+10"),
            "指数表記が展開された:\n{raw}"
        );
    }

    #[test]
    fn the_file_ends_with_a_newline_like_a_normal_text_file() {
        let t = TempDir::new("reg-newline");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        assert!(fs::read_to_string(t.join(SETTINGS_FILE))
            .unwrap()
            .ends_with('\n'));
    }

    // --- 想定外の形 ---

    // `"hooks": []` は打ち間違いとして十分ありうる。落ちてはいけないし、作り直して
    // ユーザーが書いたものを捨ててもいけない。
    #[test]
    fn a_hooks_key_that_is_not_an_object_is_an_error_not_a_crash() {
        let t = TempDir::new("reg-hooks-shape");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        for broken in [r#"{"hooks":[]}"#, r#"{"hooks":null}"#, r#"{"hooks":"x"}"#] {
            t.write(SETTINGS_FILE, broken);

            assert!(matches!(
                register(t.path(), &spec),
                Err(SettingsError::UnexpectedShape(..))
            ));
            assert_eq!(fs::read_to_string(t.join(SETTINGS_FILE)).unwrap(), broken);

            let fake = [HookRegistration {
                event: "PostToolUse".to_string(),
                group_hash: "何であれ".to_string(),
            }];
            assert!(unregister(t.path(), &fake).is_err());
            assert_eq!(fs::read_to_string(t.join(SETTINGS_FILE)).unwrap(), broken);
        }
    }

    #[test]
    fn a_settings_file_with_a_byte_order_mark_is_still_readable() {
        let t = TempDir::new("reg-bom");
        t.write(SETTINGS_FILE, "\u{feff}{\"model\":\"opus\"}");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        assert_eq!(settings_of(&t)["model"], "opus");
    }

    // --- バックアップ ---

    // 固定の名前なので、ここがリンクだと無関係のファイルを切り詰めて上書きする。
    #[cfg(unix)]
    #[test]
    fn a_symlinked_backup_path_is_refused() {
        let t = TempDir::new("reg-backup-symlink");
        let victim = t.write("victim.txt", "壊されては困る");
        let claude = t.mkdir("claude");
        fs::write(claude.join(SETTINGS_FILE), r#"{"model":"opus"}"#).unwrap();
        std::os::unix::fs::symlink(&victim, claude.join(BACKUP_FILE)).unwrap();
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        assert!(register(&claude, &spec).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "壊されては困る");
    }

    // `settings.json` には `env` の値が入りうる。0600 にしてある人のものを
    // 0644 で置き換えない。
    #[cfg(unix)]
    #[test]
    fn the_file_mode_is_carried_over() {
        use std::os::unix::fs::PermissionsExt;

        let t = TempDir::new("reg-mode");
        t.write(SETTINGS_FILE, r#"{"env":{"TOKEN":"秘密"}}"#);
        fs::set_permissions(t.join(SETTINGS_FILE), fs::Permissions::from_mode(0o600)).unwrap();
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        let mode = |p: std::path::PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(t.join(SETTINGS_FILE)), 0o600);
        assert_eq!(mode(t.join(BACKUP_FILE)), 0o600, "バックアップが緩い");
    }

    // 前回のクラッシュが残した一時ファイルで、以後ずっと書けなくなってはいけない。
    #[test]
    fn a_leftover_temporary_file_does_not_block_the_next_write() {
        let t = TempDir::new("reg-stale-tmp");
        t.write(format!("{SETTINGS_FILE}.99999.0.tmp"), "クラッシュの残骸");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(t.path(), &spec).unwrap();

        assert_eq!(
            settings_of(&t)["hooks"]["PostToolUse"][0]["matcher"],
            "Bash"
        );
    }

    #[test]
    fn registering_works_when_the_directory_does_not_exist_yet() {
        let t = TempDir::new("reg-nodir");
        let claude = t.join("not-created-yet");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        register(&claude, &spec).unwrap();

        assert!(claude.join(SETTINGS_FILE).is_file());
    }

    // --- 二重登録 ---

    // 二重に入れると、記録は 1 件しか残らないので片方が管理外の孤児になり、
    // 消したスクリプトを呼び続ける。
    #[test]
    fn registering_the_same_hook_twice_does_not_duplicate_the_group() {
        let t = TempDir::new("reg-twice");
        let spec = read_spec(&hook_dir(&t, "h", HOOK_JSON)).unwrap();

        let first = register(t.path(), &spec).unwrap();
        let second = register(t.path(), &spec).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            settings_of(&t)["hooks"]["PostToolUse"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // 一度の解除で完全に消える。
        unregister(t.path(), &first).unwrap();
        assert!(settings_of(&t).get("hooks").is_none());
    }

    // --- 何も一致しなかったとき ---

    // 一致するものが無いイベントには何もしない。空の配列が残っていても、
    // それが drybench の作ったものとは限らない。
    #[test]
    fn an_event_we_removed_nothing_from_is_left_alone() {
        let t = TempDir::new("unreg-untouched-event");
        let spec = read_spec(&hook_dir(
            &t,
            "h",
            r#"{"events":[
                {"event":"Stop","command":"/bin/true"},
                {"event":"PostToolUse","matcher":"Bash","command":"/bin/true"}
            ]}"#,
        ))
        .unwrap();
        let registrations = register(t.path(), &spec).unwrap();

        // ユーザーが PostToolUse 側だけ手で消し、空の配列を残した。
        // 加えて、無関係の空配列も置いてある。
        let mut v = settings_of(&t);
        v["hooks"]["PostToolUse"] = serde_json::json!([]);
        v["hooks"]["UserPromptSubmit"] = serde_json::json!([]);
        t.write(SETTINGS_FILE, &serde_json::to_string(&v).unwrap());

        unregister(t.path(), &registrations).unwrap();

        let after = settings_of(&t);
        assert!(
            after["hooks"].get("PostToolUse").is_some(),
            "何も消していないイベントを畳んだ: {after}"
        );
        assert!(after["hooks"].get("UserPromptSubmit").is_some());
        assert!(
            after["hooks"].get("Stop").is_none(),
            "自分のものは消えるはず"
        );
    }

    #[test]
    fn a_different_group_hashes_differently() {
        let a = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"x"}]});
        let b = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"y"}]});

        assert_ne!(group_hash(&a), group_hash(&b));
    }
}
