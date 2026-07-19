# decune

`decune` は、VS Code に依存せず、Rust 製の単一 CLI から Dev Container を起動、接続、停止、削除するためのツールです。

Dev Containers Specification の image-based / Dockerfile-based / Docker Compose-based 構成を読み込み、Docker CLI と Docker Compose v2 CLI プラグイン経由で Docker コンテナまたは Compose プロジェクトを操作します。

## 主な機能

- `decune up` で development container を起動し、remote user のシェルに接続
- `decune rebuild` / `decune down` / `decune remove` による明示的なライフサイクル管理
- `decune status` で decune が管理する workspace environment の summary/detail を確認
- `decune ports` で実行中の port forwarding と Docker published port の host 側の利用状況を確認
- attached `decune up` session 中は primary container 内の `decune status` / `decune ports` から同じ workspace を確認（[利用方法](docs/usage.md#container-内の-decune)）
- `.devcontainer/devcontainer.json`、`.devcontainer.json`、`.devcontainer/<name>/devcontainer.json` の検出
- image-based / Dockerfile-based / Docker Compose-based の Dev Container 構成を起動
- Compose clone isolation により、固定 published port・固定名・固定 IPv4 subnet・宣言済み endpoint を workspace ごとに分離
- Dev Container Features、dotfiles、Git/GitHub 認証情報転送、ポートフォワーディング、Linux UID/GID 同期を適用
- global と project の decune TOML 設定を重ね合わせ
- GitHub Releases のビルド済みアーカイブ配布

## 対象範囲

Linux / macOS ホストと Docker CLI / Docker Compose v2 を対象にします。Docker Compose-based 構成では、`devcontainer.json` の `service` で指定した Compose サービスを primary service として扱い、シェル接続、ライフサイクルコマンド、Features、dotfiles、認証情報、automatic port forwarding を適用します。

以下を意図的に対象外にします。

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

必要な Docker Compose の機能は [docs/specification.md](docs/specification.md#21-ホスト要件) を参照してください。

## インストール

公式の導入手順は GitHub Releases のビルド済みアーカイブです。インストールスクリプトはバージョンを明示指定し、選択したアーカイブを `SHA256SUMS` で検証します。

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.3.4/scripts/install.sh | sh -s -- --version 0.3.4 --dir "$HOME/.local/bin"
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

decune が管理するコンテナまたは Compose プロジェクトを停止します。volume、state、image は保持します。

```sh
decune down
```

decune が管理する Dev Container 環境を削除します。

```sh
decune remove --no-confirm
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
- `decune status`: decune が管理する workspace environment の実行状態、設定状態、health、port count、issue を read-only で表示
- `decune ports`: decune が管理している workspace について、現在有効な host 側 port の利用状況を read-only で表示。port forwarding と Docker published port を区別して確認
- `decune remove` / `decune rm`: decune が管理する Dev Container 環境を削除。`--all-workspaces` ですべての workspace を対象にし、`--images` で decune が生成した image も削除
- `decune clean`: stale な decune の生成データを確認・削除。既定では workspace cache/state/runtime だけを対象にし、`--include-feature-cache` で共有 Feature archive cache も対象に追加

実際にインストールされた CLI のリファレンスは `decune --help` または `decune <COMMAND> --help` で確認してください。詳しい利用手順は [docs/usage.md](docs/usage.md#コマンド) を参照してください。

## 注意点

- `forwardPorts`、decune `[[ports]]`、`decune up -p` は decune のポートフォワーディングであり、Docker の published port ではありません。
- `decune status` は JSON 出力や `--ports` / `--resources` option を持たず、`LAST_USED` は state の `last_used_at` だけから表示します。
- `decune ports` は decune が現在維持している port forwarding と Docker published port の両方を表示し、`TYPE` で `forwarded` / `published`、`STATE` で relocated Compose published port を区別します。workspace 横断では `decune ports --all`、JSON 出力では `decune ports --json` を使います。
- container 内の `decune status` / `decune ports` は primary container の current workspace に固定された read-only query で、active な attached `decune up` session がある間だけ利用できます。
- `appPort` は image/Dockerfile モードの Docker published port です。
- Docker Compose-based 構成では Docker published port を Compose サービスの `ports` に書きます。`appPort`、`workspaceMount`、`runArgs` は Compose モードでは unsupported error です。
- Compose automatic published port relocation policy は既定で無効です。`[compose.published_ports].automatic_relocation = true` または `decune up --automatic-published-port-relocation` / `decune rebuild --automatic-published-port-relocation` で、この実行の policy を有効化できます。`[[compose.published_ports.mappings]]` では、policy と独立に fixed TCP published port の host endpoint を明示できます。実際に host port または host IP を変更する場合は Docker Compose v2.24.4 以上が必要です。
- Compose clone isolation は既定で無効です。`[compose.clone_isolation].enabled = true` にすると、明示的な `container_name` と non-external な top-level resource の固定 `name` を workspace 固有名へ書き換え、複数 clone の同時起動時の名前衝突を避けます。固定名 volume のデータも clone ごとに分離されます。固定 IPv4 IPAM subnet も分離する場合は、`[compose.clone_isolation.networks].relocation = true` と `subnet_pool` を設定します。固定 gateway / subnet を environment から参照する場合は `[[compose.clone_isolation.endpoints]]` で relocation 後の値への書き換えを宣言できます。固定 IPv4 subnet がある構成では Docker Compose v2.24.4 以上が必要です。
- `decune up` は Dockerfile instruction、Compose build、Feature `install.sh`、ライフサイクルコマンド、hook、シェル起動ファイルを実行し得ます。信頼していないリポジトリでは起動前に内容を確認してください。
- 認証情報転送は、ホストの Git 認証情報、SSH agent、GitHub token file への到達性をコンテナ内プロセスに与え得ます。信頼していないリポジトリでは無効化または read-only に制限してください。

## ドキュメント

- [docs/usage.md](docs/usage.md): 利用手順、利用例、インストール詳細、運用上の注意
- [docs/specification.md](docs/specification.md): 公開挙動、設定スキーマ、セキュリティ境界
- [docs/development.md](docs/development.md): 開発環境の準備、ローカルインストール、検証、リリース成果物の作成コマンド
- [docs/release.md](docs/release.md): maintainer 向けのリリース手順
- [docs/glossary.md](docs/glossary.md): プロジェクト用語と表記基準

## ライセンス

MIT License. See [LICENSE](LICENSE).
