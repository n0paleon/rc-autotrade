const targetPrice = 0.014;     // 🎯 Harga maksimal per-unit (RLT)
const desiredQty = 100;          // 🛒 Jumlah item yang ingin dibeli

// Advance settings
const highStockThreshold = 200;  // Jika quantity di market >= threshold, maka quantity per-order akan dikali 2
const allowPartialOrder = true;  // ✅ Jika true, ambil order meski qty < desiredQty
const minAcceptableQty = 1;     // ⚠️ Minimum qty yang diterima jika partial order diizinkan
const delayPerOrder = 200;       // ⏱️ Delay antar request API (ms)

let stopped = false;
let totalProfitRLT = 0;
let lastRequestTime = 0;
const pendingOrders = new Map(); // order_id => startTime

const itemId = window.location.pathname.split('/').pop();
const itemName = document.querySelector('h4.main-item-title')?.innerText.trim() || "Unknown";

if (!getToken()) {
  console.warn("❌ Token tidak ditemukan. Harap refresh halaman.");
  throw new Error("Token tidak tersedia.");
}

printCredit();
console.log("⏳ Menghubungkan ke server...");
const socket = new WebSocket(`wss://nws.rollercoin.com/?token=${getToken()}`);

socket.addEventListener("open", () => {
  console.log("🔌 WebSocket terkoneksi.");
});

socket.addEventListener("close", (event) => {
  if (!stopped) {
    console.warn(`⚠️ WebSocket terputus. Kode: ${event.code} | Alasan: ${event.reason || 'tidak diketahui'}`);
    console.warn("🔁 Silakan refresh halaman atau restart bot jika diperlukan.");
  }
});

socket.addEventListener("error", (err) => {
  console.error("‼️ Terjadi error pada WebSocket:", err);
});

socket.addEventListener("message", async (event) => {
  if (stopped) return;

  try {
    const msg = JSON.parse(event.data);

    if (
      msg.cmd === "marketplace_orders_update" &&
      msg.value?.item_id === itemId &&
      msg.value?.currency === "RLT"
    ) {
      // Panggil fungsi terpisah untuk memproses update pasar secara non-blocking
      processMarketUpdate(msg.value);
    } else if (
      msg.cmd === "marketplace_orders_filled" &&
      msg.value?.item_id === itemId
    ) {
      // Panggil fungsi terpisah untuk menangani konfirmasi order
      handleOrderConfirmation(msg.value);
    }

  } catch (err) {
    console.warn("⚠️ Error parsing pesan WebSocket:", err);
  }
});

// --- Fungsi Baru untuk Memproses Update Pasar ---
async function processMarketUpdate(marketData) {
  const startTime = Date.now();
  const list = marketData.data?.list;
  if (!Array.isArray(list) || list.length === 0) return;

  const cheapest = list[0];
  const unitPriceRaw = cheapest.price;
  const unitPrice = unitPriceRaw / 1_000_000;
  const quantity = cheapest.quantity;
  const now = getFormattedDateTimeWithMilliseconds(new Date());
  const itemType = marketData?.item_type || "mutation_component";

  console.log(`[${now}] ${itemName} => ${unitPrice.toFixed(6)} RLT x ${quantity} tersedia`);

  const nowTime = Date.now();
  if (unitPrice <= targetPrice && nowTime - lastRequestTime >= delayPerOrder) {
    let qty = 0;

    if (quantity >= desiredQty) {
      qty = desiredQty;
      if (quantity >= highStockThreshold) {
        qty *= 2;
        console.log(`⚠️ Stok tinggi! Quantity per-order dinaikkan 2x lipat => ${qty}`);
      }
    } else if (allowPartialOrder && quantity >= minAcceptableQty) {
      qty = quantity;
      console.log(`⚠️ Partial order diaktifkan. Bot akan membeli quantity seadanya => ${qty}`);
    } else {
      // Tidak cukup stock, dan partial order tidak diizinkan atau di bawah minimal
      return;
    }

    lastRequestTime = nowTime;
    console.log("🎯 Harga cocok. Mengirim order...");
    logExecTime(startTime);
    // PENTING: Panggil sendOrder tanpa 'await' di sini
    // Ini memastikan bahwa event listener pesan tidak menunggu sendOrder selesai.
    sendOrder({ unitPriceRaw, qty, itemType });
  }
}

