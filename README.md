# http-proxy

ローカル HTTP プロキシ。4D アプリケーション等から `http://127.0.0.1:<port>` に送信したリクエストを、指定した HTTPS サーバへ高速に転送します。

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
