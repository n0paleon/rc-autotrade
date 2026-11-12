(async () => {
// ================== KONFIGURASI ==================
// Istilah mengikuti panel "My power" di game:
//   MinersPower       -> baris "Miners"        (power mentah miner, BUKAN "Current power")
//   BonusPowerPercent -> persen di "Bonus Power" (contoh: 619.53)
const CONFIG = {
  MinersPower: "71.769 Eh/s",  // isi dari baris "Miners"
  BonusPowerPercent: 619.53,   // isi dari persen di baris "Bonus Power" (tanpa % dan tanda panah)
  limit: 192                   // batas halaman
};

// ================== SATUAN POWER ==================
// Basis internal: GH/s
const POWER_UNITS = [
  { unit: "Zh/s", factor: 1e12 },
  { unit: "Eh/s", factor: 1e9 },
  { unit: "Ph/s", factor: 1e6 },
  { unit: "Th/s", factor: 1e3 },
  { unit: "Gh/s", factor: 1 }
];

// "15Eh/s" / "10 Ph/s" / "5 th/s" -> angka dalam GH/s
function parsePowerToGH(text) {
  const match = String(text)
    .replace(/,/g, '')
    .trim()
    .match(/^([\d.]+)\s*([a-zA-Z]+)\s*\/?\s*s?$/);

  if (!match) throw new Error(`Format power tidak valid: "${text}" (contoh: 10Gh/s, 10Ph/s, 15Eh/s)`);

  const value = parseFloat(match[1]);
  const unitKey = match[2].toLowerCase(); // gh, th, ph, eh, zh
  const found = POWER_UNITS.find(u => u.unit.toLowerCase().startsWith(unitKey));
  if (!found) throw new Error(`Satuan tidak dikenal: "${match[2]}" (gunakan Gh/s, Th/s, Ph/s, Eh/s, Zh/s)`);

  return value * found.factor;
}

// angka GH/s -> "11.148 Ph/s" (otomatis pilih satuan terbesar yang pas)
function formatPowerFromGH(gh, decimals = 3) {
  const abs = Math.abs(gh);
  const picked = POWER_UNITS.find(u => abs >= u.factor) || POWER_UNITS[POWER_UNITS.length - 1];
  return `${(gh / picked.factor).toFixed(decimals)} ${picked.unit}`;
}

// ================== MAIN ==================
async function main() {
  const minersPower = parsePowerToGH(CONFIG.MinersPower); // internal tetap GH/s
  const bonusPowerPercent = CONFIG.BonusPowerPercent;

  // Bonus Power (Eh/s) yang kamu punya sekarang, sama seperti di panel "My power"
  const currentBonusPower = minersPower * (bonusPowerPercent / 100);

  console.log(`Miners: ${formatPowerFromGH(minersPower)} | Bonus Power: +${bonusPowerPercent}% / ${formatPowerFromGH(currentBonusPower)}`);
  console.log("Scraping halaman...");

  const allItems = await scrapeAllPages(minersPower, bonusPowerPercent, CONFIG.limit);

  // Urutkan berdasarkan rasio terbaik (nilai mentah)
  allItems.sort((a, b) => b.ratio - a.ratio);

  console.log("Selesai. Total item:", allItems.length);

  // Format output tabel dengan satuan
  const table = allItems.map(m => ({
    'Name': m.name,
    'Price': m.price,
    'Item Power': formatPowerFromGH(m.itemPower),
    'Item Bonus[%]': m.itemBonusPercent,
    'Ratio': m.ratio.toFixed(2),
    'Bonus Power Tambahan (Miners lama)': formatPowerFromGH(m.extraBonusOnOldMiners),
    'Bonus Power (Item baru)': formatPowerFromGH(m.bonusOnNewItem),
    'Added Power': formatPowerFromGH(m.addedPower),
    'New Current power': formatPowerFromGH(m.addedPower + minersPower + currentBonusPower),
    'New Bonus Power[%]': (m.newBonusFraction * 100).toFixed(3)
  }));

  console.table(table);
}

// ================== PAGINATION ==================
function findNextPageButton() {
  const activeBtn = document.querySelector('.marketplace-pagination-btn[disabled]');
  if (!activeBtn) return null;

  const activeLi = activeBtn.closest('li');
  if (!activeLi) return null;

  let nextLi = activeLi.nextElementSibling;
  while (nextLi) {
    const btn = nextLi.querySelector('.marketplace-pagination-btn');
    if (btn && !btn.disabled) {
      return btn;
    }
    nextLi = nextLi.nextElementSibling;
  }

  return null;
}

async function scrapeAllPages(minersPower, bonusPowerPercent, maxPages = 5) {
  const allItems = [];
  const seen = new Set();

  for (let i = 0; i < maxPages; i++) {
    console.log(`🔄 Memproses halaman ${i + 1}...`);

    const items = getItemsFromCurrentPage(minersPower, bonusPowerPercent, seen);
    allItems.push(...items);

    const nextBtn = findNextPageButton();
    if (nextBtn) {
      nextBtn.click();
      await waitForContentUpdate(); // Tunggu halaman baru selesai dimuat
    } else {
      console.log("✅ Tidak ada halaman selanjutnya.");
      break;
    }
  }

  return allItems;
}

// ================== PARSING HALAMAN ==================
function getItemsFromCurrentPage(minersPower, bonusPowerPercent, seenSet) {
  const items = [];
  const titleEls = document.getElementsByClassName('item-title');
  const priceEls = document.getElementsByClassName('item-price');
  const powerEls = document.getElementsByClassName('item-addition-power');
  const bonusEls = document.getElementsByClassName('item-addition-bonus');

  for (let i = 0; i < titleEls.length; i++) {
    try {
      const name = titleEls[i]?.textContent.trim();
      const powerText = powerEls[i]?.textContent.trim();
      const bonusText = bonusEls[i]?.textContent.trim();
      const priceText = priceEls[i]?.textContent.trim();

      if (!name || !powerText || !bonusText || !priceText) continue;

      const itemPower = parsePowerToGH(powerText);   // power item di marketplace (GH/s)
      const itemBonusPercent = parseFloat(bonusText); // bonus item di marketplace (%)
      const price = parseFloat(priceText.replace(/\s/g, '').replace('RLT', ''));

      if (seenSet.has(name) || itemPower < 5 || itemBonusPercent <= 0 || price <= 0) continue;
      seenSet.add(name);

      // Bonus Power baru setelah membeli item ini (pecahan, 2.4887 = 248.87%)
      const newBonusFraction = (itemBonusPercent + bonusPowerPercent) / 100;

      // Tambahan Bonus Power untuk Miners yang sudah kamu punya
      const extraBonusOnOldMiners = minersPower * (itemBonusPercent / 100);

      // Bonus Power yang dihasilkan item baru itu sendiri
      const bonusOnNewItem = itemPower * newBonusFraction;

      // Total kenaikan "Current power" jika item dibeli
      const addedPower = itemPower + extraBonusOnOldMiners + bonusOnNewItem;

      const ratio = (addedPower / 1000) / price;

      // Simpan nilai mentah (GH/s), format satuan dilakukan saat menampilkan tabel
      items.push({
        name, price, itemPower, itemBonusPercent, ratio,
        extraBonusOnOldMiners, bonusOnNewItem, addedPower, newBonusFraction
      });
    } catch (err) {
      console.warn(`❌ Error parsing item ke-${i + 1}:`, err);
    }
  }

  return items;
}

// ================== TUNGGU UPDATE KONTEN ==================
function waitForContentUpdate(timeout = 7000) {
  return new Promise((resolve) => {
    let lastTitles = Array.from(document.querySelectorAll('.item-title')).map(el => el.textContent.trim());
    const observer = new MutationObserver(() => {
      const currentTitles = Array.from(document.querySelectorAll('.item-title')).map(el => el.textContent.trim());
      // Jika daftar judul berubah, anggap konten sudah update
      if (currentTitles.length && !arraysEqual(lastTitles, currentTitles)) {
        // Delay sedikit untuk memastikan render benar-benar selesai
        setTimeout(() => {
          observer.disconnect();
          resolve();
        }, 800);
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });

    // Timeout fallback jika tidak ada perubahan konten
    setTimeout(() => {
      observer.disconnect();
      resolve();
    }, timeout);
  });
}

// Helper function untuk cek equality array string
function arraysEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

  await main();
})();