// --- Fungsi Baru untuk Menangani Konfirmasi Order ---
async function handleOrderConfirmation(orderData) {
  const { count, price, status, order_id } = orderData;
  const now = getFormattedDateTimeWithMilliseconds(new Date());
  const startTime = pendingOrders.get(order_id);
  const duration = startTime ? (Date.now() - startTime) : null;

  if (status === 12 && count > 0 && price > 0) {
    const totalPrice = price / 1_000_000;
    const unit = totalPrice / count;
    const diff = targetPrice - unit;
    const percent = ((diff / targetPrice) * 100).toFixed(2);
    const profit = diff * count;
    totalProfitRLT += profit;

    console.log("✅ ORDER TERKONFIRMASI!");
    console.log(`🧱 Nama Item     : ${itemName}`);
    console.log(`🛒 Jumlah Barang : ${count}`);
    console.log(`🎯 Harga Target  : ${targetPrice.toFixed(6)} RLT`);
    console.log(`📦 Harga Dibayar : ${totalPrice.toFixed(6)} RLT (${unit.toFixed(6)} x ${count})`);
    console.log(`📉 Selisih/unit  : ${diff.toFixed(6)} RLT (${percent}%)`);
    console.log(`📊 Total Profit  : ${profit.toFixed(6)} RLT (akumulasi: ${getProfit()})`);
    console.log(`🆔 Order ID      : ${order_id}`);
    if (duration !== null) console.log(`⚡ Durasi        : ${duration}ms`);
    console.log(`🕓 Waktu         : ${now}`);
    console.log('----------------------------------------\n\n');
    playSound();
  } else if (status === 2 && count === 0 && price === 0) {
    console.warn("❌ ORDER GAGAL: Tidak mendapatkan item.");
    console.warn(`🆔 Order ID: ${order_id}`);
    if (duration !== null) console.warn(`⚡ Durasi  : ${duration}ms`);
    console.warn(`🕓 Waktu   : ${now}`);
    console.warn('----------------------------------------\n\n');
  }

  if (pendingOrders.has(order_id)) {
    pendingOrders.delete(order_id);
  }
}

async function sendOrder({ unitPriceRaw, qty, itemType }) {
  const body = {
    challenge: "",
    action: "marketplace",
    itemId,
    itemType,
    totalCount: qty,
    currency: "RLT",
    totalPrice: unitPriceRaw * qty,
  };

  const headers = {
    "accept": "application/json",
    "content-type": "application/json",
    "authorization": `Bearer ${getToken()}`,
  };

  try {
    const startTime = Date.now();
    const res = await fetch("/api/marketplace/purchase-item", {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      credentials: "include"
    });

    const result = await res.json();

    if (res.ok && result.success && result.data?.usersPurchaseOrder) {
      const orderId = result.data.usersPurchaseOrder;
      pendingOrders.set(orderId, startTime);
    }

    if (res.status == 401) refreshToken();

  } catch (err) {
    console.error("‼️ Gagal mengirim order:", err);
  }
}

async function refreshToken() {
  try {
    const body = {
      refresh_token: localStorage.getItem("refreshToken"),
    }

    const headers = {
      "accept": "application/json",
      "content-type": "application/json",
    };

    const res = await fetch("/api/auth/refresh", {
      body: JSON.stringify(body),
      headers,
      method: "POST"
    });

    const result = await res.json();

    if (res.ok && result.success) {
      localStorage.setItem("token", result.data.access_token);
      localStorage.setItem("refreshToken", result.data.refresh_token);
      console.log('ℹ️ Token baru saja diperbarui');
    } else {
      console.warn('❌ Gagal memperbarui token');
    }
  } catch (err) {
    console.warn(err);
  }
}

function getProfit() {
  return totalProfitRLT.toFixed(6) + " RLT";
}

function playSound() {
  const successAudio = new Audio('https://storage.n0pal.net/artifacts/u_oepgi4ep3v-som_matricula-464025.mp3');
  successAudio.volume = 1;
  successAudio.play().catch(e => console.warn('Gagal memutar audio:', e));
}

function logExecTime(startTime) {
  console.log(`🚀 Durasi eksekusi: ${Date.now() - startTime}ms`);
}

function printCredit() {
  print("Selamat datang di TuruRC Bot!");
  print("Original author: @nopaleon.0f (Instagram)\n\n");
}

function print(...args) {
  console.log(`[✉️]`, ...args);
}

function getToken() {
  return localStorage.getItem("token");
}

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    stopped = true;
    socket.close();
    pendingOrders.clear();
    console.warn("⛔ Bot dihentikan oleh pengguna (ESC ditekan).");
    console.info(`📊 Total Profit: ${getProfit()}`);
  }
});

function getFormattedDateTimeWithMilliseconds(date) {
  const d = date || new Date();

  const month = d.getMonth() + 1;
  const day = d.getDate();
  const year = d.getFullYear();

  let hours = d.getHours();
  const minutes = d.getMinutes();
  const seconds = d.getSeconds();
  const milliseconds = d.getMilliseconds();

  const ampm = hours >= 12 ? 'PM' : 'AM';
  hours = hours % 12;
  hours = hours ? hours : 12;

  const formattedMinutes = minutes.toString().padStart(2, '0');
  const formattedSeconds = seconds.toString().padStart(2, '0');
  const formattedHours = hours.toString().padStart(2, '0');
  const formattedMilliseconds = milliseconds.toString().padStart(3, '0');

  return `${month}/${day}/${year}, ${formattedHours}:${formattedMinutes}:${formattedSeconds}.${formattedMilliseconds} ${ampm}`;
}
