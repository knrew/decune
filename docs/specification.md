# decune 仕様

この文書は、`decune` の公開挙動、CLI の契約、設定スキーマ、Docker/Compose 連携、状態とリソース、セキュリティ境界の正本である。外部から観測・依存できる契約と互換性の約束を定義する。利用手順と操作例は [usage.md](usage.md)、開発・検証手順は [development.md](development.md)、用語は [glossary.md](glossary.md) を参照する。

## 1. スコープ

### 1.1 目的

`decune` は、Dev Containers Specification の Dev Container を Rust 製の単一 CLI から起動、接続、停止、削除するためのツールである。VS Code や Node.js ベースの Dev Container CLI には依存しない。

`decune` は Dev Container の次の 3 構成を正式対象にする。

1. image-based: `image`
2. Dockerfile-based: `build.dockerfile`
3. Docker Compose-based: `dockerComposeFile` + `service`

global/project の decune config を Dev Container configuration に重ねる。VS Code Dev Containers が暗黙に提供する Git/GitHub 認証、dotfiles、port forwarding、UID/GID 同期も decune の責務として明示的に扱う。

### 1.2 対応する挙動

- Rust 製単一バイナリの CLI。
- Docker の image / container / exec / copy / inspect 操作を `docker` CLI アダプター経由で行う。
- Docker Compose 操作を `docker compose` v2 CLI アダプター経由で行う。
- Docker Engine API を Rust 型で直接操作するクライアント crate(`bollard` など)は使わない。外部操作は CLI アダプターに限定する。
- Dev Container の image-based / Dockerfile-based / Docker Compose-based configuration。
- JSONC としての `devcontainer.json` 読み込み。
- TOML による global/project 設定。
- Dev Container Features の OCI レジストリからの取得、digest lock、local Feature、インストール、メタデータのマージ。
- Docker Compose モードでは、Feature、dotfiles、認証情報、lifecycle、リモートシェル、port forwarding を primary service に適用する。
- Git HTTPS credential helper、SSH agent、GitHub CLI token forwarding。
- manual port forwarding と automatic port forwarding。
- Linux ホストでの `updateRemoteUserUID` による UID/GID 同期。
- lifecycle command と decune hook。
- `up`、`rebuild`、`down`、`status`、`ports`、`remove` / `rm`、`clean` コマンド。
- GitHub Releases のビルド済みアーカイブによる公式配布。

### 1.3 Docker Compose サポートの定義

この文書における「Docker Compose 完全サポート」とは、Dev Containers Specification が定義する Docker Compose-based configuration を、image-based / Dockerfile-based configuration と同じ decune 機能群で扱えることを指す。

具体的には以下を満たす。

- `dockerComposeFile` は文字列と文字列の配列の両方を受け付け、配列順を保持して Compose に渡す。
- `service` を primary service として扱い、リモートシェル、lifecycle、Feature、dotfiles、認証情報、UID/GID 同期、automatic forwarding の既定対象にする。
- `runServices` を受け付ける。未指定時は Compose プロジェクトの全サービスを起動対象にする。指定時も primary service は必ず起動対象に含める。
- Compose YAML のマージ、include、profiles、アンカー、拡張フィールド、環境変数の展開、相対パスの解決、ビルドのセマンティクス、ネットワーク / ボリューム / config / secret のセマンティクスは decune が再実装せず、Docker Compose v2 CLI に委譲する。
- decune は `docker compose config --format json` で正規化済みの Compose モデルを取得し、検証、ハッシュ、対象のサービス / コンテナの解決に使う。
- `forwardPorts` の `"service:port"` 形式を Compose のサービス名として扱い、primary service 以外の明示的な転送に対応する。
- Compose が作成したリソースの lifecycle は Compose プロジェクト単位で管理する。decune は Compose プロジェクト名を明示指定し、他のプロジェクトを拾わない。

「完全サポート」は、Compose Specification の全属性を decune が自前で解釈することを意味しない。Compose 仕様の追随は Docker Compose CLI に委譲し、decune は Dev Container と decune 固有機能を Compose プロジェクトに安全に重ねる責務を持つ。

### 1.4 対象外

- 旧 `docker-compose` v1 の単体バイナリの公式対応。`docker compose` v2 プラグインを必須にする。
- Kubernetes、Swarm スタック、Docker Desktop UI、クラウドプロバイダー固有のオーケストレーターの直接サポート。
- Compose ファイルを `dockerComposeFile` から git URL / OCI artifact / stdin で参照する構成。Dev Container の `dockerComposeFile` は `devcontainer.json` からのローカルパスとして扱う。
- primary service のレプリカ / scale が 2 以上の構成。リモートシェルと lifecycle の対象コンテナが一意に決まらないためエラーにする。
- VS Code 拡張機能のインストールや `customizations.vscode` の適用。
- GPG agent forwarding。
- コンテナから任意のホストコマンドを実行する API。
- Windows ホスト向け公式配布。
- crates.io または `cargo install --git` による公式インストール。

## 2. ホスト要件と互換性

### 2.1 ホスト要件

- Linux または macOS ホスト。
- Docker CLI `docker`。
- Docker Compose v2 プラグイン。`docker compose version` が成功し、以下の機能を使えること。
  - `docker compose config --format json`
  - `docker compose ps --format json`
  - `docker compose build --with-dependencies`
  - `docker compose pull --policy always`
  - `docker compose pull --ignore-buildable`
  - `docker compose pull --include-deps`
  - `docker compose up --force-recreate`
  - `docker compose up --remove-orphans`
- Docker デーモンへ接続できる権限。
- Git 認証連携を使う場合: ホスト側の `git`、必要に応じて `SSH_AUTH_SOCK`。
- GitHub CLI 連携を使う場合: ホスト側の `gh` と `gh auth token` が成功する状態。

### 2.2 Docker Compose v2.24.4 が必要になる条件

decune は decune-generated Compose override で Compose の `!override` タグを使う場合に Docker Compose v2.24.4 以上を要求する。バージョンを判定できない、または古い Compose は、Docker リソースを変更する前にエラーにする。`!override` タグを使うのは次の場合である。

- Compose published port mapping / automatic relocation が実際にホスト側ポートまたはホスト IP を変更し、サービスの `ports` を置換する場合(8.8 節)。
- clone isolation の network relocation が固定 IPv4 サブネットを検出し、`networks.<key>.ipam.config` を置換する場合(8.9 節)。
- clone isolation の name rewrite が `volumes_from` / `external_links` のリスト置換を必要とする場合(8.9 節)。`network_mode` / `ipc` / `pid` のスカラー書き換えだけならこの条件を課さない。

上記に該当しない実行では、この追加のバージョン条件を課さない。

### 2.3 環境変数の継承

Docker のエンドポイント、コンテキスト、credential helper、BuildKit、Compose の profiles などは Docker CLI / Docker Compose CLI の標準挙動を継承する。decune は `DOCKER_HOST`、`DOCKER_CONTEXT`、`DOCKER_CONFIG`、`COMPOSE_PROFILES` などのホスト環境変数を原則としてそのまま子プロセスに渡す。

### 2.4 Podman 互換性

- Docker CLI は Docker デーモンと同じホスト / リモートのコンテキストを指す。
- Compose CLI は Docker CLI と同じ `DOCKER_HOST` / `DOCKER_CONTEXT` / `DOCKER_CONFIG` を継承する。
- Podman 互換のエンドポイントは、Docker CLI / Compose CLI が透過的に扱える範囲でのみ対象にする。Podman Compose 固有の挙動は公式対象外。

## 3. CLI

### 3.1 共通形式

```text
decune <COMMAND> [OPTIONS] [WORKSPACE]
```

- `WORKSPACE` の既定値はカレントディレクトリ。
- `WORKSPACE` は実在するディレクトリでなければならない。
- Git リポジトリ内ではリポジトリルートを workspace root とする。Git リポジトリでなければ指定ディレクトリを workspace root とする。
- `devcontainer.json` を必須とする。decune config はオーバーレイであり、ベースイメージ / ビルド / Compose 定義の置き換えには使わない。
- CLI の出力、ログ、エラーメッセージは英語にする。
- 設定変更が既存のコンテナ / プロジェクトに反映できない場合、`up` は暗黙の再作成を行わず、`Run decune rebuild` を促して終了する。

### 3.2 `up`

```text
decune up [OPTIONS] [WORKSPACE]
```

役割:

- 開発コンテナを作成または起動し、リモートユーザーのシェルに接続する。
- image/Dockerfile モードでは単一のコンテナを作成または起動する。
- Compose モードでは Compose プロジェクトを作成または起動し、primary service のコンテナのシェルに接続する。
- 既に起動済みで reuse hash が一致する場合、作成処理をスキップし、シェルへの接続のみ行う。
- decune host daemon、credential forwarding、port forwarding は `up` のプロセスが生きている間だけ動作する。

主なオプション:

- `--config <PATH>`: `devcontainer.json` を明示する。相対パスは workspace root 相対。
- `--detach`: シェルに接続せず起動だけ行う。
- `--rebuild`: 既存のコンテナ / プロジェクトを破棄または再作成する。decune が管理するボリュームは保持する。
- `--no-cache`: Dockerfile のビルド、Compose サービスのビルド、Feature レイヤーのビルドでキャッシュを使わない。
- `--pull`: ベースイメージまたは Compose サービスのイメージを pull してからビルド / 作成する。Compose モードでは reuse hash が一致する実行中のコンテナでも再利用の高速経路に入らず、pull したイメージを反映するため `docker compose up -d --force-recreate` まで進む。
- `--no-global-config`: global decune config を適用しない。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `--automatic-published-port-relocation`: Compose automatic published port relocation のポリシーをこの実行で有効化する。
- `--no-automatic-published-port-relocation`: Compose automatic published port relocation のポリシーをこの実行で無効化する。
- `-p, --port <SPEC>`: manual forwarding。複数指定可。

`-p` / `--port <SPEC>` は次の 4 形式を受け付ける。

| 形式 | 例 | 意味 |
| --- | --- | --- |
| `container` | `3000`、`3000/tcp` | ホスト IP は既定 `127.0.0.1`。ホスト側ポートはコンテナのポートと同じ番号を試し、占有済みなら空きポートを探索する |
| `host:container` | `8080:3000` | ホスト側ポートを明示する |
| `host_ip:container` | `127.0.0.1:3000`、`[::1]:3000` | ホスト IP を明示し、ホスト側ポートはコンテナのポートと同じ番号から探索する |
| `host_ip:host:container` | `127.0.0.1:8080:3000`、`[::1]:8080:3000` | ホスト IP とホスト側ポートを明示する |

- ホスト IP の `localhost` は `127.0.0.1` に正規化する。
- プロトコルサフィックスなしは TCP、`/tcp` は許可、`/udp` は未対応のエラーとする。
- Compose モードで primary service 以外を対象にしたい場合は devcontainer の `forwardPorts` の `"service:port"` を使う。

automatic published port relocation のポリシーは、後続の Compose automatic published port relocation 処理(8.8 節)が参照する設定である。既定は無効である。`--no-auto-forward` は automatic port forwarding だけを無効化し、このポリシーは変更しない。

`--detach` では `up` のプロセス終了時に decune host daemon も停止するため、manual/automatic forwarding と Git HTTPS の `host-helper` は維持されない。detached なコンテナで外部公開が必要なポートは、image/Dockerfile モードでは `appPort`、Compose モードでは Compose ファイルの `ports` を使う。`--detach` と CLI `-p` / `--port` の併用はエラーとする。設定由来の `forwardPorts` / `[[ports]]` は警告を出して無視する。

### 3.3 `rebuild`

```text
decune rebuild [OPTIONS] [WORKSPACE]
```

`up --rebuild` と同等の明示的なコマンドである。既存のコンテナ / プロジェクトを停止・削除するか force recreate し、再度ビルド / 作成 / 起動する。decune が管理するボリュームは保持する。

主なオプション:

- `--config <PATH>`: `devcontainer.json` を明示する。相対パスは workspace root 相対。
- `--detach`
- `--no-cache`
- `--pull`
- `--update-features`: Feature lock よりレジストリ / タグの再解決を優先する。
- `--no-global-config`: global decune config を適用しない。
- `--no-auto-forward`: automatic port forwarding を無効化する。
- `--automatic-published-port-relocation`
- `--no-automatic-published-port-relocation`
- `-p, --port <SPEC>`

Compose モードでは、`docker compose build` と `docker compose up -d --force-recreate` を使う。`--no-cache` は Compose サービスのビルドと Feature レイヤーのビルドの両方に適用する。`--pull` は Compose サービスのビルド / pull に適用するが、decune が生成したローカルイメージを親にする Feature / UID/GID 同期 / entrypoint shim のレイヤービルドには適用しない。

### 3.4 `down`

```text
decune down [--timeout <SECONDS>] [WORKSPACE]
```

- `--timeout <SECONDS>`: 正常停止のタイムアウト。既定値は 10 秒。
- image/Dockerfile モード: decune が管理するコンテナを停止する。ボリューム、状態、イメージは削除しない。
- Compose モード: decune が管理する Compose プロジェクトを停止する。ボリューム、状態、イメージは削除しない。`runServices` 指定時も、Compose が `depends_on` 等で起動した依存サービスを残さないようプロジェクト全体を停止対象にする。
- Compose モードで現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、またはサービス名の変更等で既存のリソースと一致しない場合も、状態または Docker ラベルから decune が管理する Compose プロジェクトを特定して停止する。
- 現在の設定が Compose モードでも、同じワークスペースに過去の image/Dockerfile モード由来で decune が管理するコンテナが残っている場合は停止する。

明示的な `decune down` は `shutdownAction` に関係なく停止を行う。

### 3.5 `status`

```text
decune status [WORKSPACE]
```

役割:

- `WORKSPACE` なしでは、状態ファイルと decune が付けた Docker ラベルから見つかるワークスペース環境の summary を表示する。
- `WORKSPACE` 指定時は、そのワークスペースの detail を表示する。ワークスペースは通常のワークスペース解決と同じく Git リポジトリのルートを workspace root とする。
- `status` は read-only のコマンドとする。状態、ランタイムファイル、Docker リソースを修復、削除、更新しない。`last_used_at` も更新しない。

summary:

- 対象は `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` の有効な状態ファイル、および `decune.managed=true` と有効な `decune.workspace_id` ラベルを持つ Docker のコンテナ / ボリュームである。
- ランタイムディレクトリや port status ディレクトリだけが残っているワークスペースは summary の対象に含めない。
- 対象が 0 件の場合も成功とし、`No decune-managed workspace environments found` を表示する。
- 1 件以上の場合は集計行と表を表示する。表の列は `ID WORKSPACE RUNTIME CONFIG HEALTH FWD/PUB ISSUES LAST_USED` とする。
- 並び順は表示用ワークスペースパスの辞書順とし、同順位は workspace id で決める。ワークスペースパスが不明なエントリは末尾に置く。
- `LAST_USED` は状態の `last_used_at` だけから表示する。`created_at` や `last_started_at` へフォールバックしない。値がない、不正、未来の時刻の場合は `-` とする。
- `FWD/PUB` は現在有効な転送ポート数と Docker published port の数を `<forwarded>/<published>` 形式で表示する。

detail:

- `WORKSPACE` 指定時は devcontainer のメタデータを必須とする。メタデータが見つからない、または複数候補がある場合はエラーにする。
- メタデータがあり、状態 / Docker evidence がない場合は `not-created` として成功し、`Run decune up to create the environment.` を対処として表示する。
- detail はヘッダー (`Workspace`, `ID`, `Mode`) と、`Summary`、`Config`、問題がある場合の `Issues`、Compose モードの `Services`、`Runtime`、`Ports`、`Resources`、未完了の lifecycle がある場合の `Lifecycle`、必要な場合の `Action` を表示する。`Issues` は `code [severity]: message`、`Action` は対処を持つ全問題を `code: action` 形式で表示する。
- lifecycle が正常完了している場合は、lifecycle の各段階の詳細を表示しない。
- `Ports` 節は `decune ports` の単一ワークスペースの表と同じ形式を使う。active なポートがない場合は `No active ports for this workspace` を表示する。
- 現在の reuse hash は、ワークスペースパスと設定が読める場合に read-only で計算し、状態または Docker ラベル由来の reuse hash と比較して `current` / `needs-rebuild` を判定する。`[[mounts]].create = "directory"` および Dev Container bind mount の `bind-create-src` は、存在しないホスト側パスを作成せず、既存の祖先ディレクトリを正規化して存在しない末尾を合成したパスでハッシュを計算する。計算できない場合は `unreadable` または `unknown` の問題として表示し、状態、ホスト側パス、Docker リソースは変更しない。
- 出力には秘密情報の値、生のラベル、生の Compose モデル、コンテナの環境変数、ビルド引数、マウント元の過剰な列挙、reuse hash の値を出してはならない。
- JSON 出力、`--ports`、`--resources` などの status のオプションは提供しない。

### 3.6 `ports`

```text
decune ports [--json] [WORKSPACE]
decune ports [--json] --all
```

役割:

