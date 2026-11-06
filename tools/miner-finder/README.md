# Miner finder (browser console)

Script sekali pakai untuk membandingkan **miner** di marketplace Rollercoin dan
mengurutkannya berdasarkan rasio power per RLT. Ini bukan bagian dari worker:
tidak ada yang mengimpornya, dan backend tidak pernah menjalankannya.

## Cara pakai

1. Buka `https://rollercoin.com/marketplace`, pilih tab **miners**.
2. Set filter/sort yang diinginkan (misalnya urut harga termurah).
3. Buka DevTools → **Console**, tempel seluruh isi [`miner-finder.js`](miner-finder.js), Enter.
4. Sebelum menjalankan, sesuaikan tiga nilai di baris paling atas:

   | Variabel | Arti |
   | --- | --- |
   | `CurrentPower` | Total power akun saat ini, dalam GH/s |
   | `CurrentBonus` | Bonus mining akun saat ini, dalam persen |
   | `limit` | Maksimal halaman yang discrape (default 192) |

5. Script akan mengklik pagination sendiri, menunggu tiap halaman selesai render,
   lalu mencetak tabel hasil yang sudah diurutkan dari rasio terbaik.

## Kolom hasil

| Kolom | Arti |
| --- | --- |
| `Name` | Nama miner |
| `Price` | Harga listing (RLT) |
| `Power` | Power miner (GH/s) |
| `Bonus[%]` | Bonus mining miner (%) |
| `Ratio` | `Addition Power / 1000 / Price` — makin besar makin baik |
| `Cur.Pow X Add.Bonus` | Power akun × bonus miner |
| `Mine.Pow X Tot.Bonus` | Power miner × (bonus miner + bonus akun) |
| `Addition Power` | Total tambahan power dari miner ini |
| `TotalPower [GH/S]` | Power akun + tambahan power |
| `Total Bonus %` | Bonus akun + bonus miner |

## Batasan

- Selector DOM (`.item-title`, `.item-price`, `.item-addition-power`,
  `.item-addition-bonus`, `.marketplace-pagination-btn`) bisa berubah kapan saja
  saat Rollercoin memperbarui UI; kalau hasil kosong, periksa selector-nya dulu.
- Maksimal 192 halaman, dan tiap halaman menunggu sampai 7 detik.
- Miner dengan nama sama hanya dihitung sekali (dedupe berdasarkan nama).
- Biarkan tab tetap aktif selama scraping; tab background bisa menunda render.
