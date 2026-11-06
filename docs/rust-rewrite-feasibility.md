# Rewrite backend ke Rust — feasibility study

Status: **historis — studi ini sudah dieksekusi.** Backend Rust sekarang satu-satunya backend;
worker JavaScript yang dirujuk di sini sudah dihapus. Tautan `src/worker/*` dan `src/shared/*` di
dokumen ini menunjuk ke kode pra-migrasi (lihat commit sebelum penghapusan) dan sengaja dibiarkan
sebagai catatan asal-usul angka.

Basis angka: benchmark lokal (`scripts/bench-hotpath.mjs`, sudah dihapus) + 22 order asli terakhir di `data/rollercoin.db` + probe jaringan.

---

## 0. Ringkasan

- **Bisa.** Backend ini kecil (9.8k LOC, 176 test) dan batasnya jelas, jadi rewrite Rust sangat mungkin secara teknis.
- **Tapi bukan di situ bottleneck-nya.** Di database-mu sendiri, jalur aplikasi dari sinyal sampai request keluar adalah **p50 1 ms / p99 2 ms**. RTT ke Rollercoin **p50 351 ms / p99 615 ms**.
- **3–7 ms yang kamu lihat bukan dari JSON parse atau logika order.** Benchmark menunjukkan seluruh keputusan order makan **12–19 µs**. Yang bisa mencapai 3–7 ms hanya dua hal: **rebuild model trend yang jalan inline di frame pemicu** (1.2 ms per target, 9.3 ms untuk 8 target sekaligus) dan **GC pause** (outlier 4–7 ms terlihat di benchmark).
- **Rust realistis memangkas ~0.6–0.8 ms** dari jalur 350 ms (≈0.2%). Yang benar-benar didapat Rust: **hilangnya tail latency** (GC, JIT warmup, event-loop stall), RSS lebih kecil, binary tunggal.
- **Rekomendasi:** perbaiki dua hotspot JS dulu (1–2 hari, menghapus 90% tail), ukur ulang, baru putuskan Rust. Kalau tetap ingin Rust, mulai dari **sidecar hot path**, bukan full rewrite.

---

## 1. Baseline terukur

### 1.1 Jalur order di produksi (data asli)