- decune が管理しているワークスペースについて、現在有効なホスト側ポートの利用状況を表示する。
- 表示対象は、実行中の attached `up` のプロセスが維持している port forwarding と、Docker が現在 publish しているポートのバインディングである。
- port forwarding は `forwardPorts`、decune `[[ports]]`、CLI `-p`、automatic forwarding を含む。
- Docker published port は image/Dockerfile モードの `appPort` と Compose サービスの `ports` を含む。
- `--all` は decune が管理しているワークスペースを横断して表示する。`--all` と `WORKSPACE` は同時指定できない。
- `ports` は read-only のコマンドとする。状態、ランタイムファイル、Docker リソースを修復、削除、更新しない。`last_used_at` も更新しない。
- 現在有効なホスト側ポートがない場合も成功とし、通常出力は単一ワークスペースで `No active ports for this workspace`、`--all` で `No active ports`、JSON 出力は `[]` とする。

通常出力:

- `WORKSPACE`: `--all` の場合だけ表示するワークスペースパス。不明なら `<unknown>`。
- `ID`: `--all` の場合だけ表示する workspace id。
- `LOCAL`: forwarding では実際に待ち受けているホスト側エンドポイント。Docker published port では現在有効なホスト側エンドポイントを表示する。Compose published port relocation のメタデータがある published のエントリでは、planned endpoint を通常出力向けの要約として表示する。ホスト IP が省略された planned endpoint は `*:<port>` と表示し、Docker inspect で得た実際のバインディングは JSON の `actual_bindings` で確認できる。
- `TYPE`: `forwarded` または `published`。
- `TARGET`: 転送先、または Docker published port のコンテナ側エンドポイント。primary container は `container:<port>/<protocol>`、Compose サービスは `<service>:<port>/<protocol>`。
- `SOURCE`: forwarding は `configured` または `auto`、published port は `appPort` または `compose`。
- `REQUESTED`: port forwarding が requested endpoint から別のエンドポイントへフォールバックした場合、または Compose published port mapping/relocation により requested endpoint と planned endpoint が異なる場合に、requested endpoint を表示する。それ以外は `-`。Compose published port でホスト IP が省略されている場合は `*:<port>` と表示し、明示的な `0.0.0.0` と区別する。
- `STATE`: Compose published port mapping/relocation により requested endpoint と planned endpoint が異なる場合は `relocated`。ホスト IP だけが異なる場合も含む。それ以外は `-`。
- `LABEL`: ポートのラベル。未指定なら `-`。

`--json` は通常出力の表を再構成できる JSON の配列を stdout に出力する。

- 各エントリは `host_ip`、`host_port`、`type`、`service`、`container_port`、`protocol`、`source`、`label` を持つ。
- `--all` では `workspace` と `workspace_id` も含める。
- requested endpoint と実際のエンドポイントが異なる forwarding のエントリでは、`requested_host_ip` と `requested_host_port` を含める。
- decune が Compose published port relocation のメタデータを保存している published のエントリでは、`target`、`requested`、`planned`、`actual_bindings`、`relocated`、`port_entry_index` を含める。`target` は `port` と `protocol`、`requested` / `planned` は `host_ip` と `host_port` を持つ。`actual_bindings` は Docker inspect から得た現在の actual binding の配列で、各要素は `host_ip` と `host_port` を持つ。
- 同じ published のエントリでは既存の JSON 利用側との互換のため、`requested_host_ip_kind`、`requested_host_port`、`planned_host_ip_kind`、`planned_host_port`、`relocated` も含める。
- 同じメタデータのエンドポイントでホスト IP が明示されている場合は、`requested_host_ip` または `planned_host_ip` も含める。

`requested.host_ip` / `planned.host_ip` は、ホスト IP が省略された場合は `null`、明示された場合は文字列とする。`*_host_ip_kind` は `omitted` または `explicit` である。`omitted` は Compose ファイル上でホスト IP が省略されたことを表し、この場合、対応するフラットな `*_host_ip` は省略する。published port の requested endpoint は、Docker が実際に publish しているバインディングだけからは復元しない。

### 3.7 `remove` / `rm`

```text
decune remove [--no-confirm] [--images] [WORKSPACE]
decune rm     [--no-confirm] [--images] [WORKSPACE]
decune remove [--no-confirm] [--images] --all-workspaces
decune rm     [--no-confirm] [--images] --all-workspaces
```

- image/Dockerfile モード: decune が管理するコンテナ、decune が管理するボリューム、状態 / ランタイムデータを削除する。`--images` 指定時だけ生成イメージを削除する。
- Compose モード: decune が管理する Compose プロジェクトを `docker compose down --volumes --remove-orphans` 相当で削除し、状態 / ランタイムデータを削除する。external なボリューム / ネットワークは Compose の標準挙動に従い削除しない。`--images` 指定時だけ decune が生成したイメージを削除する。利用者が Compose ファイルで指定したイメージを `--rmi all` で削除してはならない。
- Compose モードで現在の `devcontainer.json` / `dockerComposeFile` が削除、移動、またはサービス名の変更等で既存のリソースと一致しない場合も、状態または Docker ラベルから decune が管理する Compose プロジェクトを特定して削除する。
- 現在の設定が Compose モードでも、同じワークスペースに過去の image/Dockerfile モード由来で decune が管理するコンテナやボリュームが残っている場合は削除する。
- `--all-workspaces` は、すべてのワークスペースで decune が管理する Dev Container 環境を削除する。`WORKSPACE` とは排他である。
- `--all-workspaces` の探索対象は `decune.managed=true` と有効な `decune.workspace_id` を持つ Docker のコンテナ / ボリューム、および `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` の有効な状態ファイルとする。有効な workspace id は、Docker ラベル由来・状態ディレクトリ名由来のいずれも 12 桁の小文字 16 進 (`[0-9a-f]{12}`) に完全一致する値だけである。無効なラベル値や状態ディレクトリ名は対象外として無視し、状態 / ランタイムパスの組み立てに使わない。読み込めない状態ファイルは警告を出して無視する。
- `--all-workspaces` で Compose プロジェクトを削除する場合は、decune が管理するコンテナの `com.docker.compose.project` ラベルまたは decune の状態の `compose_project_name` から所有を確認できるプロジェクトだけを対象にする。プロジェクト名の前方一致だけでは、利用者が管理する Compose プロジェクトを対象にしない。
- `--all-workspaces` は対象ワークスペースの状態 / ランタイムデータを削除する。ワークスペースのキャッシュと共有 Feature archive cache は削除しない。

`rm` は `remove` の別名とする。`--no-confirm` は確認プロンプトだけを省略し、decune が管理するリソースだけを対象にする安全境界や使用中のリソースの保護は迂回しない。

削除対象がある状態で TTY でない環境から `remove` を `--no-confirm` なしで実行した場合は、確認不能としてエラーにする。`--all-workspaces` で削除対象が 0 件の場合は、TTY でない環境でも確認せず成功とする。

### 3.8 `clean`

```text
decune clean [--dry-run] [--no-confirm] [--json]
decune clean --include-feature-cache [--dry-run] [--no-confirm] [--json]
```

`clean` は decune-managed data を削除する保守用コマンドとする。Docker のコンテナ、Compose プロジェクト、Docker のボリューム、Docker のイメージ、Docker のビルダーキャッシュ、利用者が管理しているファイルシステムは削除しない。`--all` と `--force` は提供しない。

既定の削除対象は stale なワークスペースデータだけである。

- `$XDG_CACHE_HOME/decune/<workspace_id>` または `~/.cache/decune/<workspace_id>`
- `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`
- port forwarding の status 用の兄弟ディレクトリ (`<runtime parent>/<workspace_id>-ports`)

ワークスペースデータは workspace id 単位で扱い、キャッシュ / 状態 / ランタイムデータの一部だけを意図的に削除するモードは提供しない。有効な workspace id は 12 桁の小文字 16 進 (`[0-9a-f]{12}`) に完全一致する値だけである。無効なディレクトリ名や Docker のラベル値は削除対象パスの組み立てに使わない。

`--include-feature-cache` は、既定のワークスペースデータ削除に共有 Feature archive cache (`$XDG_CACHE_HOME/decune/features` または `~/.cache/decune/features`) を追加するオプションとする。既定の `clean` は共有 Feature archive cache を削除しない。Feature archive cache の削除は Feature の取得・展開処理と同じプロセス間ロックで保護し、`up` / `rebuild` と同時にアーカイブのキャッシュを変更しない。

安全性の規則:

- 設定された XDG のルートと、仕様で定義したフォールバック配下の、decune が管理しているパスだけを探索する。
- symlink は辿らない。削除対象自体または配下のエントリに symlink がある対象は `unsafe_path` としてスキップする。
- decune が管理しているルート外のパスは削除しない。
- Docker のラベルから `decune.managed=true` と有効な `decune.workspace_id` を持つコンテナ / ボリュームが見つかるワークスペースは、decune が管理している再利用可能なリソースとみなしてスキップする。
- ランタイムディレクトリまたは port status ディレクトリ配下に接続可能な Unix ソケット、または取得できないロックファイルがあるワークスペースは active とみなしてスキップする。
- Docker リソースの探索に失敗した場合、削除の実行は安全性を判定できないためエラーにする。`--dry-run` ではファイルシステム上の候補を `docker_unavailable` としてスキップ表示できる。
- ワークスペース側のファイルである `.decune/config.toml` と `.decune/features.lock.toml` は対象外である。
- ランタイムディレクトリのファイル内容は stdout/stderr、状態、ラベル、ログに出してはならない。

TTY / non-TTY:

- TTY + `--no-confirm` なし + 削除候補あり: summary を表示し、`[y/N]` で確認する。
- non-TTY + `--no-confirm` なし + 削除候補あり: エラーにする。
- `--no-confirm`: 確認プロンプトだけを省略する。active / 再利用可能なワークスペースの保護や symlink の拒否は迂回しない。
- `--dry-run`: 削除しないため確認不要。non-TTY でも実行できる。

`--json` は stdout に JSON オブジェクトを出力する。ルートは `dry_run`、`include_feature_cache`、`summary`、`targets` を持つ。`summary` は `remove_candidates`、`removed`、`skipped` を持つ。ワークスペースの target は以下を持つ。

- `kind`: `"workspace"`
- `workspace_id`
- `action`: `"remove"` または `"skip"`
- `reason`: `"stale_workspace_data"`、`"managed_resource"`、`"active_runtime"`、`"unsafe_path"`、`"docker_unavailable"`、`"missing"` のいずれか
- `removed`: 実削除した場合だけ `true`
- `paths`: `cache`、`state`、`runtime`、`port_status`
- `existing_paths`: `"cache"`、`"state"`、`"runtime"`、`"port_status"` の配列

Feature cache の target は `kind = "feature_cache"`、`action`、`reason`、`removed`、`path` を持つ。Feature cache の `reason` は `"feature_cache_included"`、`"unsafe_path"`、`"missing"` のいずれかである。

### 3.9 コンテナ内の decune CLI

container-side tools bundle はコンテナ内 CLI を artifact 名 `decune` として配布する。実行時の配置先は `/run/decune/decune`、利用者向けのコマンド名は `decune` とする。コンテナ起動後の `/usr/local/bin/decune -> /run/decune/decune` symlink の準備と制約は 12.6 節に従う。

利用条件:

- 実効的な `[container.cli].enabled` が true である(5.13 節)。
- 対象ワークスペースの attached `decune up` session がホストで動作している。detached モードは対象外であり、detached な `up` の lifecycle command のために decune host daemon が動作している間もクエリは `container_cli_disabled` で拒否される。
- コンテナ内 CLI は現在のワークスペースだけを対象とする。

対応するコマンドとクエリ:

| コンテナ内のコマンド | 送信するクエリ |
| --- | --- |
| `decune status` | `status` + `text` |
| `decune ports` | `ports` + `text` |
| `decune ports --json` | `ports` + `json` |

使い方のエラーとローカル動作:

- `status --json`、ワークスペースの位置引数(`.` を含む)、`ports --all`、重複する `ports --json` は、ソケットへ接続する前に使い方のエラーとする。
- `up`、`rebuild`、`down`、`remove` / `rm`、`clean` はホスト専用コマンドとしてローカルで拒否する。
- `--help` / `-h` / `help`、コマンドのヘルプ、`--version` / `-V` はローカル表示とし、ホスト専用コマンドのヘルプはホストで実行するコマンドであることを説明する。ヘルプのオプションは引数を左からパースして到達した時点でローカルのヘルプを表示し、それより前に検出した未知のオプションや重複オプションは使い方のエラーとする。
- 引数なし、未知のコマンド / オプション、UTF-8 でない引数は panic せず使い方のエラーとする。

出力契約:

- コンテナ専用の status は、記録済みの状態とクエリ時点の Docker evidence の比較だけを表示する。`Config snapshot: consistent` は両者が整合することだけを表し、live なワークスペース設定は常に `Live workspace: not checked` と表示する。
- 記録済みの primary container が Docker evidence に存在しない場合、または identity を持つ decune-managed コンテナのいずれかが記録済みの identity と一致しない場合は `runtime-mismatch` とする。既知の identity 不一致がなく、primary container の identity を取得できない場合、または状態 / Docker evidence 自体を取得できない場合は `unavailable` とし、ホスト側 status の `current` / `needs-rebuild` とは区別する。identity を持たない primary 以外のコンテナは比較から除外する。
- ヘルスの集計が `mixed` でも、実際に `unhealthy` な decune-managed コンテナがなければ `unhealthy-container` の問題は表示しない。この問題の条件と重大度(`error`)はホスト側 status と同じにする。
- ホストのワークスペース / 設定パス、生のハッシュ / ラベルは表示せず、ホストで実行する対処は `Action (run on host)` 節に表示する。
- コンテナ内 `ports` のテキスト出力はホストの単一ワークスペースの表と同じ列、意味、並び順を使い、JSON 出力はホストの単一ワークスペースの JSON スキーマと同じにする。JSON の各エントリで `workspace` / `workspace_id` は省略する。ポートのスナップショットは、ワークスペースパスと workspace id のフィールドを構造上持たない。
- テキスト / JSON とも末尾の改行はちょうど 1 個とする。

exit code とストリームの分離:

- 成功時の警告は配列順に `Warning: <message>` として stderr へ書き、成功時の出力は改変せず stdout へ書く。
- 成功は警告の有無にかかわらず exit `0`、daemon / transport / 不正な response のエラーは exit `1`、使い方のエラーは exit `2` とする。
- daemon error code は未知の将来値も受理し、そのメッセージを `Error: <message>` として stderr へ書き、stdout は空に保つ。
- 警告とエラーの末尾の改行は 1 個に正規化するが、成功時の出力には改行を追加しない。

transport 契約:

- クエリの transport は request の書き込み完了後に Unix ソケットの書き込み側を shutdown し、response を EOF まで読む。
- decune host daemon の response は `version`、`ok`、任意の `output`、任意の `error`、任意の `warnings` を持つ。成功の response は `output` が必須で `error` を持たず、警告を 0 件以上持てる。エラーの response は `code` と `message` を持つ `error` が必須で、`output` と警告を持たない。クライアントはこの不変条件に違反する response を不正な response として拒否する。`warnings` がない version 1 の response は空の警告リストとして扱う。
- コンテナ内 CLI が受理する response は最大 1 MiB(1,048,576 バイト)とし、上限を超える response は不正な response として拒否する。
- daemon handoff 中のソケット交換を許容するため、connect の `NotFound` / `ConnectionRefused` に限り短い固定間隔で限られた回数だけ再試行する。権限エラー、request の書き込み / 読み取りエラー、不正な response、daemon のエラーは再試行しない。再試行を使い切った場合は、attached `decune up` session が必要で detached モードでは利用できないことを示す canonical unavailable error とする。他の transport のエラーは、daemon の停止や認可の失敗と断定しない一般エラーとする。

クエリの処理境界、認可、daemon error code は 12.5 節と 13.3 節を参照する。

## 4. devcontainer.json

### 4.1 検出順序

workspace root から以下の順で検出する。

1. `.devcontainer/devcontainer.json`
2. `.devcontainer.json`
3. `.devcontainer/<name>/devcontainer.json`

`--config <PATH>` が指定された場合は自動検出を行わず、そのパスを `devcontainer.json` として使う。相対パスは workspace root 相対で解決する。3 に複数候補がある場合は自動選択せず、`--config .devcontainer/<name>/devcontainer.json` で明示する。

### 4.2 構成モードの判定

| mode | 必須 property | 禁止 property | 備考 |
| --- | --- | --- | --- |
| image | `image` | `build`, `dockerComposeFile`, `service` | イメージを pull してコンテナを作る |
| Dockerfile | `build.dockerfile` | `image`, `dockerComposeFile`, `service` | Dockerfile をビルドしてコンテナを作る |
| Docker Compose | `dockerComposeFile`, `service` | `image`, `build` | Compose が image/build を持つ |

`dockerComposeFile` と `service` は片方だけ指定してはならない。`runServices` は Compose モード専用であり、指定する場合は `dockerComposeFile` と `service` も必須である。

### 4.3 JSONC

