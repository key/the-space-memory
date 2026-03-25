# The Space Memory

## DevContainer 方針
- ベースイメージは `mcr.microsoft.com/devcontainers/base:ubuntu`
- ツール管理は mise (`.mise.toml`) に集約する。devcontainer features は最小限に
- Claude Code は native installer で導入（npm/features 不可）
- `.env` にシークレットを格納。git 管理外

## MCP
- Serena MCP は Docker 経由 (`ghcr.io/oraios/serena:latest`)。設定は `.mcp.json`

## Git
- co-author 属性は無効（`~/.claude/settings.json` の `attribution` で制御）

## ライセンス適合性確認

ライブラリ・依存関係を追加する際は、プロジェクトの LICENSE ファイルを確認し、選択されたライセンスとの互換性を検証すること。

### 互換性ガイドライン

| プロジェクト | 利用可能な依存ライセンス | 利用不可 |
|---|---|---|
| MIT | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL, LGPL, AGPL, MPL（条件付き）|
| Apache-2.0 | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL-2.0, AGPL |
| GPL-3.0 | MIT, BSD, ISC, Apache-2.0, LGPL, GPL-3.0, Unlicense | AGPL-3.0（条件付き）|
| LGPL-3.0 | MIT, BSD, ISC, Apache-2.0, LGPL-3.0, Unlicense | - |
| AGPL-3.0 | MIT, BSD, ISC, Apache-2.0, LGPL, GPL, AGPL-3.0, Unlicense | - |
| MPL-2.0 | MIT, BSD, ISC, Apache-2.0, MPL-2.0, Unlicense | GPL（ファイル単位で分離可）|
| BSL-1.0 | MIT, BSD, ISC, BSL-1.0, Unlicense | GPL, LGPL, AGPL |
| Unlicense | MIT, BSD, ISC, Unlicense | GPL, LGPL, AGPL, MPL |
| Proprietary | MIT, BSD, ISC, Apache-2.0, Unlicense | GPL, LGPL, AGPL, MPL |

- 互換性が不明な場合はユーザーに確認を求める
- devDependencies（テスト・ビルドツール等）はライセンス制約の対象外
