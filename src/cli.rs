//! コマンドライン引数。
//!
//! v0.1 の必須項目。ソースディレクトリを「リポジトリ直下の `drafts/` を上方探索」に
//! 固定していると、他人の環境では初回起動で「見つからない」と言われて終わる（企画 §7, §8-4）。
//!
//! 解決順:
//!   1. `--source` / `--target`
//!   2. 環境変数 `DRYBENCH_SOURCE` / `DRYBENCH_TARGET`
//!   3. カレントからの上方探索で見つかる `drafts/`
//!   4. `~/.drybench/source` / `~/.claude`
//!
//! TODO(migrate): `clap::Parser` で `Args` を定義する。