`devcontainer.json` は JSON with Comments として扱う。コメント除去を正規表現で実装しない。`//` の行コメント、`/* ... */` のブロックコメント、末尾カンマは JSONC として受け付ける。

JSON5 全体はサポートしない。単引用符の文字列、引用符なしのキー、16 進数、`#` コメントなどの JSON5 専用構文は不正なメタデータとして扱う。

### 4.4 対応プロパティ

この表は decune が認識する Dev Container プロパティと各モードでの扱いを定義する。利用者の `devcontainer.json`、Dockerfile / イメージの `devcontainer.metadata` ラベル、Feature メタデータの各レイヤーを同じスキーマで解釈し、レイヤー固有の制約(イメージメタデータのレイヤーは `initializeCommand` を指定できない)に違反する場合はエラーにする。表にないプロパティはエラーにも警告にもせず保持し、実行時挙動には使わない。

| property | image | Dockerfile | Compose | 備考 |
| --- | --- | --- | --- | --- |
| `image` | yes | no | no | image-based モード |
| `build.dockerfile` | no | yes | no | Dockerfile-based モード |
| `build.context` | no | yes | no | `devcontainer.json` からの相対パス |
| `build.args` | no | yes | no | 文字列の値のみ |
| `build.options` | no | partial | no | Docker ビルドの argv に渡す。decune が管理するオプションとコンテキストパスは不可 |
| `build.target` | no | yes | no | multi-stage build の target |
| `build.cacheFrom` | no | partial | no | Docker CLI で扱える形式 |
| `dockerComposeFile` | no | no | yes | 文字列 / 文字列の配列。ローカルパスのみ |
| `service` | no | no | yes | primary service |
| `runServices` | no | no | yes | 未指定時は全サービス。primary service は常に含める |
| `features` | yes | yes | yes | Compose モードは primary service の最終イメージに適用 |
| `overrideFeatureInstallOrder` | yes | yes | yes | Feature のインストール順序に反映 |
| `overrideCommand` | yes | yes | yes | image/Dockerfile 既定 true、Compose 既定 false |
| `mounts` | partial | partial | partial | bind/volume 対応。Compose モードは primary service に override として追加。tmpfs はエラー |
| `workspaceMount` | yes | yes | no | Compose モードは未対応のエラー。Compose ファイルの primary service の `volumes` を使う |
| `workspaceFolder` | yes | yes | yes | Compose モードの既定は `/` |
| `containerEnv` | yes | yes | yes | Compose モードは primary service の `environment` を上書き。秘密情報の保存先ではない |
| `remoteEnv` | yes | yes | yes | exec / lifecycle command / シェルに適用。`${localEnv:...}` 由来の値は argv / ログの redaction 対象 |
| `remoteUser` | yes | yes | yes | シェル / lifecycle command のユーザー |
| `containerUser` | yes | yes | yes | Compose モードは primary service の `user` を上書き |
| `updateRemoteUserUID` | yes | yes | yes | Linux ホストで既定 true |
| `userEnvProbe` | yes | yes | yes | `none`, `loginShell`, `interactiveShell`, `loginInteractiveShell` |
| `forwardPorts` | yes | yes | yes | TCP のみ。プロトコルサフィックスなしは TCP、`/tcp` は許可、`/udp` は未対応のエラー。Compose モードは `"service:port"` を受け付ける |
| `portsAttributes` | partial | partial | partial | `label`, `onAutoForward`, `requireLocalPort`。`protocol`, `elevateIfNeeded` は警告して無視 |
| `otherPortsAttributes` | partial | partial | partial | automatic forwarding の既定。未対応のフィールドは警告 |
| `appPort` | yes | yes | no | TCP のみ。プロトコルサフィックスなしは TCP、`/tcp` は許可、`/udp` は未対応のエラー。Compose モードは未対応のエラー。Compose ファイルのサービス `ports` を使う |
| `runArgs` | partial | partial | no | Compose モードは未対応のエラー。Compose ファイルのサービス属性を使う |
| `init` | yes | yes | yes | Compose モードは primary service の `init` を上書き |
| `privileged` | yes | yes | yes | Compose モードは primary service の `privileged` を上書き |
| `capAdd` | yes | yes | yes | Compose モードは primary service の `cap_add` を上書き |
| `securityOpt` | yes | yes | yes | Compose モードは primary service の `security_opt` を上書き |
| `entrypoint` | yes | yes | yes | 主にイメージのラベル / Feature メタデータのレイヤーで指定する。収集した entrypoint は entrypoint shim に反映(7.1 節) |
| lifecycle commands | yes | yes | yes | Feature メタデータ由来のコマンドは利用者のコマンドより前に実行 |
| `waitFor` | partial | partial | partial | パースするが attached `up` は `postAttachCommand` まで同期実行 |
| `name` | ignored | ignored | ignored | 無視する。実行時挙動には使わない |
| `shutdownAction` | partial | partial | partial | attached `up` 終了時に適用。明示 `down` / `remove` が正 |
| `hostRequirements` | ignored | ignored | ignored | 検証せず無視する |
| `customizations` | ignored | ignored | ignored | 保持するが実行しない |

`portsAttributes` / `otherPortsAttributes` の `onAutoForward` は `notify`、`silent`、`ignore` に加え、互換のため `openBrowser`、`openBrowserOnce`、`openPreview` を受理する。ブラウザ / プレビュー系の値は CLI では `notify` と同じ扱いにする。

### 4.5 `runArgs` 許可リスト

image/Dockerfile モードが受け付ける `runArgs` は以下のみ。

- `--init`
- `--privileged`
- `--cap-add <CAP>`
- `--security-opt <OPT>`
- `--add-host <HOST:IP>`
- `--dns <IP>`
- `--dns-search <DOMAIN>`
- `--network <NETWORK>`
- `--network-alias <ALIAS>`
- `--hostname <HOSTNAME>`
- `--device <HOST_PATH[:CONTAINER_PATH[:PERMISSIONS]]>`
- `--group-add <GROUP>`
- `--ulimit <NAME=SOFT[:HARD]>`
- `--ipc <MODE>`
- `--shm-size <SIZE>`
- `--gpus <REQUEST>`

値を取るオプションは `--foo=value` と `--foo value` の両方を受け付け、内部では `--foo`, `value` へ正規化する。`--init` と `--privileged` は値なしの真偽値オプションとしてのみ受け付ける。`--cap-add` と `--security-opt` は Dev Container の専用フィールドと同じ扱いでマージする。その他の許可オプションは Docker create に `option value` として渡す。

上記以外は未対応のエラーとする。特に decune がコンテナの identity、環境変数、ユーザー / 作業ディレクトリ、マウント、published port、ラベル、entrypoint、lifecycle / 制御を管理するため、`--name`、`--env` / `-e`、`--env-file`、`--user` / `-u`、`--workdir` / `-w`、`--mount`、`--volume` / `-v`、`--tmpfs`、`--volumes-from`、`--publish` / `-p`、`--publish-all` / `-P`、`--expose`、`--entrypoint`、`--label`、`--label-file`、`--rm`、`--detach` / `-d`、`--restart` は予約オプションとして拒否する。published port は `appPort` または Compose サービスの `ports`、port forwarding は `forwardPorts` / decune `[[ports]]` / CLI `-p`、マウントは `mounts`、ユーザーは `containerUser`、作業ディレクトリは `workspaceFolder`、環境変数は `containerEnv` を使う。

Compose モードでは `runArgs` を未対応のエラーとする。Compose サービスの `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`devices`、`network_mode`、`ports`、`volumes`、`user`、`environment` などを Compose ファイルに書くか、Dev Container の cross-orchestrator プロパティを使う。

### 4.6 `workspaceMount` / `workspaceFolder`

image/Dockerfile モードでは、`workspaceMount` を明示する場合は `workspaceFolder` も明示必須とする。`workspaceFolder` はワークスペースのマウント先の配下でなければならない。`workspaceMount` 未指定時は `/workspaces/<localWorkspaceFolderBasename>` を bind mount の対象パスとし、`workspaceFolder` 未指定時はその対象パスを作業ディレクトリとする。

Compose モードでは `workspaceMount` は未対応のエラーとする。ワークスペースのマウントは Compose ファイルの primary service の `volumes` に定義する。`workspaceFolder` 未指定時の既定は `/` である。

## 5. decune config

### 5.1 配置

- global: `$XDG_CONFIG_HOME/decune/config.toml`
- global フォールバック: `~/.config/decune/config.toml`
- project: `<workspace>/.decune/config.toml`

project 設定は Git 管理してよい。秘密情報を設定ファイルに直接書かない。

### 5.2 マージ順序

最終設定は以下の順で合成する。後勝ちが基本である。

1. decune default
2. イメージメタデータの `devcontainer.metadata`
3. Feature メタデータ
4. global decune config
5. `devcontainer.json`
6. project decune config
7. CLI オプション

`decune up --no-global-config` / `decune rebuild --no-global-config`、または project config の `use_global_config = false` を指定した場合、4 の global decune config は読み込まず、合成対象にも含めない。global config を読み込まないため、global config ファイルのパース / 検証エラーも発生しない。CLI オプションは一時的な強制無効化として扱い、project config で再有効化できない。

`--config <PATH>` は `devcontainer.json` を選択するだけであり、decune config の追加指定ではない。

### 5.3 マージルール

- スカラー: 後勝ち。
- `container.cli.enabled`: 真偽値のスカラーとして後勝ち。global の `false` は project の `true` で再有効化できる。
- `init` / `privileged`: 真偽値のスカラーとして後勝ち。上位レイヤーの `false` は下位レイヤーの `true` を打ち消せる。
- `capAdd` / `securityOpt`: セキュリティ系のリストとして重複排除した和集合。
- マップ: キーごとにマージ。同一キーは後勝ち。
- decune config の配列: 原則追記。ただし identity を持つ要素は置換。
- Feature の identity: canonical Feature ID と具体的な参照。同一の具体的な参照はオプションをマージする。`enabled = false` は canonical Feature ID 単位で無効化する。
- マウントの identity: `target`。
- dotfiles の identity: `target`。
- ポートの identity: `protocol + service + container + host_ip`。`service` 未指定は primary service を表す。
- Compose published port mapping の identity: `service + protocol + target`。同一 identity は後のレイヤーが置換し、`enabled = false` は下位レイヤーの mapping を削除する。
- decune hook の identity: identity なし。順序を保って追記。

### 5.4 設定例

```toml
version = 1
use_global_config = true
shell = "/bin/zsh"

[features."ghcr.io/devcontainers/features/github-cli:1"]
version = "latest"

[[dotfiles]]
source = "~/.config/nvim"
target = ".config/nvim"
read_only = true
resolve_symlink = true
on_conflict = "replace-symlink"

[[mounts]]
source = "~/work"
target = "/workspaces/work"
type = "bind"
read_only = false
resolve_symlink = true
create = false

[[ports]]
container = 3000
host = 3000
host_ip = "127.0.0.1"
protocol = "tcp"
require_local = false
label = "web"

[ports.auto]
enabled = true
min = 1024
max = 32768
ignore = [22, 2375, 2376]
on_auto_forward = "notify"

[compose.published_ports]
automatic_relocation = false
warn_on_relocation = false

