# decune の開発

この文書は、コントリビューター向けの環境構築、検証、リリース成果物の作成コマンドをまとめます。利用手順は [usage.md](usage.md)、公開挙動は [specification.md](specification.md) を参照してください。

## ソースからのローカルインストール

ソースコードからのインストールは、公式のローカルインストール手順です。Git credential forwarding と port forwarding に必要なコンテナ側ツールを build し、ホスト側バイナリに埋め込んでから `decune` をインストールします。

Linux 用のコンテナ側ツールを build するため、以下の Rust target が必要です。

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

xtask のインストールコマンドを実行します。

```sh
cargo run --locked -p xtask -- install --locked
```

同等の Cargo 手順は以下です。

```sh
cargo run --locked -p xtask -- build-container-tools --out target/decune-source-install/container-tools --locked
DECUNE_CONTAINER_TOOLS_BUNDLE=required \
  DECUNE_CONTAINER_TOOLS_BUNDLE_DIR=target/decune-source-install/container-tools \
  cargo install --path . --locked --profile dist --bin decune
```

コンテナ側ツールの bundle を埋め込まない build は公式インストール手順ではありません。軽いローカル確認だけならインストールせず、通常の Cargo コマンドを使ってください。

## 標準検証

通常の変更では formatting と lint を確認します。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

対象を絞った test は package/module/test filter を指定して実行します。

```sh
cargo test -p decune <module_or_test_name>
```

## Docker を使う検証

Docker-backed test を含む全体検証には、Docker デーモンと Docker Compose v2 プラグインへ接続できる環境が必要です。

```sh
cargo run --locked -p xtask -- workspace-test
cargo run --locked -p xtask -- compose-integration
```

Compose integration test だけを実行する場合:

```sh
docker version
docker compose version
cargo run --locked -p xtask -- compose-integration
```

`compose_integration` の Docker-backed test は `#[ignore]` として定義します。通常の unit test では実行されず、`compose-integration` が Docker/Compose availability を確認したうえで ignored integration test を one test thread で実行します。

## リリース成果物

配布アーカイブを作成します。

```sh
cargo run --locked -p xtask -- dist \
  --target x86_64-unknown-linux-musl \
  --version 0.1.0 \
  --locked
```

checksum と release manifest を生成します。

```sh
cargo run --locked -p xtask -- checksum --dist-dir target/dist --version 0.1.0
cargo run --locked -p xtask -- release-manifest --dist-dir target/dist --version 0.1.0
```

生成済みのバイナリ成果物は Git リポジトリに commit しません。

## ドキュメント管理

- [README.md](../README.md) は概要、インストール、クイックスタート、主要リンクに絞る。
- 操作手順を中心にした利用者向け説明は [usage.md](usage.md) に置く。
- [specification.md](specification.md) は公開挙動、CLI の契約、設定スキーマ、セキュリティ境界の正本として保つ。
- コントリビューター向けの環境構築、検証、リリース成果物の作成コマンドはこの文書に置く。
- プロジェクト用語は [glossary.md](glossary.md) に揃える。

公開挙動、CLI option、設定 key、セキュリティ境界を変更した場合は、同じ変更で関連する利用者向けドキュメントも更新してください。
