//! `ItemKind`（Skill/Agent/Hook）、`DraftItem`、`ItemState`（On/Off/Conflict/Unmanaged）。
//!
//! skill と hook はディレクトリ単位、agent は単一ファイル単位で同期する。
//! `ItemState::is_locked()` が「触ってよいか」の唯一の判断点であり、Conflict と
//! Unmanaged は常にロックされる（設計原則 1, 2）。
//!
//! TODO(migrate): 実装は `apps/proteus/src/model.rs` から移す。
//! 移設時に加えること: `ItemState::Unmanaged` を「表示しない」から「一覧の先頭に出す」へ。
//! 管理外の実体が見えていること自体が入口になる（企画 §7）。

use std::path::PathBuf;

/// 同期の単位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Skill,
    Agent,
    Hook,
}

impl ItemKind {
    /// 走査順であり、一覧の表示順でもある。
    pub const ALL: [ItemKind; 3] = [Self::Skill, Self::Agent, Self::Hook];

    /// ソース／ターゲット直下の、この種別が置かれるサブディレクトリ名。
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Skill => "skills",
            Self::Agent => "agents",
            Self::Hook => "hooks",
        }
    }

    /// ディレクトリ単位で同期するか（agent だけが単一ファイル）。
    pub fn is_dir_unit(self) -> bool {
        self.required_file().is_some()
    }

    /// ディレクトリ単位の種別で、実体と認めるために必須のファイル。
    /// これが無いディレクトリは項目として扱わない。
    pub fn required_file(self) -> Option<&'static str> {
        match self {
            Self::Skill => Some("SKILL.md"),
            Self::Hook => Some("hook.json"),
            Self::Agent => None,
        }
    }
}

/// 項目の状態。**`is_locked()` が「触ってよいか」の唯一の判断点**（設計原則 1, 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// マニフェストに記録があり、ハッシュも一致する。
    On,
    /// ソースにあるが、ターゲットには入っていない。
    Off,
    /// マニフェストに記録はあるが、ハッシュが一致しない（手で編集された）。
    Conflict,
    /// ターゲットに実体はあるが、マニフェストに記録が無い。
    Unmanaged,
}

impl ItemState {
    /// 上書き・削除してはいけない状態か。**Conflict と Unmanaged は常にロックされる。**
    /// 書き込み経路を追加するときは必ずここを通すこと（設計原則 1, 2）。
    pub fn is_locked(self) -> bool {
        matches!(self, Self::Conflict | Self::Unmanaged)
    }

    /// 一覧の並び順。管理外の実体が見えていること自体が import の入口なので、
    /// Unmanaged を先頭に置く（企画 §7）。
    pub fn sort_key(self) -> u8 {
        match self {
            Self::Unmanaged => 0,
            Self::Conflict => 1,
            Self::On => 2,
            Self::Off => 3,
        }
    }
}

/// 走査で見つかった 1 項目。状態は持たない（`sync::resolve_state` が後から解決する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftItem {
    pub kind: ItemKind,
    pub name: String,
    /// 実体そのもののパス。ディレクトリ単位ならそのディレクトリ、agent ならファイル。
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    // 設計原則 1, 2: Conflict と Unmanaged は常にロックされる。ここが「触ってよいか」の
    // 唯一の判断点なので、4 状態すべてを固定する。
    #[test]
    fn conflict_and_unmanaged_are_always_locked() {
        assert!(!ItemState::On.is_locked());
        assert!(!ItemState::Off.is_locked());
        assert!(ItemState::Conflict.is_locked());
        assert!(ItemState::Unmanaged.is_locked());
    }

    // skill と hook はディレクトリ単位、agent は単一ファイル単位。
    #[test]
    fn skill_and_hook_are_directories_agent_is_a_file() {
        assert!(ItemKind::Skill.is_dir_unit());
        assert!(ItemKind::Hook.is_dir_unit());
        assert!(!ItemKind::Agent.is_dir_unit());
    }

    #[test]
    fn directory_kinds_have_a_required_file() {
        assert_eq!(ItemKind::Skill.required_file(), Some("SKILL.md"));
        assert_eq!(ItemKind::Hook.required_file(), Some("hook.json"));
        assert_eq!(ItemKind::Agent.required_file(), None);
    }

    #[test]
    fn each_kind_lives_in_its_own_subdirectory() {
        assert_eq!(ItemKind::Skill.dir_name(), "skills");
        assert_eq!(ItemKind::Agent.dir_name(), "agents");
        assert_eq!(ItemKind::Hook.dir_name(), "hooks");
    }

    // 一覧では Unmanaged を先頭に出す（企画 §7）。見えていること自体が import の入口。
    #[test]
    fn unmanaged_sorts_before_every_other_state() {
        let mut states = [
            ItemState::On,
            ItemState::Conflict,
            ItemState::Unmanaged,
            ItemState::Off,
        ];
        states.sort_by_key(|s| s.sort_key());
        assert_eq!(states[0], ItemState::Unmanaged);
    }
}