[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "tcp"
host = 1502
host_ip = "127.0.0.1"

[container.cli]
enabled = true

[credentials.git]
enabled = true
copy_user = true
copy_global_config = false
https = "host-helper"
ssh_agent = "auto"

[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true

[[hooks.before_post_create]]
command = "scripts/before-post-create.sh"
where = "container"
user = "remote"
shell = true
```

### 5.5 トップレベル

- `version`: 必須。`1` のみ。
- `use_global_config`: 任意。既定 true。project config で false にすると global decune config を適用しない。
- `shell`: 任意。`decune up` で接続するシェルのパスまたはコマンド名。
- 未知のキーはエラー。

### 5.6 `[features]`

TOML のテーブルキーに Feature の参照を引用符で囲んで指定する。

```toml
[features."ghcr.io/devcontainers/features/go:1"]
version = "1.23"
enabled = true
```

- `enabled = false` で global 設定 / イメージメタデータ / Feature メタデータ由来の Feature を project 側から無効化できる。
- `enabled` は decune の予約キーであり、Feature のオプションとしては渡さない。
- それ以外のキーは Feature のオプションとして扱う。

### 5.7 `[[dotfiles]]`

dotfiles はホスト側パスをリモートユーザーのホームディレクトリに直接 bind mount しない。`/opt/decune/dotfiles/<target>` にマウントし、コンテナのセットアップ時にホームディレクトリへ symlink を作る。`/opt/decune/dotfiles` と `/opt/decune/dotfile-backings` は decune の dotfiles 用内部パスとして予約する。

- `source`: ホスト側パス。global config では `~` または絶対パス。project config の相対パスは workspace root 相対。
- `target`: リモートユーザーのホームディレクトリからの相対パス。絶対パスは禁止。
- `enabled`: 既定 true。false の場合は同一 `target` を無効化。
- `read_only`: 既定 true。
- `resolve_symlink`: 既定 true。true の場合は `source` を正規化する。ファイルの場合は正規化済みの `source` を直接 bind mount する。
- `on_conflict`: `fail`, `replace-symlink`, `backup`。既定 `fail`。

`resolve_symlink = true` のディレクトリの `source` は次のとおり扱う。

- 配下に symlink がない場合は正規化済みの `source` を直接 bind mount する。
- 配下に symlink があり、同一の backing root に完全一致する場合は、その backing root を直接 bind mount する。
- 完全一致しない場合は状態ディレクトリにマウント用の skeleton を作成する。skeleton のルートは `/opt/decune/dotfiles/<target>` に bind mount する。`source` 由来のファイルは個別ファイルの bind mount ではなく、正規化済みの親ディレクトリを `/opt/decune/dotfile-backings/<n>` に bind mount し、skeleton 内に backing ファイルへの symlink を作る。`<n>` は dotfiles のマウント計画全体で一意に採番する。
- 同じ正規化済み親ディレクトリと `read_only` を使う複数のエントリは backing のマウントを共有し、`read_only` が異なる場合は別の対象パスを割り当てる。symlink を含まない実ディレクトリは、ディレクトリの直接 bind mount として表現する。skeleton、backing ディレクトリのマウント、ディレクトリの直接マウントの書き込み可否は `read_only` に従う。

skeleton と backing マウントの観測可能な挙動:

- backing は親ディレクトリ単位でマウントするため、同じ親ディレクトリの兄弟ファイルは `/opt/decune/dotfile-backings/<n>` 経由でコンテナから見える。
- `read_only = false` の skeleton のみのパスにコンテナから新規作成されたファイル / ディレクトリは、元の `source` ではなく状態ディレクトリの skeleton に保存し、以後の skeleton 準備でも保持する。ただし decune が計画した skeleton 内の symlink がコンテナ内で通常ファイルなどに置換された場合、次回の skeleton 準備で計画どおりの symlink に戻す。
- `read_only = true` の skeleton では、現在の dotfiles の構成に不要な stale エントリを削除するが、実行中の既存コンテナの再利用では skeleton を再生成しない。dotfiles の内容は状態ディレクトリにコピーしない。
- 通常ファイルのホスト側でのアトミックな置換と、解決済み symlink の対象ファイルのホスト側でのアトミックな置換は、起動中のコンテナから見える。`source` 側の symlink のパス自体がホスト側の rename で通常ファイルに置換される場合は、起動中のコンテナへ自動反映しない。反映にはコンテナの再作成が必要である。

壊れた symlink、循環する symlink、特殊ファイル、マウント数の上限超過など、対応する bind mount の計画として表現できない場合はエラー。

Compose モードでは primary service に dotfiles の bind mount とセットアップ用の lifecycle を適用する。

### 5.8 `[[mounts]]`

任意の追加マウント。

- `type`: `bind`, `volume`, `tmpfs`。`bind` と `volume` に対応し、`tmpfs` はエラー。
- `source`: `bind` では必須。`volume` ではボリューム名。
- `target`: コンテナ内の絶対パス。`/opt/decune` と `/run/decune` の配下、およびワークスペースのマウント先と同一の `target` は禁止。特に `/opt/decune/dotfiles` と `/opt/decune/dotfile-backings` は dotfiles 用の内部パスとして予約する。
- `enabled`: 既定 true。false の場合は同一 `target` を無効化。
- `read_only`: 既定 false。
- `resolve_symlink`: bind の `source` にのみ適用。既定 true。
- `create`: `false`, `"directory"`。既定 false。ファイルの自動作成は行わない。

Compose モードでは primary service に decune-generated Compose override として追加する。

### 5.9 `[[ports]]`

manual port forwarding 設定。Docker published port ではない(9 章)。

- `container`: コンテナ側のポート。必須。
- `host`: ホスト側ポート。省略時は `container` と同じ番号を試し、占有済みなら空きポートを探索する。
- `host_ip`: 既定 `127.0.0.1`。`0.0.0.0` は明示された場合のみ許可。
- `protocol`: `tcp` のみ。省略時も TCP。`udp` は未対応のエラー。
- `service`: Compose モードで対象サービスを指定する任意フィールド。未指定は primary service。image/Dockerfile モードでは指定不可。
- `enabled`: 既定 true。
- `require_local`: true の場合、要求したホスト側ポートと異なるポートにフォールバックしたら警告する。
- `label`: 表示用。

### 5.10 `[ports.auto]`

automatic port forwarding の設定。挙動は 9.7 節。

- `enabled`: 既定 true。
- `min`: 既定 1024。
- `max`: 既定 32768。
- `ignore`: automatic forwarding から除外するポート。
- `on_auto_forward`: `notify`, `silent`, `ignore`, `openBrowser`。`openBrowser` は互換のため受理し、CLI では `notify` と同じ扱いにする。

### 5.11 `[compose.published_ports]`

Docker Compose-based configuration の Compose サービス `ports` に対する automatic published port relocation のポリシーと explicit published port mapping。計画作成の挙動契約は 8.8 節。

```toml
[compose.published_ports]
automatic_relocation = false
warn_on_relocation = false

[[compose.published_ports.mappings]]
service = "app"
target = 502
protocol = "tcp"
host = 1502
host_ip = "127.0.0.1"
```

- `automatic_relocation`: 既定 false。ただし `[compose.clone_isolation].enabled = true` かつ `automatic_relocation` 未指定の場合は既定 true。true の場合、対象となる fixed TCP published port の requested endpoint が使えなければ、ホスト側のポート番号を変更する relocation の候補を自動探索してよい。
- `warn_on_relocation`: 既定 false。true の場合、後続の relocation 処理は requested endpoint と planned endpoint が異なる relocation について警告を出してよい。既存 Compose プロジェクトの published binding を変更するためにコンテナの再作成を伴う場合の警告は、この設定に関係なく常に出す。
- `mappings`: fixed TCP published port の planned endpoint を明示する配列。`automatic_relocation = false` でも有効であり、automatic relocation の有効/無効とは独立する。

`[[compose.published_ports.mappings]]` のフィールドは次のとおり。

- `service`: 必須。Compose のサービス名。空文字列はエラー。
- `target`: 必須。Compose のポートエントリのコンテナ側ポート。`1..=65535`。
- `protocol`: 任意。既定 `tcp`。`tcp` のみ対応する。
- `host`: 有効な mapping では必須。planned のホスト側ポート。`1..=65535`。
- `host_ip`: 任意。IPv4 または IPv6 アドレス。省略時は、対応する Compose のポートエントリの requested のホスト IP を、ホスト IP 省略の場合を含めて継承する。
- `enabled`: 任意。既定 true。false のエントリは `service + protocol + target` だけを identity として下位レイヤーの mapping を削除し、`host` / `host_ip` を指定してはならない。

同じ設定ファイル内に同一 identity の mapping が複数ある場合はエラーとする。global / project 等のレイヤー間では通常のマージ順序に従って後の mapping が前の mapping を置換する。mapping の追加・変更・削除は reuse hash に含み、既存プロジェクトへの反映には `decune rebuild` が必要になる場合がある。

CLI `--automatic-published-port-relocation` / `--no-automatic-published-port-relocation` は、この実行で `automatic_relocation` を上書きする。`--no-auto-forward` はこのポリシーを変更しない。

### 5.12 `[compose.clone_isolation]`

同じ Compose-based のワークスペースを複数のクローンから同時起動するためのオプトイン設定。挙動契約は 8.9 節。

`enabled` は全体の有効化スイッチで、既定は false。false の場合、下位のテーブルや `endpoints` が指定されていても無効として扱い、その内容は検証しない。ただし `endpoints` が 1 個以上あれば、無効な宣言を黙って無視せず警告を表示する。

```toml
[compose.clone_isolation]
enabled = false

[compose.clone_isolation.networks]
relocation = false
subnet_pool = "10.200.0.0/16"
# subnet_prefix = 24

[compose.clone_isolation.names]
rewrite_container_names = true
rewrite_resource_names = true

[[compose.clone_isolation.endpoints]]
service = "app"
env = "HOST_AGENT_ENDPOINT"
value = "grpc://${decune.network.fixed_net.gateway}:50051"
```

- `enabled`: 既定 false。false の場合、clone isolation による書き換えをすべて無効にする。true で `[compose.published_ports].automatic_relocation` が未指定の場合、その既定値を true に切り替える。global / project / CLI のいずれかで `automatic_relocation = false` が明示されていれば、明示値を優先する。
- `networks.relocation`: 既定 false。true の場合、固定サブネットをワークスペースごとに relocation する対象とする。
- `networks.subnet_pool`: `networks.relocation = true` のとき必須。relocation 先を割り当てる IPv4 CIDR のプール。`enabled = true` のとき、指定値が IPv4 CIDR でなければエラー。
- `networks.subnet_prefix`: 任意。省略時は元のサブネットのプレフィックス長を維持する。指定する場合は `subnet_pool` のプレフィックス以上かつ 31 未満でなければならない。
- `names.rewrite_container_names`: 既定 true。明示的なサービスの `container_name` をワークスペース固有名へ書き換える対象とする。
- `names.rewrite_resource_names`: 既定 true。トップレベルの `name` を持つ `networks` / `volumes` / `configs` / `secrets` をワークスペース固有名へ書き換える対象とする。
- `endpoints`: 0 個以上。`service` は環境変数を設定する Compose サービス、`env` は環境変数名、`value` は値のテンプレート。同一 `service` + `env` の重複宣言はエラー。

### 5.13 `[container.cli]`

```toml
[container.cli]
enabled = true
```

- `enabled` はコンテナ内の read-only の decune container CLI query(3.9 節)を許可するプロジェクト側の設定で、既定は true とする。
- global / project 間では通常の真偽値スカラーと同じ後勝ちでマージし、global の `false` は project の `true` で再有効化できる。`use_global_config = false` または `--no-global-config` では global 値を読み込まない。
- 実効値が false の場合は decune host daemon がクエリを拒否する。強制の正は daemon の拒否であり、artifact の削除や symlink の有無ではない(12.5 節、12.6 節)。
- この設定は、信頼していないリポジトリから解除できないセキュリティ上のオプトアウトではない。リポジトリから解除できない拒否ポリシーが必要な場合は、認証情報を含むホスト側専用のポリシー層を別途設計する。
- `container.cli.enabled` は reuse hash に含めない。この値だけの変更ではコンテナまたは Compose プロジェクトの再作成を要求しない。
- クエリが返す Docker evidence はサーバー側で短時間キャッシュされ、読み込み完了時点から最大 2 秒程度 stale になり得る。状態と forwarding status はキャッシュしない。

### 5.14 `[credentials.git]`

```toml
[credentials.git]
enabled = true
copy_user = true
copy_global_config = false
https = "host-helper"
ssh_agent = "auto"
```

- `enabled`: 既定 true。
- `copy_user`: ホストの `git config --global` の `user.name` / `user.email` をコンテナのリモートユーザーに設定する。既定 true。
- `copy_global_config`: `~/.gitconfig` 全体をコンテナにコピーする。既定 false。
- `https`: `off`, `host-helper`, `host-helper-read-only`。既定 `host-helper`。
- `ssh_agent`: `off`, `auto`, `required`。既定 `auto`。

`host-helper` はコンテナ内に `git-credential-decune` を配置し、decune host daemon 経由でホストの `git credential fill/approve/reject` を呼ぶ。このヘルパーはコンテナの OS/arch 用の artifact であり、ホストの `decune` バイナリをそのまま bind mount しない。

`host-helper-read-only` は同じヘルパーの配置 / マウントを使うが、コンテナからの認証情報の読み出しだけを許可する。Git credential の `get` はホストの `git credential fill` に転送し、`store` / `erase` はホストの `approve` / `reject` に渡さず、成功として空の出力を返す。信頼していないリポジトリではホストの credential store への書き込みを避けるため、`host-helper-read-only` または `off` を推奨する。`host-helper-read-only` は SSH agent forwarding を変更しないため、SSH agent が不要な場合は `ssh_agent = "off"` も設定する。

`https = "off"` または `enabled = false` の場合、decune host daemon は Git credential 要求をホストの Git credential helper に渡してはならない。

### 5.15 `[credentials.github]`

```toml
[credentials.github]
enabled = true
mode = "gh-token-file"
install_feature_if_missing = true
```

- `enabled`: 既定 true。
- `mode`: `off`, `gh-token-file`。既定 `gh-token-file`。
- `install_feature_if_missing`: ホストのトークンが取得でき、コンテナに `gh` がない場合に `ghcr.io/devcontainers/features/github-cli:1` を追加する。既定 true。

`gh-token-file` はホストの `gh auth token` を実行し、ランタイムディレクトリにモード 0600 のトークンファイルを作る。コンテナには `/run/decune/secrets/github-token` として read-only でファイルをマウントする。`GH_CONFIG_DIR=/run/decune/gh` は書き込み可能な一時ディレクトリとして分離する。

トークンの値は argv、イメージレイヤー、Docker/Compose のラベル、コンテナの環境変数、状態、reuse hash、decune-generated Compose override のファイルに入れない。ただしコンテナ内プロセスはトークンファイルに到達できるため、信頼していないリポジトリでは `[credentials.github].enabled = false` を推奨する。

### 5.16 `[[hooks.*]]`

`[[hooks.*]]` は、lifecycle stage の前後に実行する decune hook を定義する。実行順序は 7.3 節。

利用可能なフック名:

- `before_initialize`
- `after_initialize`
- `before_on_create`
- `after_on_create`
- `before_update_content`
- `after_update_content`
- `before_post_create`
- `after_post_create`
- `before_post_start`
- `after_post_start`
- `before_post_attach`
- `after_post_attach`

フックのエントリ:

```toml
[[hooks.before_post_create]]
command = "scripts/setup.sh"
where = "container"
user = "remote"
shell = true
```

- `command`: 文字列または文字列の配列。配列は 1 要素以上。
- `where`: `host`, `container`。`initialize` 系はホストのみ。
- `user`: `remote`, `root`, `<name>`。コンテナ側フックのみ。既定 `remote`。
- `shell`: true なら `/bin/sh -lc` で実行。文字列のコマンドの既定は true、配列のコマンドの既定は false。
- `workdir`: 省略時、ホスト側フックは workspace root、コンテナ側フックは `workspaceFolder`。

## 6. 変数展開とパス解決

以下を文字列の値で展開する。

- `${localEnv:VAR}` / `${localEnv:VAR:default}`
- `${containerEnv:VAR}` / `${containerEnv:VAR:default}`
- `${localWorkspaceFolder}` / `${localWorkspaceFolderBasename}`
- `${containerWorkspaceFolder}` / `${containerWorkspaceFolderBasename}`
- `${devcontainerId}`
- `${uid}` / `${gid}`
- `${remoteUser}`
- `${remoteUserHome}`

少なくとも `build.args` の値、`build.target`、`build.cacheFrom`、`workspaceFolder`、`containerEnv`、`remoteEnv`、`remoteUser`、`containerUser`、`mounts`、dotfiles、`runArgs` の値の部分で変数展開する。`workspaceFolder` は変数展開後に絶対パスの検証を行う。`workspaceFolder` 内の `${containerWorkspaceFolder}` は既定のワークスペースフォルダーを基準に展開する。`workspaceFolder` 未指定時に decune が合成する既定のワークスペースフォルダーは設定の文字列値ではないため、変数展開せずそのままのパスとして扱う。lifecycle command 本体、`dockerComposeFile`、`service`、`runServices`、`forwardPorts`、`appPort` の追加変数展開は行わない。

`build.args`、`build.target`、`build.cacheFrom` は Dockerfile のビルド前に展開するため、最終イメージや実行時のコンテナからしか分からない値には依存できない。これらのフィールドで `${remoteUserHome}` を使う構成はエラーとする。`${remoteUser}` は、`remoteUser` または `containerUser` が設定 / メタデータからビルド前に決まる場合だけ使える。Dockerfile の `USER`、Compose サービスの `user`、イメージ設定の `User` 由来のユーザーは、ビルド前の `build.*` 変数展開には使わない。

`${remoteUserHome}` は `/home/<user>` と推測せず、コンテナ / イメージ内の passwd データベースから解決する。`workspaceFolder`、`containerEnv`、`remoteEnv`、`mounts`、dotfiles、`runArgs` など実行時のユーザー解決後に評価できるフィールドでは、実効リモートユーザーの決定後に `${remoteUser}` / `${remoteUserHome}` を展開する。`containerEnv` 自体の中で `${containerEnv:...}` を使う構成はエラーとする。

`${localEnv:...}` から展開された `containerEnv` / `remoteEnv` / `build.args` の値は secret-sensitive value として追跡する。decune はその実値を状態、reuse hash、decune-generated Compose override、Docker/Compose のラベル、argv、通常のエラーログに平文保存してはならない。reuse hash ではキーを保持し、`containerEnv` と `build.args` は変更検出のため実値ではなく非可逆な digest を含め、`remoteEnv` は redaction 済みのマーカーに置き換える。Compose モードの decune-generated Compose override では primary service の `environment` に `${DECUNE_CONTAINER_ENV_<SAFE_KEY>}` 形式のプレースホルダーを書き、実値は `docker compose` の子プロセスの環境変数として渡す。プレースホルダーの変数名の `<SAFE_KEY>` は、`containerEnv` のキーから ASCII 英数字 / アンダースコアのみへ正規化した値とする。Docker のビルド引数はプロセスの環境変数と `--build-arg KEY` で Docker CLI に渡し、argv に値を直接載せない。

`containerEnv` はコンテナ作成時の環境変数であり、コンテナ内プロセスや Docker inspect から見える。`build.args` は Docker のビルドに渡り、イメージレイヤーやビルド出力に残る可能性がある。`runArgs`、`workspaceFolder`、`remoteUser`、`containerUser`、`build.target`、`build.cacheFrom` はコマンド、状態、ラベル、コンテナの identity に出る可能性がある。decune はこれらを秘密情報の保存先として保証しない。直接書かれた秘密の文字列や、decune が `${localEnv:...}` 由来と追跡できない値は、自動では secret-sensitive value と判定しない。ビルド時の秘密情報には Docker BuildKit の secret を使う。

通常の `up` / `rebuild` におけるホスト側 bind の `source` の処理順:

1. `~` を展開。
2. `${...}` を展開。
3. 相対パスを基準ディレクトリから絶対パスにする。
4. `create = "directory"` ならディレクトリを作成。
5. `resolve_symlink = true` なら正規化する。
6. 存在しないパスは `create` が指定されていない限りエラー。

`status <WORKSPACE>` の現在の reuse hash 計算は read-only のため、`create = "directory"` / `bind-create-src` で指定された存在しないパスは作成しない。`resolve_symlink = true` の場合は既存の祖先ディレクトリを正規化し、存在しない末尾を合成したパスを解決済みマウントとして扱う。`resolve_symlink = false` の場合は既存の祖先ディレクトリの存在を確認した上で、元の絶対パスを解決済みマウントとして扱う。`create` がない存在しない `source` は通常どおりエラーとする。

Compose ファイル内の環境変数の展開は Docker Compose CLI に委譲する。decune は `devcontainer.json` と decune config の値だけを自前で展開する。

## 7. 実行モデル

### 7.1 ビルドと Feature

#### image-based

1. ベースイメージを pull する。`--pull` 指定時は常に pull を試す。
2. Feature があれば Feature 適用済みのイメージをビルドする。
3. Linux ホストで UID/GID 同期が必要なら同期レイヤーをビルドする。
4. 収集した entrypoint があれば、生成した entrypoint shim のレイヤーをビルドする。
5. Feature、UID/GID 同期、entrypoint shim が不要ならベースイメージをそのまま作成に使う。

#### Dockerfile-based

1. `build.context` と `build.dockerfile` を `devcontainer.json` 相対で解決する。
2. Dockerfile 固有の ignore ファイル `<Dockerfile>.dockerignore` があれば、コンテキストルートの `.dockerignore` より優先する。
3. Docker CLI のビルドへ tar のコンテキストまたはコンテキストディレクトリを渡す。
4. Dockerfile のビルド結果イメージの `devcontainer.metadata` ラベルを読み、イメージメタデータのレイヤーとして `devcontainer.json` や decune config とマージする。
5. Dockerfile のビルド結果イメージに Feature を重ねる。
6. 必要なら UID/GID 同期レイヤーと entrypoint shim レイヤーを重ねる。

`build.options` は、Docker ビルドのコンテキスト引数 `-` より前に argv として渡す。シェル文字列には連結しない。decune が管理する `-f` / `--file`、`-t` / `--tag`、`--label`、`--build-arg`、`--target`、`--cache-from`、`--no-cache`、`--pull`、`--rm` / `--force-rm`、`--iidfile`、`--metadata-file`、`--output` などのオプションは `build.options` では指定できない。`build.options` はオプションだけを受け付け、ビルドコンテキストのパスは decune が stdin の tar と最後の `-` で管理する。

`--platform`、`--ssh`、`--secret`、`--add-host`、`--network` など Docker CLI に委譲できるビルドオプションは指定できる。ただし `build.options` の値は argv に出るため、秘密の文字列そのものを直接書かない。秘密情報は `--secret id=npm,env=NPM_TOKEN` のようにホストの環境変数やファイルパスを参照する形にする。

既知の制限:

- Dockerfile がビルドコンテキスト外にある構成は未対応のエラーとする。decune はビルドコンテキストの tar を生成して `docker build -` に渡すため、`--file` は tar 内のパスを指す必要がある。このため `build.dockerfile` は解決後の `build.context` 配下に存在しなければならない。回避策は、`build.context` を Dockerfile を含む上位ディレクトリに広げるか、Dockerfile をコンテキスト内へ移動することである。将来互換性を上げる場合は、コンテキスト外の Dockerfile を合成した tar エントリとして追加し、Dockerfile 固有の ignore ファイルとコンテキストの digest のセマンティクスを Docker CLI と揃える必要がある。
- Dockerfile のビルド後に判明する `devcontainer.metadata` ラベルはビルド入力には使わない。このため `build.args`、`build.target`、`build.cacheFrom` の `${remoteUser}` は、`devcontainer.json` や decune config などビルド前に解決できる `remoteUser` / `containerUser` だけを参照できる。

#### Docker Compose-based

Compose の primary service の `image` / `build` をベースイメージとして扱う。Feature は primary service の最終イメージにだけ適用する。sidecar service には Feature、UID/GID 同期、entrypoint shim、dotfiles、認証情報を自動適用しない。

primary service に `build` がある場合、まず Compose CLI でサービスのイメージをビルドする。primary service に `image` のみがある場合、必要に応じて pull する。ベースイメージの解決後、image/Dockerfile モードと同じ Feature / UID/GID 同期 / entrypoint のレイヤー適用を行い、decune-generated Compose override で primary service のイメージを最終イメージに差し替える。Compose モードのビルド手順全体は 8.6 節。

#### Feature

- OCI レジストリの参照とローカルの `./` 参照に対応する。
- HTTPS の tgz を直接参照する Feature は未対応。
- レジストリ認証は Docker CLI 互換で `credHelpers`、`credsStore`、`auths` の順に認証元を選ぶ。選択した認証元が失敗しても別の認証元へフォールバックしない。
- manifest 本体とレイヤーの blob は sha256 digest を検証する。
- local Feature のパスは `devcontainer.json` のディレクトリからの相対 `./` パスに限定し、絶対パスとパスの外部逸脱を拒否する。
- local Feature のディレクトリ名と `devcontainer-feature.json` の `id` は一致必須。
- `devcontainer-feature.json` と `install.sh` は必須。
- OCI Feature は `<workspace>/.decune/features.lock.toml` に digest lock を記録する。
- `rebuild --update-features` は lock より再解決を優先する。
- Feature のメタデータは必須フィールド `id`, `version`, `name` を要求する。
- `installsAfter` は弱い依存関係として扱い、インストール対象の一覧に存在しない Feature を追加しない。仕様上はバージョンタグ / digest を含められないが、互換性のため照合用にはタグ / digest を落とした canonical Feature ID として扱う。
- Feature のオプションは Features 仕様に従って環境変数キーに変換し、既定値のオプションも出力する。環境変数キーの衝突はエラー。
- Feature メタデータの `containerEnv` は、Feature レイヤーの Dockerfile の `ENV` として各 Feature の `install.sh` 実行前に適用し、後続の Feature と最終イメージに継承する。`PATH="/tool:${PATH}"` のような Dockerfile の環境変数置換は Docker のビルダーに委譲する。Feature 由来の `containerEnv` はコンテナ作成 / decune-generated Compose override の `environment` には再投入せず、利用者 / devcontainer / project 由来の `containerEnv` だけを実行時の上書きとして適用する。

### 7.2 コンテナの作成・起動とユーザー

image/Dockerfile モードでは、ワークスペースのマウント未指定時は `/workspaces/<localWorkspaceFolderBasename>` へ bind mount する。

Compose モードではワークスペースのマウントを自動追加しない。primary service の Compose `volumes` にワークスペースの bind mount がない場合でも decune は起動を続けるが、`workspaceFolder` が存在しない場合は lifecycle / シェルの実行前にエラーとする。

ユーザー解決:

- 実効コンテナユーザー: `containerUser`、イメージ / Feature メタデータの `containerUser`、Compose サービスの `user`、Docker イメージ設定の `User`、`root`。
- 実効リモートユーザー: `remoteUser`、イメージ / Feature メタデータの `remoteUser`、実効コンテナユーザー。

存在しない実効リモートユーザーは `root` へフォールバックせず設定エラーとする。数値の UID/GID は passwd のエントリがなくても実行時の identity として扱えるが、ホームディレクトリが必要な処理ではエラーになるか、警告を出してスキップする。

`updateRemoteUserUID` は Linux ホストで既定 true。リモートユーザーが明示されていればリモートユーザーを、なければ `containerUser`、イメージ / Feature メタデータの `containerUser`、Compose サービスの `user` のいずれかでコンテナユーザーが明示されている場合にコンテナユーザーを同期対象とする。Linux 以外のホスト、`root` が対象の場合、`updateRemoteUserUID = false`、passwd のエントリがない数値の対象は、何もしないか、警告を出してスキップする。

Compose モードで UID/GID 同期が必要な場合、primary service のベースイメージに同期レイヤーを重ねた最終イメージを作る。実行中のコンテナ内で `/etc/passwd` を直接書き換えない。UID/GID 同期によって実行時のユーザー表現が変わる場合、decune-generated Compose override の primary service の `user` には同期後のユーザー / グループを反映し、元の数値の UID/GID で主プロセスを起動しない。

### 7.3 lifecycle とシェル接続

Dev Container の lifecycle は以下の順で扱う。

1. `initializeCommand`(ホスト)
2. `onCreateCommand`
3. `updateContentCommand`
4. `postCreateCommand`
5. `postStartCommand`
6. `postAttachCommand`

`initializeCommand` はイメージ作成 / Compose プロジェクト作成より前に実行する。コンテナ側の lifecycle command は primary container 内で実行する。

decune hook は各 lifecycle stage の前後に実行する。Feature メタデータ由来の lifecycle command は Feature のインストール順に収集し、利用者の `devcontainer.json` 由来のコマンドより先に実行する。

lifecycle command が失敗した場合、対応する after 側のフックと後続処理は実行しない。作成時 lifecycle の成功済み stage は状態に記録し、次回の再利用時に二重実行しない。

detach でない `up` / `rebuild` は lifecycle 後にリモートユーザーのシェルを TTY で接続し、シェルの exit code を CLI の exit code として返す。シェル接続は `docker exec` 相当の CLI アダプターで primary container に対して実行する。Compose モードでも `docker compose exec` ではなく、コンテナ ID を解決して `docker exec` 相当を使ってよい。

`--detach` では接続時の lifecycle、転送のリスナー、`postAttachCommand`、シェル接続を実行しない。

### 7.4 `shutdownAction`

Dev Container の既定値に合わせる。

- image/Dockerfile モードの既定: `stopContainer`
- Compose モードの既定: `stopCompose`

attached な `up` でシェルが終了したとき:

- `none`: コンテナ / プロジェクトを停止しない。
- `stopContainer`: primary container だけ停止する。
- `stopCompose`: Compose モードでは Compose プロジェクト全体を停止する。image/Dockerfile モードでは `stopContainer` と同じ。

明示的な `decune down` / `decune remove` は利用者の操作として扱い、`shutdownAction` によって無効化されない。

## 8. Docker Compose モード

### 8.1 委譲原則と制限

Compose モードでは Compose サービスの実行時設定を Docker Compose に委譲する。Compose YAML のマージ、profiles、環境変数の展開、相対パス、ビルド、ネットワーク、ボリュームのセマンティクスは decune が再実装せず、Docker Compose v2 CLI の canonical model と実行結果を利用する。

decune は以下の Dev Container プロパティを decune-generated Compose override へ自動変換せず、メタデータ検証で未対応のエラーとする。

| Dev Container property | Compose モードの扱い | 代替 |
| --- | --- | --- |
| `workspaceMount` | 未対応のエラー | ワークスペースの bind mount を primary service の `volumes` に書く |
| `appPort` | 未対応のエラー | Docker published port 設定を Compose サービスの `ports` に書く |
| `runArgs` | 未対応のエラー | `init`、`privileged`、`cap_add`、`security_opt`、`extra_hosts`、`dns`、`dns_search`、`devices`、`network_mode` など Compose サービスのフィールドに書く |

Docker published port 設定は Compose ファイルに委譲する。Compose モードで外部公開が必要なポートは Compose サービスの `ports` を使い、decune の port forwarding は `forwardPorts`、decune `[[ports]]`、CLI `-p` を使う。

Compose モードでも decune は、対応している cross-orchestrator プロパティと実行時機能を primary service または primary service のコンテナに適用する。対象は `containerEnv`、`remoteEnv`、`containerUser`、`remoteUser`、`init`、`privileged`、`capAdd`、`securityOpt`、`mounts`、dotfiles のマウント、認証情報 / 実行時のマウント、lifecycle command、リモートシェル、automatic forwarding である。`remoteEnv` は primary service のコンテナで実行する lifecycle command、decune hook、リモートシェルに適用する。

### 8.2 Compose ファイルの解決

`dockerComposeFile` は文字列または文字列の配列である。各パスは `devcontainer.json` のあるディレクトリから相対解決する。絶対パスは可搬性がないため警告の対象とする。パスの外部参照は許可するが、状態 / ハッシュには正規化済みパスとファイルの digest を含める。存在しないパスはエラーとする。

解決した Compose ファイルは指定順に `docker compose -f <file>` へ渡す。後続のファイルが前のファイルを上書き / 追加する Compose 標準のマージのセマンティクスに従う。相対パス解決の基準は Docker Compose CLI の標準挙動に合わせ、第一 Compose ファイルの親ディレクトリをプロジェクトディレクトリとする。必要に応じて `--project-directory <first-compose-file-parent>` を明示する。Docker Compose の子プロセスのカレントディレクトリもプロジェクトディレクトリに固定し、Compose の変数展開の `.env` 解決が decune 呼び出し元の PWD ではなく Compose のプロジェクトディレクトリ基準になるようにする。第一 Compose ファイルが symlink の場合、プロジェクトディレクトリは最終的な symlink を辿った正規化済みパスの親ではなく、`devcontainer.json` 相対で解決した入力パスの親とする。

`dockerComposeFile` から git URL、OCI artifact、stdin を参照する構成は未対応のエラーとする。

### 8.3 Compose プロジェクト名

decune は Compose のプロジェクト名を必ず明示する。トップレベルの `name:`、`COMPOSE_PROJECT_NAME`、カレントディレクトリ名に依存しない。

```text
decune-<safe_workspace_slug>-<workspace_id>
```

- 小文字 ASCII、数字、ダッシュのみ。
- 先頭は `decune-` 固定。
- `workspace_id = hex(sha256(canonical_workspace_path))[0..12]`。
- reuse hash はプロジェクト名に含めない。同じワークスペースの再作成でプロジェクト名は安定する。

Compose CLI には `--project-name <project>` を渡す。`COMPOSE_PROJECT_NAME` がホストの環境変数に存在しても、CLI オプションを優先する。

### 8.4 正規化と検証

Compose モードの計画作成時、decune は以下を実行する。

```text
docker compose --project-name <project> --project-directory <dir> -f <file>... config --format json
```

この出力を canonical Compose model として扱う。decune は Compose YAML を直接パースしない。

検証:

- `service` が canonical model の `services` に存在する。
- `runServices` の各サービスが canonical model の `services` に存在する。
- primary service の実行コンテナが一意に決まる。`docker compose ps --format json <service>` が 0 件または 2 件以上を返す状態でシェル / lifecycle を実行しない。
- profile により primary service が無効になる構成はエラー。必要な profile はホストの環境変数 `COMPOSE_PROFILES` または Docker Compose CLI の標準手段で有効化する。
- `workspaceFolder` は絶対パスでなければならない。

### 8.5 runServices

- `runServices` 未指定: `docker compose up -d` をサービス引数なしで実行し、Compose モデル上の有効なサービスを起動対象にする。
- `runServices` 指定あり: primary の `service` と `runServices` の和集合をサービス引数として `docker compose up -d <services...>` に渡す。
- image / Dockerfile モード、または `dockerComposeFile` と `service` が揃っていない構成で `runServices` を指定した場合はエラーとする。
- サービス依存関係の起動順、`depends_on`、healthcheck、profiles の扱いは Compose CLI に委譲する。
- `down` / attached `up` 終了時の `stopCompose` は、`runServices` のサービス引数で対象を狭めず、Compose プロジェクト全体を停止する。これは Compose が `depends_on` 等で暗黙に起動した依存サービスを残さないためである。`remove` はプロジェクト全体を削除対象にする。

### 8.6 ビルド / pull / 再作成

Compose モードのイメージ作成は次の順で行う。

1. `initializeCommand` をホストで実行する。
2. 利用者の Compose ファイルだけで `docker compose config --format json` を実行し、primary service のベースイメージ / ビルド情報を検証する。
3. `docker compose build` または `docker compose up -d --build` で primary service と必要なサービスのイメージを準備する。`--no-cache` と `--pull` は Compose のビルドオプションに反映する。
4. primary service のベースイメージを特定する。Compose サービスに `build` がある場合は Compose がタグ付けしたサービスイメージを使う。`image` がないビルド専用のサービスでは Compose の既定タグ `<project-name>-<service>` を使う。サービスに `image` のみがある場合はそのイメージを使い、メタデータ解決前に存在しないイメージを pull する。
5. Feature、UID/GID 同期、entrypoint shim が必要な場合、ベースイメージに decune が生成するレイヤーを重ね、decune が生成するイメージタグを作る。
6. decune-generated Compose override に primary service のイメージ差し替えを反映する。decune が生成したローカルイメージに差し替える場合は `pull_policy: never` も反映する。
7. decune-generated Compose override 込みで `docker compose up -d` を実行する。`--pull` または `rebuild` の場合は `--force-recreate` を渡す。
8. `docker compose ps --format json` と `docker inspect` で primary container の ID を解決し、lifecycle とシェル接続に進む。

`--pull` は利用者の Dockerfile のビルド、ベースイメージの pull、Compose サービスのビルド / pull にだけ適用する。Feature、UID/GID 同期、entrypoint shim などの decune が生成するレイヤーは、直前に準備したローカルイメージのタグを `FROM` にすることがあるため、これらのレイヤーのビルドには Docker ビルドの `--pull` を渡さない。

`rebuild` は生成イメージと Compose サービスを再作成する。匿名ボリュームは保持する。`remove --images` 以外で利用者のイメージや Compose サービスのイメージを削除してはならない。

### 8.7 decune-generated Compose override

Compose モードで decune 固有機能を適用するため、状態 / ランタイムディレクトリに decune-generated Compose override のファイルを作る。このファイルは利用者が編集しない。

目的:

- primary service に decune のラベルを付与する。
- primary service のイメージを Feature / UID/GID 同期 / entrypoint 適用済みの最終イメージに差し替える。
- primary service のイメージを decune が生成したローカルイメージに差し替える場合、元の Compose サービスの `pull_policy` を引き継いでレジストリから pull しないよう、decune-generated Compose override で `pull_policy: never` を明示する。
- `containerEnv`、`containerUser`、`init`、`privileged`、`capAdd`、`securityOpt`、`mounts`、dotfiles のマウント、認証情報 / 実行時のマウントを primary service に追加する。
- `overrideCommand = true` の場合、primary service のコマンドを keepalive 用のコマンドに差し替える。
- Compose published port mapping/relocation で planned endpoint が requested endpoint と異なるサービスの `ports` を、planned の `published` / `host_ip` を持つリストに置換する。
- clone isolation の name rewrite が有効な場合、対象サービスの `container_name` とトップレベルリソースの `name` をワークスペース固有名へ書き換え、元のコンテナ名をネットワークエイリアスとして追加し、対象コンテナ名へのサービス内参照を追随させる。
- 秘密情報の値を override のファイルに書かない。GitHub のトークンはホストのランタイムファイルを bind mount し、トークンの値自体はファイル内容にのみ存在する。

decune-generated Compose override のファイルは、利用者の `dockerComposeFile` より後に `-f` で渡す。計画作成時の検証、primary service / コンテナの解決、reuse hash に含める canonical Compose model は、利用者の `dockerComposeFile` だけを `docker compose config --format json` で正規化したモデルとする。decune-generated Compose override 自体は Compose YAML として decune が生成し、ハッシュには最終の canonical model ではなく decune-generated Compose override semantic hash input として別に含める。

### 8.8 published port mapping と relocation

`[compose.published_ports]`(5.11 節)の mapping と automatic relocation の対象は fixed TCP published port に限る。計画作成は以下の契約に従う。

mapping の解決:

- mapping は canonical Compose model の `service + protocol + target` に一致するポートエントリを解決する。存在しないサービス、active なサービス内で一致するエントリが 0 件または複数件、または一致エントリが fixed TCP published port でない場合は `compose_published_port_mapping_invalid` で起動前にエラーにする。存在するが今回の active なサービス集合に含まれないサービスの mapping は、その実行では適用しない。
- 同じポートエントリでは explicit published port mapping、同一 Compose プロジェクトの既存バインディング、Compose ファイルの requested endpoint の順に優先する。mapping のエンドポイントが reservation または availability probe と衝突した場合は `compose_published_port_mapping_conflict` とし、automatic relocation へフォールバックしない。mapping 自身が requested endpoint と同じ場合も計画作成の対象だが、エンドポイントの差分がなければ decune-generated Compose override は不要である。
- 同一 Compose プロジェクトで実行中の別の mapping identity が保持するエンドポイントは、再作成の計画作成でも reservation として扱う。複数の mapping のエンドポイントを相互に入れ替える場合、実行中のプロジェクトに対してアトミックな入れ替えは行わず `compose_published_port_mapping_conflict` とする。既存のバインディングを解放するため `decune down` の後に `decune rebuild` を実行する。
- mapping によりホスト側ポートまたはホスト IP が変わる場合は relocation として扱う。既存コンテナのバインディングと異なれば再作成が必要であり、decune-generated Compose override は `published` と `host_ip` の両方を planned endpoint に合わせる。

reservation とプローブ:

- イメージメタデータや Feature メタデータをマージした後の最終的な `forwardPorts` / `[[ports]]` / CLI `-p` の forwarding reservation を考慮し、同じホスト側エンドポイントを Compose published port と decune の port forwarding の両方へ割り当ててはならない。
- mapping または automatic relocation が active な実行では、接続先 Docker デーモンの実行中コンテナを列挙し、`NetworkSettings.Ports` にある実際の TCP published binding を外部の reservation として扱う。現在の Compose プロジェクトのラベルを持つコンテナはここから除外し、同一プロジェクト内のバインディングは既存バインディングの規則で別途扱う。`docker ps` と inspect の間にコンテナが消えた場合は残った inspect 結果で継続し、それ以外の列挙 / inspect のエラーは文脈付きのエラーとして失敗にする。
- 外部の reservation は requested endpoint と relocation の候補の両方に適用する。IPv4 のワイルドカード `0.0.0.0` は IPv4 アドレスと、IPv6 のワイルドカード `::` は IPv6 アドレスと同じホスト側ポートで衝突し、IPv4 と IPv6 のファミリは分離する。この判定は decune の forwarding reservation と同じ helper contract を使う。
- availability probe は decune のプロセスからの TCP bind プローブで行う。プローブが `AddrInUse` で失敗したホスト側ポートは使用中と扱う。プローブが `PermissionDenied` で失敗したホスト側ポートは、特権ポートなど decune のプロセスの権限では空き・占有を判別できない unprobeable なポートとして扱い、使用中とも利用可能とも予期しないエラーとも区別する。`PermissionDenied` 以外の予期しないプローブのエラーは従来どおり縮退せずエラーにする。

計画作成:

- 同一 Compose プロジェクトの既存コンテナが同一サービス / 同一プロトコル / 同一の対象ポートの published binding を持つ場合、要求されたポートより既存バインディングのホスト側ポート維持を優先する。実行中のコンテナ由来のバインディングは、自プロジェクトが bind しているものとして availability probe なしで採用してよい。停止中のコンテナ由来のバインディングは実際には bind されていないため、採用前に availability probe を行う。停止中のコンテナ由来のバインディングが unprobeable な場合は、そのバインディングを採用して実際の bind の成否を Docker デーモンに委ねる。
- 既存のバインディングが使えない場合、要求されたホスト側ポートを試す。要求されたホスト側ポートが unprobeable な場合は、reservation と衝突していない限り要求されたホスト側ポートを維持して実際の bind の成否を Docker デーモンに委ねる。reservation には最終的な forwarding reservation、同じ計画内で割り当て済みの Compose published port、同一 Compose プロジェクトの実行中コンテナ由来の既存 published binding を含める。停止中のコンテナ由来のバインディングは予約にはしない。
- 要求されたホスト側ポートが使用中または reservation と衝突する場合は、ホスト IP の指定方法を維持したまま要求されたホスト側ポート + 1 から昇順に relocation の候補を探索する。relocation の候補が unprobeable な場合は採用せず、次の候補へ進む。OS 割り当てポートへのフォールバックは行わず、65535 まで候補がない場合はエラーにする。
- Docker の実際のバインディングの reservation で検出できないプロセスが、unprobeable な要求されたホスト側ポートまたは既存バインディングを使用していた場合、Docker/Compose 起動時の published port 衝突の診断になる。
- 既存コンテナの実際の published binding と新しい計画の planned endpoint が異なり、コンテナを再作成しなければ起動できない場合、decune は published port relocation による再作成であることを警告し、`docker compose up --force-recreate` 相当で自動再作成して起動を継続する。この警告は `warn_on_relocation` と独立に常に出す。
- relocation 済みのバインディングは、塞いでいた要因が消えて要求されたホスト側ポートが再び利用可能になっても維持する。要求されたホスト側ポートへ戻すのは再作成時のみである。
- mapping または relocation により実際にホスト側ポートかホスト IP を変更する場合、decune-generated Compose override は Compose の `!override` タグでサービスの `ports` を置換する。このため Docker Compose v2.24.4 以上が必要で、バージョンを判定できない、または古い Compose ではエラーにする(2.2 節)。
- UDP、ポート範囲、コンテナ側のみのポートエントリ、`expose`、`network_mode: host` のサービスにあるポートのマッピングは relocation の対象外であり、存在するだけでは警告しない。

ポリシーと独立な published port の診断条件:

- 実効レプリカ数が 2 以上のサービスが fixed TCP published port を持つ場合、decune はレプリカごとのホスト側ポートの割り当てを行わず `compose_published_port_multi_replica_unsupported` でエラーにする。実効レプリカ数は Docker Compose config の `scale`、なければ `deploy.replicas` から読む。
- 不正なホスト IP、不正な形式のポート表記、予期しない availability probe のエラーは単純な衝突として扱わず、decune が判定できる場合は `compose_published_port_invalid` でエラーにする。

diagnostic code の定義一覧は 13.1 節。

### 8.9 clone isolation

#### 8.9.1 分離境界

Compose-based configuration の複数クローンを同一 Docker デーモン上で同時利用するとき、decune は次の境界でリソースを分離する。

| Category | Resources |
| --- | --- |
| Always workspace-scoped | project name、generated image、default network / volume |
| Opt-in rewrite | fixed TCP port / name / IPv4 subnet、declared endpoint |
| No automatic rewrite | external resource、IPv6、static service address、undeclared endpoint |

- 常にワークスペース単位となるリソースは、`safe_workspace_slug` と `workspace_id`、またはワークスペース固有の Compose プロジェクト名によりクローンごとに分離する。
- オプトインの書き換えは `[compose.clone_isolation].enabled = true` を全体の有効化スイッチとし、published port と固定名をワークスペース固有値へ、network relocation と clone isolation endpoint 宣言を明示した対象を relocation 後の値へ書き換える。
- 自動で書き換えないリソースのうち、external なリソースは利用者の共有契約を維持する。relocation 対象ネットワークの IPv6 / 固定アドレスは、対処方法が分かる診断で停止する。relocation 後も環境変数に残る旧エンドポイントのアドレスは起動前に診断するが、宣言なしに値を推測して書き換えない。
- clone isolation は external なリソースのクローン別複製や共有設定を自動化しない。

#### 8.9.2 preflight

Compose モードの `up` / `rebuild` は、利用者の Compose ファイルだけから得た canonical Compose model を使い、`docker compose up -d` の前に clone isolation preflight を常時実行する。

- `runServices` が指定されている場合、走査対象は primary service と `runServices`、Docker Compose がそれらの依存関係として展開したサービス、およびそのサービス群が使用するトップレベルリソースに限定し、起動対象ではないサービスと未使用のリソースは走査しない。`runServices` が指定されていない場合は Compose プロジェクト全体を走査する。
- preflight 自体は利用者の Compose ファイルを変更しない。`[compose.clone_isolation]` の name rewrite、network relocation、endpoint の書き換えが有効な対象は decune-generated Compose override で書き換え、衝突の照合にも書き換え後の値を使う。オプトインが無い対象は検出のみを行う。

対象:

- `networks.*.ipam.config[].subnet` に固定 IPv4 サブネットを持つ external ではないネットワーク。既存 Docker ネットワークの `IPAM.Config[].Subnet` と重複する場合、`compose_network_subnet_overlap` でエラーにする。IPv6 サブネットはこの preflight の重複判定の対象外である。
- サービスの `container_name`。
- トップレベルの `networks` / `volumes` / `configs` / `secrets` の `name:`。ただし Docker Compose が自プロジェクト名で名前空間を分けた既定名と一致するものは固定名扱いしない。

照合規則:

- `external: true` のトップレベルリソースは、利用者が共有リソースとして扱う契約なので clone isolation preflight の対象外である。
- 既存 Docker リソースとの照合では、`com.docker.compose.project` ラベルが現在の decune の Compose プロジェクト名と一致するリソースを自プロジェクトとみなし、衝突相手から除外する。ラベルが無いリソースは他のリソースとして扱い、衝突相手に含める。
- 固定 IPv4 サブネットの重複は、同じ IPAM ドライバかつ同じ IPAM のアドレス空間に属するネットワーク間だけを衝突として扱う。IPAM ドライバ未指定は `default` とみなす。Compose ネットワークの既定ドライバ、`bridge`、`macvlan`、`ipvlan` はローカルのアドレス空間、`overlay` はグローバルのアドレス空間とみなし、既存 Docker ネットワークの `Scope` が `local` ならローカル、`swarm` または `global` ならグローバルとみなす。カスタムのネットワークドライバ、欠落した `Scope`、未知の `Scope` などアドレス空間を確定できないメタデータは、実際の衝突を見逃さないため保守的に比較対象へ含める。
- 固定名が同種の既存 Docker リソースと衝突する場合、`compose_fixed_name_conflict` でエラーにする。診断メッセージには Compose 側のリソース、要求したサブネット / 名前、衝突相手の Docker リソース名、衝突相手の `com.docker.compose.project` ラベルがあればその値を含める。
- 複数の衝突を検出した場合、preflight は最初の 1 件だけでなく、検出したすべての診断を 1 回のエラーにまとめて報告する。
- canonical Compose model にクローン間で衝突しうる構成が 1 つも無い場合、decune は clone isolation preflight のための Docker デーモンのリソース照会を行わない。

#### 8.9.3 network relocation

`enabled = true` かつ `networks.relocation = true` の固定 IPv4 サブネットの relocation は次の契約に従う。

- スロット数は `2^(subnet_prefix - pool_prefix)` とする。`subnet_prefix` 省略時は元のサブネットのプレフィックス長を使う。
- 初期スロットは SHA-256 の入力 `decune-clone-isolation-subnet-v1:<workspace_id>:<compose-network-key>` の先頭 8 バイトをビッグエンディアンの整数として読み、スロット数で剰余を取って決める。そこから線形探索し、自プロジェクト以外の同じ IPAM のアドレス空間にあるデーモンのネットワークのサブネット、または同一計画で割り当て済みのサブネットと重複するスロットを飛ばす。空きがなければ `compose_clone_isolation_pool_exhausted` でエラーにする。
- 別プロセスの relocation preflight とネットワーク作成はアトミックではない。同じ初期スロットを選ぶ複数の `decune up` を同時に実行すると、相互のネットワークがデーモンのスナップショットにまだ現れず、後続の Docker ネットワーク作成がサブネット重複で失敗する場合がある。その場合は、先に成功した起動のネットワーク作成後に失敗した `decune up` を再実行し、最新のデーモンのスナップショットから再計画する。
- 元の IPAM 設定にゲートウェイがある場合、元のサブネット内のホストのオフセットを新しいサブネットでも保存する。オフセットが新しいプレフィックスに収まらなければ `compose_clone_isolation_invalid` でエラーにする。元のゲートウェイがなく、対応する clone isolation endpoint 宣言から `.gateway` が参照されている場合は planned のサブネットの先頭ホストアドレスを明示ゲートウェイとして生成する。それ以外ではゲートウェイを生成しない。
- 元の IPAM 設定に `ip_range` がある場合は CIDR のプレフィックスと元のサブネット内のネットワークアドレスのオフセットを、`aux_addresses` がある場合は各マップキーとアドレスのオフセットを新しいサブネットでも保存する。フィールドが IPv4 でない、元のサブネット外にある、またはオフセットを新しいプレフィックスに収容できない場合は、Docker リソースを変更する前に `compose_clone_isolation_unsupported` または `compose_clone_isolation_invalid` で停止する。診断にはネットワークキーとフィールド名を含め、フィールドの値全体は含めない。
- 同じ Compose プロジェクトの既存ネットワークが、対象の Compose ネットワークキーに対してプール内の重複しないサブネットを保持していれば最優先で再利用する。次に状態の前回割り当てを再利用する。通常の `up` では塞いでいた要因が消えても割り当てを維持し、要求されたサブネットを再度優先するのは再作成時だけとする。
- 自プロジェクトの既存ネットワークと新しい計画のサブネット、元の設定の明示ゲートウェイまたはエンドポイント参照のために生成したゲートウェイ、`ip_range`、`aux_addresses` が一致しない場合、接続コンテナがなければネットワークを削除し、Compose に再作成させる。接続コンテナがある場合は `compose_clone_isolation_invalid` で停止し、`decune down` でプロジェクトを停止してから `decune rebuild` するよう案内する。これには、旧バージョンの decune が `ip_range` / `aux_addresses` を欠落させて作成したネットワークも含む。
- decune-generated Compose override は、planned のサブネットが要求されたサブネットと同じ場合も含め、トップレベルの `networks.<key>.ipam.config: !override` で IPAM の設定リストを置換し、`subnet`、明示 `gateway`、`ip_range`、`aux_addresses` を意味保存して再生成する。ネットワークの `driver` や IPAM の `driver` / `options` など設定リスト外の利用者設定は変更しない。relocation が有効で固定 IPv4 サブネットを検出した場合は、Compose の `!override` タグのため Docker Compose v2.24.4 以上が必要で、バージョンを判定できない、または古い Compose はエラーにする(2.2 節)。
- canonical Compose model の IPAM 設定に decune が意味を解釈できないフィールド、または `subnet` のない設定エントリがある場合は、リストの一部を黙って破棄せず `compose_clone_isolation_unsupported` で停止する。同じネットワークに未知のフィールドが複数ある場合は、フィールド名を決定的な順序ですべて列挙し、フィールドの値は含めない。
- `external: true` のネットワークは検出・書き換えの対象外とする。固定 IPv6 サブネット、および対象ネットワークに接続するサービスの `ipv4_address` / `ipv6_address` / `link_local_ips` は付け替えず、`compose_clone_isolation_unsupported` でエラーにする。

stale なエンドポイントの検出:

- ネットワークが実際に別のサブネットへ relocation された場合、preflight はそのネットワークに直接接続するサービスと、`network_mode: service:<service>` で接続を継承するサービスを対象にする。canonical Compose model の `services.*.environment` にエンドポイントの展開結果を後勝ちで重ねた実効的な文字列値を走査し、元のサブネットの基底 IPv4 アドレス、または元のゲートウェイが前後を数字・ドットとしないトークン境界付きで残っていれば、`compose_clone_isolation_endpoint_unsafe` で `docker compose up` 前にエラーにする。clone isolation endpoint 宣言があっても、同じ値に別の relocation されたネットワークの旧アドレスが残っていればエラーになる。`10.99.0.1` は `10.99.0.100` や `110.99.0.1` に一致しない。planned のサブネットが要求されたサブネットと同じ場合は stale の検出を行わない。
- stale の検出対象はサービスの環境変数の値内の元のサブネットの基底アドレスと元のゲートウェイだけである。`aux_addresses` 自体は IPAM の設定内で付け替えるが、その元アドレスを環境変数、`extra_hosts`、サービスの `command`、設定ファイルの内容などから参照していても自動検出・書き換えしないため、該当する外部のエンドポイント契約は利用者が確認する。
- 診断にはサービス名、環境変数名、Compose のネットワークキー、一致した元アドレスだけを含め、環境変数の値全体を状態、ラベル、ログ、reuse hash、診断メッセージへ残してはならない。

#### 8.9.4 clone isolation endpoint 宣言

`endpoints.value` では `${decune.network.<compose-network-key>.gateway}` と `${decune.network.<compose-network-key>.subnet}` の 2 形式を clone isolation 専用のプレースホルダーとして予約する。これは一般の decune config の変数展開(6 章)とは別に扱う。

- endpoint の書き換えの preflight は、サービスと Compose のネットワークキーの存在、および参照先ネットワークが固定 IPv4 サブネットの relocation の対象であることを検証し、プレースホルダーを planned のゲートウェイまたは CIDR 表記の planned のサブネットへ文字列置換する。
- 未知または未終端の decune プレースホルダー、存在しないサービス / ネットワーク、relocation 対象でないネットワークへの参照は `compose_clone_isolation_invalid` でエラーにする。`enabled = true` でも `networks.relocation = false` のままプレースホルダーを参照した場合は、network relocation を有効にする設定のヒントを付けて同じ診断でエラーにする。
- decune のプレースホルダー以外の `$` はそのままの文字列としてコンテナの環境変数へ渡し、Compose のホスト環境変数の展開は適用しない。
- 展開後の値は decune-generated Compose override の `services.<service>.environment.<env>` にマップ形式で書き込み、Compose の後勝ちのマップのマージにより利用者の Compose ファイルの値を置き換える。`!override` タグは使わない。
- 元の IPAM 設定にゲートウェイがなく `.gateway` プレースホルダーが参照された場合に限り、planned のサブネットの先頭ホストアドレスを明示ゲートウェイとしてネットワークの IPAM の上書きに追加し、その値を展開する。

#### 8.9.5 name rewrite

`enabled = true` の name rewrite は decune-generated Compose override に次の規則で出力する。

- `names.rewrite_container_names = true` のとき、サービスの明示的な `container_name: <name>` を `<name>-<workspace_id>` にする。`workspace_id` は正規化済みワークスペースパスから算出する 12 桁の小文字 16 進である。
- 書き換え対象サービスが接続するすべての Compose ネットワークに元の `container_name` をネットワークエイリアスとして追加する。利用者の Compose ファイルがサービスのネットワークを短縮リスト形式で指定していても、decune-generated Compose override のマップ形式と Docker Compose のマージによりエイリアスを追加する。
- active な canonical Compose model 内で、書き換え対象サービスの元の `container_name` を正確に参照する `services.*.network_mode` / `ipc` / `pid` の `container:<name>`、`volumes_from` の `container:<name>[:ro|rw]`、`external_links` の `<name>[:alias]` は、参照先だけを `<name>-<workspace_id>` へ追随させる。アクセスモードとリンクのエイリアスは維持する。サービス名を参照するエントリと、書き換え対象ではない外部コンテナへのエントリは変更しない。
- `volumes_from` / `external_links` は decune-generated Compose override の `!override` リストで完全置換し、書き換え対象外のエントリも元の順序と値を維持して再出力する。このリストの書き換えが実際に必要な場合だけ Docker Compose v2.24.4 以上を要求し、バージョンを判定できない、または古い Compose は Docker リソースを変更する前に `compose_clone_isolation_unsupported` で停止する。`network_mode` / `ipc` / `pid` のスカラー書き換えだけならこの追加のバージョン条件を課さない(2.2 節)。
- `names.rewrite_resource_names = true` のとき、トップレベルの `networks` / `volumes` / `configs` / `secrets` の明示的な `name: <name>` を `<name>-<workspace_id>` にする。
- `external: true` のトップレベルリソースは共有契約を維持し、書き換えない。

利用者側の追随:

- 固定名ボリュームの書き換えは、クローンごとに別のボリュームを使いデータを分離する。
- 元の `container_name` を指定して Compose プロジェクト外から実行する `docker exec <name>` などのツールは、書き換え後の名前へ追随する必要がある。
- Compose ネットワーク内から元の名前を使う接続はネットワークエイリアスで維持するが、名前空間の共有、ボリュームの継承、legacy link は DNS の名前解決ではないため、それぞれの明示参照を書き換える。

ハッシュの扱い:

- name rewrite の結果値である書き換え後のコンテナ / リソース名、元の `container_name` のために生成するネットワークエイリアス、および追随して書き換えるコンテナ名の参照は decune-generated Compose override semantic hash input に含めない。これらは workspace id と canonical Compose model から決定的に導出される relocation の結果値として扱う。
- name rewrite のポリシー自体と利用者の Compose ファイルの元の名前・元の参照は従来どおり reuse hash の入力に含める。

clone isolation の diagnostic code の定義一覧は 13.2 節。

## 9. ポート

### 9.1 forwarding と published port の区別

`forwardPorts`、decune `[[ports]]`、CLI `-p` は port forwarding であり Docker published port ではない。ホスト側の待ち受けアドレスの既定は `127.0.0.1`。コンテナ内で `127.0.0.1:<container port>` にだけ待ち受けているプロセスにも届くよう、コンテナ側の `decune-forward-agent` 経由で中継する。

### 9.2 TCP-only

port forwarding と decune が生成する published port のメタデータは TCP のみとする。CLI `-p`、decune `[[ports]]`、Dev Container `forwardPorts`、Dev Container `appPort` はプロトコルサフィックスなしを TCP として扱い、`/tcp` は明示的な TCP 指定として受け付ける。`/udp` は未対応のエラーとする。decune は UDP の転送と、Dev Container `appPort` からの UDP published port のメタデータ生成に対応しない。

`decune ports` は Docker の container inspect から読み取ったバインディングを表示するため、Docker published port に UDP のバインディングが含まれる場合は、そのバインディングも現在有効なホスト側ポートとして表示する。

### 9.3 `appPort`

`appPort` は image/Dockerfile モードの Docker published port であり、コンテナの作成時に決まる。ホスト IP が指定されない場合、Docker の既定ですべてのインターフェースに公開される可能性があるため警告の対象とする。`appPort` の published port のメタデータも TCP のみである。

Compose モードでは Docker published port 設定は Compose ファイルの `ports` に委譲する。`appPort` は未対応のエラーとする。

### 9.4 ホスト IP の指定

CLI `-p` と Dev Container `appPort` のホスト IP は IPv4 / ホスト名 / 角括弧付き IPv6 を受け付ける。IPv6 のホスト IP は `[::1]:8080:3000` のように角括弧付きの形式で指定し、内部のモデルでは角括弧なしで保持する。角括弧なしの IPv6 はコロン区切りと曖昧なためエラーとする。`forwardPorts` の文字列の `[::1]:3000` はホスト IP `::1` への転送として扱い、`[::1]:8080:3000` のようなホスト側ポートのマッピングは `forwardPorts` では未対応のエラーとする。

### 9.5 manual forwarding の優先順位とフォールバック

manual forwarding の優先順位:

1. CLI `-p`
2. project decune `[[ports]]`
3. devcontainer `forwardPorts`
4. global decune `[[ports]]`

ホスト側ポートが占有済みの場合、昇順で空きポートを探索し、上限に達した場合は OS 割り当てのポートへフォールバックする。`require_local = true` なら要求したホスト側ポートと実際の転送ポートが異なる場合に警告し、false なら警告なしでフォールバックする。空き確認後に別のプロセスがポートを取得した場合も、待ち受けの bind 時に再度フォールバックする。

forwarding のホスト側ポートの予約は IP ファミリの境界を尊重する。IPv4 のワイルドカード `0.0.0.0` は IPv4 アドレスとだけ衝突し、IPv6 のループバック / 具体アドレスとは同一ホスト側ポートを共有できる組み合わせとして扱う。同様に IPv6 のワイルドカード `::` は IPv6 アドレスとだけ衝突する。

### 9.6 Compose モードの service 解決と sidecar forwarding

- `forwardPorts` の数値: primary service のポート。
- `forwardPorts` の文字列 `"3000"`: primary service のポート。
- `forwardPorts` の文字列 `"db:5432"`: Compose サービス `db` のポート。
- `portsAttributes` のキー `"db:5432"`: Compose サービス `db` のポート属性。
- `[[ports]].service = "db"`: Compose サービス `db` のポート。

`forwardPorts` の `"service:port"` 形式と `[[ports]].service` は Compose モード専用である。image/Dockerfile モードではサービス名で対象コンテナを解決できないため未対応のエラーとする。

sidecar service の forwarding は、そのサービスのコンテナ ID を解決し、必要な container-side tool を実行時にインストールして port forward agent を起動する。対象サービスには forwarding 用の実行時マウントと decune の identity ラベルだけを decune-generated Compose override で追加し、認証情報、dotfiles、GitHub のトークン、SSH agent は自動注入しない。サービスのレプリカが 2 以上ならエラーとする。

### 9.7 automatic forwarding

automatic forwarding は TCP で待ち受けているソケットのみを対象にする。コンテナ側の agent が `/proc/net/tcp` と `/proc/net/tcp6` を読み、TCP の LISTEN ポートを検出する。UDP のソケットは検出・転送しない。既定のスキャン間隔は 2 秒、初回遅延は 3 秒。

manual forwarding 済みのポート、Docker published port として扱われるポート、除外リスト、`portsAttributes.onAutoForward = "ignore"` は除外する。Compose モードの automatic forwarding は primary service のみを対象にする。sidecar service は明示的な `forwardPorts` または `[[ports]].service` で指定する。

### 9.8 現在有効なポートの確認

現在有効なホスト側ポートの利用状況は `decune ports` で確認できる。forwarding の実効的な対応は、`decune up` のプロセスがランタイムディレクトリに公開するホスト内の status ソケットに問い合わせる。Docker published port は Docker の container inspect からバインディングを読み取る。forwarding の実効的な対応は `state.toml` には保存しない。Compose published port relocation の状態メタデータがある場合も、現在有効な published port は Docker の container inspect から読み取ったバインディングを正とする。stale なメタデータや接続できない status ソケットは、現在有効な forwarding ではないものとして無視する。

## 10. Docker リソースと状態

### 10.1 workspace id とリソース名

workspace id:

```text
hex(sha256(canonical_workspace_path))[0..12]
```

Docker/Compose のラベルから読み取る `decune.workspace_id` は、12 桁の小文字 16 進 (`[0-9a-f]{12}`) に完全一致する場合だけワークスペースの identity や状態 / ランタイムパスの解決に使う。

image/Dockerfile モードの Docker リソース名にはワークスペースのディレクトリ名をそのまま使わず、ASCII safe slug と workspace id を組み合わせる。

- コンテナ: `decune-<safe_workspace_slug>-<workspace_id>`
- イメージ: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- 状態ディレクトリ: `$XDG_STATE_HOME/decune/<workspace_id>` または `~/.local/state/decune/<workspace_id>`
- ランタイムディレクトリ: `$XDG_RUNTIME_DIR/decune/<workspace_id>` または `/tmp/decune-<uid>/<workspace_id>`

Compose モード:

- プロジェクト: `decune-<safe_workspace_slug>-<workspace_id>`
- 生成する primary のイメージ: `decune/<safe_workspace_slug>-<workspace_id>:<config_hash>`
- decune-generated Compose override: `$XDG_STATE_HOME/decune/<workspace_id>/compose.override.yaml`
- 状態 / ランタイムディレクトリは image/Dockerfile モードと同じ。

### 10.2 ラベルと再利用

主な decune のラベル:

- `decune.managed=true`
- `decune.workspace=<canonical_workspace_path>`
- `decune.workspace_id=<workspace_id>`
- `decune.config_hash=<hash>`
- `decune.version=<version>`
- `devcontainer.local_folder=<canonical_workspace_path>`
- `devcontainer.config_file=<path>`

Compose モードでは上記のラベルを primary service に追加する。明示的な sidecar service forwarding の対象サービスには、forwarding 用の実行時マウントの再作成判定に必要な `decune.managed=true` と `decune.workspace_id=<workspace_id>` を追加する。Compose が付与する `com.docker.compose.project` と `com.docker.compose.service` もコンテナの identity に使う。`com.docker.compose.*` のプレフィックスを decune-generated Compose override で上書きしてはならない。

既存のコンテナ / プロジェクトの再利用は `decune.managed=true` と `decune.workspace_id` が一致するものに限る。他のツールのコンテナは拾わない。

### 10.3 reuse hash

reuse hash に含める入力:

- 解決済みのメタデータ / 設定、Feature lock、関係する CLI オプション。
- Dockerfile の内容、`build.options`、有効な ignore ファイル、ビルドコンテキストの digest。
- entrypoint の計画、Linux ホストの UID/GID 同期の入力。
- Compose モードでは、利用者の Compose ファイルから得たサニタイズ済みの canonical Compose model、Compose ファイルの digest、decune-generated Compose override semantic hash input。

reuse hash に含めない入力:

- manual/automatic forwarding の現在値。
- `container.cli.enabled`。
- Compose published port relocation により生成されるサービス `ports` の上書き。
- clone isolation の network relocation により生成されるサブネット / ゲートウェイ。
- credential のトークンの値、SSH agent のソケットパス、GitHub のトークンファイルのパス。
- `${localEnv:...}` 由来の `remoteEnv` の値。
- Compose secrets の解決済みの値。

secret-sensitive value の扱い:

- `${localEnv:...}` 由来の `containerEnv` の値は平文では含めず、コンテナ作成時の環境変数の変更を検出するため非可逆な digest として含める。
- Compose モードでは、利用者の Compose ファイルだけを対象にした `docker compose config --format json` が解決した変数展開 / env ファイル / profile / マージの結果から、`services.<service>.environment` の末端の値を平文ではなく digest マーカーに置き換えた canonical Compose model をハッシュに含める。
- この digest の入力は `decune-compose-env-value-hash-v1` で domain separation とバージョン付けをし、JSON のパス、JSON の値の型、正規化した JSON の値を含める。digest マーカーは `decune-compose-env-value-hash-v1:sha256:<hex>` 形式とし、環境変数の値の平文を状態、ラベル、ログ、reuse hash の入力に残してはならない。

decune-generated Compose override semantic hash input:

- primary service、decune が追加するラベル / 環境変数 / マウント / ユーザー / セキュリティオプション / 起動コマンド、および decune が生成したイメージへ差し替えるかどうかを含める。
- `${localEnv:...}` 由来の `containerEnv` の値は redaction 済みのマーカーまたはプレースホルダーとして扱い、実値をハッシュの入力にしない。
- decune-generated Compose override 内の `decune.config_hash` ラベルやハッシュ由来のイメージタグなど、ハッシュ自身から派生する値は循環を避けるためハッシュの入力にしない。

clone isolation の relocation の結果値:

- clone isolation の name rewrite により生成されるコンテナ / リソース名、元の `container_name` のために生成するネットワークエイリアス、追随して書き換えるコンテナ名の参照、network relocation により生成されるサブネット / ゲートウェイ、エンドポイントのプレースホルダーの展開後の環境変数の値は relocation の結果値なので、decune-generated Compose override semantic hash input には含めない。
- clone isolation のポリシーとエンドポイントの未展開のテンプレートは reuse hash の入力に含める。

### 10.4 状態ファイル

状態ファイルは `$XDG_STATE_HOME/decune/<workspace_id>/state.toml` に保存する。状態ファイルは decune の内部形式であり、以下の互換性契約だけを公開挙動とする。

- 書き込みはアトミックに行う。
- Docker/Compose のラベルと状態が矛盾する場合、コンテナ / プロジェクトの identity と reuse hash は実行時のラベルを正とする。
- lifecycle の完了マーカーと `devcontainer.json` のパスは状態に記録し、作成時 lifecycle の二重実行の防止や `up --config` 後の Compose プロジェクトの lifecycle 復元に使う。
- 状態には起動時のモードを `image` / `dockerfile` / `compose` のスナップショットとして記録する。新規のコンテナと再利用したコンテナのどちらでも、その起動で解決したモードへ同期する。モードのフィールドがない既存の version 1 の状態は `unknown` として読み、状態の version は `1` を維持する。解決済みの設定全体や設定内容をモードのスナップショットのために保存しない。
- Compose published port relocation では、requested endpoint、planned endpoint、`relocated`、起動時に Docker inspect で観測した actual binding を表示補助のメタデータとして状態に記録する。このメタデータは現在有効な Docker のバインディングの正本ではない。
- Compose clone isolation の network relocation では、Compose のネットワークキーごとの要求されたサブネット、planned のサブネット、planned のゲートウェイ、`relocated` を表示補助のメタデータとして状態に記録する。現在有効なサブネットの正本は Docker の network inspect とする。
- `last_used_at` は `decune up` / `decune rebuild` がワークスペースを利用可能にした成功時だけ `unix:<seconds>` 形式で更新し、`created_at` / `last_started_at` から推測しない。`last_used_at` がない状態の最終利用表示は不明 / `-` とする。`status`、`ports`、`down`、`remove` / `rm`、`clean` は状態の最終利用情報を更新しない。

## 11. 配布の契約

公式配布は GitHub Releases のビルド済みアーカイブを第一導線とし、ソースコードからのローカルインストールを第二導線とする。crates.io publish と `cargo install --git` は公式導線にしない。

リリースアーカイブは以下を含む。

- `decune` binary
- `LICENSE`
- `README.md`

release asset:

- `decune-v{version}-{host_triple}.tar.gz`
- `SHA256SUMS`
- `release-manifest.json`

`scripts/install.sh` はリリースアーカイブのインストール補助として提供する。`latest` の自動解決は行わず、利用者が指定したバージョンの OS/arch 対応 asset を取得し、`SHA256SUMS` で検証してからインストールする。

初期のターゲット:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

release asset は `SHA256SUMS` で検証できる。GitHub Actions のリリースワークフローは build provenance attestation を作成し、リリース公開前に全 asset を draft release に添付する。

container-side tools:

- container-side tools(`git-credential-decune`、`decune-forward-agent`、コンテナ内 CLI)はリリースビルド時にホストのバイナリへ埋め込む。コンテナ内 CLI の bundle 内の artifact 名と利用者向けのコマンド名は `decune` とする。
- container-side tool のプラットフォームは `linux-amd64` と `linux-arm64` とする。
- Git リポジトリには生成済みのバイナリ成果物を入れない。
- container-side tool の実行時の配置はアトミックに行い、SHA-256 検証済みのバイトだけを実行時の配置先として公開する。部分的に書き込まれた artifact をコンテナから見えるパスに公開してはならない。

ソースの checkout からのローカルインストールは `cargo run --locked -p xtask -- install --locked` を公式入口とする。このコマンドは container-side tools bundle をビルド / 検証し、bundle を埋め込んだ `decune` をインストールする。container-side tools bundle を埋め込まないビルドは正式なインストール手順ではない。

`decune --version` はリリースタグから作る公式成果物では `decune {version}` を表示する。ソースの checkout からのローカルビルドでは、タグ外のコミットや dirty な作業ツリーを公式成果物と区別できるように SemVer のビルドメタデータの接尾辞を表示してよい。Git 情報を取得できないソースビルドでは、ソースビルドであることを示す接尾辞を表示してよい。

## 12. セキュリティ境界

### 12.1 実行され得るコードと到達性

- `decune up` は Dockerfile、Compose サービスのビルド、local/OCI Feature の `install.sh`、Feature / lifecycle command、decune hook、`userEnvProbe` 対象のシェル起動ファイルを実行し得る。
- Dev Container のメタデータと Compose ファイルは、bind mount、`privileged`、`capAdd`、`securityOpt`、published port、SSH agent forwarding、Git/GitHub credential forwarding によりホストや秘密情報への強い到達性をコンテナへ与え得る。
- GitHub token forwarding を有効にすると、コンテナ内プロセスはトークンファイルにアクセスできる。
- 信頼していないリポジトリでは `.devcontainer/`、Compose ファイル、local Feature を確認し、必要に応じて `[credentials.git].https = "host-helper-read-only"`、`[credentials.git].ssh_agent = "off"`、`[credentials.git].enabled = false`、`[credentials.github].enabled = false` を設定する。

### 12.2 外部コマンド実行と redaction

- `decune` 本体は外部コマンドをシェル文字列で実行しない。argv の配列で子プロセスを起動する。
- ログには必要最小限のコマンド名とサニタイズ済みの argv を出す。秘密情報の値を argv に入れる必要がある設計は禁止する。
- 秘密情報の値をログ、状態、ハッシュ、ラベル、イメージレイヤーに保存してはならない。
- Docker CLI / Compose CLI の実行失敗は、実行した高レベルの操作、対象リソース、exit status、stderr の短い抜粋を含むエラーに変換する。stderr の全文に秘密情報が混じる可能性がある場合は redaction の規則を通す。
- JSON を読む操作は、CLI の JSON 出力を型付きのスキーマへパースする。

### 12.3 credential forwarding と到達性

Git HTTPS:

- `[credentials.git].https = "host-helper"` の場合、コンテナ内に `git-credential-decune` を配置し、Git credential helper として設定する。このヘルパーは decune host daemon にバージョン付きの JSON request を送り、ホストの `git credential fill/approve/reject` を実行する。
- `[credentials.git].https = "host-helper-read-only"` の場合もコンテナ側ヘルパーのプロトコルは同じである。decune host daemon がポリシーを適用し、`get` は `fill` として実行する一方、`store` / `erase` はホストの credential store に伝播せず、成功として何もしない。
- `https = "off"` または `enabled = false` の場合、decune host daemon は Git credential 要求をホストの Git credential helper に渡してはならない。

SSH agent:

- `ssh_agent = "auto"` ではホストの `SSH_AUTH_SOCK` が Unix ソケットの場合のみ転送を設定する。コンテナの環境変数 `SSH_AUTH_SOCK` は `/run/decune/ssh-agent.sock`。`ssh_agent = "required"` でソケットが利用できない場合はエラー。
- Compose モードでは SSH agent のソケットのマウントは primary service にのみ追加する。

GitHub CLI:

- ホストの `gh auth token` が成功した場合、トークンをランタイムディレクトリにモード 0600 のファイルとして作り、コンテナには `/run/decune/secrets/github-token` として read-only でマウントする。`GH_CONFIG_DIR=/run/decune/gh` は書き込み可能な一時ディレクトリとする。トークンファイルは `up` 終了時に内容を消去し、`down` / `remove` で削除する。
- Compose モードでは GitHub のトークンファイルのマウントは primary service にのみ追加する。

### 12.4 decune host daemon

decune host daemon は `decune up` の子タスクとして起動し、`up` 終了時に停止する。常駐のシステムデーモンではない。

責務:

- Git credential helper の request の処理。
- GitHub のトークンファイルの一時管理。
- port forwarding の実行時のソケット基盤。
- attached session の現在のワークスペースに限定した decune container CLI query の処理。

protocol:

- container-side tool と decune host daemon の JSON プロトコルバージョンは `1` とする。request の `version` と `type` はトップレベルの envelope で検証し、`credential` と `cliQuery` を request type として予約する。
- `cliQuery` は `version`、`type`、`command`、`format` だけを持つ厳格なスキーマとし、未知のフィールドは拒否する。`status` + `text`、`ports` + `text`、`ports` + `json` だけを実行し、`status` + `json` とその他の未対応の format は `unsupported_format`、未知のコマンドは `unsupported_command` とする。実効的な `container.cli.enabled` が false の場合、有効なクエリは `container_cli_disabled` とする。予約済みの `portForward` request は `not_implemented` のまま維持する。
- request body、接続数、同時クエリ数、クエリ処理時間には固定上限を設け、超過はそれぞれ `request_too_large`、接続待機、`cli_query_busy`、`cli_query_timeout` として扱う(13.3 節)。

ソケットと権限:

- ソケットは既定で `/run/decune/host-daemon.sock` をコンテナ側パスとして使う。
- ランタイムディレクトリは 0700、ソケットは 0600 を基本とする。権限の調整時も decune host daemon は Unix ソケットの peer UID を検証する。

daemon の再利用とバージョン:

- attached session の decune host daemon は実効的な `container.cli.enabled` と daemon query context を起動時に固定し、detached session の decune host daemon は実効設定値にかかわらずクエリポリシーを `Disabled` に固定する。
- クエリポリシーまたは daemon query context が異なる active な daemon は暗黙に共有せず、対象ワークスペースのすべての active な `decune up` session を終了してから再実行するようエラーにする。したがってクエリが有効な attached session と detached session は同時に daemon を共有しない。再利用した daemon を監視するセッションは、所有者の終了後も同じポリシーと同じ daemon query context で daemon を再起動する。プロトコルバージョン、peer UID/GID、Git HTTPS のモード、ソケットの inode 等の既存の再利用条件も維持する。
- プロトコルバージョンは `1` のままとし、capability の一覧、ビルドの SHA、daemon のリビジョンは追加しない。decune v0 段階では新旧の daemon とクライアントが混在する構成の互換性を保証しない。アップグレード時は対象ワークスペースのすべての active な `decune up` を終了してから、新しいバージョンで起動し直す。再利用判定で active な daemon のメタデータを現在のバージョンとして読めない場合、またはプロトコルバージョンが一致しない場合も暗黙に共有せず、バージョン不一致の可能性を示してすべての active な `decune up` の終了を促すエラーにする。

### 12.5 decune container CLI query の境界

クエリが扱う情報:

- decune container CLI query 用のモデルは、検証済みの workspace id、起動時のモード、コンテナの ID / 名前 / サービス、実行状態 / ヘルス、decune-managed ボリューム名、lifecycle / タイムスタンプ、サニタイズ済みのポートだけを保持する。
- 生の `ContainerInspect`、Docker/Compose のラベルマップ、ワークスペース / 設定のパス、生の reuse hash、環境変数、ビルド引数、秘密情報、マウント元、外部コマンドの生の stderr、他のワークスペースのリソースはモデル、キャッシュ、renderer へ渡さない。
- daemon query context は検証済みの workspace id とそこから導出する固定サーバーパスのコンテキストだけを保持し、live な設定やクライアント入力からホスト側パスを再解決しない。request のコマンド、format、パス、リソース名を Docker のフィルタやホスト側パスに使わない。
- 成功時の出力 / 警告とエラーの response には、秘密情報、生の reuse hash / ラベル、ホスト側パス、他のワークスペースの情報、外部コマンドの生の stderr を含めない。縮退し得る状態、forwarding、Docker の診断は、プレフィックスと末尾の改行を持たないサニタイズ済みのメッセージとして成功 response の `warnings` に格納する。テキスト / JSON の完成済みの出力は `output` だけに格納し、特に `ports` の JSON へ警告を混在させない。

認可と生存期間:

- decune container CLI query を処理するソケット接続は、起動時に解決したリモートユーザーと peer UID が一致する場合だけ認可する。`root` または別 UID からの接続は、認可の詳細を開示しない一般の接続エラーとする。
- クエリ用の daemon は attached な `up` の guard の生存期間中だけ提供し、detached session の daemon の lifecycle は変更しない。detached な `up` 後に artifact が残っていてもクライアントは attached session が必要であることを示す canonical unavailable error を返し、常駐の daemon や監視は追加しない。
- decune container CLI query は、active な attached `decune up` session の decune host daemon が存在する間だけ利用できる。detached モードは対象外とし、detached な `up` の lifecycle command のために decune host daemon が動作している間も `cliQuery` は `container_cli_disabled` で拒否する。

縮退:

- クエリが返す Docker evidence はサーバー側で短時間キャッシュされ、読み込み完了時点から最大 2 秒程度 stale になり得る。状態と forwarding status はキャッシュしない。
- Docker への問い合わせが失敗・縮退した場合は、生の stderr を含まないサニタイズ済みの警告付きのスナップショットとして返す。

### 12.6 decune container CLI artifact と symlink

artifact の配置:

- 実効的な `container.cli.enabled` が true の `up` は、コンテナの作成 / 再利用判定に使う primary のランタイム領域の準備時に、プラットフォームが一致する現在のホストの artifact を検証して `/run/decune/decune` のホスト側の実体へアトミックに置換する。
- artifact は `up` のセッション終了時と `down` では削除せず、次回の `up`、再作成、バージョン変更時に現在のホストの artifact へ置換する。実効設定が false の `up` は stale な artifact を削除する。
- 有効時の配置と無効時の削除は、対象が通常ファイルまたは symlink の場合だけ置換・削除し、symlink のリンク先は辿らない。ディレクトリその他のエントリはランタイム領域の破損としてエラーにする。
- ワークスペースの `remove` と stale なデータの削除は、既存のランタイムディレクトリの削除により artifact も削除する。
- 無効時の削除は lifecycle 上の後始末でありセキュリティ上の強制ではなく、daemon の `container_cli_disabled` response を正とする。

利用者向けの symlink:

- コンテナ起動後、decune host daemon を利用可能にしてから最初の利用者の lifecycle command を実行する前に、`root` での exec により `/usr/local/bin/decune -> /run/decune/decune` を期待状態に揃える。
- 有効時は `/usr/local/bin` がなければ作成し、配置先が不在なら symlink を作成する。リンク先が正確に一致するかの判定は symlink が保持するリンク文字列で行い、リンク先の存在は確認しないため、リンク先が存在しなくても正確に一致する symlink は準備済みとして変更しない。
- 通常ファイル、ディレクトリ、正確に一致しない symlink は、そのリンク先が存在するかにかかわらず衝突として扱い、上書き・削除しない。
- 親ディレクトリの作成失敗、read-only のルートファイルシステム、書き込み不可、衝突は、理由、既存の配置先を変更しなかったこと、直接実行するコマンド `/run/decune/decune` を含む英語の警告を出して `up` を継続する。
- 無効時はリンク先を辿らず、正確に一致する symlink だけを decune-managed とみなして削除し、その他の配置先は変更しない。symlink の検査と削除は別々のファイルシステム操作であり、コンテナ内のプロセスがその間に配置先を差し替える場合、decune-managed symlink だけを削除する保証は厳密ではなく、できる範囲の対応になる。decune-managed symlink の削除失敗も同じく警告に縮退する。
- decune が作成した可能性のある空 `/usr/local/bin` は追跡・削除しない。

注入対象の限定:

- artifact、primary の実行時マウント、decune host daemon のソケット、利用者向けの symlink の自動注入は、image/Dockerfile のコンテナまたは Compose の primary service のコンテナだけを対象とする。転送を指定しない sidecar service へ `/run/decune` を追加しない。
- 明示的な sidecar forwarding はサービス固有のランタイムディレクトリへ port forward agent だけを配置し、decune host daemon のソケット、decune container CLI の artifact、symlink を追加しない。
- 利用者が Compose ファイルその他の利用者定義のマウントで primary のランタイム領域の内容を sidecar と共有する構成は検出・拒否せず、decune の分離保証の対象外とする。

### 12.7 禁止事項

- コンテナから任意のホストコマンドを実行する API を提供しない。
- Docker のソケットをコンテナに暗黙にマウントしない。
- Compose プロジェクトに利用者が指定していない Docker のソケットのマウントを追加しない。

### 12.8 Notice / Warning の方針

`decune up` は、意図した設定どおりに動作するセキュリティ上の注意点については `Notice:` として表示する。設定が無視される、機能が縮退する、または補助処理の失敗から継続する場合は `Warning:` として表示する。

## 13. diagnostic code

この章は decune 固有の diagnostic code の発生条件を定義する。対処手順は仕様の対象外であり、[ports.md](ports.md) と [clone-isolation.md](clone-isolation.md) のトラブルシューティングを参照する。

### 13.1 Compose published port

発生条件の詳細は 8.8 節。

- `compose_published_port_multi_replica_unsupported`: 実効レプリカ数が 2 以上のサービスが、decune が対応しない fixed TCP published port を持つ。
- `compose_published_port_unsupported`: 起動失敗が、ホスト側エンドポイントを安全に照合できる範囲で decune が対応しない Compose published port のエントリに関係している。
- `compose_published_port_invalid`: 不正なホスト IP、不正な形式の表記、予期しない availability probe のエラーなど、単純な衝突ではない不正な published port の状態。
- `compose_published_port_collision`: fixed TCP published port の requested endpoint が使用できない。
- `compose_published_port_automatic_relocation_failed`: automatic relocation の候補を割り当てられない。
- `compose_published_port_bind_race`: 計画作成の後に別のプロセスが planned endpoint を取得した可能性がある。
- `compose_published_port_mapping_invalid`: mapping のサービス / identity が canonical Compose model の fixed TCP published port に一意に対応しない。
- `compose_published_port_mapping_conflict`: explicit published port mapping が要求するエンドポイントが reservation または availability probe と衝突した。automatic relocation へはフォールバックしない。

### 13.2 Compose clone isolation

発生条件の詳細は 8.9 節。

- `compose_network_subnet_overlap`: preflight で、固定 IPv4 サブネットを持つ external ではないネットワークが、既存 Docker ネットワークのサブネットと同じ IPAM のアドレス空間内で重複した。
- `compose_fixed_name_conflict`: preflight で、固定 `container_name` またはトップレベルリソースの固定 `name` が同種の既存 Docker リソースと衝突した。
- `compose_clone_isolation_invalid`: clone isolation の設定または対象ネットワークの状態が不正である。エンドポイントのプレースホルダーの不正参照、ゲートウェイ / `ip_range` / `aux_addresses` のオフセットの非収容、接続コンテナがある既存ネットワークと計画の不一致を含む。
- `compose_clone_isolation_unsupported`: decune が安全に書き換えできない構成である。IPv6 / 固定のサービスアドレス、解釈できない IPAM の設定フィールド、`!override` を要するリスト書き換えに対する古い Compose のバージョンを含む。
- `compose_clone_isolation_pool_exhausted`: `subnet_pool` 内に割り当て可能な空きスロットがない。
- `compose_clone_isolation_endpoint_unsafe`: network relocation 後のサービスの環境変数に、元のサブネット / 元のゲートウェイのアドレスが残っている。

### 13.3 decune host daemon error code

decune host daemon error code は小文字の snake_case とする。wire 上の `code` は将来の追加値を受理できる文字列とする。

- `invalid_request`: request を envelope として解釈できない、またはスキーマに違反している。
- `unsupported_protocol_version`: request の `version` が daemon のプロトコルバージョンと一致しない。
- `request_too_large`: request body が固定上限を超えた。
- `unknown_request_type`: `type` が予約された request type のいずれでもない。
- `not_implemented`: 予約されているが未実装の request type(`portForward`)である。
- `credential_failed`: ホスト側の Git credential 処理が失敗した。
- `unsupported_command`: `cliQuery` の `command` が対応外である。
- `unsupported_format`: `cliQuery` の `command` + `format` の組み合わせが対応外である。
- `container_cli_disabled`: 実効的な `container.cli.enabled` が false、または detached なセッションの daemon にクエリが届いた。
- `cli_query_failed`: クエリの collector / render / serialization で致命的な失敗が起きた。
- `cli_query_busy`: 同時に処理できる `cliQuery` の上限に達している。
- `cli_query_timeout`: クエリが処理期限を超えた。

エラーの response には部分的な出力と警告を含めない(3.9 節)。
