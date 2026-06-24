# captu 設計仕様書

---

## 概要

地デジ録画TSファイルから字幕テキストを抽出・索引化し、
文言検索 → コンタクトシートでフレーム選定 → JPEG出力 → 共有/コピー
までを行うWebアプリ。

---

## 動作環境

- **言語**: Rust (edition 2021)
- **実行環境**: Docker（`compose.yaml` 参照）
- **TSファイル保管**: NFS等でマウントしたNAS。パスは `config.toml` で指定
- **アクセス**: LAN・VPN等、任意の手段でブラウザからアクセス

> **メモ**: Clipboard API / Web Share API は secure context（HTTPS または localhost）でのみ動作する。
> HTTP 環境では自動的にダウンロードフォールバックに切り替わるので、captu 自体は HTTPS を要求しない。

---

## ディレクトリ構成

```
captu/
├── src/
│   ├── main.rs                    # エントリーポイント・axumルータ組み立て
│   ├── lib.rs                     # クレートルート (モジュール宣言)
│   ├── config.rs                  # 設定構造体・config.toml読み込み
│   ├── db.rs                      # SQLiteスキーマ初期化・接続プール
│   ├── ingest.rs                  # TSスキャン・取り込みオーケストレーション
│   ├── scheduler.rs               # 定期スキャン (tokio-cron-scheduler, IngestGuard)
│   ├── ts/                        # TSパース層
│   │   ├── mod.rs
│   │   ├── b24.rs                 # ARIB STD-B24テキストコーデック
│   │   ├── epg.rs                 # EIT/EPGパーサ → EpgInfo
│   │   ├── pes.rs                 # ARIB字幕PESデマクサ
│   │   └── subtitle.rs            # libaribcaption FFI経由の字幕抽出・on-demand PNG描画
│   ├── media/
│   │   ├── mod.rs
│   │   └── capture.rs             # ffmpeg 単一パスサムネ生成 (コンタクトシート / フル解像度)
│   └── routes/
│       ├── mod.rs                 # AppState, display_title(), fmt_ms(), like_escape()
│       ├── search.rs              # GET / , GET /search
│       ├── contact.rs             # GET /contact/:id
│       ├── capture.rs             # GET /thumb/:id/:n , GET /full/:id/:n , POST /select/:id/:n
│       ├── episodes.rs            # GET /api/episodes
│       ├── tags.rs                # POST /caption/:id/tags , POST /caption/:id/tags/delete , GET /api/tags
│       └── ingest.rs              # GET /ingest/status , POST /reingest/:id
│
├── ui/
│   ├── templates/                 # askamaテンプレート (コンパイル時検証, askama.toml で root 宣言)
│   │   ├── layouts/base.html
│   │   ├── macros.html
│   │   ├── pages/                 # index.html / contact.html / ingest_status.html
│   │   └── fragments/             # episodes.html / search_results.html / tag_options.html / tags.html
│   └── static/
│       ├── app.js                 # フレーム選択・JPEG共有/コピー/ダウンロード
│       └── search.js              # 検索フィルタ・タグチップ・セッション復元
│
├── docker/
│   ├── assets/fonts/              # ARIB字幕用 Rounded M+ フォント
│   └── Dockerfile                 # マルチターゲット (builder-base / builder / dev / runtime)
│
├── cache/                         # volume
│   └── {ts_stem}/
│       ├── captions.pes           # PESブロブ (取り込み時に保存)
│       ├── sub/
│       │   └── {caption_id}.png   # 字幕PNG (on-demand描画・キャッシュ)
│       ├── thumbs/
│       │   └── {caption_id}_{n:02}.jpg  # コンタクトシートJPEG (縮小表示用)
│       └── full/
│           └── {caption_id}_{n:02}.jpg  # フル解像度JPEG (DL/共有用、on-demand)
│
├── data/
│   └── captions.db                # volume
│
├── docs/spec.md                   # 本ドキュメント
├── CLAUDE.md                      # 開発ガイド
└── compose.yaml
```

---

## ビルド要件

