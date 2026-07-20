# decune

`decune` は、VS Code に依存せず、Rust 製の単一 CLI から Dev Container を起動、接続、停止、削除するためのツールです。

Dev Containers Specification の image-based / Dockerfile-based / Docker Compose-based configuration を読み込み、Docker CLI と Docker Compose v2 CLI プラグイン経由で Docker コンテナまたは Compose プロジェクトを操作します。

## 主な機能

- image-based / Dockerfile-based / Docker Compose-based configuration の検出と起動
- `decune up` だけでビルド、起動、リモートユーザーのシェル接続までを実行
- `rebuild` / `down` / `status` / `ports` / `remove` / `clean` による明示的な lifecycle 管理と状態確認
- Dev Container Features、dotfiles、Git/GitHub credential forwarding、Linux UID/GID 同期の適用
- decune の port forwarding と Docker published port の管理・一覧表示
- global と project の 2 層の decune config を `devcontainer.json` へ重ね合わせ
- 同じ Docker Compose-based リポジトリの複数クローンの同時利用(オプトインの Compose clone isolation)

## 対象範囲

Linux / macOS ホストと Docker CLI / Docker Compose v2 プラグインを対象にします(旧 `docker-compose` v1 の単体バイナリは対象外)。Docker Compose-based configuration では、`service` で指定した Compose サービスを主対象にシェル接続と lifecycle 管理を適用し、Compose ファイルの解釈と実行時設定は Docker Compose に委譲します。

Kubernetes などのオーケストレーターと Docker Desktop UI の直接サポート、VS Code 拡張機能のインストールと `customizations.vscode` の適用、GPG agent forwarding、コンテナから任意のホストコマンドを実行する API、Windows ホスト向け公式配布、crates.io 経由の公式インストールは対象外です。対象範囲と対象外の正確な一覧は [docs/specification.md](docs/specification.md#1-スコープ) を参照してください。

## 要件

- Linux または macOS ホスト
- Docker CLI `docker`
- Docker Compose v2 プラグイン
- Docker デーモンへ接続できる権限
- Git credential forwarding を使う場合: ホスト側の `git`
- GitHub CLI token forwarding を使う場合: ホスト側の `gh`

必要な Docker Compose の機能は [docs/specification.md](docs/specification.md#21-ホスト要件) を参照してください。

## インストール

公式の導入手順は GitHub Releases のビルド済みアーカイブです。インストールスクリプトはバージョンを明示指定し、ダウンロードしたアーカイブを `SHA256SUMS` で検証します。

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://raw.githubusercontent.com/knrew/decune/v0.3.4/scripts/install.sh | sh -s -- --version 0.3.4 --dir "$HOME/.local/bin"
```

`$HOME/.local/bin` が `PATH` に含まれていない場合は、利用しているシェルの設定で追加してください。

手動アーカイブインストールとアップグレード時の注意は [docs/usage.md](docs/usage.md#インストール)、ソースからのインストールは [docs/development.md](docs/development.md#ソースからのローカルインストール) を参照してください。

## クイックスタート

対象リポジトリに Dev Container configuration を用意します。

```jsonc
// .devcontainer/devcontainer.json
{
  "name": "example",
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "remoteUser": "vscode"
}
```

起動してリモートユーザーのシェルに接続します。

```sh
decune up
```

作業を終えたら停止します。ボリューム、状態、イメージは保持され、次の `decune up` で再利用されます。

```sh
decune down
```

Dockerfile-based / Docker Compose-based の例は [docs/usage.md](docs/usage.md#クイックスタート) を参照してください。

## コマンド

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

`WORKSPACE` の既定値はカレントディレクトリです。Git リポジトリ内ではリポジトリルートを workspace root として扱います。

- `decune up`: 開発コンテナを作成または起動し、シェルに接続
- `decune rebuild`: 開発コンテナまたは Compose プロジェクトを再作成
- `decune down`: decune が管理するリソースを停止(ボリューム、状態、イメージは保持)
- `decune status`: decune が管理するワークスペース環境の状態を read-only で表示
- `decune ports`: 現在有効なホスト側ポートの利用状況を read-only で表示
- `decune remove` / `decune rm`: decune が管理する Dev Container 環境を削除
- `decune clean`: stale な decune-managed data を確認・削除

使い方と例は [docs/usage.md](docs/usage.md#コマンド)、各コマンドの正確な契約と全オプションは [docs/specification.md](docs/specification.md#3-cli) を参照してください。

## セキュリティ上の注意

- `decune up` は、ビルド、Feature のインストールスクリプト、lifecycle command、decune hook などの任意コードを実行し得ます。信頼していないリポジトリでは、起動前に内容を確認してください。
- credential forwarding は、ホストの Git 認証情報、SSH agent、GitHub トークンファイルへの到達性をコンテナ内プロセスに与え得ます。信頼していないリポジトリでは、無効化するか read-only に制限してください。

確認ポイントと推奨設定は [docs/usage.md](docs/usage.md#安全な使い方)、セキュリティ境界の定義は [docs/specification.md](docs/specification.md#12-セキュリティ境界) を参照してください。

## ドキュメント

- [docs/usage.md](docs/usage.md): インストール、クイックスタート、日常操作の基本ガイド
- [docs/configuration.md](docs/configuration.md): decune config の使い方と挙動説明のガイド
- [docs/ports.md](docs/ports.md): port forwarding と published port の利用ガイド
- [docs/clone-isolation.md](docs/clone-isolation.md): 複数クローン同時利用(Compose clone isolation)のガイド
- [docs/specification.md](docs/specification.md): 公開挙動、CLI 契約、設定スキーマ、セキュリティ境界、diagnostic code 定義の正本
- [docs/internals.md](docs/internals.md): 内部実装の説明(非規範の内部設計ノート)
- [docs/development.md](docs/development.md): コントリビューター向けの環境構築、検証、ドキュメント執筆規約
- [docs/release.md](docs/release.md): メンテナー向けのリリース手順書
- [docs/glossary.md](docs/glossary.md): 用語の定義

## ライセンス

MIT License. See [LICENSE](LICENSE).
