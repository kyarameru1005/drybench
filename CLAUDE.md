# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 現状

実装は未着手。`src/*.rs` はモジュールドキュメント（`//!`）だけのスケルトンで、
`main.rs::run()` はエラーを返す。

**各ファイルの `//!` が仕様書。** 実装する前に対象ファイルの `//!` を読むこと。
マニフェストの JSON スキーマ（`manifest.rs`）や状態判定のルール表（`sync.rs`）は
そこにしか無い。

`TODO(migrate)` が指す `apps/proteus` は**このリポジトリに存在しない**（別リポジトリの
前身プロジェクト）。探しても見つからないので、移設作業では元コードの在り処を先に確認する。

## コマンド

```sh
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check

cargo test <テスト名> -- --exact    # 単一テスト
```

CI は `RUSTFLAGS: -D warnings` 付きで回る。ローカルの `cargo build` が通っても、
警告があれば落ちる。

## 変更するときに効く制約

- **[docs/design-principles.md](docs/design-principles.md) はガイドラインではなく受け入れ条件。**
  `~/.claude` はユーザーの実運用環境で、drybench が置いたもの以外には触らない。
  書き込み経路を足すときは必ず読むこと。
- **パス解決は `main.rs` と `cli.rs` だけ。** I/O 関数はディレクトリを引数で受け取る。
  テストが実在の `~/.claude` に到達しないための契約であり、交渉の余地がない。
- **依存を足さない。dev-dependencies は持たない。** テストは `std::env::temp_dir()` だけで
  完結させる。
- **v0.1 のスコープは 4 項目に固定されている。** それ以外を実装する前に
  [docs/roadmap.md](docs/roadmap.md) を確認する。
- **UI 文字列の言語は未決**（[docs/roadmap.md](docs/roadmap.md) の Open questions）。
  方針が決まるまで一括変換しない。

## ブランチ運用

`dev` が起点。作業ブランチは `dev` から切り、`dev` へマージする。`main` へは、まとまった
単位が完全に仕上がった時点でのみ `dev` からマージする。`main` と `dev` のどちらでも
直接作業しない。

```sh
git checkout dev && git pull
git checkout -b claude/<作業名>
gh pr create --base dev
```

`ci.yml` の push トリガは `main` のみだが、pull_request は全ブランチ対象なので
`dev` 宛の PR でも CI は回る。

## 参照

| ファイル | 内容 |
|---|---|
| [docs/architecture.md](docs/architecture.md) | モジュール一覧、データフロー、種別ごとの単位 |
| [docs/design-principles.md](docs/design-principles.md) | 安全ゲート、`settings.json` の扱い |
| [docs/roadmap.md](docs/roadmap.md) | スコープと未決事項 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 開発の作法 |
