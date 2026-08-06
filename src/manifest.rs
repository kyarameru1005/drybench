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