Sumber: `orders.timings` di `data/rollercoin.db`, 22 order real (bukan `mock_`), semuanya 2026-11-06.
Field `timings` diisi oleh [api.js](src/worker/api.js) dan [engine.js](src/worker/engine.js#L562-L567).

| Fase | p50 | p90 | p99 | max | Arti |
| --- | ---: | ---: | ---: | ---: | --- |
| `signalAt → handleAt` | 0 | 1 | 1 | 1 | socket frame → handler engine |
| `handleAt → orderAt` | 0 | 1 | 2 | 2 | normalisasi + plan + trend |
| `orderAt → dispatchAt` | 0 | 1 | 1 | 1 | broadcast, log, `store.save()` |
| `dispatchAt → sendAt` | 0 | 0 | 1 | 1 | masuk `#submit` |
| **`signalAt → sendAt`** | **1** | **2** | **2** | **2** | **total sisi aplikasi** |
| `sendAt → responseAt` | 351 | 417 | 615 | 615 | TLS + network + server upstream |
| `responseAt → completeAt` | 0 | 0 | 2 | 2 | baca + parse respons |

Catatan: resolusi `Date.now()` adalah 1 ms, jadi semua nilai sub-milidetik tampil sebagai `0`. Angka "1 ms" itu batas atas, bukan pembulatan ke bawah.

Seluruh 1 707 order (termasuk yang tanpa `timings`): `latency_ms` p50 **185 ms**, p90 **456 ms**, p99 **5.8 s** (timeout/retry).

### 1.2 Jaringan dari mesin ini

`curl` sekali ke `https://rollercoin.com/` (Cloudflare, `172.67.74.3`):

| Tahap | Waktu |
| --- | ---: |
| DNS (cold) | 37 ms |
| TCP connect | 103 ms total (≈66 ms RTT) |
| TLS handshake | +76 ms |
| TTFB | **696 ms** |

Artinya dari 351 ms pembelian, **±285 ms adalah pemrosesan upstream**, bukan jaringan dan bukan aplikasimu.

### 1.3 Benchmark lokal per tahap

`bun scripts/bench-hotpath.mjs` — Bun 1.3.1, frame pasar sintetis 12 & 60 listing, SQLite sungguhan di direktori temp.

| Operasi | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| `JSON.parse` frame 12 listing | 1.9 µs | 5.2 µs | 7.4 µs | 151 µs |
| `JSON.parse` frame 60 listing | 6.3 µs | 13.6 µs | 19.6 µs | 519 µs |
| `normalizeUpstreamEvent` + `estimateFillPrice` + `planOrder` | 11.9 µs | 21.9 µs | 26.9 µs | 6.9 ms |
| `JSON.stringify` broadcast pasar | 0.73 µs | 1.8 µs | 3.0 µs | 63 µs |
| `store.saveMarketSnapshot` (upsert) | 6.3 µs | 12.1 µs | 16.9 µs | 87 µs |
| `store.recordPricePoint` (insert) | 3.5 µs | 6.9 µs | 9.8 µs | 129 µs |
| `store.save()` + `flush()` (transaksi) | 43 µs | 74 µs | 89 µs | 775 µs |
| `engine.handleFrame` (dry-run, SQLite asli) | 14.5 µs | 25.2 µs | 33.0 µs | 728 µs |
| `engine.handleFrame` (dry-run + 1 UI client) | 19.2 µs | 35.7 µs | 45.2 µs | 730 µs |
| **`engine.handleFrame` HOT (jalur kirim order)** | **12.2 µs** | **22.1 µs** | **27.4 µs** | **629 µs** |
| `engine.handleFrame` dengan trend selalu stale (history pendek) | 131 µs | 214 µs | 266 µs | 1.1 ms |
| `buildTrend` 360 titik | 299 µs | 464 µs | 772 µs | 3.1 ms |
| **`buildTrend` 1 440 titik (24 jam)** | **1 248 µs** | **2 158 µs** | **2 369 µs** | **4.0 ms** |
| **8× `buildTrend` dalam satu tick** | **9.3 ms** | **10.6 ms** | **13.5 ms** | **16.1 ms** |

---

## 2. Dari mana angka 3–7 ms

Tiga kandidat, dalam urutan keyakinan:

### 2.1 Rebuild model trend yang jalan inline di frame pemicu (paling mungkin)

Di [engine.js:496](src/worker/engine.js#L496), `#computeTrend` dipanggil **sebelum** `planOrder` pada setiap frame — termasuk frame yang akhirnya memicu pembelian. Cache-nya 10 detik (`TREND_REFRESH_MS`, [engine.js:18](src/worker/engine.js#L18)), jadi setiap ~10 detik frame berikutnya membayar penuh biaya rebuild.

Biayanya tumbuh linear terhadap panjang history:

| History | p50 |
| --- | ---: |
| 66 titik (kondisi DB-mu sekarang) | 61 µs |
| 360 titik | 299 µs |
| 1 440 titik (worker hidup 24 jam) | **1 248 µs** |

Penyebabnya ada di [trend.js:144-164](src/shared/trend.js#L144-L164): filter Hampel memanggil `values.slice()`, `median()` (yang melakukan `[...values].sort()`), dan `slice.map()` **dua kali per titik**. Untuk 1 440 titik itu ≈ 2 900 alokasi array kecil dan ≈ 2 900 sort per rebuild. Bukan lambat karena JavaScript — lambat karena algoritmanya mengalokasi di dalam loop.

Kalau beberapa target jadi stale bersamaan (feed mengirim update banyak item dalam satu burst), biayanya dijumlahkan: **8 target = 9.3 ms p50**. Dengan 12 target aktif, worst case satu tick ≈ 14 ms.

### 2.2 GC pause

Lihat kolom `max`: fungsi murni 12 µs pernah mencatat 4.3–6.9 ms, `buildTrend` 4.0 ms, `JSON.parse` 519 µs. Ini pause GC, bukan kerja. Di jalur order, satu pause seperti ini muncul sebagai "3–7 ms diproses aplikasi". Ini satu-satunya bagian yang **memang** dihilangkan Rust.

### 2.3 Mungkin metriknya bukan `signalAt → sendAt`

Kalau angka 3–7 ms berasal dari bar di **Loot Detail Modal** ([LootDetailModal.jsx:102-145](ui/src/components/LootDetailModal.jsx#L102-L145)), ini pemetaannya:

| Bar di modal | Fase | Isi sebenarnya | p50 di DB-mu |
| --- | --- | --- | ---: |
| Feed delivery → engine | `signalAt → handleAt` | frame socket → handler mulai | 0 ms |
| Signal evaluated | `handleAt → orderAt` | normalisasi, depth, target, trend | 0 ms |
| Order commit & broadcast | `orderAt → dispatchAt` | counter, `store.save()`, broadcast | 0 ms |
| Submit → fetch | `dispatchAt → sendAt` | payload + dispatch `fetch` | 0 ms |
| Purchase request → headers | `sendAt → responseAt` | network + Rollercoin | 351 ms |

Jumlah empat bar pertama = **1 ms**. Kalau di production jumlahnya 3–7 ms, kandidatnya tetap §2.1 dan §2.2 — bukan bar kelima.

**Perlu dikonfirmasi:** angka 3–7 ms itu dibaca dari mana? Data order yang tersimpan tidak menunjukkan itu di jalur keputusan.

### 2.4 Bonus temuan: 57 target, 12 aktif

`targets` berisi 57 baris, hanya 12 `active = 1`. Tapi [engine.js:424](src/worker/engine.js#L424) memproses **semua** config yang cocok: trend dihitung, snapshot ditulis, event di-broadcast — untuk target nonaktif juga. `planOrder` baru menolaknya di akhir. Jadi worker membayar penuh untuk 45 target yang tidak dipakai.

---

## 3. Apa yang Rust benar-benar beli

Perkiraan konservatif dengan algoritma yang sama (bukan hasil pengukuran):

| Tahap | Sekarang (JS) | Rust | Selisih |
| --- | ---: | ---: | ---: |
| `JSON.parse` frame | 1.9–6.3 µs | 0.4–1.2 µs (`sonic-rs`/`simd-json`) | −5 µs |
| normalisasi + `planOrder` | 11.9 µs | 0.5–1 µs (tanpa alokasi, `Copy` structs) | −11 µs |
| `buildTrend` 1 440 titik | 1 248 µs | 60–150 µs (port langsung), 20–40 µs (incremental) | −1.1 ms |
| broadcast serialize | 0.73 µs | 0.3 µs | −0.4 µs |
| SQLite upsert | 6.3 µs | 4–6 µs (`rusqlite` + WAL sama) | ±0 |
| dispatch HTTP | tidak terukur di sini | hyper + prebuilt `Bytes` body | −50…200 µs? |
| **GC / JIT tail** | **4–7 ms (jarang)** | **0** | **−4…7 ms di p99.9** |
| **Total p50 jalur order** | **±1 ms** | **±0.2–0.4 ms** | **−0.6…0.8 ms** |

Kesimpulan matematisnya: pada jalur 350 ms, ini **0.2%**. Terhadap kompetitor bot, 0.6 ms hanya berarti kalau mereka juga berada dalam jendela 1 ms yang sama — dan itu kalah penting dibanding lokasi server serta kecepatan feed WebSocket.

Yang tetap berharga dari Rust, tapi bukan "lebih cepat":

1. **Prediktabilitas** — tidak ada GC, tidak ada JIT warmup, tidak ada event-loop stall 9 ms karena satu perhitungan sinkron.
2. **Footprint** — RSS jauh lebih kecil, cocok untuk VPS 512 MB.
3. **Deploy** — satu binary statis (musl), tanpa Bun/Node di server.
4. **Kepemilikan penuh atas syscall** — `TCP_NODELAY`, `SO_BUSY_POLL`, `TCP_QUICKACK`, affinity, `SCHED_FIFO` jadi mungkin tanpa perantara runtime.

---

## 4. Apa yang Rust tidak bisa beli

- **RTT 66 ms ke edge Cloudflare.** Tidak berubah.
- **±285 ms pemrosesan upstream.** Tidak berubah.
- **Antrean pencocokan di server Rollercoin.** Order dinilai saat tiba di sana; 0.6 ms lebih cepat tidak menolong kalau lawanmu berada 20 ms lebih dekat ke origin.
- **Cloudflare.** ClientHello Rust berbeda dari Chrome/Bun; bot management bisa menantang. Risiko nyata, lihat §5.
- **Perilaku marketplace.** Format frame tidak resmi dan bisa berubah; rewrite tidak mengurangi risiko itu.

Untuk konteks: memindahkan worker ke region/POP yang lebih dekat, atau memastikan koneksi TLS sudah panas saat order dikirim (yang sudah dicoba lewat `keepalive.js`), bernilai **10–300× lebih besar** daripada seluruh keuntungan Rust.

---

## 5. Biaya dan risiko rewrite

| Aspek | Kenyataan |
| --- | --- |
| Ukuran | `src/` 9 831 LOC (worker + shared), `test/` 4 496 LOC / 176 test case |
| UI | 9 101 LOC React — tidak perlu ditulis ulang, tetap disajikan sebagai `dist/` |
| SQLite | `bun:sqlite` → `rusqlite`/`sqlx`; skema + migrasi + `VACUUM INTO` backup harus diporting persis |
| WebSocket upstream | `tokio-tungstenite` + TLS; custom header `Origin`/`User-Agent`/`Cookie` |
| Control plane | HTTP + WS broadcast + auth token + static assets (`axum` + `tower-http`) |
| Backup/restore | [backup.js](src/worker/backup.js) 928 LOC, format portabel + snapshot mentah |
| Session | JWT decode, refresh scheduler, rotasi refresh token, cookie |
| Observability | 4 kanal latency, health sampler (procfs), DB metrics, log buffer — semua harus setara |
| Deployment | PM2 fork → systemd; cross-compile musl |

**Risiko terbesar: TLS fingerprint.** [gateway.js:28-32](src/worker/gateway.js#L28-L32) sudah mencatat bahwa client `ws` (lewat `node:tls`) **gagal TLS handshake** ke edge Rollercoin, sementara native WS Bun berhasil. Itu gejala fingerprint/stack, bukan bug. `rustls` punya ClientHello yang jelas berbeda dari Chrome; kalau Cloudflare menandainya, worker-mu tidak bisa connect sama sekali. Mitigasi: pakai `native-tls`/OpenSSL, atau binding BoringSSL (`boring`/`btls`) agar menyerupai Chrome — dan uji dengan padanan [scripts/diagnose-ws.js](scripts/diagnose-ws.js) sebelum menulis apa pun yang lain.

**Risiko kedua: parity.** Ada 176 test yang mengunci perilaku halus — pencocokan fill yang datang sebelum respons HTTP ([engine.js:682-687](src/worker/engine.js#L682-L687)), dedupe order, format backup, hash token. Rewrite tanpa harness diferensial akan diam-diam mengubah perilaku. Kabar baiknya: `orders.raw_events` menyimpan frame asli, jadi kamu punya bahan replay yang bagus.

Estimasi effort:

| Jalur | Effort | Risiko |
| --- | --- | --- |
| Optimasi JS (tanpa Rust) | 2–4 hari | Rendah |
| Sidecar Rust untuk hot path | 1.5–2.5 minggu | Sedang |
| Full rewrite Rust | 4–8 minggu penuh | Tinggi (TLS + parity) |

---

## 6. Tiga opsi arsitektur

### Opsi A — Full rewrite

`tokio` (current_thread) + `axum` + `tokio-tungstenite` + `rusqlite` + `serde`/`sonic-rs`. Satu binary, tanpa GC, kontrol penuh.

- **Untuk:** hasil paling bersih, paling memuaskan kalau tujuannya belajar/optimasi ekstrem.
- **Menentang:** biaya tertinggi, risiko TLS, harus mencapai parity 176 test, dan tetap tidak menggerakkan 350 ms.

### Opsi B — Sidecar hot path (rekomendasi kalau tetap ingin Rust)

Rust memiliki **hanya** jalur panas: WebSocket feed → sinyal → HTTP order. Semua sisanya (UI, SQLite riwayat, backup, config, session refresh, health) tetap di Bun. Komunikasi lewat Unix socket / HTTP lokal.

- **Untuk:** 80% manfaat, 25% biaya, dan bisa diuji berdampingan dengan worker lama (shadow mode: kirim order hanya dari satu pihak).
- **Menentang:** dua runtime, dua sumber state, butuh protokol internal yang jelas.

### Opsi C — Tetap Bun, perbaiki hotspot (rekomendasi utama)

1. **Hilangkan alokasi di Hampel** — rolling window 5 elemen, median via insertion ke array tetap; atau ganti filter untuk series panjang. Target: 1 248 µs → <100 µs.
2. **Jangan rebuild trend di jalur keputusan** — hitung setelah dispatch, atau di `setImmediate`/worker thread; frame pemicu order tidak boleh membayar 1.2 ms.
3. **Skip config nonaktif lebih awal** — 57 → 12 target memangkas 79% kerja dan DB write.
4. **Kurangi titik**: downsample history ke 1 titik/5 menit untuk model (288 titik) — akurasi praktis sama, biaya 5× lebih kecil.
5. **Pastikan koneksi panas**: sudah ada `keepalive.js`; verifikasi `fetch` benar-benar reuse socket saat order dikirim (TLS handshake = +76 ms kalau tidak).
6. **Ukur ulang** dengan benchmark yang sama + data order asli.

Target terukur: `signalAt → sendAt` p99 ≤ 500 µs, dan tidak ada stall > 2 ms di event loop.

### Matriks keputusan

| Kriteria | A. Full Rust | B. Sidecar | C. Optimasi JS |
| --- | ---: | ---: | ---: |
| Pengurangan latency nyata | ±0.7 ms di p50 | ±0.6 ms di p50 | p50 tetap ±1 ms, tapi stall 1–14 ms hilang |
| Effort | 4–8 minggu | 1.5–2.5 minggu | 2–4 hari |
| Risiko TLS/Cloudflare | Tinggi | Tinggi | Nol |
| Risiko parity | Tinggi | Sedang | Nol |
| Nilai belajar | Tinggi | Sedang | Rendah |
| Menghapus GC tail | Ya | Ya | Tidak |

---

## 7. Rekomendasi

1. **Kunci dulu atribusinya.** Konfirmasi 3–7 ms itu dari fase mana. Data yang ada menunjukkan jalur order 1 ms; kalau angka itu dari `latency_ms`, yang kamu optimasi adalah network, bukan kode.
2. **Kerjakan Opsi C.** Dua hotspot (Hampel + trend di jalur keputusan) bertanggung jawab atas hampir semua variasi 1–14 ms yang terukur. Ini juga prasyarat: kalau nanti Rust dipakai, algoritmanya harus diperbaiki lebih dulu, kalau tidak kamu hanya memindahkan masalah.
3. **Kalau setelah itu masih ingin Rust** (dan alasannya "saya suka optimasi ekstrem", bukan "bot saya kalah cepat"), pilih **Opsi B**. Mulai dari `diagnose-ws` versi Rust untuk membuktikan TLS bisa lewat, lalu port feed + signal + submit, dengan harness replay dari `orders.raw_events`.
4. **Jangan port full rewrite sebagai langkah pertama.** Kamu akan menghabiskan berminggu-minggu untuk UI/backup/health yang kontribusinya nol terhadap latency.

---

## 8. Kalau lanjut: desain sidecar Rust

```
┌────────────────────┐   frame JSON    ┌──────────────────────┐
│ Rollercoin WS feed │ ──────────────► │  Rust hot path       │
└────────────────────┘                 │  • parse (sonic-rs)  │
                                       │  • signal + plan     │
┌────────────────────┐   config/target │  • prebuilt request  │
│ Bun worker         │ ──────────────► │  • HTTP POST (hyper) │
│ UI/DB/backup/auth  │ ◄────────────── │  • timings + result  │
└────────────────────┘   order events  └──────────────────────┘
```

- Satu thread hot path (`tokio::runtime::Builder::new_current_thread`), tanpa lock, tanpa `spawn` untuk keputusan.
- Request body di-`serialize` sekali saat config berubah; saat sinyal hanya menempelkan `orderId`/`qty` ke `Bytes` yang sudah ada.
- SQLite ditulis dari thread terpisah lewat channel — disk tidak pernah memblokir keputusan.
- `TCP_NODELAY` + `TCP_QUICKACK` + `SO_BUSY_POLL`; affinity ke core terisolasi (`isolcpus`) sebagai eksperimen terakhir, bukan pertama.
- Field `timings` (`signalAt`, `handleAt`, `orderAt`, `dispatchAt`, `sendAt`, `responseAt`, `completeAt`) **wajib** dipertahankan supaya debugger UI tetap jalan.

Kriteria sukses (harus diukur sebelum dan sesudah):

| Metrik | Sekarang | Target |
| --- | ---: | ---: |
| `signalAt → sendAt` p50 | 1 ms | ≤ 300 µs |
| `signalAt → sendAt` p99 | 2 ms | ≤ 500 µs |
| Stall maksimum di jalur panas | 4–7 ms (GC) | ≤ 1 ms |
| Parity replay 1 707 order | — | 100% sama |
| Connect ke WS + REST | jalan | jalan (uji TLS lebih dulu) |

---

## 9. Lampiran — cara reproduce

```bash
bun scripts/bench-hotpath.mjs          # tabel §1.3
curl -s -o /dev/null \
  -w 'dns=%{time_namelookup} connect=%{time_connect} tls=%{time_appconnect} ttfb=%{time_starttransfer}\n' \
  https://rollercoin.com/             # tabel §1.2
```

Query fase order:

```sql
SELECT id, status, latency_ms, timings FROM orders
WHERE timings IS NOT NULL ORDER BY at DESC;
```

Caveat:

- Angka benchmark diukur di mesin ini (Bun 1.3.1, Linux x64), frame pasar sintetis 12/60 listing; frame asli bisa lebih besar (`raw_events` p50 2.9 KB di DB-mu).
- Sampel produksi hanya 22 order dengan `timings`, semuanya satu hari. Cukup untuk menyimpulkan "jalur aplikasi ≈ 1 ms", belum cukup untuk p99.9.
- Estimasi Rust adalah perkiraan berbasis kompleksitas algoritma, bukan hasil pengukuran.
