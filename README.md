# http-proxy

ローカル HTTP プロキシ。任意のアプリケーション等から `http://127.0.0.1:<port>` に送信したリクエストを、指定した HTTPS サーバへ高速に転送します。

## 特徴

- keep-alive・接続プール常時有効（全リクエストで `reqwest::Client` を共有）
- HTTP/2 を ALPN 経由で自動利用
- リダイレクト非追従（3xx レスポンスをそのまま返す）
- 自動解凍なし（Content-Encoding をそのまま転送）
- Hop-by-hop ヘッダの適切な除去

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
http_proxy \
  --target=https://example.com \
  --listen=127.0.0.1:18080
```

### 引数

| 引数 | 必須 | デフォルト | 説明 |
|---|---|---|---|
| `--target` | ✓ | — | 転送先のベース URL（末尾スラッシュ不要） |
| `--listen` | — | `127.0.0.1:18080` | ローカル待受アドレス |

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

## ログ

通常時は起動メッセージのみ出力します。

```
Listening on 127.0.0.1:18080
Target: https://example.com
```

エラー発生時のみ `[ERROR]` プレフィックス付きで詳細を標準エラーに出力します。

## 4D との連携

### 準備

[リリースページ](../../releases) からビルド済みバイナリをダウンロードし、4D プロジェクトの `Resources` フォルダに配置します。

| OS | ファイル名 |
|---|---|
| macOS (Apple Silicon) | `http_proxy-macos-aarch64` |
| Windows | `http_proxy-windows-x86_64.exe` |

### ワーカーメソッド "Proxy" の実装

以下の内容で `Proxy` メソッドを作成します。

```4d
var $1; $mode : Text
var $2; $host : Text

var $dir; $path; $command : Text
var sw : 4D.SystemWorker

If (Count parameters>=1)
    $mode:=Lowercase($1)
End if 

If (Count parameters>=2)
    $host:=$2
End if 

Case of 
    : ($mode="start")
        
        $dir:=Get 4D folder(Current resources folder)
        
        Case of 
            : (Is macOS)
                $path:=$dir+"http_proxy-macos-aarch64"
            : (Is Windows)
                $path:=$dir+"http_proxy-windows-x86_64.exe"
        End case 
        
        $command:=$path+" --target "+$host
        
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

$WORKER:="RUST_PROXY"
$host:="https://yourhost.com"

// システム起動時に実行
CALL WORKER($WORKER; "Proxy"; "start"; $host)

//　↓↓↓　ここにメイン処理を実装　↓↓↓


//　↑↑↑　ここにメイン処理を実装　↑↑↑

// システム終了時に実行
CALL WORKER($WORKER; "Proxy"; "end")
```

起動後は `http://127.0.0.1:18080` 宛のリクエストが `$host` に転送されます。
