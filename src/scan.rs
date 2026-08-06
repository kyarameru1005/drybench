//! ソースディレクトリの走査、ディレクトリ探索、名前バリデーション。
//!
//! 名前に `..` やパス区切り（`/` `\`）、先頭 `.` を含むものは対象外にする
//! （パストラバーサル防止）。
//!
//! TODO(migrate): 実装は `apps/proteus/src/scan.rs` から移す。
//! 移設時に加えること: `~/.claude` 側の実体も走査し、ソースに対応の無いものを
//! Unmanaged として一覧に載せる（企画 §7 / `import` の前提）。

use std::fs;
use std::path::Path;

use crate::model::{DraftItem, ItemKind};

/// 項目名として受け付けてよいか。**走査時にここで弾く**ことが、パストラバーサルに対する
/// 最初の防壁になる（設計原則 4）。
///
/// 弾くもの: 空文字、`.` と `..`、パス区切り（`/` `\`）を含むもの、先頭が `.` のもの。
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains('/') && !name.contains('\\')
}

/// `root` 配下の `skills/` `agents/` `hooks/` を走査する。
///
/// ソースにもターゲットにも同じ関数を使う。**`root` は必ず引数で受け取り、この中で
/// `~/.claude` を組み立てることはしない**（テスト隔離の契約）。
///
/// 存在しないディレクトリは空として扱い、エラーにしない。ソースがまだ無い状態でも
/// 一覧画面から始められる必要があるため（企画 §7）。
pub fn scan(root: &Path) -> Vec<DraftItem> {
    let mut items = Vec::new();
    for kind in ItemKind::ALL {
        items.extend(scan_kind(root, kind));
    }
    items
}

/// 1 種別ぶんの走査。名前順に並べて返す（走査順は OS 依存なので、ここで決定的にする）。
fn scan_kind(root: &Path, kind: ItemKind) -> Vec<DraftItem> {
    let dir = root.join(kind.dir_name());
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut items: Vec<DraftItem> = entries
        .flatten()
        .filter_map(|entry| to_item(&entry.path(), kind))
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

/// パスを 1 項目として解釈する。項目として認められなければ `None`。
fn to_item(path: &Path, kind: ItemKind) -> Option<DraftItem> {
    let name = match kind.required_file() {
        // ディレクトリ単位: ディレクトリ名がそのまま項目名。必須ファイルが無ければ項目でない。
        Some(required) => {
            if !path.is_dir() || !path.join(required).is_file() {
                return None;
            }
            path.file_name()?.to_str()?.to_string()
        }
        // 単一ファイル単位: `<name>.md` の stem が項目名。
        None => {
            if !path.is_file() || path.extension()? != "md" {
                return None;
            }
            path.file_stem()?.to_str()?.to_string()
        }
    };

    is_valid_name(&name).then(|| DraftItem {
        kind,
        name,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    fn names(items: &[DraftItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    // --- 名前バリデーション（パストラバーサル防止） ---

    #[test]
    fn ordinary_names_are_valid() {
        assert!(is_valid_name("my-skill"));
        assert!(is_valid_name("skill_1"));
        assert!(is_valid_name("a"));
    }

    #[test]
    fn names_that_could_escape_the_directory_are_rejected() {
        assert!(!is_valid_name(".."));
        assert!(!is_valid_name("."));
        assert!(!is_valid_name("../evil"));
        assert!(!is_valid_name("foo/../bar"));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("a\\b"));
    }

    #[test]
    fn hidden_and_empty_names_are_rejected() {
        assert!(!is_valid_name(""));
        assert!(!is_valid_name(".hidden"));
        assert!(!is_valid_name(".DS_Store"));
    }

    // --- 走査 ---

    #[test]
    fn finds_items_of_every_kind() {
        let t = TempDir::new("every-kind");
        t.dir_item(ItemKind::Skill, "my-skill");
        t.agent_item("my-agent");
        t.dir_item(ItemKind::Hook, "my-hook");

        let items = scan(t.path());

        assert_eq!(items.len(), 3);
        assert_eq!(
            items
                .iter()
                .find(|i| i.kind == ItemKind::Skill)
                .map(|i| i.name.as_str()),
            Some("my-skill")
        );
        // agent の name は拡張子を落とした stem。
        assert_eq!(
            items
                .iter()
                .find(|i| i.kind == ItemKind::Agent)
                .map(|i| i.name.as_str()),
            Some("my-agent")
        );
        assert_eq!(
            items
                .iter()
                .find(|i| i.kind == ItemKind::Hook)
                .map(|i| i.name.as_str()),
            Some("my-hook")
        );
    }

    #[test]
    fn a_directory_without_its_required_file_is_not_an_item() {
        let t = TempDir::new("missing-required");
        fs::create_dir_all(t.path().join("skills").join("no-skill-md")).unwrap();
        fs::create_dir_all(t.path().join("hooks").join("no-hook-json")).unwrap();

        assert!(scan(t.path()).is_empty());
    }

    #[test]
    fn invalid_names_are_skipped_during_the_scan() {
        let t = TempDir::new("invalid-names");
        t.dir_item(ItemKind::Skill, ".hidden-skill");
        t.dir_item(ItemKind::Skill, "good-skill");
        t.agent_item(".hidden-agent");

        assert_eq!(names(&scan(t.path())), vec!["good-skill"]);
    }

    #[test]
    fn non_markdown_files_are_not_agents() {
        let t = TempDir::new("agent-ext");
        let dir = t.path().join("agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::write(dir.join("real.md"), b"x").unwrap();

        assert_eq!(names(&scan(t.path())), vec!["real"]);
    }

    #[test]
    fn a_missing_root_scans_to_nothing_rather_than_failing() {
        let t = TempDir::new("missing-root");
        // skills/ agents/ hooks/ をひとつも作らない。初回起動はこの状態で始まる。
        assert!(scan(t.path()).is_empty());
        assert!(scan(&t.path().join("does-not-exist")).is_empty());
    }

    #[test]
    fn results_are_ordered_deterministically() {
        let t = TempDir::new("ordering");
        for name in ["c-skill", "a-skill", "b-skill"] {
            t.dir_item(ItemKind::Skill, name);
        }

        assert_eq!(
            names(&scan(t.path())),
            vec!["a-skill", "b-skill", "c-skill"]
        );
    }

    #[test]
    fn the_recorded_path_points_at_the_item_itself() {
        let t = TempDir::new("paths");
        t.dir_item(ItemKind::Skill, "my-skill");
        t.agent_item("my-agent");

        let items = scan(t.path());
        let skill = items.iter().find(|i| i.kind == ItemKind::Skill).unwrap();
        let agent = items.iter().find(|i| i.kind == ItemKind::Agent).unwrap();

        // skill はディレクトリ、agent はファイルを指す。
        assert_eq!(skill.path, t.path().join("skills").join("my-skill"));
        assert_eq!(agent.path, t.path().join("agents").join("my-agent.md"));
    }
}
