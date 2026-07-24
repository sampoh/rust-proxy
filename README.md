# http-proxy

ローカル HTTP プロキシ。任意のアプリケーション等から `http://127.0.0.1:18080` に送信したリクエストを、指定した HTTPS サーバへ高速に転送します。

## 特徴

- keep-alive・接続プール常時有効（全リクエストで `reqwest::Client` を共有）
- HTTP/2 を ALPN 経由で自動利用
- WebSocket (ws/wss) の双方向トンネリング対応
- リダイレクト非追従（3xx レスポンスをそのまま返す）
- 自動解凍なし（Content-Encoding をそのまま転送）
- Hop-by-hop ヘッダの適切な除去
- ポート使用中のドーマントモード（待機して空き次第自動起動）

## ビルド

```bash
# 開発ビルド
cargo build

# リリースビルド（推奨）
cargo build --release
```

ビルド後のバイナリは `target/release/http_proxy`（Windows: `target\release\http_proxy.exe`）に生成されます。

## 実行

```bash
http_proxy --target https://example.com --listen 127.0.0.1:18080
```

### 引数

| 引数 | 必須 | デフォルト | 説明 |
|---|---|---|---|
| `--target` | ✓ | — | 転送先のベース URL（末尾スラッシュ不要） |
| `--listen` | — | `127.0.0.1:18080` | ローカル待受アドレス |
| `--target2` | — | — | 2 つ目の転送先ベース URL。指定した場合、2 つのポートで同時に待機します |
| `--listen2` | — | `--listen` のポート番号 + 1 | 2 つ目のローカル待受アドレス。`--target2` を指定していない場合は無視されます |
| `--timeout` | — | `200` | アップストリームへのリクエストタイムアウト（秒）。`0` を指定するとタイムアウトなし |
| `--timeout2` | — | `--timeout` の値 | `--target2` 向けのリクエストタイムアウト（秒）。省略時は `--timeout` の値を使用。`0` を指定するとタイムアウトなし |

### 複数ターゲット

`--target2` を指定すると、2 つの独立したプロキシが同時に起動します。`--listen2` を省略した場合は `--listen` のポート番号 + 1 が自動的に使用されます。`--target2` を指定していない場合、`--listen2` は無視されます。

```bash
# 18080 → example.com、18081 → exampletwo.com（listen2 は listen+1）
http_proxy --target https://example.com --target2 https://exampletwo.com

# 1234 → example.com、1235 → exampletwo.com
http_proxy --target https://example.com --listen 127.0.0.1:1234 --target2 https://exampletwo.com

# 1234 → example.com、23450 → exampletwo.com
http_proxy --target https://example.com --listen 127.0.0.1:1234 --target2 https://exampletwo.com --listen2 127.0.0.1:23450
```

### タイムアウトの設定

デフォルトは 200 秒です。4D の `HTTP Request` コマンドのデフォルトタイムアウト（120 秒）およびフレームワーク（180 秒）より長く設定することで、4D 側が先にタイムアウトして既存のエラーハンドリングがそのまま動作します。

```bash
# デフォルト（200秒）
http_proxy --target https://example.com

# タイムアウトを任意の値に変更（例: 60秒）
http_proxy --target https://example.com --timeout 60

# タイムアウトなし
http_proxy --target https://example.com --timeout 0
```

### 動作例

```
GET http://127.0.0.1:18080/api/test?q=1
      ↓
GET https://example.com/api/test?q=1
```

## シャットダウン

以下のいずれかで Graceful Shutdown が実行されます。進行中のリクエストが完了してからプロセスが終了します。

| 方法 | 説明 |
|---|---|
| `POST /__shutdown` | HTTP エンドポイント経由でシャットダウン |
| SIGTERM | `kill <pid>` 等によるシグナル送信（Unix） |
| SIGINT | Ctrl+C |
| stdin EOF | 標準入力を閉じると終了（4D の `sw.closeInput()` 等） |

## ドーマントモード

起動時に指定ポートが既に使用中の場合、即時終了せず 1 秒ごとにポートが空くのを待ちます。ポートが解放された時点で自動的にリッスンを開始します。この待機中に stdin が閉じられた場合はそのまま終了します。

```
[INFO] Port 127.0.0.1:18080 is already in use. Waiting in dormant mode...
[INFO] Port 127.0.0.1:18080 is now available. Starting HTTPD.
```

## ログ

通常時は起動メッセージのみ出力します。

```
Listening on 127.0.0.1:18080
Target: https://example.com
```

以下のプレフィックスで標準エラーに詳細を出力します。

| プレフィックス | 説明 |
|---|---|
| `[ERROR]` | 致命的なエラー（アップストリーム失敗、バインド失敗 等） |
| `[INFO]` | 情報メッセージ（ドーマントモード開始・終了 等） |
| `[WS]` | WebSocket プロキシ関連のエラー |

## 4D との連携

### 準備

[リリースページ](../../releases) からビルド済みバイナリをダウンロードし、4D プロジェクトの `Resources` フォルダに配置します。

| OS | ファイル名 |
|---|---|
| macOS (Universal Binary) | `http_proxy-macos-universal` |
| Windows | `http_proxy-windows-x86_64.exe` |
| Linux | `http_proxy-linux-x86_64` |

### ワーカーメソッド "Proxy" の実装

以下の内容で `Proxy` メソッドを作成します。

```4d
var $1; $mode : Text
var $2; $host : Text
var $3; $subhost : Text

var $dir; $path; $command : Text
var sw : 4D.SystemWorker

If (Count parameters>=1)
    $mode:=Lowercase($1)
End if 

If (Count parameters>=2)
    $host:=$2
End if 

If (Count parameters>=3)
    $subhost:=$3
End if 

Case of 
    : ($mode="start")
        
        $dir:=Convert path system to POSIX(Get 4D folder(Current resources folder))
        
        Case of 
            : (Is macOS)
                $path:=$dir+"http_proxy-macos-universal"  // Universal Binary (Apple Silicon + Intel)
            : (Is Windows)
                $path:=$dir+"http_proxy-windows-x86_64.exe"
        End case 
        
        $command:="\""+$path+"\" --target "+$host
        
        If ($subhost#"")
            $command:=$command+" --target2 "+$subhost
        End if 
        
        sw:=4D.SystemWorker.new($command)
        
    : ($mode="end")
        
        If (sw#Null)
            sw.closeInput()
        End if 
        
End case 
```

> `sw.closeInput()` により標準入力の EOF が通知され、プロキシプロセスが Graceful Shutdown します。

### メインプロセスからの呼び出し

アプリケーション起動・終了のタイミングで以下のように呼び出します。

```4d
var $WORKER : Text
var $host : Text
var $subhost : Text

$WORKER:="RUST_PROXY"
$host:="https://example.com"
$subhost:="https://example2.com"

// システム起動時に実行
CALL WORKER($WORKER; "Proxy"; "start"; $host; $subhost)

//　↓↓↓　ここにメイン処理を実装　↓↓↓


//　↑↑↑　ここにメイン処理を実装　↑↑↑

// システム終了時に実行
CALL WORKER($WORKER; "Proxy"; "end")

KILL WORKER($WORKER)
```

起動後は `http://127.0.0.1:18080` 宛のリクエストが `$host` に転送されます。
