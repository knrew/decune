# decune

`decune` は、VS Code に依存せず、Rust 製の単一 CLI から Dev Container を起動、接続、停止、削除するためのツールです。

Dev Containers Specification の image-based / Dockerfile-based / Docker Compose-based 構成を読み込み、Docker CLI と Docker Compose v2 CLI プラグイン経由で Docker コンテナまたは Compose プロジェクトを操作します。

## 主な機能

- `decune up` で development container を起動し、remote user のシェルに接続
- `decune rebuild` / `decune down` / `decune clean` による明示的なライフサイクル管理
- `.devcontainer/devcontainer.json`、`.devcontainer.json`、`.devcontainer/<name>/devcontainer.json` の検出
- image-based / Dockerfile-based / Docker Compose-based の Dev Container 構成を起動
- Dev Container Features、dotfiles、Git/GitHub 認証情報転送、ポートフォワーディング、Linux UID/GID 同期を適用
- global と project の decune TOML 設定を重ね合わせ
- GitHub Releases のビルド済みアーカイブ配布

## 対象範囲

v0.1 は Linux / macOS ホストと Docker CLI / Docker Compose v2 を対象にします。Docker Compose-based 構成では、`devcontainer.json` の `service` で指定した Compose サービスを primary service として扱い、シェル接続、ライフサイクルコマンド、Features、dotfiles、認証情報、automatic port forwarding を適用します。

v0.1 では以下を意図的に対象外にします。

- 旧 `docker-compose` v1 standalone binary の公式対応
- Kubernetes、Swarm stack、Docker Desktop UI、cloud provider 固有 orchestrator の直接サポート
- VS Code extension installation と `customizations.vscode` の適用
- GPG agent forwarding
- コンテナから任意のホストコマンドを実行する API
- Windows ホスト向け公式配布
- crates.io または `cargo install --git` による公式インストール

## 要件

- Linux または macOS ホスト
- Docker CLI `docker`
- Docker Compose v2 プラグイン
- Docker デーモンへ接続できる権限
- Git 認証情報転送を使う場合: ホスト側の `git`
- GitHub CLI token 転送を使う場合: ホスト側の `gh`

必要な Docker Compose の機能は [docs/specification.md](docs/specification.md#ホスト要件) を参照してください。

## インストール

公式の導入手順は GitHub Releases のビルド済みアーカイブです。インストールスクリプトはバージョンを明示指定し、選択したアーカイブを `SHA256SUMS` で検証します。

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.1.0/scripts/install.sh | sh -s -- --version 0.1.0 --dir "$HOME/.local/bin"
```

`$HOME/.local/bin` が `PATH` に含まれていない場合は、利用しているシェルの設定で追加してください。

手動アーカイブインストールとソースコードからのインストールは [docs/usage.md](docs/usage.md#インストール) と [docs/development.md](docs/development.md#ソースからのローカルインストール) を参照してください。

## クイックスタート

対象リポジトリに Dev Container 構成を用意します。

```jsonc
// .devcontainer/devcontainer.json
{
  "name": "example",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "remoteUser": "vscode",
  "features": {
    "ghcr.io/devcontainers/features/github-cli:1": {}
  },
  "forwardPorts": [5173],
  "postCreateCommand": "echo ready"
}
```

起動して接続します。

```sh
decune up
```

コンテナを再作成します。

```sh
decune rebuild --no-cache
```

decune が管理するコンテナまたは Compose プロジェクトを停止します。volume と状態は保持します。

```sh
decune down
```

decune が管理するリソースを削除します。

```sh
decune clean --force
```

Dockerfile-based / Docker Compose-based の例は [docs/usage.md](docs/usage.md#クイックスタート) を参照してください。

## コマンド

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

`WORKSPACE` の既定値はカレントディレクトリです。Git リポジトリ内ではリポジトリルートを workspace root として扱います。

- `decune up`: development container を作成または起動し、シェルに接続
- `decune rebuild`: development container または Compose プロジェクトを再作成
- `decune down`: decune が管理するリソースを停止し、volume、状態、image を保持
- `decune clean`: decune が管理するリソースを削除。`--images` で decune が生成した image も削除

実際にインストールされた CLI のリファレンスは `decune --help` または `decune <COMMAND> --help` で確認してください。詳しい利用手順は [docs/usage.md](docs/usage.md#コマンド) を参照してください。

## 注意点

- `forwardPorts`、decune `[[ports]]`、`decune up -p` は decune のポートフォワーディングであり、Docker の published port ではありません。
- `appPort` は image/Dockerfile モードの Docker published port です。
- Docker Compose-based 構成では Docker published port を Compose サービスの `ports` に書きます。`appPort`、`workspaceMount`、`runArgs` は Compose モードでは unsupported error です。
- `decune up` は Dockerfile instruction、Compose build、Feature `install.sh`、ライフサイクルコマンド、hook、シェル起動ファイルを実行し得ます。信頼していないリポジトリでは起動前に内容を確認してください。
- 認証情報転送は、ホストの Git 認証情報、SSH agent、GitHub token file への到達性をコンテナ内プロセスに与え得ます。信頼していないリポジトリでは無効化または read-only に制限してください。

## ドキュメント

- [docs/usage.md](docs/usage.md): 利用手順、利用例、インストール詳細、運用上の注意
- [docs/specification.md](docs/specification.md): v0.1 の公開挙動、設定スキーマ、セキュリティ境界
- [docs/development.md](docs/development.md): 開発環境の準備、ローカルインストール、検証、リリース成果物の作成コマンド
- [docs/release.md](docs/release.md): maintainer 向けのリリース手順
- [docs/glossary.md](docs/glossary.md): プロジェクト用語と表記基準

## ライセンス

MIT License. See [LICENSE](LICENSE).
