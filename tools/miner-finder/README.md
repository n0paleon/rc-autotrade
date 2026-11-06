# Miner finder (browser console)

A one-off script that compares **miners** on the Rollercoin marketplace and sorts them by power-to-RLT ratio. It is not part of the worker. Nothing imports it, and the backend never runs it.

## Usage

1. Open `https://rollercoin.com/marketplace` and select the **miners** tab.
2. Apply the filters and sorting you want, for example, sort by lowest price.
3. Open DevTools, go to **Console**, paste the entire contents of [`miner-finder.js`](miner-finder.js), and press Enter.
4. Before running it, adjust the three values at the top:

   | Variable | Meaning |
   | --- | --- |
   | `CurrentPower` | Current total account power in GH/s |
   | `CurrentBonus` | Current account mining bonus as a percentage |
   | `limit` | Maximum number of pages to scrape (default 192) |

5. The script clicks through pagination on its own, waits for each page to finish rendering, and then prints a result table sorted by the best ratio.

## Output columns

| Column | Meaning |
| --- | --- |
| `Name` | Miner name |
| `Price` | Listing price in RLT |
| `Power` | Miner power in GH/s |
| `Bonus[%]` | Miner mining bonus in percent |
| `Ratio` | `Addition Power / 1000 / Price`. Higher is better |
| `Cur.Pow X Add.Bonus` | Account power x miner bonus |
| `Mine.Pow X Tot.Bonus` | Miner power x (miner bonus + account bonus) |
| `Addition Power` | Total additional power from this miner |
| `TotalPower [GH/S]` | Account power + additional power |
| `Total Bonus %` | Account bonus + miner bonus |

## Limitations

- DOM selectors (`.item-title`, `.item-price`, `.item-addition-power`, `.item-addition-bonus`, and `.marketplace-pagination-btn`) can change whenever Rollercoin updates its UI. If the results are empty, check the selectors first.
- The script scrapes at most 192 pages and waits up to 7 seconds per page.
- Miners with the same name are counted only once.
- Keep the tab active while scraping because background tabs can delay rendering.
