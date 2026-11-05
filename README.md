# TURU.RC // MARKET SNIPER

Headless **Rollercoin marketplace sniper** dengan control deck bergaya arcade, dibangun dengan React 19 + Tailwind CSS v4. Tiga peran tipografi: **Arcana** untuk display, **Chakra Petch** untuk UI dan teks, **JetBrains Mono** untuk data.

> Font: **Arcana** oleh vilogsign. File demo di `public/fonts/` gratis untuk penggunaan pribadi; untuk distribusi komersial, beli lisensi penuh dari author. **Chakra Petch** dan **JetBrains Mono** berlisensi OFL; file lisensinya ada di `public/fonts/`.

Dulu `trade.js` harus ditempel di console browser dan ikut melambat saat tab inactive.
Sekarang engine berjalan sebagai proses Bun terpisah; browser hanya menjadi **data plane**
untuk login, mengatur target, memantau market, dan melihat loot.

![Control deck](docs/preview.png)

| Boot screen | Mobile |
| --- | --- |
| ![Boot](docs/preview-boot.png) | ![Mobile](docs/preview-mobile.png) |

---

## Daftar isi

- [Fitur](#fitur)
- [Arsitektur](#arsitektur)
- [Quick start](#quick-start)
- [Jalankan dengan PM2](#jalankan-dengan-pm2)
- [Access key](#access-key)
- [Target configuration](#target-configuration)
- [Mekanisme parameter target](#mekanisme-parameter-target-deep-dive)
- [Link sesi Rollercoin](#link-sesi-rollercoin)
- [Environment variables](#environment-variables)
- [Worker API](#worker-api)
- [HTTPS dan mixed content](#https-dan-mixed-content)
- [Caddy tutorial](#caddy-tutorial-debianubuntu-vps)
- [Data dan keamanan](#data-dan-keamanan)
- [Mock mode](#mock-mode)
- [Testing](#testing)
- [Struktur proyek](#struktur-proyek)
- [Troubleshooting](#troubleshooting)

## Fitur

- **Worker headless** — koneksi WebSocket ke `nws.rollercoin.com`, mendengarkan semua frame,
  mencocokkan event dengan daftar target, lalu menembak `purchase-item` lewat HTTPS.
- **Port + access key** — worker membuka control plane (HTTP + WebSocket) dengan bearer key.
  Key dibuat sekali saat pertama dijalankan, atau dipin lewat `WORKER_TOKEN`.
- **Target dinamis + auto label** — tambah/edit/hapus/arm target dari UI tanpa restart. Label
  diambil otomatis dari `item-info` Rollercoin (`rarityGroup.title.en` + `name.en`); jika
  `rarityGroup` tidak ada, label memakai nama item saja.
- **React chart + success rate** — card memakai chart Recharts minimalist dengan timeframe
  **1m / 5m / 15m / 1h**, garis target, tooltip, dan 24 jam history per menit. Meter price/book
  yang redundan diganti dengan **PURCHASE SUCCESS** per target.
- **React + Vite data plane** — dashboard dibangun dengan React 19 + Recharts; worker menyajikan
  hasil build `dist/` sehingga tetap same-origin dan tanpa CDN runtime.
- **Session manager** — menyimpan access/refresh token, memperbarui token sebelum kedaluwarsa,
  mencoba ulang sekali saat request kena `401`, dan bisa diganti atau **dihapus langsung dari UI**
  (termasuk cookie Cloudflare) tanpa menyentuh database `data/rollercoin.db` secara manual.
- **Token bridge** — opsional: console snippet sekali pakai atau **userscript Tampermonkey** yang
  dipasang sekali dan otomatis menyinkronkan token setiap 30 detik, bahkan saat tab background.
- **Safety rails** — cooldown antar order, batas order per menit, backoff saat purchase ditolak,
  dry-run, partial-order minimum, validasi ketat, dan konfirmasi untuk aksi destruktif. Tombol
  **PAUSE / START** tunggal di header menghentikan atau menjalankan kembali seluruh proses buying;
  market feed tetap hidup agar harga langsung segar saat di-resume.
- **Loot log + profit report** — setiap fill/failure dicatat **permanen** di SQLite; panel loot log
  default menampilkan 50 transaksi terbaru, bisa ditambah lewat **Load older**, dan punya pencarian
  server-side berdasarkan nama produk sehingga halaman tetap ringan walau log sudah ribuan baris.
  Log yang sama diagregasi per produk (fills, qty, spend, avg buy, profit, margin, success rate,
  last activity). Profit all-time juga tampil langsung di tiap product card. Panel
  **PROFIT BY PRODUCT** bisa diekspor ke CSV; angkanya tetap utuh setelah worker restart.
- **Live data plane** — HUD, target cards + chart, market wire, terminal event, loot log, profit
  report, health token, runtime config, sound effect, dan tampilan mobile.
- **Worker health monitor** — tab **SYSTEM** menampilkan CPU worker dan host, heap (kompatibilitas
  V8) + batasnya, RSS, RAM sistem (MemAvailable, bukan MemFree), swap, load average, latency event
  loop (p50/p95/p99), active handles, plus sparkline historis. Status `HEALTHY / DEGRADED /
  CRITICAL` dihitung dari tekanan heap, RAM, dan event loop, dan setiap transisi ikut tercatat di log.
- **DB performance metrics** — setiap statement SQLite di-timing (read/write, avg/p95/max, error,
  slow query), plus ukuran file DB/WAL/-shm, page/freelist, mode jurnal, cache, dan transaksi.
  Sparkline query rate membantu melihat lonjakan I/O yang menahan event loop.
- **Mock mode** — simulator market bawaan untuk mencoba seluruh alur tanpa akun Rollercoin.
- **Test suite** — unit test untuk rules/validation/event/store + integration test end-to-end.

### Loot log

Setiap fill dan failure ditulis ke tabel `orders` di `data/rollercoin.db` dan **tidak pernah
dipangkas** — jumlah transaksi tidak dibatasi. Agar halaman tetap ringan, browser hanya menerima
halaman pertama (default **50** baris, `LOOT_LOG_PAGE_SIZE`) lewat snapshot; baris yang lebih lama
diambil on-demand dari `GET /api/history` memakai cursor `beforeAt` + `beforeId` (bukan offset),
jadi entri baru yang masuk saat paging tidak menggeser window.

Panel loot log punya kotak **Search product**: pencarian dijalankan di server (SQLite `LIKE`,
case-insensitive, wildcard `%`/`_` di-escape) sehingga mencakup seluruh log permanen, bukan hanya
50 baris yang terlihat. Tombol **Load older** menambah 50 baris per klik dan badge menampilkan
`shown / total`. `MAX_HISTORY = 200` sekarang hanya ukuran window in-memory untuk push live ke
browser; engine memakai `store.hasOrder()` (query SQLite) untuk cek duplikat, jadi fill lama tidak
tercatat dua kali walau sudah keluar dari window.

### Profit by product

Agregat per produk tampil di bawah loot log: total profit, spend, margin, lalu tabel per produk
dengan fills/missed, qty, spend, avg buy, profit, margin, success rate, dan aktivitas terakhir.
Setiap product card juga menampilkan **PROFIT · ALL TIME** (dengan fills, qty, dan margin) untuk
item tersebut. Tombol **CSV** mengekspor seluruh baris untuk analisis lanjutan.

![Profit by product](docs/preview-report.png)

---

## Arsitektur

```text
┌──────────────────────────── Browser (data plane) ────────────────────────────┐
│  login access key  ·  target editor  ·  session link  ·  feed  ·  loot log   │
└───────────────▲──────────────────────────────────────────┬───────────────────┘
                │ HTTP + WebSocket (bearer key)             │
┌───────────────┴──────────────────────────────────────────▼───────────────────┐
│ Worker                                                                        │
│  control server · auth · SQLite store · matching engine · rate limiter         │
│  session refresh · order history · mock upstream                              │
└───────────────▲──────────────────────────────────────────┬───────────────────┘
                │ WSS market frames                        │ HTTPS purchase + refresh
        wss://nws.rollercoin.com                  https://rollercoin.com/api/*
```

- **Worker** (`src/worker/`) adalah satu-satunya komponen yang menyentuh Rollercoin dan menyimpan
  kredensial. Ia tidak peduli tab browser aktif atau tidak.
- **Market feed** memakai WebSocket native Bun (bukan paket `ws`), sehingga TLS handshake memakai
  stack runtime Bun dan header `Origin`/`User-Agent`/`Cookie` tetap terkirim.
- **Data plane** (`ui/` React + Vite → `dist/`, dilayani `ControlServer`) adalah UI + API/event
  stream yang dipakai client. UI disajikan oleh worker agar same-origin, tetapi semua kontrol
  tetap melewati bearer key.
- **Shared rules** (`src/shared/`) berisi validasi, aturan pembelian, dan konstanta yang dipakai
  worker maupun test.

### Alur satu frame market

1. Gateway menerima frame apa pun dari Rollercoin dan menaikkan counter event.
2. Engine menormalkan frame (`marketplace_orders_update`, fill, atau tipe lain).
3. Jika `item_id` cocok dengan target dan currency `RLT`, engine mengambil listing termurah.
4. `planOrder()` memutuskan: harga di bawah target? stok cukup? partial? high-stock ×2?
5. Cooldown global, in-flight guard, dan rate limit dicek.
6. Order dikirim tanpa memblokir frame berikutnya. Order id dicatat untuk menunggu konfirmasi.
7. `marketplace_orders_filled` memperbarui profit, statistik, loot log, dan UI.
8. Semua perubahan penting di-broadcast ke data plane lewat WebSocket.

---

## Quick start

Butuh **Bun >= 1.2** (dites di Bun 1.3). Tidak ada Node.js, node-gyp, atau native addon yang perlu
di-compile: SQLite memakai `bun:sqlite`, HTTP memakai `fetch` bawaan Bun, dan test berjalan dengan
`bun run test`.

> **Migrasi dari versi Node:** `bun install` menghapus `better-sqlite3`/`undici`, lalu worker
> memakai `bun:sqlite`. File `data/rollercoin.db` lama tetap dipakai karena formatnya SQLite
> standar — tidak perlu export/import.

```bash
bun install
cp .env.example .env      # opsional; edit bila perlu
bun start                 # build React → dist/, lalu jalankan worker
```

Saat pertama kali berjalan, worker mencetak access key satu kali:

```text
  ┌─ ACCESS KEY — SHOWN ONCE, SAVE IT NOW ──────────────────┐
  │ <43-character key>                                      │
  └─────────────────────────────────────────────────────────┘
```

Buka `http://127.0.0.1:8787`, tempel key tersebut, lalu tekan **PRESS START**.

Mau mencoba tanpa akun? Jalankan simulator:

```bash
bun run mock              # build React + worker dalam mode simulasi
```

Satu perintah menyalakan worker dan Vite sekaligus:

```bash
bun run dev               # worker di 127.0.0.1:8787 + Vite di 127.0.0.1:5173
```

Butuh memisahkan prosesnya? Jalankan di dua terminal:

```bash
bun run dev:worker        # worker di 127.0.0.1:8787
bun run dev:ui            # Vite di 5173, proxy /api dan /ws ke 8787
```

| Perintah | Yang jalan | UI | Worker | Cocok untuk |
| --- | --- | --- | --- | --- |
| `bun run dev` | `scripts/dev.js` → worker `--watch` + Vite | Vite 5173, hot reload | 8787, hot restart | kerja full-stack sehari-hari |
| `bun run dev:ui` | Vite saja | Vite 5173, hot reload | **tidak dijalankan** — butuh worker terpisah di 8787 | mengerjakan UI/CSS saja |
| `bun run dev:worker` | worker `--watch` saja | `dist/` statis di 8787, tanpa hot reload | 8787, hot restart | mengerjakan engine/API/gateway |
| `bun start` | build lalu worker | `dist/` statis di 8787 | 8787, tanpa watch | uji hasil build seperti production |
| `bun run mock` | build lalu worker `MOCK=1` | `dist/` statis di 8787 | feed simulasi | demo/offline tanpa akun |

- `dev` **tidak** menjalankan `build`. Worker di 8787 menyajikan `dist/` yang mungkin lama; untuk
  melihat UI terbaru saat memakai `dev`, buka **http://127.0.0.1:5173** (Vite yang menyajikan source
  dan proxy `/api` + `/ws` ke 8787).
- `dev:worker` memakai `--watch`, jadi perubahan `src/worker/**` otomatis restart. Perubahan UI
  **tidak** ikut ter-build; jalankan `bun run build` lalu refresh 8787.
- `dev:ui` hanya menyalakan Vite. Kalau worker belum jalan di 8787, login/API akan gagal.
- Vite **wajib** dijalankan dengan Node, bukan Bun (karena itu `dev` men-spawn Vite via `node`).
  Proxy WS milik Vite memanggil `socket.destroySoon()`, API `net.Socket` yang tidak ada di socket
  upgrade Bun, sehingga `/ws` yang gagal (worker belum listen, atau balasan HTTP biasa atas upgrade)
  mematikan dev server dengan `TypeError: socket.destroySoon is not a function`. Karena itu juga
  `dev` menunggu port 8787 terbuka dulu sebelum menyalakan Vite.
- Jangan menjalankan `dev` dan `dev:worker` bersamaan: keduanya berebut port 8787.
- Kalau salah satu child di `dev` mati, `scripts/dev.js` mematikan pasangannya; Ctrl+C kini
  menutup keduanya dengan cepat.

Mock mode mengisi dua target contoh, membuat harga naik-turun, dan mengirim fill palsu sehingga
seluruh dashboard bisa diuji offline. Script `bun run mock` memakai environment assignment Bun
shell, jadi tetap jalan di Windows.

## Jalankan dengan PM2

PM2 hanya supervisor; proses worker-nya sendiri dijalankan dengan interpreter **Bun**
(`ecosystem.config.cjs` → `interpreter: "bun"`). Build React tetap harus dibuat karena `dist/`
tidak disimpan di repo.

> PM2 masih berjalan di atas Node.js; yang dipindahkan ke Bun adalah worker-nya. Kalau server
> benar-benar tanpa Node, jalankan worker langsung dengan `bun src/worker/index.js` atau lewat
> systemd (`ExecStart=/usr/local/bin/bun src/worker/index.js`).
>
> Setelah mengubah `interpreter` di `ecosystem.config.cjs`, `pm2 restart` tidak mengganti
> interpreter proses lama. Jalankan `pm2 delete rc-autotrade && pm2 start ecosystem.config.cjs`.

1. Hentikan worker manual yang sedang berjalan (`Ctrl+C`) agar port `8787` bebas.
2. Install PM2 sekali:
   ```bash
   bun install -g pm2
   ```
3. Install dependency dan build UI:
   ```bash
   bun install --frozen-lockfile
   bun run build
   ```
4. Start process:
   ```bash
   pm2 start ecosystem.config.cjs
   # atau: bun run pm2:start  (build + start/reload)
   ```
5. Verifikasi:
   ```bash
   pm2 status
   curl http://127.0.0.1:8787/api/health
   pm2 logs rc-autotrade --lines 100
   ```
6. Aktifkan auto-start setelah reboot:
   ```bash
   pm2 save
   pm2 startup
   # jalankan command sudo yang dicetak PM2, lalu:
   pm2 save
   ```

> Saat pertama kali berjalan, access key dicetak ke log `pm2 logs rc-autotrade`. Simpan key-nya,
> lalu jangan bagikan isi log. Setelah state ada, key tidak dicetak lagi.

### Update setelah pull

```bash
git pull
bun install --frozen-lockfile
bun run build
pm2 restart rc-autotrade --update-env
```

### Perintah harian

| Perintah | Fungsi |
| --- | --- |
| `pm2 status` | Lihat status worker |
| `pm2 logs rc-autotrade --lines 100` | Log worker |
| `pm2 restart rc-autotrade --update-env` | Restart setelah `.env`/build berubah |
| `pm2 stop rc-autotrade` | Stop sementara |
| `pm2 delete rc-autotrade` | Hapus dari PM2 |
| `pm2 monit` | Monitor CPU/RAM |

Catatan:

- Gunakan **satu instance** (`fork`), jangan `-i max`/cluster. Worker memegang satu koneksi
  WebSocket dan state order; beberapa instance bisa saling menimpa state dan mengirim order ganda.
- `.env` dibaca oleh worker dari `cwd` (`ecosystem.config.cjs` mengarah ke root project).
- Data target/session, loot log, agregat profit, dan cache harga pasar ada di database SQLite
  `data/rollercoin.db` (mode WAL). PM2 restart tidak menghapusnya.
- Log default ada di `~/.pm2/logs/`. Jangan commit log karena bisa memuat access key saat boot pertama.
- `pm2 startup` membutuhkan sudo. Jika tidak tersedia, jalankan PM2 dari systemd user service.

### Access key

| Kebutuhan | Cara |
| --- | --- |
| Key pertama kali | Dicetak di terminal, hanya sekali |
| Key tetap | Set `WORKER_TOKEN` (minimal 16 karakter) di `.env` |
| Key hilang | **Stop worker**, lalu `bun run key:reset` |
| Rotasi dari UI | Tab **LINK → ROTATE KEY** (ditolak bila `WORKER_TOKEN` dipin) |
| Ganti key | Semua socket lama dicabut (`4003`) dan client harus login ulang |

> `bun run key:reset` harus dijalankan saat worker berhenti. Kalau worker masih hidup, state
> in-memory-nya bisa menimpa key baru saat shutdown.

---

## Target configuration

Contoh dari brief, bisa langsung dipakai di UI atau `PUT /api/configs`:

```json
[
  {
    "itemId": "6571aa39ca8bfef34adcd9e9",
    "targetPrice": 0.1,
    "desiredQty": 10,
    "highStockThreshold": 20,
    "minAcceptableQty": 1
  }
]
```

| Field | Wajib | Aturan | Arti |
| --- | --- | --- | --- |
| `itemId` | ya | 6–64 karakter `A-Za-z0-9_-`, atau URL marketplace | Item yang diburu |
| `label` | otomatis | maks 48 karakter | Diisi worker dari item-info; UI tidak meminta input manual |
| `itemType` | tidak | default `mutation_component` | Dipakai untuk item-info dan purchase; worker auto-mencoba `mutation_component`, `miner`, `rack`, `battery`, dan mengoreksi dari frame market |
| `rarityColor` | otomatis | 6 digit hex | Warna aksen card, diisi dari `rarityGroup.baseHexColor` bila ada |
| `targetPrice` | ya | > 0, ≤ 1.000.000, maks 6 desimal | Harga maksimum **per unit** yang mau dibayar |
| `desiredQty` | ya | integer 1–10.000 | Qty normal **per order** |
| `highStockThreshold` | ya | integer `desiredQty`–1.000.000 | Book ≥ nilai ini → qty order diminta 2× `desiredQty` |
| `minAcceptableQty` | ya | integer 1..`desiredQty` | Batas bawah partial order |
| `allowPartialOrder` | tidak | boolean, default `true` | Izinkan beli seadanya saat stok < `desiredQty` |
| `maxSpendPerFill` | — | turunan UI, read-only | `targetPrice × desiredQty × 2`, plafon teoretis satu fill |
| `active` | tidak | boolean, default `true` | Arm/disarm target |

Arti detail keempat angka utama, urutan gerbang keputusan, contoh angka, dan kombinasi yang sering
salah dipahami ada di [Mekanisme parameter target](#mekanisme-parameter-target-deep-dive).

Perilaku pembelian:

- Hanya **listing termurah** yang dievaluasi (sama seperti `trade.js` asli), currency `RLT`.
  Listing diurutkan naik berdasarkan harga, lalu qty terbesar lebih dulu pada harga yang sama.
- Jika harga listing termurah di atas `targetPrice`, frame dilewati walau stok melimpah.
- Jika stok cukup, order berisi `desiredQty` (atau 2× saat high-stock aktif); jika stok kurang,
  order hanya terjadi ketika `allowPartialOrder` aktif dan stok >= `minAcceptableQty`.
- Target price dan book diperbarui dari setiap frame market; UI menandai card `HOT` saat harga
  berada di bawah target dan book memenuhi minimum.
- **BOOK** adalah quantity pada listing termurah dari frame market terakhir. Fill sukses
  menguranginya langsung; order gagal **tidak pernah** menambah atau mengubahnya. Angka ini tetap
  dipakai engine (dan ditampilkan di ticker), tetapi card target sekarang menampilkan success rate,
  bukan meter price/book.
- **ALLOWED RANGE** — `allowedPriceMin` / `allowedPriceMax` dari setiap frame market dikonversi dari
  micro-RLT ke RLT dan di-cache per target di runtime worker. Card menampilkan band tersebut, dan
  snapshot `/api/state` tetap membawanya setelah halaman di-reload. Frame baru langsung memperbarui
  nilai cache; frame yang tidak membawa band tidak menghapus nilai lama. Cache hilang saat worker
  restart, sama seperti chart history. Band ini murni informasi tampilan dan tidak pernah dipakai
  sebagai syarat beli.
- **PURCHASE SUCCESS** di card = `fills / (fills + failures) × 100%` untuk target tersebut selama
  sesi worker. Order yang masih pending tidak dihitung sampai fill atau gagal terkonfirmasi.
  Reset lifetime stats juga mereset counter per target ini.
- **Chart** menyimpan satu bucket per menit (maks 1.440 bucket = 24 jam) di runtime worker.
  Selector **1m / 5m / 15m / 1h** mengagregasi data di browser; 1m menampilkan 60 menit terakhir,
  1H menampilkan 24 jam. Mock mode mengisi 24 jam awal agar semua timeframe langsung terlihat;
  mode live mulai kosong dan terisi seiring waktu. History runtime hilang saat worker restart.
- **Profit** di dashboard = `(targetPrice - hargaBeli) × qty`, yaitu selisih terhadap harga target,
  bukan hasil penjualan kembali.
- **PROFIT BY PRODUCT** mengagregasi semua fill dan failure per `itemId` ke tabel `report_products` di `data/rollercoin.db`
  (`report.items`). Karena counter disimpan terpisah dari loot log, laporan tetap akurat setelah
  worker restart dan setelah sebagian besar log berada di luar window in-memory `MAX_HISTORY = 200`.
  Kolomnya: fills, failed, qty, spend, avg buy, profit, margin (`profit / spend`), success rate,
  dan last activity. `DELETE /api/history` mereset report bersama stats; menghapus satu target tidak
  menghapus riwayat produknya. Produk yang disimpan dibatasi `MAX_REPORT_PRODUCTS = 500`; saat daftar
  penuh, produk dengan profit terendah dibuang lebih dulu. Saat upgrade dari state lama tanpa
  `report`, worker membangun ulang laporan sekali dari seluruh loot log yang tersimpan.

### Trend watch — sinyal, bukan eksekusi

**TREND WATCH** membandingkan `targetPrice` tiap target dengan trend harga pasar, lalu memberi label
tanpa pernah mengirim order. `planOrder()` tidak berubah: `targetPrice` tetap satu-satunya gerbang
beli, dan panel ini murni informasi.

Modelnya ada di `src/shared/trend.js` (fungsi murni, tanpa akses store/clock) dan
`src/shared/depth.js` untuk estimasi harga eksekusi. Input trend **bukan lagi ask termurah**,
melainkan depth-weighted fill price:

0. Setiap frame, `estimateFillPrice()` menelusuri `list` (lalu `rest`) untuk `desiredQty` unit dan
   menghasilkan VWAP. Listing qty-1 hanya berbobot `1/q`, jadi tidak bisa mendikte trend. `rest`
   dipakai hanya bila ukuran order melewati kedalaman yang terlihat, dan hasilnya ditandai
   `approx`. Seri ini disimpan per menit di `price_points.fill_price` (schema v5), terpisah dari
   `price` ask termurah yang tetap dipakai chart. Karena VWAP memakai ukuran order, membeli 25 unit
   dari level 88 tidak mengubah `p_fill(25)` — sinyal tidak langsung hilang di mode live seperti
   pada seri ask termurah.
1. Harga diubah ke log space, lalu **Hampel filter** (±2 titik, 3σ, lantai 0,5%) membuang spike
   satu-dua frame sebelum smoothing melihatnya. Pergeseran harga yang bertahan tetap lolos setelah
   ~2 titik.
2. **EMA sadar-waktu** (`1 − exp(−dt/τ)`) untuk kaki cepat (15 mnt), lambat (60 mnt), dan sinyal
   (20 mnt). `dt` dipakai apa adanya, tanpa cap: setelah lubang 6 jam rata-rata lama tidak lagi
   membawa informasi, jadi `α → 1` dan kaki langsung menyesuaikan. Sebelumnya `dt` di-clamp 30 menit
   dan kaki 60 menit hanya bergerak 39% menuju harga baru — osilator masih melaporkan momentum
   berjam-jam setelah pergerakan selesai. Pada data tersimpan, melepas cap mempersempit band
   terkalibrasi (p50 7,8% → 7,3% pada coverage yang sama) dan menaikkan korelasi peringkat antara
   band dan error nyata (0,21 → 0,26).
3. **Level acuan** yang dibandingkan dengan `targetPrice` adalah **median geometrik 2 jam terakhir**,
   bukan ujung EMA. EMA adalah campuran eksponensial, jadi τ 60 menit tertinggal kira-kira satu τ
   penuh saat harga bergerak; pada replay data tersimpan ujung EMA berjarak median 4,2% (p75 8,7%)
   dari harga live, sedangkan median 2 jam 3,4% (p75 7,5%). Bila jendela 2 jam berisi kurang dari 3
   titik, model melebar ke 6 jam, lalu baru jatuh ke EMA. Kaki cepat/lambat tetap dipakai untuk
   osilator.
4. **Momentum** = `(fast − slow) × 100` — satu-satunya keluaran osilator yang dipakai (`signal` dan
   `hist` ala MACD sudah dibuang karena tidak ada yang mengonsumsinya; `momentumPct` tetap dipakai
   panel dan band).
5. **Slope** regresi berbobot durasi 3 jam (%/jam) memberi arah independen beserta `slopeTStat`,
   dan **noise** (MAD residual) menjadi salah satu masukan band ketidakpastian.

Saat depth history belum mencapai window minimum (instalasi baru, atau ~30 menit pertama setelah
upgrade), model jatuh ke seri ask termurah dan menandai `source: "top"`; setelah itu `source:
"depth"`. Tooltip pada panel dan chip menyebut sumber serta ukuran order yang dipakai.

Klasifikasi memakai `proximity = targetPrice / trend − 1`, tetapi **lebarnya band tidak lagi
konstan**. Model menerbitkan `errorBandPct` — perkiraan seberapa jauh level acuan itu bisa meleset
dari harga pasar saat ini — dari dispersi (`noisePct`) dan lag (`|momentumPct|`, jarak fast/slow yang
persis merupakan lag level terhadap pergerakan), lalu dikalikan faktor hasil kalibrasi:

```
rawBandPct   = clamp(0.9 × noisePct + 1.2 × |momentumPct|, 2%, 20%)
errorBandPct = clamp(rawBandPct × factor, 2%, 20%)
bandPct      = clamp(errorBandPct, 2%, 10%)
```

`factor` dibaca dari `src/shared/trend-calibration.js` (lihat bagian kalibrasi di bawah); nilainya 1
bila worker belum pernah dikalibrasi. Field `bandSource` menyebut mana yang dipakai (`calibrated`
atau `formula`).

| State | Syarat | Arti |
| --- | --- | --- |
| `ABOVE` | proximity > +`bandPct` | Target di atas trend: bot membeli di harga pasar, bukan saat diskon |
| `NEAR` | \|proximity\| ≤ `bandPct`, band < 5% | Target menempel di trend; margin tipis |
| `UNSURE` | \|proximity\| ≤ `bandPct`, band ≥ 5% | Jarak lebih kecil dari ketidakpastian model: arah tidak bisa dibaca |
| `BELOW` | < −`bandPct` | Target di bawah trend; menunggu dip |
| `FAR_BELOW` | ≤ −20% | Terlalu jauh di bawah trend; fill bisa langka |
| `STALE` | tidak ada frame > 30 mnt | Sinyal dibekukan, tidak dipakai sebagai saran |
| `NO_DATA` | < 12 titik atau span < 30 mnt | Belum cukup history |

Hysteresis keluar di 1,5 × `bandPct` (minimum 3%) mencegah label berkedip di batas. `UNSURE`
dipertahankan selama masih di dalam band, jadi label tidak bolak-balik ke `NEAR` hanya karena
dispersi bergeser sedikit. Panel menampilkan nilai band sebagai `±x%` di bawah harga trend, dan
tooltip kolom confidence menyebutnya juga.

**Confidence** 0–100 menjawab satu pertanyaan saja: **seberapa banyak bukti yang dimiliki model?**
Presisi adalah pertanyaan lain dan punya angkanya sendiri (`errorBandPct`). Sebelumnya dispersi dan
lag ikut dihitung di confidence *dan* di band, jadi seri berisik dihukum dua kali dan skornya sulit
dijelaskan.

```
confidence = 100 × quality × (0.4 + 0.6 × density) × Σ wᵢ · sᵢ
w = { fresh 0.34, span 0.28, count 0.22, gap 0.16 }
quality = 0.85 bila source "top" · 0.7 bila thinBook · 0.9 bila fillApprox
density = clamp(coveragePct / 25, 0, 1)
```

`density` mengalikan **seluruh** skor: window 12 jam berisi 17 titik (coverage 2%) tidak boleh
memperoleh nilai tinggi hanya karena frame terakhir segar. Ambang 25% bukan 100% karena
`coveragePct` diukur terhadap grid 1 menit yang tidak pernah dipenuhi feed event-driven — satu
titik per ~4 menit sudah dianggap padat untuk EMA 15/60 menit. `stale` tetap memotong skor ke ≤ 20.

`reliable` adalah verdict yang menggabungkan kedua sumbu:

```
reliable = confidence ≥ 50  &&  errorBandPct ≤ 10%
```

`reliableReason` menyebut mana yang gagal — `low-confidence` atau `wide-band` — dan panel
menampilkannya sebagai `low conf` / `wide band`, bukan satu label generik.

### Next step (`action`)

Panel punya satu kolom **Next step**, diturunkan dari state dan verdict `reliable`:

| Action | Kapan | Arti |
| --- | --- | --- |
| `LOWER` | state `ABOVE` **dan** reliable | Turunkan ceiling: target di atas level, bot akan beli di pasar |
| `HOLD` | state `BELOW` atau `NEAR` | Biarkan; sisi sehat, bot menunggu dip |
| `REVIEW` | state `FAR_BELOW` | Tinjau: target > 20% di bawah level, fill bisa langka |
| `WAIT` | `UNSURE`, `STALE`, `NO_DATA`, atau `ABOVE` yang belum reliable | Belum bisa diputuskan |
| `SET` | `NO_TARGET` | Belum ada ceiling |

`actionReason` menyebut sebabnya (`above-trend`, `below-trend`, `at-trend`, `far-below`,
`unresolved`, `low-confidence`, `wide-band`, `stale`, `no-data`, `no-target`) dan dipakai sebagai
tooltip.

Aturannya asimetris dengan sengaja: **hanya perubahan harga** yang menuntut bacaan terpercaya.
`LOWER` baru muncul kalau `reliable` (bukti cukup **dan** band ≤ 10%); `HOLD` dan `REVIEW` menjaga
status quo, jadi tetap boleh walau datanya tipis — target yang 50% di bawah level tetap layak
ditinjau meski confidence-nya rendah. Model tidak pernah mengusulkan "beli" dan tidak pernah
menyebut angka; `planOrder()` tetap satu-satunya yang bisa mengirim order.

### Kalibrasi band (`bun run calibrate:trend`)

`errorBandPct` punya dua sumber. Yang pertama rumus di atas, selalu tersedia dan hanya kira-kira
benar. Yang kedua hasil pengukuran: `scripts/calibrate-trend.js` me-replay `price_points` yang
tersimpan, membandingkan tiap estimasi dengan error yang benar-benar terjadi, lalu menulis faktor
koreksi ke `src/shared/trend-calibration.js`:

```
factor = kuantil_target(error / rawBandPct)      // default target 0.80
```

Faktornya satu untuk semua bucket, bukan kurva. Kurva per bucket terlihat lebih rapi di atas kertas,
tetapi butuh ribuan sampel sebelum batas bucket berhenti menjadi noise — pada database 12 jam ia
menghasilkan band 24% untuk bucket paling tenang karena dua lonjakan kebetulan masuk ke sana. Satu
faktor membawa informasi yang sama dengan varians jauh lebih kecil, dan tidak bisa terbalik. Faktor
per sumber (`top` / `depth`) hanya dipakai bila sumber itu punya ≥ 200 sampel; kalau tidak, ia
mewarisi `pooled`.

```bash
bun run calibrate:trend                       # 14 hari terakhir, kuantil 80%
bun run calibrate:trend --days=7 --target=0.9
```

Skrip menolak menulis bila replay menghasilkan < 200 sampel (tabel dari beberapa lusin titik lebih
buruk daripada rumus), mencetak coverage rumus vs hasil kalibrasi, dan menghitung **holdout**: faktor
diukur pada 70% data tertua lalu diuji pada 30% terbaru, supaya angka in-sample yang bagus tidak
menipu. Contoh pada replay terakhir: rumus menutup 76,1% error, setelah kalibrasi (faktor 1,20 untuk
`top`, 1,13 untuk `depth`) 81,8%, holdout 89,1%. Jalankan ulang setelah beberapa hari data terkumpul
— angkanya akan membaik dengan sendirinya.

Skrip yang sama juga menguji klaim arah. Setiap kali dijalankan ia mencetak hit rate label
`direction` pada horizon 60 menit dan base rate-nya, supaya kalimat "ini bukan ramalan" di bawah
bisa diverifikasi ulang, bukan diwarisi.

### Arah slope (`direction`)

`direction` hanya `up`/`down` bila regresi slope 3 jam punya `|slopeTStat| ≥ 2`; kalau tidak, `flat`.
Label ini **deskripsi tiga jam terakhir, bukan ramalan**. Pada replay terakhir: sign
`slopePctPerHour` benar 51,6% pada horizon 60 menit dan label non-flat 48,5%, sementara base rate
30,6% naik / 69,4% turun — selalu menebak "turun" justru lebih akurat. Panel tidak memakainya untuk
keputusan apa pun; angka ini dicetak setiap kali `bun run calibrate:trend` dijalankan, jadi klaimnya
bisa diverifikasi ulang.

Kolom **Signal** (state) dan **Next step** (`action`) sengaja dipisah: yang pertama menjawab "di
mana posisi target", yang kedua "apa yang harus dilakukan". Panel menampilkan tab **at/above trend** (target ≥ trend, gap terbesar di atas — yang paling perlu
diturunkan), **below trend** (sisi sehat, menunggu dip), dan **all**, dengan tombol edit serta tautan
marketplace. Daftar default sengaja hanya target ≥ trend: itulah yang perlu di-follow up. Card target
menampilkan chip trend yang sama.

Model dihitung di engine saat frame market (throttle 10 detik), saat hydrate setelah restart, dan
langsung dihitung ulang lewat `engine.refreshTrend()` ketika target ditambah/diedit. Hasilnya ikut di
`runtime.trend` pada `/api/state` dan pada event `market`. Field-nya: `status`, `source` (`depth` /
`top`), `trend`, `fast`, `momentumPct`, `slopePctPerHour`, `noisePct`,
`confidence`, `reliableReason`, `action`, `actionReason`, `slopeTStat`, `errorBandPct`, `bandPct`, `bandSource`, `uncertain`, `points`, `spanMs`,
`maxGapMs`, `coveragePct`, `direction`, `lastSampleAt`, `computedAt`, `target`, `proximityPct`, `gapPct`,
`state`, `reliable`, `stale`, `bookQty`, `thinBook`, `fillPrice`, `fillQty`, `fillPartial`,
`fillApprox`.

Batasannya: `rest` hanya mengungkap harga lantai tail, bukan bentuk lengkapnya (karena itu
`approx`); window efektif 24 jam; dan butuh sekitar 30 menit depth history sebelum sinyal pindah
dari `top` ke `depth`. Perbandingan tetap `targetPrice` vs trend, dan `planOrder()` tidak berubah —
ia masih hanya melihat listing termurah.

Label otomatis diambil dari:

```http
GET https://rollercoin.com/api/marketplace/item-info?itemId=<id>&itemType=mutation_component&currency=RLT
```

Worker menyusun `label` menjadi `${rarityGroup.title.en} ${name.en}`. `rarityGroup` boleh tidak ada
— fallback-nya adalah `name.en`, lalu `ITEM <short-id>` bila keduanya kosong.

Karena tidak semua item adalah `mutation_component`, resolver mencoba beberapa tipe yang valid
(`mutation_component`, `miner`, `rack`, `battery`) sampai item ditemukan. Saat frame market tiba,
`itemType` dari socket juga dipakai untuk mengoreksi target dan memicu lookup ulang. Contoh:
Rack `63e140e4547cfab9a21d7d2b` akan resolve sebagai **“Jet-Black Rack 6”** meski awalnya
dicoba sebagai `mutation_component`.

Endpoint ini juga tersedia lewat control plane: `GET /api/item-info?itemId=...&itemType=...`.
Jika dipanggil untuk target yang sudah ada, endpoint sekaligus memperbaiki label target tersebut.

---

## Mekanisme parameter target (deep dive)

Form target hanya meminta empat angka, tetapi keputusan beli diambil oleh satu fungsi murni —
`planOrder()` di `src/shared/rules.js` — yang hanya melihat **harga listing termurah**, **stok
listing termurah**, dan config. Tidak ada order book agregat, tidak ada averaging, dan tidak ada
pembatalan order. Bagian ini menjelaskan urutan gerbangnya, arti tiap field, dan kombinasi yang
sering membuat order "tidak jalan".

### Peta label UI ke field

| Label di form target | Field | Inti |
| --- | --- | --- |
| **TARGET PRICE (RLT)** — *Never pay more than this per item.* | `targetPrice` | Plafon harga **per unit** |
| **DESIRED QTY** | `desiredQty` | Qty dasar **per order** |
| **HIGH STOCK THRESHOLD** — *At or above this stock, quantity doubles.* | `highStockThreshold` | Pemicu qty 2× |
| **MIN ACCEPTABLE QTY** — *Partial fills below this are skipped.* | `minAcceptableQty` | Ambang bawah partial |
| **MAX SPEND PER FILL** — *Target price × doubled quantity ceiling.* | turunan (read-only) | Plafon biaya satu fill |
| **ALLOW PARTIAL ORDER** — *Buy whatever is available above the minimum.* | `allowPartialOrder` | Sakelar partial |
| **ARM TARGET** — *Start hunting as soon as it is saved.* | `active` | Arm/disarm target |

### Satu frame = satu keputusan

`engine.js` memanggil `planOrder()` untuk setiap frame market yang cocok. Rantai keputusannya:

```text
frame market (item cocok, currency RLT)
        │
        ▼
listing termurah → unitPrice, available
        │
        ├─ paused ───────────────────────────────────────► skip   paused
        ├─ target inactive ──────────────────────────────► skip   inactive
        ├─ harga tidak valid (NaN / ≤ 0) ────────────────► skip   invalid-price
        ├─ available < 1 ────────────────────────────────► skip   no-stock
        ├─ unitPrice > targetPrice (+1e-9) ──────────────► skip   price-above-target
        ▼
   available >= desiredQty ?
        │
        ├─ ya ─ available >= highStockThreshold ? ──► qty = 2 × desiredQty (doubled)
        │         └─ tidak ─────────────────────────► qty = desiredQty
        │
        └─ tidak ─ allowPartialOrder && available >= minAcceptableQty ?
                    ├─ ya ──────────────────────────► qty = available
                    └─ tidak ───────────────────────► skip   stock-below-minimum
        ▼
qty = clamp(floor(qty), 1 .. min(available, 10 000))
notional = unitPrice × qty
```

Setelah `planOrder()` memutuskan `buy`, masih ada gerbang runtime di `engine.js` yang bisa menahan
order: `paused`, order lain yang belum selesai (`inflight`), backoff setelah rejection
(5s → 10s → 20s → 40s, maksimal 60s), cooldown `delayPerOrder`, batas `maxOrdersPerMinute`, dan
ketersediaan access token (kecuali `dryRun`). Gerbang-gerbang ini **tidak** mengubah qty.

### `targetPrice` — gerbang harga, per unit

| Aspek | Perilaku |
| --- | --- |
| Satuan | RLT per **satu item**, bukan total order |
| Batas | > 0, maks 1.000.000 RLT, maks 6 desimal |
| Dibandingkan dengan | `unitPrice` listing termurah (harga micro-RLT ÷ 1.000.000) |
| Lolos bila | `unitPrice <= targetPrice + 1e-9` (epsilon float, jadi harga yang persis sama tetap dibeli) |
| Ditolak bila | `unitPrice > targetPrice` → `price-above-target`, frame dilewati |
| Sumber angka | `config.targetPrice` |

- Anda **tidak** menawar. Kalau listing termurah di bawah target, engine tetap membayar harga
  listing tersebut (bukan `targetPrice`), sehingga biaya nyata ≤ plafon.
- `targetPrice` bukan budget dan bukan batas total. Total satu order = `unitPrice × qty`, dan bisa
  sampai 2× `desiredQty` item saat high-stock aktif — lihat `maxSpendPerFill` di bawah.
- Band harga dari Rollercoin (`allowedPriceMin` / `allowedPriceMax`) hanya informasi untuk card.
  Band ini tidak dipakai sebagai syarat beli dan tidak mengubah `targetPrice`.
- Menurunkan `targetPrice` membuat order lebih jarang tetapi profit per item lebih besar; menaikkan
  mendekati harga pasar membuat order lebih sering dengan margin lebih tipis. Profit yang dicatat
  adalah `(targetPrice − hargaBeli) × qty`, jadi target yang terlalu tinggi juga menggelembungkan
  angka profit.

### `desiredQty` — qty dasar per order

| Aspek | Perilaku |
| --- | --- |
| Tipe | integer 1–10.000 |
| Dipakai saat | `available >= desiredQty`; menjadi dasar doubling |
| Batas nyata | `min(desiredQty, available, 10.000)` saat doubling tidak aktif |
| Bukan | target harian, total pembelian, atau plafon saldo |

- Ini angka **per order**, bukan "beli 50 lalu berhenti". Setiap frame market berikutnya yang lolos
  bisa memicu order baru; yang membatasi adalah cooldown, `inflight`, dan rate limit.
- Kalau `available < desiredQty`, `desiredQty` tidak dipakai untuk memotong qty; cabang partial
  mengambil `available` apa adanya (bila diizinkan).
- Nilai `desiredQty` besar tidak memaksa satu order raksasa: dengan `desiredQty = 5.000` dan book
  60, qty tetap 60 saat partial aktif — atau tidak ada order sama sekali saat partial dimatikan.

### `highStockThreshold` — pemicu qty 2×

| Aspek | Perilaku |
| --- | --- |
| Tipe | integer, `>= desiredQty`, maks 1.000.000 |
| Kondisi aktif | `available >= highStockThreshold` |
| Efek | qty **diminta** menjadi 2× `desiredQty` (bukan 2× threshold) |
| Batas | tetap `min(2 × desiredQty, available, 10.000)` |

Contoh dengan `desiredQty = 50`, `highStockThreshold = 100`:

| `available` | qty | `doubled` | Catatan |
| --- | --- | --- | --- |
| 49 | 49 | tidak | partial order |
| 50 | 50 | tidak | pas desiredQty |
| 99 | 50 | tidak | stok belum menyentuh threshold |
| 100 | 100 | ya | 2× desiredQty, masih tertutup stok |
| 150 | 100 | ya | plafon 2× desiredQty |

Dengan `highStockThreshold = 60` dan `available = 70`, engine meminta 100 tetapi memotongnya menjadi
70 — doubling tetap "sebanyak stok yang ada", bukan paksa 2×.

Hal yang sering keliru:

- Doubling **tidak** berarti qty = 2× `available`; ia hanya meminta 2× `desiredQty`.
- `highStockThreshold == desiredQty` membuat fitur menyala setiap kali stok cukup, sehingga qty
  dasar `desiredQty` praktis tidak pernah dipakai (selalu 2× atau dipotong stok).
- Threshold yang jauh di atas book normal (misal 10.000 untuk item sepi) membuat fitur ini tidak
  pernah aktif — tidak ada efek samping lain.
- Threshold dihitung dari stok listing termurah pada frame itu, bukan akumulasi beberapa listing.
- Saat fill sukses, book runtime dikurangi qty fill; frame market berikutnya bisa mengembalikan book
  ke angka asli bila memang ada stok baru. Jadi doubling bisa menyala-mati antar frame.
- Qty hasil doubling tetap dibatasi `10.000`, jadi `desiredQty` di atas 5.000 tidak akan pernah
  menghasilkan 2× penuh.

### `minAcceptableQty` + `allowPartialOrder` — kapan boleh beli seadanya

Keduanya hanya bekerja sebagai pasangan. `allowPartialOrder` adalah sakelar, `minAcceptableQty`
adalah ambang batas bawahnya.

| Stok tersedia (`available`) | `allowPartialOrder = true` | `allowPartialOrder = false` |
| --- | --- | --- |
| `>= highStockThreshold` | 2× `desiredQty` (dipotong stok) | sama |
| `>= desiredQty`, `< threshold` | `desiredQty` | sama |
| `>= minAcceptableQty`, `< desiredQty` | `available` (partial fill) | **skip** `stock-below-minimum` |
| `< minAcceptableQty` | **skip** `stock-below-minimum` | **skip** `stock-below-minimum` |

- `minAcceptableQty` hanya dievaluasi di cabang stok kurang dari `desiredQty`. Ia tidak membatasi
  qty saat stok cukup, dan tidak mempengaruhi doubling.
- Saat partial aktif, qty = seluruh `available` (bukan `minAcceptableQty`). `minAcceptableQty = 5`
  dengan stok 40 dan `desiredQty = 50` menghasilkan order 40, bukan 5.
- `allowPartialOrder = false` membuat `minAcceptableQty` tidak relevan sama sekali: stok di bawah
  `desiredQty` selalu dilewati.
- `minAcceptableQty = desiredQty` adalah cara lain mematikan partial tanpa mengubah switch: partial
  tidak akan pernah memenuhi syarat karena `available < desiredQty` berarti `available < min`.
- Partial fill lebih mahal per usaha: satu order menghabiskan satu slot cooldown dan satu slot rate
  limit, berapa pun qty-nya. Menurunkan `minAcceptableQty` menambah jumlah order kecil.

> Catatan `HOT`: card menandai `HOT` saat target aktif, harga ≤ `targetPrice`, dan
> `lastStock >= minAcceptableQty`. Penanda ini sengaja longgar sebagai petunjuk "harga masuk".
> Keputusan final tetap `planOrder()`; dengan `allowPartialOrder = false` sebuah card bisa tampak
> `HOT` padahal engine melewatinya karena stok < `desiredQty`. Event `market` di WebSocket memakai
> definisi yang lebih ketat: `hot = plan.action === "buy"`.

### `maxSpendPerFill` — plafon teoretis, bukan field

Nilai abu-abu di form target **tidak dikirim** ke worker dan tidak disimpan ke database. UI
menghitungnya sebagai `targetPrice × desiredQty × 2` supaya Anda bisa melihat skenario terburuk
satu fill: harga pas di target **dan** high-stock menyala.

```text
maxSpendPerFill = targetPrice × desiredQty × 2
biaya sebenarnya = unitPrice × qty  ≤  maxSpendPerFill
```

- Kalau ingin aman terhadap saldo, pastikan saldo RLT ≥ total `maxSpendPerFill` dari semua target
  yang armed, karena beberapa target bisa menembak hampir bersamaan.
- Worker tidak mengecek saldo sebelum order. Bila saldo kurang, purchase bisa ditolak marketplace;
  target lalu masuk backoff dan tercatat sebagai failure.
- Plafon ini hanya bergantung pada `targetPrice` dan `desiredQty` (faktor 2 selalu dihitung),
  sehingga menurunkan `highStockThreshold` tidak menurunkan plafon.

### Contoh lengkap (config di screenshot)

`targetPrice = 0.012`, `desiredQty = 50`, `highStockThreshold = 100`, `minAcceptableQty = 5`,
`allowPartialOrder = true`, sehingga `maxSpendPerFill = 1.200000 RLT`:

| Frame market | Keputusan | Alasan |
| --- | --- | --- |
| harga 0.0115, stok 60 | buy 50 | stok ≥ 50, < 100 |
| harga 0.0115, stok 120 | buy 100 | stok ≥ 100 → doubled, total ≤ 1.2 RLT |
| harga 0.0115, stok 40 | buy 40 | partial (40 ≥ 5) |
| harga 0.0115, stok 4 | skip | `stock-below-minimum` |
| harga 0.0125, stok 500 | skip | `price-above-target` |
| harga 0.0120, stok 100 | buy 100 | harga tepat di target tetap lolos (epsilon) |
| sama, `allowPartialOrder = false`, stok 40 | skip | partial dimatikan |

### Resep tuning

| Tujuan | `desiredQty` | `highStockThreshold` | `minAcceptableQty` | Partial |
| --- | --- | --- | --- | --- |
| Sniper konservatif, order kecil | 10 | 20 | 1 | on |
| Borong saat book tebal | 50 | 100 | 5 | on |
| Hanya mau lot penuh | 50 | 100 | 50 | off |
| Hindari pecahan receh | 20 | 40 | 10 | on |
| Uji strategi tanpa risiko | apa saja | apa saja | apa saja | apa saja + `dryRun` |

Aturan praktis:

- `minAcceptableQty = 1` memaksimalkan jumlah fill, tetapi banyak order 1-item yang menghabiskan
  cooldown.
- `minAcceptableQty` mendekati `desiredQty` memberi fill lebih jarang dengan qty lebih padat.
- `highStockThreshold = 2 × desiredQty` adalah nilai paling intuitif: doubling hanya aktif saat book
  benar-benar dua kali lipat kebutuhan normal.
- Selalu cek `maxSpendPerFill` terhadap saldo sebelum meng-arm banyak target sekaligus.

### Reason code yang muncul di log

| Reason | Arti | Perbaikan |
| --- | --- | --- |
| `paused` | PAUSE global aktif | Resume dari header |
| `inactive` | Target di-disarm | Arm target |
| `invalid-price` | Harga frame tidak valid / ≤ 0 | Tunggu frame berikutnya |
| `no-stock` | Book kosong | Tunggu restock |
| `price-above-target` | Harga termurah > `targetPrice` | Naikkan target atau tunggu harga turun |
| `stock-below-minimum` | Stok < `desiredQty` dan syarat partial tidak terpenuhi | Turunkan `minAcceptableQty`, aktifkan partial, atau turunkan `desiredQty` |

`planOrder()` sendiri tidak mengembalikan alasan untuk gerbang runtime (`inflight`, cooldown,
rate limit, backoff, no-token); kondisi itu terlihat di log `scan`/`order` dan di state card.

### Di mana di kode

| Bagian | Lokasi |
| --- | --- |
| Keputusan beli murni | `planOrder()` di `src/shared/rules.js` |
| Validasi & normalisasi field | `validateTarget()` di `src/shared/validation.js` |
| Gerbang runtime + eksekusi | `#handleMarket()` / `#submit()` di `src/worker/engine.js` |
| Preview `maxSpendPerFill` | `TargetModal` di `ui/src/components/Modals.jsx` |
| Penanda `HOT` di card | `runtimeHot()` di `ui/src/lib/format.js` |

---

## Link sesi Rollercoin

### Cara manual

1. Login ke `rollercoin.com` di browser.
2. DevTools → **Application → Local Storage → https://rollercoin.com**.
3. Salin nilai `token` dan `refreshToken`.
4. Buka tab **LINK → PASTE TOKEN** di control deck.
5. Isi cookie Cloudflare hanya jika API purchase ditolak (lihat troubleshooting).

Worker menyimpan access + refresh token di `data/rollercoin.db` (mode `0600`) dan otomatis
memperbarui access token **2 menit sebelum** JWT kedaluwarsa. Token Rollercoin umumnya berumur
600 detik, jadi refresh terjadi sekitar tiap 8 menit — bukan tiap 30 detik. Untuk token opaque,
fallback-nya tiap 25 menit. Refresh terjadwal **tidak** menutup/membuka ulang market socket;
socket hanya di-reconnect saat statusnya tidak online atau saat token diganti manual/bridge.
Tombol **REFRESH NOW** memaksa refresh. Jika refresh gagal, status berubah menjadi `TOKEN ERROR`
dan token baru harus ditempel ulang.

> Bridge tetap mengirim POST `/api/session/push` tiap 30 detik. Itu sinkronisasi token HTTP,
> bukan reconnect WebSocket.

### Token bridge — pilih salah satu

Bridge **tidak wajib**. Kalau Anda hanya ingin worker berjalan, tempel token sekali di tab **LINK**;
worker menyimpannya dan memperbarui sendiri selama refresh token masih valid. Bridge berguna saat
Anda juga membuka Rollercoin dan ingin `localStorage` situs tetap sinkron dengan worker.

Tab **LINK → TOKEN BRIDGE** sekarang punya dua mode:

#### 1. Userscript — pasang sekali, jalan otomatis (disarankan)

1. Install **Tampermonkey** (atau Violentmonkey) di browser.
2. Buka control deck → **LINK → TOKEN BRIDGE** → tab **USERSCRIPT · AUTO**.
3. Klik **COPY USERSCRIPT** atau **DOWNLOAD .USER.JS**.
4. Tampermonkey Dashboard → **+** (script baru) → paste → **Ctrl+S**.
5. Buka `rollercoin.com` dalam keadaan login. Userscript otomatis mulai, mengirim token tiap
   30 detik, dan restart sendiri setiap halaman di-reload.
6. Biarkan tab Rollercoin terbuka (boleh inactive). Worker tetap mengeksekusi order.

Setelah ini alur harian Anda hanya: **nyalakan worker → buka Rollercoin seperti biasa**. Tidak perlu
paste token lagi selama refresh token masih hidup.

> Jika access key dirotasi atau worker diganti domain/port, buka bridge lagi dan pasang userscript
> baru. File userscript memuat access key + URL worker; jangan dibagikan.
>
> Userscript memakai `GM_xmlhttpRequest`, sehingga tidak terkena CORS/CSP halaman Rollercoin.
> Jika Tampermonkey meminta izin koneksi ke `127.0.0.1`, pilih **Allow**. Kalau sebelumnya sudah
> memasang userscript versi lama, regenerate dan ganti script-nya — versi lama masih memakai
> `fetch` biasa.

#### 2. Console snippet — sekali pakai

1. Buka `rollercoin.com` yang sudah login.
2. DevTools → **Console** → paste snippet → Enter.
3. Bridge berjalan sampai halaman di-reload. Setelah reload, paste ulang.

#### Yang dilakukan bridge

Setiap 30 detik:

1. Mengirim `token`/`refreshToken` dari `localStorage` Rollercoin ke worker.
2. Menerima balik token yang lebih baru dari worker.
3. Menulis token baru ke `localStorage` Rollercoin bila berbeda.

Dengan begitu worker dan tab Rollercoin tidak saling menimpa token. Worker tetap melakukan refresh
sendiri; bridge hanya menjembatani localStorage. Hentikan dengan `__TURU_RC_BRIDGE__.stop()`.

> Kedua mode memuat access key control deck. Jangan bagikan screenshot/console yang memuatnya.
> CORS dan Private Network Access diizinkan untuk endpoint push, tetapi tetap membutuhkan bearer key.
> Jika halaman Rollercoin HTTPS tidak bisa menghubungi worker HTTP, gunakan URL `localhost`/
> `127.0.0.1` atau pasang worker di balik HTTPS.

### Cookie Cloudflare

Purchase dari worker tidak membawa cookie browser. Bila Rollercoin membalas `403`/halaman challenge,
salin header cookie dari request browser (minimal `cf_clearance`, sering juga `__cf_bm`) ke field
**CLOUDFLARE COOKIE** atau `RC_COOKIE`. Cookie dan IP harus cocok; ini bukan jaminan bypass.

---

## Environment variables

| Variable | Default | Keterangan |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | Bind address control plane |
| `PORT` | `8787` | Port control plane (`0` = pilih acak) |
| `WORKER_TOKEN` | — | Pin access key (>= 16 karakter) |
| `DATA_DIR` | `./data` | Lokasi database `rollercoin.db` (+ `-wal`/`-shm`) |
| `RC_API_BASE` | `https://rollercoin.com` | Base URL API |
| `RC_WS_URL` | `wss://nws.rollercoin.com/` | URL market socket |
| `RC_COOKIE` | — | Cookie header awal (bisa juga dari UI) |
| `RC_KEEPALIVE_MS` | `30000` | Interval warm probe; `0` = mati (5000–600000 ms) |
| `RC_KEEPALIVE_URL` | `${RC_API_BASE}/api/events/config` | Endpoint probe publik |
| `SYSTEM_SAMPLE_MS` | `2000` | Interval sampling health worker (500–60000 ms) |
| `MOCK` | `0` | `1` = simulator, tidak ada order nyata |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn`, `error` |

Bun mengelola pool koneksi keep-alive per origin secara otomatis (tidak ada agent/dispatcher yang
perlu dikonfigurasi). Warm probe hanya dikirim bila tidak ada request nyata dalam ~90% interval,
memakai `GET` publik (bukan payload palsu), dan otomatis berhenti 5 menit bila menerima `403`/`429`.

### Latency worker → upstream

Worker mengukur RTT ke Rollercoin secara pasif di empat channel — tidak ada request tambahan
yang dikirim hanya untuk pengukuran:

| Channel | Sumber sampel |
| --- | --- |
| `HTTP` | Purchase, item-info, token refresh (termasuk yang gagal/timeout) |
| `WARM PROBE` | Warm probe keepalive (`GET` periodik) |
| `WS HANDSHAKE` | Koneksi socket market feed (connect → open) |
| `WS PING` | Ping/pong heartbeat socket market feed (~25 detik sekali) |

Setiap channel menyimpan sampel berjalan (maks 120) dan diekspos sebagai last/avg/p50/p95/min/max/
jitter + sparkline di tab **LATENCY** control deck (pratinjau ringkas juga ada di HUD). Panel ini
live update: worker broadcast view latency terbaru tiap 5 detik lewat control socket, dan tab
LATENCY juga mem-poll `GET /api/latency` tiap 10 detik sebagai fallback. Call yang
gagal tetap dihitung karena tetap memakai waktu upstream. Tombol **UKUR SEKARANG** menjalankan
warm probe on-demand via `POST /api/latency/probe`. `DELETE /api/history` menghapus sampel latency
bersama statistik lainnya.

`HOST=0.0.0.0` memungkinkan akses dari LAN. Jangan lakukan itu tanpa firewall/TLS, dan ingat
control deck memegang kredensial Rollercoin.

### Health worker & host

Worker men-sample dirinya sendiri tiap `SYSTEM_SAMPLE_MS` (default 2 detik) tanpa dependency
tambahan, lalu menyimpan maksimal 120 titik historis di memori (tidak pernah ditulis ke SQLite).
Tab **SYSTEM** menampilkan:

| Metrik | Sumber / arti |
| --- | --- |
| `Worker CPU` | `process.cpuUsage()` delta ÷ wall time — persen dari satu core |
| `Host CPU` | Delta `os.cpus()` — persen dari seluruh core |
| `Heap` | `heapUsed / heap_size_limit` dari V8 |
| `RSS` | Resident set size proses worker |
| `External` | Buffer/native memory di luar heap V8 |
| `System RAM` | `MemTotal − MemAvailable` (bukan `MemFree`, agar page cache tidak dihitung) |
| `Load 1m` | `os.loadavg()[0]`, dinormalisasi per core untuk indikator warna |
| `Loop p95` | Histogram `monitorEventLoopDelay` — indikator event loop tersendat |
| `DB` / `WAL` | Ukuran file `rollercoin.db`, `-wal`, dan `-shm` di disk |
| `Q/S` | Statement SQLite per detik sejak sampel terakhir |
| `DB p95` | Persentil 95 durasi statement dalam 60 detik (lihat bagian performa database) |
| `Handles` | `process.getActiveResourcesInfo().length` |

Status health: `HEALTHY` → `DEGRADED` pada heap ≥ 80%, RAM ≥ 88%, event-loop p95 ≥ 100 ms, DB p95
≥ 25 ms, WAL ≥ 64 MB, ada error SQLite, atau CPU worker ≥ 95%; `CRITICAL` pada heap ≥ 92%, RAM ≥
96%, p95 ≥ 250 ms, DB p95 ≥ 100 ms, atau WAL ≥ 256 MB. Transisi ke `DEGRADED`/`CRITICAL` (dan
pemulihannya) ditulis ke log scope
`system`, jadi tetap terlihat walau tidak ada browser yang terbuka. Panel live lewat broadcast
`system` tiap 5 detik dan juga mem-poll `GET /api/system` tiap 10 detik sebagai fallback.

### Performa database (SQLite)

Database memakai `bun:sqlite` — SQLite yang sudah tertanam di Bun, tanpa native addon atau
node-gyp. File `data/rollercoin.db` tetap format SQLite standar, jadi database lama dari versi
Node/better-sqlite3 langsung terbaca dan bisa dibuka dengan `sqlite3` CLI.

Semua statement yang melewati `Store` diukur. `DbMetrics` membungkus `db.prepare()` dan
`db.transaction()` **sebelum** prepared statement hot-path dibuat, lalu mencatat durasi setiap
`run` / `get` / `all` dengan `performance.now()`. Yang dikumpulkan:

| Blok | Isi |
| --- | --- |
| `files` | Ukuran `rollercoin.db`, `-wal`, `-shm`, dan totalnya |
| `storage` | `page_size`, `page_count`, `freelist_count`, payload bytes, free bytes, utilisasi halaman |
| `sqlite` | `driver` (`bun:sqlite`), `sqlite_version`, `journal_mode`, `synchronous`, `cache_size`, `wal_autocheckpoint`, `busy_timeout` |
| `queries` | Total statement, read/write/pragma/schema, error, `SQLITE_BUSY`, avg/max/last ms, rate per detik |
| `window` | Agregat 60 detik terakhir: count per jenis, avg/max/p95 ms, error, slow |
| `transactions` | Jumlah commit/rollback, total dan max wall time |
| `recentSlow` | 10 statement terakhir di atas ambang lambat, berisi SQL snippet (bukan value) |
| `history` | Titik per sampel: `qps`, `avgMs`, `p95Ms`, `dbBytes`, `walBytes` |

Ambang: statement ≥ 20 ms masuk daftar **slow**; health naik ke `DEGRADED` saat DB p95 ≥ 25 ms,
ada error/busy di window, atau WAL ≥ 64 MB; `CRITICAL` saat p95 ≥ 100 ms atau WAL ≥ 256 MB. Panel
**SYSTEM** menampilkan blok SQLite (DB/WAL, Q/S, p95, avg, max, TX, ERR), dua slow query terbaru,
dan sparkline **SQL WINDOW**. Instrumentasi hanya menyimpan teks SQL, tidak pernah nilai parameter;
overhead-nya satu `performance.now()` per statement, dan `db.pragma()` untuk sampling tidak ikut
dihitung sebagai query aplikasi.

---

## Worker API

Semua endpoint kecuali `/api/health` membutuhkan:

```http
Authorization: Bearer <access-key>
```

| Method | Path | Fungsi |
| --- | --- | --- |
| `GET` | `/api/health` | Health publik (tanpa auth) |
| `GET` | `/api/state` | Snapshot lengkap: config, runtime, stats, report, session, 50 loot terbaru (`history` + `historyTotal`) |
| `GET` | `/api/report` | Agregat profit per produk all-time (`totals` + `items`) |
| `GET` | `/api/history?q=&status=&limit=&offset=&beforeAt=&beforeId=` | Halaman loot log permanen: `q` cari nama produk/item id, `status` = `filled`/`failed`, limit max 200, cursor `beforeAt`+`beforeId` untuk halaman lama |
| `GET` | `/api/item-info?itemId=&itemType=` | Resolve label/rarity/itemType dari marketplace |
| `GET` | `/api/configs` | Daftar target + runtime |
| `PUT` | `/api/configs` | Ganti seluruh daftar (`[ ... ]` atau `{ "configs": [ ... ] }`) |
| `POST` | `/api/configs` | Tambah satu target |
| `PATCH` | `/api/configs/:itemId` | Update parsial / arm-disarm |
| `DELETE` | `/api/configs/:itemId` | Hapus target |
| `PATCH` | `/api/settings` | `delayPerOrder`, `maxOrdersPerMinute`, `dryRun`, `paused`, `autoRefresh` |
| `POST` | `/api/control` | `{ "action": "pause" \| "resume" \| "toggle" \| "reconnect" }` |
| `POST` | `/api/session` | Simpan access/refresh token dan/atau cookie; `clearAccessToken`/`clearRefreshToken`/`clearCookie` untuk menghapus |
| `DELETE` | `/api/session` | Hapus access token, refresh token, dan cookie sekaligus |
| `POST` | `/api/session/push` | Endpoint bridge (CORS); balas token terbaru |
| `POST` | `/api/session/refresh` | Refresh paksa |
| `POST` | `/api/auth/rotate` | Rotasi access key (409 bila dipin env) |
| `GET` | `/api/latency` | Statistik latency worker → upstream per channel (HTTP, warm probe, WS handshake, WS ping) |
| `POST` | `/api/latency/probe` | Jalankan warm probe sekarang dan balas sampel baru |
| `GET` | `/api/system` | Snapshot health worker/host: CPU, memory, event loop, load, history |
| `DELETE` | `/api/history` | Reset profit, counter, loot log, dan sampel latency |

Error validasi memakai status `422` dan `fields`, misalnya:

```json
{
  "ok": false,
  "error": "Target is invalid.",
  "fields": {
    "highStockThreshold": "High-stock threshold must be greater than or equal to desired quantity."
  }
}
```

### WebSocket

- URL: `ws://<host>:<port>/ws`
- Auth: subprotocol `rcw.v1, <access-key>` (fallback query `?token=...`)
- Key salah: handshake tetap diterima (Bun belum bisa mengirim HTTP 403 pada upgrade socket), lalu
  server menutup socket dengan code `4001`. Rotasi key memakai `4003` agar client langsung reconnect
  dengan key baru.
- Client boleh mengirim `{ "type": "ping", "t": 123 }`; server membalas `{ "type": "pong", ... }`.
- Event server: `hello`, `snapshot`, `log`, `market`, `order`, `history`, `stats`, `report`,
  `session`, `gateway`, `settings`, `configs`, `latency`, `system`, `history-cleared`.
- `order.phase`: `submitted`, `accepted`, `filled`, `failed`, `dry`.
- `market` membawa `price`, `stock`, `targetPrice`, `hot`, `at`, `trend` (model TREND WATCH), `fill`
  (estimasi `{price, filled, requested, partial, approx}` untuk `desiredQty`), serta
  `allowedPriceMin`, `allowedPriceMax`, `allowedAt` (cache band harga terakhir per target).

---

## HTTPS dan mixed content

Jika control deck dibuka lewat `http://<IP-publik>:8787`, bridge yang berjalan di halaman
`https://rollercoin.com` akan diblokir browser:

```text
Mixed Content: ... requested an insecure resource 'http://18.205.189.239:8787/api/session/push'
```

Ini bukan masalah CORS dan tidak bisa diperbaiki dari JavaScript. Halaman HTTPS tidak boleh
memanggil HTTP non-loopback. Cara paling mudah dan rapi adalah menaruh **Caddy** di depan worker
sebagai reverse proxy HTTPS.

Caddy menangani:

- sertifikat Let's Encrypt otomatis,
- redirect HTTP → HTTPS,
- WebSocket `/ws` tanpa konfigurasi tambahan,
- header CORS dari worker tetap diteruskan.

---

## Caddy tutorial (Debian/Ubuntu VPS)

Contoh di bawah memakai IP `18.205.189.239` dan domain gratis `18.205.189.239.sslip.io`.
Ganti dengan domain sendiri jika ada.

### 1. Prasyarat

- Port **80** dan **443** terbuka di firewall/security group.
- Domain A record mengarah ke IP server. Tanpa domain, `<IP>.sslip.io` otomatis mengarah ke IP
  tersebut.
- Worker berjalan di `127.0.0.1:8787`.
- Tidak ada Apache/Nginx lain yang memakai port 80/443.

Cek DNS dari server:

```bash
getent hosts 18.205.189.239.sslip.io
# atau
dig +short 18.205.189.239.sslip.io
```

Cek port yang sedang dipakai:

```bash
sudo ss -ltnp | grep -E ':(80|443|8787)\b' || true
```

### 2. Install Caddy

Jika Caddy belum ada, gunakan repo resmi Caddy:

```bash
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update
sudo apt install -y caddy
caddy version
```

Caddy otomatis membuat service systemd. Cek:

```bash
sudo systemctl status caddy --no-pager
```

### 3. Bind worker ke loopback

Di `.env` project:

```env
HOST=127.0.0.1
PORT=8787
```

Restart worker:

```bash
pm2 restart rc-autotrade --update-env
curl http://127.0.0.1:8787/api/health
```

Kalau worker dijalankan manual:

```bash
# Ctrl+C worker lama
bun start
```

> Jangan biarkan worker tetap listen di `0.0.0.0:8787`. Setelah Caddy aktif, port 8787 tidak boleh
> terbuka ke internet.

### 4. Buat Caddyfile

```bash
sudo nano /etc/caddy/Caddyfile
```

Isi:

```caddyfile
18.205.189.239.sslip.io {
    reverse_proxy 127.0.0.1:8787
}
```

Untuk domain sendiri:

```caddyfile
rc.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

Caddy **tidak perlu** konfigurasi WebSocket khusus; `reverse_proxy` meneruskan upgrade HTTP
secara otomatis. Jika memakai Cloudflare di depan domain, set DNS ke **DNS only (grey cloud)**
saat pertama kali agar challenge sertifikat Caddy tidak terhalang.

### 5. Validasi dan jalankan

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl enable --now caddy
sudo systemctl reload caddy
sudo systemctl status caddy --no-pager
```

Lihat proses penerbitan sertifikat:

```bash
sudo journalctl -u caddy -f
```

Tunggu sampai muncul sertifikat untuk domain/IP. Setelah itu cek:

```bash
curl -I https://18.205.189.239.sslip.io
curl -s https://18.205.189.239.sslip.io/api/health
```

Buka control deck:

```text
https://18.205.189.239.sslip.io
```

### 6. Tutup port 8787 dari internet

Di UFW:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw deny 8787/tcp
sudo ufw status
```

Di security group cloud (AWS/GCP/Azure), hapus inbound `8787` dan izinkan `80/443`.

Verifikasi dari mesin lain, bukan dari server:

```bash
curl -m 3 http://18.205.189.239:8787/api/health
# harus timeout / connection refused
```

### 7. Generate ulang bridge

Bridge lama masih berisi URL HTTP. Setelah HTTPS aktif:

1. Buka deck via `https://...`.
2. Masuk **LINK → TOKEN BRIDGE**.
3. Pilih **USERSCRIPT · AUTO** (lebih tahan CSP) atau console snippet.
4. Generate ulang, lalu ganti userscript lama di Tampermonkey.
5. Pastikan URL di dalam script sudah `https://...`, bukan `http://18.205...`.

Caddy meneruskan endpoint `/api/session/push` beserta header CORS-nya, jadi bridge tetap bekerja.

### 8. Auto-start

Caddy memakai systemd, worker memakai PM2:

```bash
sudo systemctl enable caddy
pm2 save
pm2 startup
# jalankan command sudo yang dicetak PM2, lalu:
pm2 save
```

Urutan startup tidak masalah: Caddy bisa hidup lebih dulu; selama worker belum hidup, request akan
mendapat `502` sementara.

### 9. Update konfigurasi

Setelah mengubah Caddyfile:

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Setelah mengubah `.env`/kode worker:

```bash
bun run build
pm2 restart rc-autotrade --update-env
```

### Troubleshooting Caddy

| Gejala | Solusi |
| --- | --- |
| Sertifikat gagal diterbitkan | DNS belum mengarah ke IP, port 80/443 tertutup, atau Cloudflare proxy aktif. Cek `journalctl -u caddy -n 100 --no-pager`. |
| `502 Bad Gateway` | Worker mati atau bukan di `127.0.0.1:8787`. Jalankan `curl http://127.0.0.1:8787/api/health`. |
| Halaman 404/blank | `dist/` belum dibuild. Jalankan `bun run build`, lalu restart worker/PM2. |
| WebSocket offline | Pastikan hanya Caddy yang memakai 443 dan tidak ada proxy lain. Caddy menangani `/ws` otomatis. |
| Masih mixed content | Masih membuka deck/bridge dari URL `http://18.205...`. Buka via HTTPS, hard refresh, lalu generate ulang snippet/userscript. |
| `address already in use` | Apache/Nginx lain memakai 80/443. Stop service tersebut atau gunakan Caddy di port lain + firewall. |
| Caddyfile error | `sudo caddy validate --config /etc/caddy/Caddyfile`; lihat pesan baris yang salah. |
| 8787 masih bisa diakses publik | `HOST` masih `0.0.0.0`, firewall/security group belum menutup 8787. |

### Alternatif tanpa Caddy

- **Cloudflare Tunnel**: `cloudflared tunnel --url http://127.0.0.1:8787`, lalu pakai URL
  `https://<acak>.trycloudflare.com`. Cocok jika port 80/443 tidak bisa dibuka.
- **SSH tunnel**: `ssh -N -L 8787:127.0.0.1:8787 user@18.205.189.239`, lalu buka
  `http://127.0.0.1:8787`. Loopback HTTP dikecualikan dari mixed content dan token lewat SSH.

> Jangan mengakali mixed content dengan memaksa `GM_xmlhttpRequest` ke HTTP publik. Secara teknis
> userscript bisa melewati aturan halaman, tetapi access key dan token Rollercoin dikirim plaintext
> lewat internet. Gunakan HTTPS, tunnel, atau SSH.

> Kalau worker sempat diakses lewat HTTP publik, rotasi access key (**LINK → ROTATE KEY**) dan
> tempel ulang session Rollercoin setelah HTTPS aktif.

---

## Data dan keamanan

- `data/rollercoin.db` adalah database SQLite (mode WAL) berisi token sesi Rollercoin (plaintext),
  target, settings, statistik, loot log, agregat profit per produk, dan cache harga pasar. File
  dibuat `0600`, direktori `0700`; `rollercoin.db-wal`/`-shm` ikut di-`chmod 0600` saat dibuka.
- Order fill/failure disimpan permanen di tabel `orders` tanpa batas jumlah. UI menampilkan 50
  terbaru by default (bisa **Load older** + dicari), sedangkan engine mengecek duplikat langsung
  ke tabel lewat `store.hasOrder()` agar fill lama tidak terhitung dua kali. Window in-memory
  `MAX_HISTORY = 200` hanya dipakai untuk push live; agregat profit ada di `report_products`.
  Tiap entri punya field `message`: `"-"` untuk fill, dan alasan error untuk failure.
  Karena log tidak pernah dipangkas, ukuran database tumbuh seiring transaksi — reset lifetime stats
  (`DELETE /api/history`) atau backup rutin bila perlu.
- Cache pasar (`market_snapshot` + `price_points`) membuat deck langsung terisi setelah restart.
  Data ini hanya untuk tampilan: order tetap hanya direncanakan dari frame socket live, dan badge
  `cached` menandai harga yang belum diperbarui frame baru.
- Skema SQLite v4 menghapus kolom `targets.auto_target` dan `targets.auto_target_margin_pct` yang
  dulu dipakai fitur auto adjust price. Database v1–v3 dimigrasikan otomatis saat boot
  (`ALTER TABLE ... DROP COLUMN`), tanpa kehilangan target yang sudah ada.
- Titik harga 1 menit disimpan 14 hari lalu dipangkas otomatis (saat boot dan tiap 6 jam). Ubah
  `PRICE_RETENTION_DAYS` di `src/shared/constants.js` kalau butuh histori lebih panjang.
- Database lama dari versi JSON dimigrasikan otomatis saat boot pertama: `state.json` dan
  `loot-log.json` diimpor lalu di-rename menjadi `*.migrated-<timestamp>` (tidak dihapus).
  Jangan commit atau backup isi `data/` ke tempat publik.
- Backup: `sqlite3 data/rollercoin.db ".backup 'backup.db'"` saat worker berjalan, atau copy
  `rollercoin.db` setelah worker berhenti. Jangan menaruh `DATA_DIR` di NFS/folder sync (Dropbox,
  OneDrive) karena WAL butuh locking lokal.
- Access key worker **tidak** disimpan plaintext; hanya hash SHA-256 + preview.
- Token Rollercoin tidak pernah dikirim ke UI dalam bentuk penuh; UI hanya menampilkan preview.
  Pengecualian: `/api/session/push` membalas token penuh ke snippet bridge yang sudah terautentikasi.
- Semua percobaan auth yang gagal dibatasi 20/menit per IP.
- Default bind loopback. Untuk akses remote, pakai reverse proxy HTTPS + firewall; jangan buka
  port mentah ke internet.
- Tidak ada telemetry, analytics, atau koneksi ke server pihak ketiga selain Rollercoin.
- Proyek ini tidak berafiliasi dengan Rollercoin. Gunakan dengan risiko sendiri dan patuhi ToS.

---

## Mock mode

```bash
bun run mock
```

- Mengisi dua target contoh bila state kosong.
- Membuat sesi JWT palsu (agar panel token tetap hidup).
- Harga berfluktuasi di sekitar target; setiap siklus tertentu turun ke bawah target.
- Purchase disimulasikan: 8% gagal, sisanya fill setelah 0.5–1.4 detik.
- Tidak ada request keluar ke Rollercoin.

Mock juga dipakai oleh integration test, jadi `bun run test` tidak membutuhkan akun atau network.

---

## Testing

```bash
bun run test
```

`pretest` otomatis menjalankan `bun run build` (Vite lewat Bun) terlebih dahulu, jadi integration
test selalu memakai UI React yang sedang aktif.

Cakupan:

- `validation.test.js` — schema target, URL, duplikat, settings, session.
- `rules.test.js` — price/qty/high-stock/partial, rate limiter, konversi micro-RLT.
- `events.test.js` — normalisasi frame market, fill, dan frame lain.
- `store.test.js` — key generation/rotation, persistensi SQLite, migrasi JSON, market cache, JWT exp,
  loot log permanen (paging cursor + search nama produk).
- `engine-cache.test.js` — snapshot hasil hydrate hanya untuk tampilan dan tidak memicu order.
- `bridge.test.js` — transport `GM_xmlhttpRequest` + sinkronisasi token dua arah.
- `item-info.test.js` — auto label, fallback tanpa `rarityGroup`, deteksi tipe (rack), cache, kegagalan API.
- `engine.test.js` — order gagal tidak menambah book; fill sukses menguranginya.
- `keepalive.test.js` — warm probe, idle timeout, dan suspend saat `403`/`429`.
- `latency.test.js` — rolling sample per channel, percentile, dan jitter.
- `system.test.js` — delta CPU/event loop, batas history, DB view, dan klasifikasi health worker.
- `db-metrics.test.js` — instrumentasi statement/transaksi, window 60 detik, slow query, ukuran file.
- `report.test.js` — agregat profit per produk melewati batas window in-memory loot log.
- `format.test.js` — formatter UI (label itemType, URL marketplace, byte/percent/ms presisi).
- `session.test.js` — jadwal refresh token dan fallback token opaque.
- `integration.test.js` — spawn worker mock, auth, CRUD, settings, CORS, session push, item-info, WS, fill.

Total 112 test; integration test memakai port acak dan data directory sementara.

---

## Struktur proyek

```text
.
├── ui/                      # data plane React + Vite
│   ├── index.html           # entry + SVG icon sprite
│   └── src/
│       ├── App.jsx          # dashboard, panels, cards
│       ├── useWorker.js     # snapshot/WS/actions state
│       ├── useToasts.js
│       ├── components/      # PriceChart (Recharts), modals
│       ├── lib/             # client, format, sound
│       └── styles.css       # entry Tailwind v4 + token desain
├── public/                  # aset statis (bridge.js, favicon, fallback)
├── dist/                    # hasil build Vite yang disajikan worker
├── src/
│   ├── shared/
│   │   ├── constants.js
│   │   ├── rules.js         # planOrder + rate limiter
│   │   └── validation.js
│   └── worker/
│       ├── index.js         # bootstrap, graceful shutdown
│       ├── config.js        # env loader
│       ├── logger.js        # console + socket log buffer
│       ├── store.js         # SQLite-backed state, token hash
│       ├── http.js          # Bun fetch + user agent
│       ├── api.js           # Rollercoin HTTP client
│       ├── keepalive.js     # warm probe with backoff/suspend
│       ├── session.js       # token refresh scheduler
│       ├── gateway.js       # Bun native WSS client + reconnect
│       ├── mock.js          # simulator upstream
│       ├── engine.js        # matching + order execution + 24h minute history
│       ├── item-info.js     # auto label/rarity resolver
│       ├── events.js        # upstream frame normalizer
│       ├── system.js        # CPU/heap/event-loop/host health sampler
│       ├── db.js            # bun:sqlite wrapper (pragma/transaction/named params)
│       ├── db-metrics.js    # SQLite statement timing, file/WAL and slow-query probe
│       ├── server.js        # control plane HTTP/WS + static UI
│       └── reset-token.js
├── test/
├── docs/                    # screenshot
├── vite.config.js           # root ui/, output dist/
├── package.json
├── bun.lock
├── trade.js                 # script asli (referensi)
├── test.json                # contoh konfigurasi
└── .env.example
```

---

## Troubleshooting

| Gejala | Penyebab / solusi |
| --- | --- |
| `ACCESS DENIED` saat login | Key salah atau sudah dirotasi. Cek terminal; reset dengan `bun run key:reset` (worker harus berhenti). |
| `429` saat login | Terlalu banyak key salah. Tunggu 1 menit. |
| Halaman menampilkan “REACT DECK READY” atau 404 | Worker lama masih menyajikan `public/`. Restart dengan `Ctrl+C` lalu `bun start` agar worker menyajikan `dist/` React. |
| `LINK LOST` | Control WebSocket putus. Worker mati? Cek proses/port. |
| `FEED DOWN` / `SYNCING` | Worker hidup tetapi socket Rollercoin belum tersambung. Cek log, token, jaringan. |
| Log gateway `TLS handshake failed` | Token **tidak** terlibat: TLS selesai sebelum URL/`?token=` dikirim, jadi token valid sekalipun bisa kena. Jalankan `bun scripts/diagnose-ws.js` di terminal yang sama dengan `npm run dev` untuk memisahkan DNS/TCP, CA/`node:tls`, `fetch`, dan WebSocket native Bun. Hentikan worker dulu agar dua socket tidak memakai sesi yang sama. Jika semua probe hijau, error lama biasanya berasal dari proses worker basi atau gangguan edge sesaat. Bandingkan juga `bun --revision` lokal vs production. Token dan Bearer sekarang diredaksi dari log/`lastError`. |
| `NO TOKEN` | Belum ada sesi Rollercoin. Buka tab LINK dan tempel token. |
| `AUTH ERR` | Token ditolak Rollercoin. Tempel sesi baru; cek cookie/UA bila perlu. |
| Purchase `403` / HTML challenge | Cloudflare. Isi cookie dari browser; pastikan IP/UA wajar. |
| Bridge `CORS` / `Failed to fetch` | Console mode memakai `fetch` halaman dan bisa diblokir CSP/Private Network Access. Pakai **USERSCRIPT · AUTO** (memakai `GM_xmlhttpRequest`). Pastikan URL bridge `http://127.0.0.1:<port>`, worker hidup, dan userscript di-generate ulang. Error `401` sekarang juga mengirim header CORS, jadi restart worker agar pesan aslinya terlihat. |
| `EADDRINUSE` | Port dipakai. `PORT=8788 bun start` atau hentikan proses lain. |
| Ctrl+C lama / worker tidak keluar | Di bawah `bun --watch`, timer exit yang di-`unref` bisa menjadi satu-satunya pending work setelah server ditutup, sehingga watcher menunggu sampai Ctrl+C kedua. Versi ini memakai timer exit yang di-ref, watchdog 5 detik, dan Ctrl+C kedua memaksa `process.exit(1)`. Kalau masih macet, cek `ss -ltnp 'sport = :8787'` untuk proses lama. |
| Key hilang | `bun run key:reset`, lalu restart worker dan login dengan key baru. |
| Harga terlihat, order tidak jalan | Target terlalu rendah, stok di bawah minimum, pause/dry-run aktif, cooldown, atau batas order/menit. Cocokkan dengan [reason code](#reason-code-yang-muncul-di-log) di log `scan` dan cek state card. |
| Label tetap `ITEM xxxx` / `Item not found` | Item ID tidak ditemukan untuk tipe yang dicoba, atau `item-info` gagal (sesi/Cloudflare/jaringan). Buka edit target dan klik **RETRY**; worker akan mencoba `mutation_component`, `miner`, `rack`, `battery`. `rarityGroup` yang tidak ada akan fallback ke nama item. |
| Chart masih kosong | Mode live mulai tanpa history; tunggu beberapa menit. Worker restart menghapus history in-memory. Mock mode mengisi 24 jam awal agar semua timeframe terisi. |
| BOOK di ticker tidak turun setelah fill | Fill sukses menurunkan book segera; frame market berikutnya bisa mengembalikannya jika memang ada stok baru. Order gagal tidak pernah menambah book. Meter BOOK di card sudah diganti success rate. |
| Tab browser inactive | Tidak masalah lagi; engine berjalan di Bun. Hanya UI yang berhenti menggambar. |

### Batasan yang disengaja

- Hanya listing termurah per update; tidak menggabungkan beberapa listing.
- Tidak ada pembatalan order.
- Format upstream tidak resmi dan bisa berubah tanpa pemberitahuan.
- Profit adalah diskon terhadap target, bukan realized P&L.
- Loot log tidak dipangkas otomatis; database tumbuh seiring jumlah transaksi. Halaman tetap ringan
  karena browser hanya memuat 50 baris per halaman, tetapi backup/reset berkala tetap disarankan.
- Pending order disimpan di memori; restart worker tidak membatalkan order yang sudah terlanjur
  dikirim, tetapi konfirmasi tetap dicatat lewat `marketplace_orders_filled`.
- Refresh token rotation mengikuti response Rollercoin; bila endpoint berubah, gunakan paste manual.

---

Original bot by **@nopaleon.log** / **@n0paleon**. Not affiliated with Rollercoin.
