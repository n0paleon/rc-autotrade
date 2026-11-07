(async () => {
// ================== KONFIGURASI ==================
const CONFIG = {
  CurrentPower: "20.745 Eh/s", // contoh: "10Gh/s", "10Ph/s", "15Eh/s" => raw power
  CurrentBonus: 238.87,         // dalam persen
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
  const CurrentPower = parsePowerToGH(CONFIG.CurrentPower); // internal tetap GH/s
  const CurrentBonus = CONFIG.CurrentBonus;

  console.log(`Power saat ini: ${formatPowerFromGH(CurrentPower)} | Bonus: ${CurrentBonus}%`);
  console.log("Scraping halaman...");

  const allMiners = await scrapeAllPages(CurrentPower, CurrentBonus, CONFIG.limit);

  // Urutkan berdasarkan rasio terbaik (nilai mentah)
  allMiners.sort((a, b) => b.ratio - a.ratio);

  console.log("Selesai. Total miner:", allMiners.length);

  // Format output tabel dengan satuan
  const table = allMiners.map(m => ({
    'Name': m.name,
    'Price': m.price,
    'Power': formatPowerFromGH(m.power),
    'Bonus[%]': m.bonus,
    'Ratio': m.ratio.toFixed(2),
    'Cur.Pow X Add.Bonus': formatPowerFromGH(m.ctm),
    'Mine.Pow X Tot.Bonus': formatPowerFromGH(m.mtb),
    'Addition Power': formatPowerFromGH(m.totalpower),
    'TotalPower': formatPowerFromGH(m.totalpower + CurrentPower),
    'Total Bonus %': (m.totalbonus * 100).toFixed(3)
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

async function scrapeAllPages(CurrentPower, CurrentBonus, maxPages = 5) {
  const allMiners = [];
  const seen = new Set();

  for (let i = 0; i < maxPages; i++) {
    console.log(`🔄 Memproses halaman ${i + 1}...`);

    const miners = getMinersFromCurrentPage(CurrentPower, CurrentBonus, seen);
    allMiners.push(...miners);

    const nextBtn = findNextPageButton();
    if (nextBtn) {
      nextBtn.click();
      await waitForContentUpdate(); // Tunggu halaman baru selesai dimuat
    } else {
      console.log("✅ Tidak ada halaman selanjutnya.");
      break;
    }
  }

  return allMiners;
}

// ================== PARSING HALAMAN ==================
function getMinersFromCurrentPage(CurrentPower, CurrentBonus, seenSet) {
  const miners = [];
  const minerTitles = document.getElementsByClassName('item-title');
  const minerPrices = document.getElementsByClassName('item-price');
  const minerPowers = document.getElementsByClassName('item-addition-power');
  const minerBonuses = document.getElementsByClassName('item-addition-bonus');

  for (let i = 0; i < minerTitles.length; i++) {
    try {
      const name = minerTitles[i]?.textContent.trim();
      const powerText = minerPowers[i]?.textContent.trim();
      const bonusText = minerBonuses[i]?.textContent.trim();
      const priceText = minerPrices[i]?.textContent.trim();

      if (!name || !powerText || !bonusText || !priceText) continue;

      const power = parsePowerToGH(powerText); // GH/s
      const bonus = parseFloat(bonusText);
      const price = parseFloat(priceText.replace(/\s/g, '').replace('RLT', ''));

      if (seenSet.has(name) || power < 5 || bonus <= 0 || price <= 0) continue;
      seenSet.add(name);

      const totalbonus = (bonus + CurrentBonus) / 100;
      const ctm = CurrentPower * (bonus / 100);
      const mtb = power * totalbonus;
      const totalpower = power + ctm + mtb;
      const ratio = (totalpower / 1000) / price;

      // Simpan nilai mentah (GH/s), format satuan dilakukan saat menampilkan tabel
      miners.push({ name, price, power, bonus, ratio, ctm, mtb, totalpower, totalbonus });
    } catch (err) {
      console.warn(`❌ Error parsing item ke-${i + 1}:`, err);
    }
  }

  return miners;
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