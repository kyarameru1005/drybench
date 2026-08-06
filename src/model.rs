//! `ItemKind`（Skill/Agent/Hook）、`DraftItem`、`ItemState`（On/Off/Conflict/Unmanaged）。
//!
//! skill と hook はディレクトリ単位、agent は単一ファイル単位で同期する。
//! `ItemState::is_locked()` が「触ってよいか」の唯一の判断点であり、Conflict と
//! Unmanaged は常にロックされる（設計原則 1, 2）。
//!
//! TODO(migrate): 実装は `apps/proteus/src/model.rs` から移す。
//! 移設時に加えること: `ItemState::Unmanaged` を「表示しない」から「一覧の先頭に出す」へ。
//! 管理外の実体が見えていること自体が入口になる（企画 §7）。