```bash
# サブモジュール初期化 (crates/aribcaption-sys/vendor/libaribcaption)
git submodule update --init

# 依存ツール: cmake, clang, libclang (bindgen用)
# ffmpegはソースビルド (--enable-libaribcaption) → Dockerfile.ffmpeg を参照
# 開発環境: scripts/dev.sh 経由でDockerコンテナ内でビルドする
scripts/dev.sh build
```

### 主な依存クレート

- Web: axum 0.7, tokio, tower-http
- テンプレート: askama (コンパイル時検証)
- DB: sqlx 0.7 (sqlite + chrono features)
- ARIB字幕: `aribcaption-sys` (ワークスペースメンバー, libaribcaptionのFFIラッパー)
- スケジューラ: tokio-cron-scheduler (6フィールドcron、秒単位指定)
- その他: serde, toml, glob, png, bincode, encoding_rs, unicode-normalization, tracing

### テスト実行
```bash
scripts/dev.sh test -p captu --lib
```

---

## 設定

`config.toml.example` をコピーして編集。環境変数 `CAPTU_NAS_MOUNT / CAPTU_TS_GLOB / CAPTU_DB_PATH / CAPTU_CACHE_DIR` で上書き可能。

```rust
pub struct PathsConfig {
    pub nas_mount: String,   // 録画ディレクトリのマウント先
    pub ts_glob: String,     // TSファイルの検索パターン (例: "**/*.ts")
    pub cache_dir: String,   // キャッシュディレクトリ
    pub db_path: String,     // SQLiteファイルパス
}

pub struct CaptureConfig {
    pub thumb_count: u32,    // コンタクトシートのサムネ枚数 (デフォルト: 6)
    // --- コンタクトシートグリッド / プレビュー表示用 (縮小) ---
    pub thumb_width: u32,    // コンタクトシート表示幅 (デフォルト: 640)
    pub thumb_height: u32,   // コンタクトシート表示高さ (デフォルト: 360)
    pub thumb_quality: u32,  // サムネ用 ffmpeg -q:v 値 (デフォルト: 4)
    // --- DL/共有用フル解像度 ---
    pub width: u32,          // フル解像度出力幅 (地デジ 1440x1080 → 1920x1080)
    pub height: u32,         // フル解像度出力高さ
    pub jpeg_quality: u32,   // フル解像度用 ffmpeg -q:v 値 (2 = 高品質)
}

pub struct IngestConfig {
    pub schedule_cron: String,        // 6フィールドcron（秒付き）。空文字で定期スキャン無効
    pub run_on_startup: bool,         // 起動時スキャン
    pub concurrency: u32,             // 並列取り込みワーカー数 (推奨: 2-4)
    pub require_captions: bool,       // 字幕PIDなしTSをスキップするか
    pub filter_include: Vec<String>,  // 対象Globパターン (空=全対象)
    pub filter_exclude: Vec<String>,  // 除外Globパターン
}

pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}
```

---

## DBスキーマ

