//! drybench — `~/.claude` を把握し、試作物を安全に出し入れするローカルの実験台。
//!
//! moira / proteus と同じフラットな `src/` 構成。すべての I/O 関数は
//! `claude_dir: &Path` / `source_dir: &Path` を引数で受け取り、実パス解決（`home_dir()`、
//! ソースディレクトリの探索）は `main.rs` と `cli.rs` だけが行う。テストは
//! `std::env::temp_dir()` に用意したディレクトリを渡すだけで完結する。

pub mod cli;
pub mod editor;
pub mod import;
pub mod manifest;
pub mod model;
pub mod scaffold;
pub mod scan;
pub mod settings;
pub mod sync;
pub mod ui;
