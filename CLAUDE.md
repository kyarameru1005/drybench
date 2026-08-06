# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

---

## このリポジトリの現状

**実装はまだ入っていない。** `src/*.rs` はすべてモジュールドキュメント（`//!`）だけの
スケルトンで、`main.rs::run()` は "まだ実装されていない" を返す。各ファイルの
`TODO(migrate)` が、移設元と移設時に変えるべき点を明記している。

移設元の `apps/proteus` は**このリポジトリには存在しない**（別リポジトリの前身プロジェクト）。
`TODO(migrate)` を見て「そのパスを探す」のは無駄なので、移設作業を頼まれたら元コードの
在り処を先に確認すること。

各モジュールの `//!` は仕様書として書かれている。実装する前に対象ファイルの
`//!` を読むこと。マニフェストの JSON スキーマ（`manifest.rs`）や状態判定ルール表
（`sync.rs`）はそこにしかない。

## コマンド

```sh
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

単一テストの実行:

```sh
cargo test <部分一致するテスト名>
cargo test --lib <モジュール>::tests::<テスト名> -- --exact
cargo test -- --nocapture          # println! を出す
```

CI（`.github/workflows/ci.yml`, ubuntu + macOS）は `RUSTFLAGS: -D warnings` 付きで
fmt / clippy / test を回す。ローカルの `cargo build` が通っても、警告があれば CI は落ちる。

## アーキテクチャ

フラットな `src/`、単一バイナリ、依存は最小。モジュール一覧は
[docs/architecture.md](docs/architecture.md) にある。ここには「複数ファイルを読まないと
分からない構造」だけを書く。

### 中心にあるのは安全性の設計

drybench は `~/.claude`（ユーザーの実運用環境、このリポジトリの外）を書き換える。
機能はどれもその上に乗っているだけで、**本体は「触ってよいものを判定する仕組み」**。
[docs/design-principles.md](docs/design-principles.md) はガイドラインではなく受け入れ条件で、
これを破る変更は入らない。

判定はすべて**マニフェスト `~/.claude/.drybench-manifest.json` に帰着する**:

| ターゲット側の状態 | 扱い |
|---|---|
| マニフェストに記録があり、ハッシュ一致 | 触ってよい（`On`） |
| マニフェストに記録があり、ハッシュ不一致 | `Conflict` — 手で編集された。既定スキップ |
| マニフェストに記録が無い | `Unmanaged` — 常にロック。上書きも削除もしない |

`ItemState::is_locked()`（`model.rs`）が「触ってよいか」の唯一の判断点になる。新しい
書き込み経路を足すときは、この 1 箇所を必ず通すこと。

書き込み直前のゲートは 4 つ（`sync.rs`）: ①マニフェスト登録の有無 → ②ハッシュ照合 →
③**実行直前の状態再判定**（一覧表示は前回スキャン時点のもの。TOCTOU）→ ④canonical path が
ターゲット配下にあることの確認と、シンボリックリンクの拒否。

### データフロー

```
scan(source) ─┐
              ├─→ resolve_state → ItemState → ui（一覧）
scan(target) ─┤                                  │ トグルして sync
manifest ─────┘                                  ▼
                                          build_plan → ui（確認画面）
                                                     │ Enter
                          ゲート1〜4を再判定 ─→ apply_plan
                                                     │
                                  ┌──────────────────┼────────────┐
                                  ▼                  ▼            ▼
                            copy / delete      settings.json   manifest
```

### hook だけが `settings.json` に触る

3 つの種別のうち、hook だけが `~/.claude/settings.json` の書き換えを伴う（ファイルを
置くだけでは発火しないため）。ユーザー所有のファイルを書き換える唯一の場所なので、
`settings.rs` の `//!` に書かれた規約に例外はない。要点:

- 挿入したグループの正規化 JSON の sha256 をマニフェストの `hook_settings` に記録し、
  除去時は**ハッシュ一致するグループだけ**を消す。他人の hook グループには触らない。
- `hooks` 以外のキーは読み書きしない。
- **JSON が壊れていたら何もしない**（Err を返す）。修復も再生成もしない。
- バックアップ（`settings.json.drybench-backup`）→ 一時ファイル → `rename`。キー順は
  `serde_json` の `preserve_order` feature で保つ（`Cargo.toml` でこの feature を外さないこと）。
- hook が 1 件も無い操作では `settings.json` を**開かない**。

種別ごとの単位: skill = ディレクトリ（`SKILL.md` 必須）、agent = 単一ファイル
（`<name>.md`）、hook = ディレクトリ（`hook.json` 必須）。

### パス解決は `main.rs` と `cli.rs` だけ

**これはテスト可能性の契約であり、交渉の余地がない。** すべての I/O 関数は
`claude_dir: &Path` / `source_dir: &Path` を引数で受け取る。呼び出しスタックの深いところで
`dirs::home_dir()` を呼んだり `~/.claude` を組み立てたりしてはいけない。

この制約があるからテストは `std::env::temp_dir()` 配下のディレクトリを渡すだけで完結し、
実在の `~/.claude` に到達しえない。新しい I/O 関数を書くときはディレクトリを引数に取ること。

ソースディレクトリの解決順（`cli.rs`）: `--source` / `--target` → 環境変数
`DRYBENCH_SOURCE` / `DRYBENCH_TARGET` → cwd から上方探索した `drafts/` → `~/.drybench/source`
と `~/.claude`。**ソースが見つからなくてもエラー終了しない** — `~/.claude` の一覧画面から
始める（見つからずに終了すると、レイアウトの違う人は初回起動で詰む）。

### 生成物も例外ではない

drybench は中身を生成しない。`$EDITOR` / `claude` を子プロセスとして起動するだけで、
モデル API を直接呼ばない（**このプロセスは API キーを持たない**）。子プロセスが書いた
ものは、Claude が書いたものも含め、ただのソース内容として扱い、通常どおり全ゲートを通す。

TUI から子プロセスを起動する場合は、raw mode と alternate screen を抜けてから spawn し、
wait 後に入り直す。panic 時も端末を復帰させてから panic 表示に渡す（`install_panic_hook`）。

## 変更するときの制約

- **依存を足さない。** 単一バイナリ・最小の依存ツリーが方針。足す場合は PR の説明に理由を書く。
- **dev-dependencies は持たない。** テストは `std::env::temp_dir()` だけで完結させる
  （`Cargo.toml` にその旨のコメントがある）。
- **雛形は `templates/` にファイルとして置き、`include_str!` で埋め込む。** バイナリ内の
  文字列リテラルにしない — リポジトリを見た人がそのまま読んで直せることが理由。
- **UI 文字列の言語は未決。** ソースコメントと README は日本語、`docs/` と `CONTRIBUTING.md`
  は英語という混在状態。方針が決まるまで一括変換しない（[docs/roadmap.md](docs/roadmap.md)
  の Open questions）。
- **v0.1 のスコープは 4 項目に固定されている**（inspect/import、toggle、scaffold、`--source`）。
  それ以外を実装する前に [docs/roadmap.md](docs/roadmap.md) を確認すること。

## ブランチ運用

**`dev` が作業の起点。** 作業ブランチは `dev` から切り、`dev` へマージする。

```sh
git checkout dev && git pull
git checkout -b claude/<作業名>
# 作業・コミット
gh pr create --base dev
```

`main` へは、まとまった単位が完全に仕上がった時点でのみ `dev` からマージする。
`main` と `dev` のどちらでも直接作業しない。

`ci.yml` の push トリガは `branches: [main]` だが、pull_request トリガは全ブランチが
対象なので、`dev` 宛の PR でも CI は回る。