```sql
-- 番組マスタ (EITのタイトルを正規化して格納)
CREATE TABLE IF NOT EXISTS programs (
    id               INTEGER PRIMARY KEY,
    title            TEXT NOT NULL UNIQUE,
    normalized_title TEXT NOT NULL       -- 検索・autocomplete用 (全角→半角, 小文字化)
);

-- TSファイル管理
CREATE TABLE IF NOT EXISTS ts_files (
    id             INTEGER PRIMARY KEY,
    path           TEXT UNIQUE NOT NULL,
    filename       TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
                   CHECK(status IN ('pending', 'ingesting', 'done', 'error')),
    error_msg      TEXT,
    ingested_at    DATETIME,
    program_id     INTEGER REFERENCES programs(id),
    episode_number INTEGER,              -- NULL = 話数不明 (series_descriptor なし)
    episode_title  TEXT,                 -- extended_event_descriptor 由来のサブタイトル
    air_date       DATE                  -- EITのstart_time; なければファイルmtime
);

-- 字幕エントリ
CREATE TABLE IF NOT EXISTS captions (
    id         INTEGER PRIMARY KEY,
    ts_file_id INTEGER NOT NULL REFERENCES ts_files(id),
    pts_start  INTEGER NOT NULL,   -- 表示開始 (ms)
    pts_end    INTEGER NOT NULL,   -- 表示終了 (ms)
    text       TEXT NOT NULL
);

-- FTS5 全文検索テーブル (trigram: 日本語部分一致対応)
-- 現在の検索ハンドラは LIKE を使用。このテーブルは将来のFTS5移行に備えて維持。
CREATE VIRTUAL TABLE IF NOT EXISTS captions_fts USING fts5(
    text,
    content=captions,
    content_rowid=id,
    tokenize='trigram'
);

-- FTS同期トリガー
CREATE TRIGGER IF NOT EXISTS captions_ai AFTER INSERT ON captions BEGIN
    INSERT INTO captions_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS captions_ad AFTER DELETE ON captions BEGIN
    INSERT INTO captions_fts(captions_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

-- タグ
-- ON DELETE CASCADE: captions 削除（再取り込み等）時にタグも自動消去される
-- 既知の制約: reingest で caption が作り直されると id が変わるためタグは失われる
CREATE TABLE IF NOT EXISTS tags (
    id         INTEGER PRIMARY KEY,
    caption_id INTEGER NOT NULL REFERENCES captions(id) ON DELETE CASCADE,
    tag        TEXT NOT NULL,
    UNIQUE(caption_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);

-- サムネ生成済みフラグ + ユーザー選択フレーム
-- ON DELETE CASCADE により captions 削除 (再取り込み等) で自動消去
CREATE TABLE IF NOT EXISTS thumbnails (
    caption_id     INTEGER PRIMARY KEY
                   REFERENCES captions(id) ON DELETE CASCADE,
    selected_frame INTEGER NOT NULL DEFAULT 0
);
```

### statusの状態遷移

```
pending → ingesting → done
                    → error
```

- スキャン時は `done` / `error` / `'ingesting'` をスキップ
- 起動時に `'ingesting'` 残骸を `pending` に戻す（クラッシュ復旧）

---

## 取り込み戦略 (ingest.rs)

### タイミング

1. **起動時スキャン** (`run_on_startup = true`)
2. **定期スキャン**: `schedule_cron`（6フィールドcron、秒付き）で周期実行。
   `scheduler::start()` が `tokio-cron-scheduler` ベースのジョブを起動。
   起動時スキャンと共有の `IngestGuard`（`Arc<Mutex<()>>`）で排他制御し、
   前のスキャンが終わっていない tick は `try_lock` で自動スキップ。
   `schedule_cron = ""` で定期スキャンを無効化できる。



### スキャン処理フロー

```
scan_and_ingest():
1. ts_glob パターンでTSファイルを列挙
2. done/error/ingesting をスキップ (HashSet で高速判定)
3. 未登録ファイルを pending で INSERT OR IGNORE
4. ingest_one() を並列ワーカー (concurrency 設定) で実行
```

### 1ファイルの取り込み処理

```
1. status を pending → 'ingesting' に CAS更新 (affected rows=0ならスキップ)
2. epg::extract_epg() でEIT解析 → program_id / episode_number / air_date 取得
3. ts/pes.rs でARIB字幕PESを抽出 → cache/{stem}/captions.pes に保存
4. ts/subtitle.rs + libaribcaption FFI で字幕テキスト+タイムスタンプ取得
5. programs / ts_files / captions に DB投入 (トランザクション)
6. status を 'ingesting' → done に更新、ingested_at をセット
7. 失敗時: status を 'ingesting' → error に更新、error_msg に記録
8. 字幕ゼロの場合: status = done で登録 (captions には何も入れない)
```

---

## EPG/EITメタデータ抽出 (ts/epg.rs)

地デジTSのSI (Service Information) テーブル。PID `0x0012` の EIT (Event Information Table) に番組メタデータが含まれる。

| descriptor | tag | 取得できる情報 |
|---|---|---|
| short_event_descriptor | `0x4D` | 番組タイトル、放送開始日時 |
| series_descriptor | `0xD5` | **話数 (12bit)**、最終話数、シリーズ名 |
| extended_event_descriptor | `0x4E` | 番組詳細テキスト（サブタイトル等） |

