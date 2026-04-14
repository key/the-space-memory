# The Space Memory

![The Space Memory](docs/assets/cover.png)

[English](README.md)

## 概要

Rustで構築されたクロスワークスペース・ナレッジ検索エンジン。
複数ワークスペースのMarkdownドキュメントをインデックス化し、
FTS5全文検索とベクトルセマンティック検索（ruri-v3-30m, 256次元）のハイブリッド検索を提供する。

## コンセプト

- **ワークスペース横断検索** — 複数のリポジトリ（個人メモ、業務プロジェクト、テックノート等）をオーケストレーションリポジトリから一括検索
- **100ms未満のローカル検索** — インデックス作成も検索もローカルで完結し、ネットワーク遅延なしに100ms未満で応答
- **Claude Codeとの透過的連携** — hookがプロンプトを読み取り、ナレッジベースを検索し、関連するコンテキストを自動的にインジェクト

## 機能

- **ハイブリッド検索** — FTS5 + ベクトル検索をRRF（Reciprocal Rank Fusion）で統合
- **形態素解析** — lindera（IPADIC）による日本語トークナイズ
- **セマンティック検索** — ruri-v3-30mの埋め込みをcandleでローカル推論（ONNX Runtime不要）
- **エンティティグラフ** — 自動エンティティ抽出とリンク推論
- **同義語展開** — WordNet + ユーザー定義CSVによるクエリ展開
- **セッション取り込み** — Claude Codeのセッション記録を検索可能なナレッジとしてインデックス化
- **シングルバイナリ** — Python不要、外部ランタイム依存なし

## はじめに

### 動作プラットフォーム

| プラットフォーム | 状態 |
|---|---|
| Linux x86_64 | メインターゲット、CI テスト済み |
| Linux arm64 | サポート、CI ビルドチェック済み |
| macOS Apple Silicon | サポート |
| macOS x86_64 | サポート |

ファイル監視は inotify（Linux）/ FSEvents（macOS）を使用。

### セットアップ

```bash
# 1. ビルド
cargo build --release

# 2. ruri-v3-30m モデルのダウンロード
tsm setup

# 3. ドキュメントのルートディレクトリを設定
export TSM_INDEX_ROOT=~/my-notes

# 4. データベースの初期化（プロジェクトルートに default .tsmignore を
#    同時に配置する。既存ファイルがある場合は上書きしない）
tsm init

# 5. デーモンの起動（embedder + ファイル監視）
tsm start

# 6. ドキュメントのインデックス
tsm index

# 7. 検索
tsm search -q "クエリ" -k 5
```

### インデックス対象

tsmは `TSM_INDEX_ROOT` 配下の `.md` ファイルを再帰的にスキャンする。
典型的なディレクトリ構成：

```text
~/my-notes/              ← TSM_INDEX_ROOT
├── projects/
│   ├── project-a.md
│   └── project-b.md
├── research/
│   └── notes.md
└── journal/
    └── 2026-04.md
```

`TSM_INDEX_ROOT` 配下のすべてのMarkdownファイルが自動的にインデックスされる。
ファイル監視により、追加・変更・削除をリアルタイムに検知する。

### メンテナンス

```bash
# デーモン稼働中に再インデックス（非破壊、バックグラウンド）
tsm reindex all       # FTS + ベクター
tsm reindex fts       # FTS のみ（辞書変更後）
tsm reindex vectors   # ベクターのみ（モデル変更後）

# ゼロから再構築（破壊的、デーモン停止が必要）
tsm rebuild           # ドライラン（DB情報を表示）
tsm rebuild --apply   # DB削除して再構築
```

`tsm doctor` でシステムの状態とデーモンのステータスを確認できる。

## ドキュメント

- [コマンドリファレンス](docs/command-reference.md) — CLIコマンド、フラグ、使用例
- [アーキテクチャ](docs/architecture.md) — プロセス構成とコンポーネントの責務
- [データフロー](docs/data-flow.md) — インデックスと検索のフロー図
- [設定リファレンス](docs/configuration.md) — 環境変数と設定ファイルのリファレンス
- [ユーザー辞書](docs/user-dictionary.md) — カスタム辞書の管理
- [設計判断](decisions/) — ADR（アーキテクチャ決定記録）

## 背景

The Space Memoryは[sui-memory](https://zenn.dev/noprogllama/articles/7c24b2c2410213)に
インスパイアされた。sui-memoryはClaude Codeのセッション記録を検索可能なデータベースとして
インデックス化するアイデアを提示した。tsmはこのコンセプトをセッション記録から
ドキュメントリポジトリ全域に拡張し、ワークスペース横断のナレッジ検索を実現する。

### なぜ自作したのか

既存のツールにはそれぞれ決定的な欠点があった：

- **Notion / GitHub検索** — ネットワーク経由のため、リアルタイムのプロンプトインジェクションには速度が不足
- **grep** — シーケンシャルスキャンで、検索語間のセマンティックな相関がない
- **Obsidian** — Markdownエディタとしては優秀だが、AIエージェントとの連携には不向き

tsmはこれらのギャップを埋めるために構築された。ローカルファーストで100ms未満の
ハイブリッド検索エンジンであり、Claude Codeとhookを通じて透過的に連携する。
FTS5とベクトル検索の組み合わせは語彙のギャップを埋め
（例：「射撃」⇔「銃砲」のマッチング）、lindera/IPADICによる日本語トークナイズは
英語圏向けツールの流用ではなく自作した主な理由である。

### 名前の由来

命名はsui-memoryのパターン（prefix + memory）に倣い、複数リポジトリを統一的な
検索空間として扱うことから "space" を冠した。
カバービジュアルは『ハイドライド3』（サブタイトル：*The Space Memories*）のオマージュ。

## ライセンス

[MIT](LICENSE)
