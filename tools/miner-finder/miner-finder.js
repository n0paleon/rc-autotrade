async function main() {
  const CurrentPower = 11148000;
  const CurrentBonus = 17.49;
  const limit = 192; // batas halaman

  console.log("Scraping halaman...");

  const allMiners = await scrapeAllPages(CurrentPower, CurrentBonus, limit);

  // Urutkan berdasarkan rasio terbaik
  allMiners.sort((a, b) => b.Ratio - a.Ratio);

  console.log("Selesai. Total miner:", allMiners.length);
  console.table(allMiners);
}

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

      const power = convertPowerToGigaHash(powerText);
      const bonus = parseFloat(bonusText);
      const price = parseFloat(priceText.replace(/\s/g, '').replace('RLT', ''));

      if (seenSet.has(name) || power < 5 || bonus <= 0 || price <= 0) continue;
      seenSet.add(name);

      const totalbonus = (bonus + CurrentBonus) / 100;
      const ctm = CurrentPower * (bonus / 100);
      const mtb = power * totalbonus;
      const totalpower = power + ctm + mtb;
      const ratio = (totalpower / 1000) / price;

      miners.push({
        'Name': name,
        'Price': price,
        'Power': power,
        'Bonus[%]': bonus,
        'Ratio': ratio.toFixed(2),
        'Cur.Pow X Add.Bonus': ctm.toFixed(0),
        'Mine.Pow X Tot.Bonus': mtb.toFixed(0),
        'Addition Power': totalpower.toFixed(0),
        'TotalPower [GH/S]': (totalpower + CurrentPower).toFixed(2),
        'Total Bonus %': (totalbonus * 100).toFixed(3)
      });
    } catch (err) {
      console.warn(`❌ Error parsing item ke-${i + 1}:`, err);
    }
  }

  return miners;
}

function convertPowerToGigaHash(powerText) {
  let power = parseFloat(powerText);
  if (powerText.includes(" Ph/s")) power *= 1_000_000;
  else if (powerText.includes(" Th/s")) power *= 1_000;
  return power;
}

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


main();