```
EIT section (PID=0x0012, table_id=0x4E: current/following)
  └─ event[]
       ├─ start_time (40bit: MJD 16bit + BCD 24bit) → air_date
       ├─ short_event_descriptor (0x4D)
       │    └─ event_name_char (ARIB B24テキスト) → title
       ├─ series_descriptor (0xD5)
       │    ├─ episode_number (12bit) → episode_number
       │    └─ series_name_char → シリーズ名
       └─ extended_event_descriptor (0x4E)
            └─ text_char → episode_title (サブタイトル)
```

```rust
pub struct EpgInfo {
    pub title: String,
    pub episode_number: Option<u16>,   // None = series_descriptor なし
    pub last_episode: Option<u16>,
    pub series_name: Option<String>,
    pub air_datetime: Option<DateTime<FixedOffset>>,
    pub detail: Option<String>,
}
```

- EITが先頭付近に存在するため、EIT発見次第パースを停止（全TS読み不要）
- タイトルは ARIB B24 → UTF-8 変換（`ts/b24.rs` の `decode_arib_b24` を使用）
- `series_descriptor` 未収録のTSは `episode_number = NULL`、`air_date` は `start_time` or ファイルmtime

---

## ffmpegパイプライン (media/capture.rs)

### コンタクトシートサムネ生成 (単一パス)

```
ffmpeg -y -ss {pre_seek} -t {dur} -i file:{ts} [-i {sub.png}]
       -vf  "scale={W}:{H},setsar=1,select='eq(n,X)+…',setpts=N/FRAME_RATE/TB"
       # 字幕ありの場合は -filter_complex でオーバーレイ:
       # "[0:v]scale=…,select='…',setpts=…[v];[v][1:v]overlay=eof_action=repeat[out]"
       -fps_mode vfr -q:v {jpeg_quality} thumbs/_tmp_%d.jpg
```

中間 MJPEG エンコード・プロセス間パイプを廃止し、scale → select → overlay を1パスで処理する。

**NAS越しシーク戦略:**
- `-ss` を `-i` の前に置く（keyframe fast seek）→ NFS転送量最小化
- フレーム選択は `select='eq(n,{frame_num})+...'`（地上波 29.97fps 前提）
- 字幕PNG（`cache/{stem}/sub/{caption_id}.png`）は on-demand 描画・キャッシュ。取り込み時に保存した PES ブロブから libaribcaption で生成（`subtitle.rs::ensure_caption_png`）

### フル解像度JPEG生成 (`ensure_full`)

`GET /full/:id/:n` からトリガーされる単一フレーム取得。
コンタクトシートと同一の ffmpeg シーク戦略だが、解像度とクオリティが異なる。

```
ffmpeg -y -ss {pre_seek} -t {dur} -i file:{ts} [-i {sub.png}]
       -vf  "scale={cfg.width}:{cfg.height},setsar=1,select='eq(n,X)',setpts=N/FRAME_RATE/TB"
       -fps_mode vfr -q:v {cfg.jpeg_quality} cache/{stem}/full/{id}_{n:02}.jpg
```

出力: `cache/{stem}/full/{caption_id}_{n:02}.jpg`（DL/共有用、初回アクセス時に生成・キャッシュ）

---

## API仕様

### GET /
検索トップページ。プルダウン用の `programs` 一覧を取得して `index.html` に渡す。

### GET /search
字幕テキスト・メタデータ検索。htmx 向け HTML フラグメント (`search_results.html`) を返す。

| パラメータ | 型 | 説明 |
|---|---|---|
| `q` | string | 字幕テキスト部分一致（LIKE `%q%`）。2文字未満は無効 |
| `program_id` | integer | 番組IDで絞り込み |
| `ep` | integer | 話数で絞り込み |
| `sub` | string | エピソードタイトルで部分一致絞り込み |
| `date_from` | date | 放送日（以降） |
| `date_to` | date | 放送日（以前） |
| `tag` | string | タグで絞り込み |
| `filter` | string | `all`（デフォルト） / `generated`（サムネあり） / `pending`（未生成） |
| `page` | integer | 0始まりページ番号（50件/ページ） |

q・フィルタ・filter が全て未指定の場合は空結果を返す。

### GET /contact/:id
コンタクトシートページ。`caption_id` を受け取り、`contact.html` を返す。
サムネは非同期生成（ページ表示後に各 `GET /thumb` リクエストで生成）。

