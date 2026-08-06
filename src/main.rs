//! 端末セットアップ、イベントループ起動、実パス解決。
//!
//! 実パス（`dirs::home_dir()` によるターゲット、ソースディレクトリの探索）を解決するのは
//! ここだけ。ライブラリ側には `claude_dir` / `source_dir` を引数として渡す。
//! panic 時も raw mode と alternate screen を解除してから panic 表示へ渡す
//! （`install_panic_hook`）。
//!
//! TODO(migrate): 実装は `apps/proteus/src/main.rs` から移す。
//! 移設時に加えること:
//!   - `cli::Args::parse()` を先頭に置き、`--source` / `--target` を優先する
//!   - ソースが見つからない場合はエラー終了せず、`~/.claude` の可視化画面から始める（企画 §7）

use std::process;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    Err("drybench はまだ実装されていない（リポジトリ構造のみ）".to_string())
}
