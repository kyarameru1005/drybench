# drybench

**Claude Code 環境のための作業台** — `~/.claude` に何が入っているかを一覧し、スキル・
サブエージェント・フックを安全に着脱し、新しいものを作って試す。

<!-- TODO: demo GIF here (record with VHS so it can be re-recorded). See docs/demo.md -->

> [!NOTE]
> **v0.1 — 未リリース。** 現時点でこのリポジトリにあるのはスケルトンのみです。

---

## 課題

スキルやサブエージェント、フックを書き溜めていくと `~/.claude` は膨らみます。そこで
二つのことが面倒になります。

- **何が入っているか分からなくなる** — そして、それがまだ必要なのかも分からない。
- **フックはファイルを置いただけでは発火しない。** `settings.json` に、正しい形式で、
  手作業で登録する必要があります。

他人のスキルを試すときも同じ構図です。ファイルをコピーし、JSON を編集し、あとで元に
戻せることを祈る。

## drybench がすること

drybench はスキル生成ツールではありません。テンプレートよりも Claude Code 自身のほうが
うまくスキルを書きます。drybench が提供するのは、**それを載せる作業台**です。持ち込み、
試し、何も残さずに降ろす。

- **Inspect（一覧）** — `~/.claude` の中身を、管理下かどうかを問わず 1 画面に表示する。
- **Import（取り込み）** — すでにそこにあるものを、非破壊的に管理下へ入れる。
- **Toggle（着脱）** — インストールとアンインストール。`settings.json` のフック登録も含む。
- **Create（生成）** — 新しいスキルの雛形を作り、そのまま `$EDITOR` や `claude` に渡す。

## 安全性は設計から

ここが後付けではない部分です。

1. **マニフェストにないものには一切触れない。** 未管理の項目は常にロックされます。
2. **ハッシュが一致しないマニフェスト項目にも触れない。** コンフリクトの解消には明示的な
   許可が必要です。
3. **書き込み直前に状態を再確認する**（TOCTOU の隙間を作らない）。
4. **書き込み先は正規化パスで宛先ディレクトリ内にあることを検証し**、シンボリックリンクの
   ターゲットは拒否します。
5. **`settings.json` は、drybench が追加したグループだけをハッシュ照合で特定して削除します。**
   JSON が壊れている場合、drybench は何もしません。書き込みはバックアップを取り、アトミックに
   行います。
6. **破壊的な操作は必ず確認画面を経由します。**
7. **生成されたものは、Claude によるものも含め、レビューなしにインストールされません。**

影響範囲は、drybench 自身が置いたものに限定されます。

## インストール

<!-- TODO: fill in once release binaries exist (macOS arm64, Linux x86_64/aarch64) -->

```sh
cargo install --path .
```

## 使い方

```sh
drybench                      # ~/.claude を一覧する
drybench --source ./drafts    # 別のソースディレクトリを使う
drybench --help
```

<!-- TODO: keybindings table -->

## 対応状況

Claude Code `<version>` で動作確認。
<!-- TODO: pin the verified version. The settings.json hook format is the moving part. -->

## やらないこと

- モデル API を直接叩くこと。drybench は `claude` バイナリを呼び出すだけなので、API キーを
  保持しません。
- レジストリや共有ハブになること。それは公式のプラグインマーケットプレイスの役割です。
- Claude Code 以外のエージェントへの対応（少なくとも v1 までは）。
- GUI。

## コントリビュート

[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。モジュール構成は
[docs/architecture.md](docs/architecture.md)、ロードマップは [docs/roadmap.md](docs/roadmap.md)、
そして譲れないルールは [docs/design-principles.md](docs/design-principles.md) にあります。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。