### GET /thumb/:id/:n
コンタクトシート用縮小JPEG配信（`cfg.thumb_width × cfg.thumb_height`）。
キャッシュがなければ生成してから返す。
同一 caption への並列リクエストはロック制御（1本のみ ffmpeg を実行、後続はキャッシュヒット）。
初回生成成功時に `thumbnails(caption_id, default_frame)` を INSERT OR IGNORE。

### GET /full/:id/:n
フル解像度JPEG配信（`cfg.width × cfg.height`）。DL・Web Share・クリップボードコピー用。
`/thumb` と同じ per-caption ロック制御。キャッシュ済みならそのまま返す。
`cache/{stem}/full/{id}_{n:02}.jpg` に保存。

### POST /select/:id/:n
ユーザーが選んだフレーム番号を永続化。
`thumbnails.selected_frame` を upsert する。検索結果のプレビュー表示に反映される。

### GET /api/episodes?program_id={id}
番組の話数・サブタイトル一覧。htmx でフィルタドロップダウンを動的更新。
- `episode_number` が全行 NULL の場合 → サブタイトル一覧 (`episode_title` の DISTINCT) を返す
- 上記以外 → 話数一覧を返す

### GET /ingest/status
取り込み状況（status 別カウント・最近のエラー）を HTML で返す。

### POST /caption/:id/tags
タグ追加（冪等）。`Form { tag: String }` を受け取り、当該 caption の最新タグリストを HTML フラグメントで返す。

### POST /caption/:id/tags/delete
タグ削除。`Form { tag: String }` を受け取り、最新タグリストを HTML フラグメントで返す。

### GET /api/tags
全ての distinct タグを返す（`<option>` リスト形式）。タグフィルタ select とオートコンプリート datalist の候補として使用。

### POST /reingest/:id
指定 `ts_file_id` の status を `pending` にリセットして再取り込みキューに投入。

---

## フロントエンド (ui/static/, ui/templates/)

**使用技術:** htmx + Tailwind CSS CDN（ビルド不要）

### 検索UI (index.html)

```html
<input type="text" name="q"
       hx-get="/search"
       hx-trigger="input changed delay:300ms"
       hx-target="#search-results" />
```

- タイトル・話数・日付フィルタを組み合わせ可能
- フィルタ状態はURLクエリパラメータで保持（ブックマーク対応）
- タブ切り替えで `filter=all/generated/pending` を切り替え（サムネ生成済みの絞り込み）
- 検索結果カードに `has_thumb` が立っている場合は選択済みフレームのプレビューを表示
- 無限スクロール: `page` パラメータで追加ロード

### コンタクトシートUI (contact.html)

- 字幕テキストと最大 `thumb_count` 枚のサムネをグリッド表示
- サムネクリック → `selectFrame(n)` で選択状態更新 + `POST /select/:id/:n` 呼び出し
- 拡大プレビューを上部に表示

### JPEG取得後の処理 (static/app.js)

```javascript
async function handleJpeg(captionId, frameN) {
    const res = await fetch(`/full/${captionId}/${frameN}`);
    const blob = await res.blob();

    if (window.isSecureContext && navigator.share && navigator.canShare) {
        const file = new File([blob], `caption_${captionId}_${frameN}.jpg`, { type: 'image/jpeg' });
        if (navigator.canShare({ files: [file] })) {
            await navigator.share({ files: [file] });  // Stage 1: Web Share (スマホ等)
            return;
        }
    }

    if (window.isSecureContext && navigator.clipboard?.write) {
        await navigator.clipboard.write([new ClipboardItem({ 'image/jpeg': blob })]);
        showToast('クリップボードにコピーしました');  // Stage 2: Clipboard (PC/HTTPS)
        return;
    }

    // Stage 3: download fallback (HTTP LAN等)
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `caption_${captionId}_${frameN}.jpg`;
    a.click();
    showToast('画像を保存しました');
}
```

---

## 未実装・将来構想

- **LLM AI検索**: 状況文 → FTS5候補 → LLMランク付け（`plans/phase7-ai-search.md`）
- **マルチモーダル AI検索**: サムネ画像込みのLLMランク付け（`plans/phase8-multimodal.md`）
