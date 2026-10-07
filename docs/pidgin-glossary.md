# Nigerian Pidgin — string glossary

Every sentence the bot says, English beside Pidgin. **This file is reviewed and
corrected before any code is written**, and it stays afterwards as the reference for
new strings.

## How to review this

Edit the **Pidgin** cell directly, or write what you want in **Notes**. The rows
marked `??` and `XX` are the ones I most need you to look at — everything else is a
courtesy check.

| Marker | Meaning |
|---|---|
| `OK` | I am confident. Skim it. |
| `??` | Uncertain — register, word choice or idiom. Please correct. |
| `XX` | No natural Pidgin equivalent. I have given options; pick one or supply your own. |

**Must keep** lists tokens that have to survive verbatim into the Pidgin. Three of
them are load-bearing: the bot *parses the user's reply against them*, so translating
one breaks the flow silently — the user types the Pidgin word, the comparison fails,
and the handler falls through to its "anything else" branch.

## Conventions

**Orthography: BBC News Pidgin.** English-friendly spelling, no tone marks. `di`,
`dis`, `dat`, `dem`, `dey`, `don`, `go`, `fit`, `sabi`, `wey`, `na`, `no`, `abeg`,
`make`, `comot`, `wetin`, `e`, `am`. Not `yu`/`tank yu`/`spik`, and not Naija Guru's
accented `gó`.

**Technical nouns stay English** — PIN, wallet, address, transaction, txid, fee,
sats, block, confirm, seed phrase, network, balance, session, bitcoin, payjoin, QR,
mempool, node. This is what Pidgin does with technical vocabulary; respelling them
would cost comprehension and buy nothing.

**Not translated, by decision:**

- the ~35 short labels inside monospace `<code>` cards — `Confirmed`, `Sending`,
  `Incoming`, `Immature`, `Total`, `To`, `Amount`, `Fee`, `Rate`, `Size`, `In/Out`,
  `Today`, `Change`, `Status` — their widths are hand-aligned;
- `comfy-table` headers — `#`, `Address`, `Used`, `Received`, `Amount`, `Fee`,
  `Status`;
- network badges — `MAINNET`, `REGTEST`, `TESTNET`, `SIGNET`;
- callback data and fee slugs — `send:fee:fast`, `bal:refresh`, …;
- command names — `/send`, `/pj_receive`, …

**Budgets.** Measured, not guessed:

- `ui::payjoin_receipt` is a **photo caption**, and Telegram rejects the whole send
  over 1024 characters. Current mainnet caption is 749 chars (245 of which is the
  escaped BIP21 URI), leaving 275 spare. **The Pidgin prose must stay under ~750
  characters total**; English is 484, so there is room, but not unlimited room.
- The three fee preset buttons render three-across on a phone. **~8 characters** for
  the label, excluding the number.

---

## 1. Head — routing, welcome, status, network

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::unrecognised#1` | I don't know `{cmd}`. /help lists everything I do. | I no sabi `{cmd}`. /help go show you everything wey I fit do. | `{cmd}`, `/help`, `<code>` | OK | |
| `ui::unrecognised#2` | I only take commands — /help lists them. | Na only command I dey take — /help go show you dem. | `/help` | OK | |
| `ui::welcome#1` | **Your wallet is ready.** | **Your wallet don ready.** | `<b>` | OK | |
| `ui::welcome#2` | /balance — what you hold | /balance — wetin you get | `/balance` | OK | |
| `ui::welcome#3` | /receive — an address to be paid at | /receive — address wey person go pay you for | `/receive` | OK | |
| `ui::welcome#4` | /send — pay someone | /send — pay person | `/send` | OK | |
| `ui::welcome#5` | /help — everything else | /help — everything else wey remain | `/help` | OK | |
| `ui::welcome#6` | **A non-custodial Bitcoin wallet.** | **Bitcoin wallet wey na you dey hold di key.** | `<b>` | ?? | "non-custodial" has no settled Pidgin term — see Decisions |
| `ui::welcome#7` | Your seed phrase is generated on this server, encrypted with a PIN only you know, and never leaves it. Nobody can move your coins without that PIN — and nobody can recover it for you either. | Dis server na im make your seed phrase, lock am with PIN wey na only you sabi, and e no dey comot from here. Nobody fit move your coin without dat PIN — and nobody fit get am back for you too. | | ?? | long one; please check it flows |
| `ui::welcome#8` | /create — a new wallet | /create — new wallet | `/create` | OK | |
| `ui::welcome#9` | /restore — an existing seed phrase | /restore — seed phrase wey you get before | `/restore` | OK | |
| `ui::status#1` | **Backend** / Tip / Latency / Calls … per minute | **Backend** / Tip / Latency / Calls … every minute | `<b>`, labels | ?? | labels are aligned — keep English? see Decisions |
| `ui::status#2` | ⚠️ The backend is slow or rate-limited; commands may lag. | ⚠️ Backend dey slow or dem don limit am; command fit slow small. | `⚠️` | OK | |
| `ui::status#3` | **Session** / 🔓 Unlocked, {n} min left | **Session** / 🔓 E don open, {n} min remain | `{n}`, `<b>`, 🔓 | OK | |
| `ui::status#4` | **Session** / 🔒 Locked | **Session** / 🔒 E don lock | `<b>`, 🔒 | OK | |
| `ui::network_card#1` | Real bitcoin, on the real chain. Transactions cannot be reversed. | Real bitcoin, for di real chain. Once transaction commot, e no fit return. | | OK | |
| `ui::network_card#2` | Because this instance reaches Core through a hosted, allowlisted proxy, two things differ from a full node: | Becos dis bot dey reach Core through proxy wey dem host and limit, two things different from full node: | | ?? | |
| `ui::network_card#3` | • incoming payments are invisible until they confirm in a block; | • money wey dey enter no dey show until e confirm inside block; | `•` | OK | |
| `ui::network_card#4` | • fee estimates come from an external API, and every rate is floored at the node's own minimum. | • fee estimate dey come from outside API, and every rate no fit go below wetin di node allow. | `•` | OK | |
| `ui::network_card#5` | A private test chain. These coins are worth nothing — which is exactly what makes it the right place to learn the flows. | Na private test chain. Dis coin no get any value — na exactly why e good for learning how everything dey work. | | OK | |
| `ui::network_card#6` | `/mine 101` mints blocks (admins only). | `/mine 101` dey make block (na admin only). | `/mine 101`, `<code>` | OK | |
| `ui::network_card#7` | State for each network is stored separately; the two can never mix. | Each network get im own separate state; di two no fit mix at all. | | OK | |

## 2. Errors — `ui::render_error`

The most important table in the file: these are what a user reads when something has
gone wrong, and §8.1 requires each to say **what happened and what to do next**.

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `err::NoWallet` | You don't have a wallet yet. /create makes one, /restore brings an existing seed phrase. | You never get wallet. /create go make one, /restore go bring di one wey you get before. | `/create`, `/restore` | OK | |
| `err::WalletExists` | You already have a wallet here. /delete removes it first — make sure your seed phrase is written down. | You already get wallet here. /delete go comot am first — make sure say you don write your seed phrase down. | `/delete` | OK | |
| `err::BackupCheckFailed` | Those words don't match. Check the numbered words against what you wrote down, then try again. | Dem words no match. Check di number wey I ask against wetin you write down, den try again. | | OK | |
| `err::InvalidMnemonic` | That isn't a valid seed phrase. Check the spelling and the word order, then try /restore again. | Dat one no be correct seed phrase. Check di spelling and di order wey di words follow, den try /restore again. | `/restore` | OK | |
| `err::InvalidPin` | A PIN must be {min}–{max} digits. Try again. | PIN must be {min}–{max} number. Try again. | `{min}`, `{max}` | OK | |
| `err::WrongPin` | Wrong PIN. {n} attempt(s) left before a lockout. | PIN no correct. You get {n} try before dem go lock you out. | `{n}` | OK | |
| `err::PinLocked` | Too many wrong PINs. Wait for the lockout to clear, then try again. | You don enter wrong PIN too many times. Wait make di lock clear, den try again. | | ?? | "lockout" — see Decisions |
| `err::Locked` | Your wallet is locked. /unlock first. | Your wallet don lock. /unlock first. | `/unlock` | OK | |
| `err::RestoreTooDeep` | That birthday is {d} blocks back — over the {m} block limit, and about {h} hours of scanning. Use a later birthday if you know one. | Dat birthday na {d} block back — e pass di {m} block limit, and e go take like {h} hours to scan. Use birthday wey near if you sabi one. | `{d}`, `{m}`, `{h}` | OK | |
| `err::InvalidPaymentTarget` | That isn't a valid address or BIP21 URI for {badge}. | Dat one no be correct address or BIP21 URI for {badge}. | `{badge}` | OK | |
| `err::InsufficientFunds` | Not enough funds: this would cost {a}, and you have {b}. | Money no reach: dis one go cost {a}, but na {b} you get. | `{a}`, `{b}` | OK | |
| `err::FeeBelowFloor` | {g} sat/vB is below the network's current minimum of {f} sat/vB, so it would never relay. Choose a higher rate. | {g} sat/vB dey below di network minimum wey be {f} sat/vB, so e no go ever waka. Choose rate wey high pass. | `{g}`, `{f}` | OK | |
| `err::OverSendCap` | {a} is over this bot's per-payment cap of {c}. | {a} pass di limit wey dis bot put for one payment, wey be {c}. | `{a}`, `{c}` | OK | |
| `err::bump.AlreadyConfirmed` | That transaction is already in a block, so its fee can't be changed — and it doesn't need to be. /tx shows how many confirmations it has. | Dat transaction don enter block already, so you no fit change di fee — and e no need am. /tx go show you how many confirmation e get. | `/tx` | OK | |
| `err::bump.NotFound` | I don't know that transaction. /history lists the ones this wallet has, and the txid has to be the whole thing. | I no sabi dat transaction. /history go list di ones wey dis wallet get, and di txid must be di full thing. | `/history` | OK | |
| `err::bump.NotReplaceable` | That transaction didn't signal replace-by-fee, so no node would accept a replacement. You'll have to wait for it to confirm. | Dat transaction no signal replace-by-fee, so no node go accept replacement. You go just wait make e confirm. | `replace-by-fee` | OK | keeping RBF in English |
| `err::bump.RateTooLow` | A replacement has to outbid the original, and that rate doesn't. The lowest the network will take here is {r} sat/vB — choose that or higher. | Replacement must pay pass di first one, and dat rate no do am. Di lowest wey di network go take na {r} sat/vB — choose dat one or higher. | `{r}` | OK | |
| `err::bump.AbsoluteFeeTooLow` | A replacement has to pay more in total than the transaction it replaces. That means at least {a} here. | Replacement must pay more in total pass di transaction wey e dey replace. So na {a} at least for dis one. | `{a}` | OK | |
| `err::QuoteExpired` | That payment card has expired, so the fee it quoted may be stale. Start /send again. | Dat payment card don expire, so di fee wey e show fit don old. Start /send again. | `/send` | ?? | "stale" → "don old"? see Decisions |
| `err::BroadcastRejected` | The network rejected the transaction: `{reason}` | Di network reject di transaction: `{reason}` | `{reason}`, `<code>` | OK | |
| `err::UnsupportedOnNetwork` | That command isn't available on {badge}. | Dat command no dey available for {badge}. | `{badge}` | OK | |
| `err::Payjoin` | The payjoin couldn't be completed. Your funds are untouched — /pj_sessions shows what happened. | Di payjoin no fit complete. Dem no touch your money — /pj_sessions go show you wetin happen. | `/pj_sessions` | OK | |
| `err::NoSuchSession` | No such payjoin session. | Payjoin session like dat no dey. | | OK | |
| `err::Misconfigured` | This bot is misconfigured and can't serve that safely. Tell whoever runs it. | Dem no set dis bot well, so e no fit do dat one safely. Tell di person wey dey run am. | | OK | |
| `err::Internal` | Something went wrong on this server. Nothing was sent. Try again in a moment. | Something spoil for dis server. Nothing no comot. Try again small time. | | OK | |
| `err::backend.MissingApiKey` | This bot can't reach the Bitcoin backend — check its BITRPC_API_KEY. Nothing was sent. | Dis bot no fit reach di Bitcoin backend — check im BITRPC_API_KEY. Nothing no comot. | `BITRPC_API_KEY` | OK | |
| `err::backend.RateLimited` | The backend is rate-limited right now. Try again in a minute; nothing was sent. | Dem don limit di backend for now. Try again for one minute; nothing no comot. | | OK | |
| `err::backend.NodeUnavailable` | The Bitcoin node is unavailable. Try again shortly; nothing was sent. | Di Bitcoin node no dey available. Try again small time; nothing no comot. | | OK | |
| `err::backend.Rpc` | The node refused that request: `{msg}` | Di node refuse dat request: `{msg}` | `{msg}`, `<code>` | OK | |
| `err::backend.Transport` | Couldn't reach the Bitcoin backend. Try again shortly; nothing was sent. | I no fit reach di Bitcoin backend. Try again small time; nothing no comot. | | OK | |
| `ui::payjoin_unavailable#1` | Couldn't start a payjoin request — the payjoin directory or relay didn't answer. Nothing was sent, and /receive still works for an ordinary payment. | I no fit start payjoin request — di payjoin directory or relay no answer. Nothing no comot, and /receive still dey work for normal payment. | `/receive`, `<code>` | OK | |

## 3. Lifecycle — create, restore, unlock, delete

These carry the safety warnings. Several are pinned by tests, and the Pidgin wording
chosen here becomes the assertion.

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::ask_pin_new#1` | **Choose a PIN** — 6–8 digits. It encrypts your seed phrase on this server, and it is the only thing standing between someone with the database and your coins. There is no way to reset it. Send it now — I'll delete your message straight away. | **Choose PIN** — 6–8 number. Na im dey lock your seed phrase for dis server, and na only am dey between person wey get di database and your coin. No way dey to reset am. Send am now — I go delete your message sharp-sharp. | `<b>` | ?? | safety-critical; "no way to reset" is pinned by a test |
| `ui::ask_pin_again#1` | Send the same PIN once more, so a typo can't lock you out. | Send di same PIN again, so say mistake no go lock you out. | | OK | |
| `ui::pin_mismatch#1` | Those two didn't match. Let's start the PIN again — send the one you want. | Di two no match. Make we start di PIN again — send di one wey you want. | | OK | |
| `ui::ask_pin#1` | Send your PIN. I'll delete the message as soon as it arrives. | Send your PIN. I go delete di message sharp-sharp once e enter. | | OK | |
| `ui::wrong_pin_retry#1` | Wrong PIN — nothing was signed and nothing was sent. {n} attempt(s) left. Send it again. | PIN no correct — we no sign anything, we no send anything. You get {n} try wey remain. Send am again. | `{n}` | OK | |
| `ui::ask_mnemonic#1` | **Send your seed phrase** — 12 or 24 words, in order, separated by spaces. I'll delete your message the instant it arrives. Only do this in a chat you trust, on a device you trust. | **Send your seed phrase** — 12 or 24 words, for order, with space between dem. I go delete your message immediately e enter. Only do dis for chat wey you trust, for device wey you trust. | `<b>` | OK | |
| `ui::ask_birthday#1` | **When was this wallet first used?** — Send the block height if you know it — scanning starts there instead of from the beginning of the chain, which is much faster. Send `skip` if you don't know. | **When you first use dis wallet?** — Send di block height if you sabi am — scanning go start from there instead of from di beginning of di chain, wey dey fast well well. Send `skip` if you no sabi. | **`skip`**, `<b>`, `<code>` | OK | **`skip` is parsed at `wallet.rs:131` — must not be translated** |
| `ui::restore_plan#1` | **Restore** / From block {a} to {b} — {n} blocks. | **Restore** / From block {a} to {b} — {n} blocks. | `{a}`, `{b}`, `{n}`, `<b>` | OK | |
| `ui::restore_plan#2` | This will be quick. | Dis one go fast. | | OK | "quick" pinned by a test |
| `ui::restore_plan#3` | ⏳ Scanning that far back takes about {d}. You can keep using the chat meanwhile; balances will fill in as it goes. | ⏳ To scan reach dat far go take like {d}. You fit still dey use di chat; balance go dey enter as e dey go. | `{d}`, ⏳ | OK | |
| `ui::restore_plan#4` | ❌ That's more than this bot will scan ({m} blocks, about {d}). The limit exists because every block costs calls against a shared, rate-limited backend. If you know a later block height for this wallet, send /restore again and use it. | ❌ Dat one pass wetin dis bot go scan ({m} blocks, like {d}). Di limit dey because every block dey cost call for backend wey everybody dey share and wey get limit. If you sabi block height wey near for dis wallet, send /restore again make you use am. | `{m}`, `{d}`, `/restore`, ❌ | OK | |
| `ui::duration#1` | under a minute | less than one minute | | ?? | |
| `ui::duration#2` | {n} minutes | {n} minutes | `{n}` | OK | |
| `ui::duration#3` | {n} hours | {n} hours | `{n}` | OK | |
| `ui::mnemonic_card#1` | **Write these down, in order, on paper.** | **Write dis words for paper, as dem dey follow.** | `<b>` | OK | "paper" pinned by a test |
| `ui::mnemonic_card#2` | ⏳ This message deletes itself in {n} seconds. | ⏳ Dis message go delete inself for {n} seconds. | `{n}`, ⏳ | OK | |
| `ui::mnemonic_card#3` | Anyone with these words has your coins. Never type them into anything that asks for them — including, after today, this bot. | Anybody wey get dis words get your coin. Shine your eye: no type dem inside anything wey ask for dem — even dis bot, afta today. | | OK | "shine your eye" is the natural idiom |
| `ui::ask_backup_word#1` | **Check {n} of 3** / What is word number {i}? | **Check {n} of 3** / Wetin be word number {i}? | `{n}`, `{i}`, `<b>` | OK | the digit is pinned by a test |
| `ui::wallet_ready#1` | ✅ **Your wallet is ready.** | ✅ **Your wallet don ready.** | `<b>`, ✅ | OK | |
| `ui::restored#1` | ✅ **Restored.** Scanning for your history now — /balance will fill in as it goes. | ✅ **E don return.** I dey scan for your history now — /balance go dey fill as e dey go. | `<b>`, ✅, `/balance` | OK | |
| `ui::unlocked#1` | 🔓 Unlocked for {n} minutes. | 🔓 E don open for {n} minutes. | `{n}`, 🔓 | OK | |
| `ui::locked#1` | 🔒 Locked. | 🔒 E don lock. | 🔒 | OK | |
| `ui::ask_delete_word#1` | **Delete this wallet?** — Your seed phrase and every address this bot knows for you will be erased here. If you have the words written down you can restore later; if you don't, the coins are gone. Type `DELETE` in capitals to continue, or anything else to stop. | **You wan delete dis wallet?** — Your seed phrase and every address wey dis bot sabi for you go clear from here. If you write di words down you fit restore later; if you no write am, di coin don go. Type `DELETE` for capital letter to continue, or anything else to stop. | **`DELETE`**, `<b>`, `<code>` | OK | **`DELETE` is compared at `wallet.rs:366` — must not be translated.** "gone" pinned by a test |
| `ui::delete_cancelled#1` | Nothing deleted. | I no delete anything. | | OK | |
| `ui::deleted#1` | Wallet deleted. **/restore** brings it back if you have the seed phrase. | Wallet don delete. **/restore** go bring am back if you get di seed phrase. | `/restore`, `<b>` | OK | |

## 4. On-chain — balance, receive, addresses, history, tx

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::balance#1` | **Balance** | **Balance** | `<b>` | OK | |
| `ui::balance#2` | ℹ️ Payments to you appear here once they're in a block, not before — this bot's node connection can't see the mempool. | ℹ️ Money wey person send you go show here once e enter block, no be before — dis bot node connection no fit see di mempool. | ℹ️ | OK | |
| `ui::play_money_caveat#1` | *Dollar values are approximate, from {src}.* | *Di dollar amount na estimate, from {src}.* | `{src}`, `<i>` | OK | |
| `ui::play_money_caveat#2` | *Test coins — worth nothing. Dollar values apply the real mainnet price from {src} to play money, so you can see what the numbers would look like.* | *Test coin — e no get value. Di dollar amount dey use di real mainnet price from {src} for play money, so you fit see how di number go look.* | `{src}`, `<i>` | OK | |
| `ui::receive#1` | **Your address** / Unused address #{n} | **Your address** / Address wey never use #{n} | `{n}`, `<b>` | OK | |
| `ui::receive#2` | ℹ️ A payment here shows up once it's in a block. Until then it won't appear in /balance, even though it's on its way. | ℹ️ Payment wey enter here go show once e don enter block. Before den e no go show for /balance, even though e dey come. | ℹ️, `/balance` | OK | |
| `ui::addresses#1` | No addresses yet. /receive makes one. | No address dey yet. /receive go make one. | `/receive` | OK | |
| `ui::addresses#2` | yes | yes | | ?? | table cell — translate or keep? see Decisions |
| `ui::history#1` | No transactions yet. /receive gives you an address to be paid at. | No transaction dey yet. /receive go give you address wey person go pay you for. | `/receive` | OK | |
| `ui::tx_detail#1` | Received | Dem send you | | ?? | heading beside the amount |
| `ui::tx_detail#2` | Sent | You send | | ?? | |
| `ui::tx_detail#3` | Moved | You move am | | ?? | internal transfer |
| `ui::tx_detail#4` | See it on mempool.space | See am for mempool.space | `<a href>` | OK | |
| `ui::status_text#1` | pending | e never confirm | | ?? | see Decisions — translate values but not labels? |
| `ui::status_text#2` | ✅ {n} confs | ✅ {n} confs | `{n}`, ✅ | OK | |
| `ui::status_text#3` | {n} conf | {n} conf | `{n}` | OK | |
| `ui::pager#1` | Page {a} of {b} | Page {a} of {b} | `{a}`, `{b}` | OK | |

## 5. Notifications

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::incoming#1` | 📥 Incoming {a} — unconfirmed | 📥 {a} dey enter — e never confirm | `{a}`, 📥 | OK | |
| `ui::incoming#2` | 📥 Received {a} — ✅ {n} conf | 📥 You don receive {a} — ✅ {n} conf | `{a}`, `{n}`, 📥, ✅ | OK | |
| `ui::incoming_many#1` | 📥 Received {n} payments — {a} /history lists them; /balance has the total. | 📥 You don receive {n} payments — {a} /history go list dem; /balance get di total. | `{n}`, `{a}`, `/history`, `/balance`, 📥 | ?? | grouped line for one sync pass |
| `ui::confirmed#1` | ✅ {id} — {n} confs | ✅ {id} — {n} confs | `{id}`, `{n}`, ✅ | OK | |
| `ui::confirmed_many#1` | ✅ {n} transactions confirmed. /history lists them; /balance has the total. | ✅ {n} transaction don confirm. /history go list dem; /balance get di total. | `{n}`, `/history`, `/balance`, ✅ | OK | |
| `ui::session_expired#1` | 🔒 Session locked after inactivity. | 🔒 Session don lock becos you no do anything for a while. | 🔒 | OK | |
| `ui::backend_degraded#1` | ⚠️ The Bitcoin backend is slow or rate-limited; commands may lag. | ⚠️ Di Bitcoin backend dey slow or dem don limit am; command fit slow small. | ⚠️ | OK | |
| `ui::backend_recovered#1` | ✅ Backend healthy again. | ✅ Backend don better again. | ✅ | OK | |

## 6. The send flow

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::fee_card#1` | **Choose a fee** | **Choose fee** | `<b>` | OK | |
| `ui::fee_card#2` | Sending {a} to `{to}` | You dey send {a} go `{to}` | `{a}`, `{to}`, `<code>` | OK | |
| `ui::fee_card#3` | Sending to `{to}` | You dey send go `{to}` | `{to}`, `<code>` | OK | |
| `ui::fee_card#4` | Rates from your node | Rate come from your node | | OK | |
| `ui::fee_card#5` | Rates from {name} | Rate come from {name} | `{name}` | OK | |
| `ui::fee_card#6` | No fee estimate available right now — enter a rate yourself | No fee estimate dey now — enter rate by yourself | | OK | |
| `ui::fee_card#7` | floor {n} sat/vB | floor {n} sat/vB | `{n}` | OK | |
| `ui::fee_label#1` | Fast | Sharp | | ?? | button, ~8 chars |
| `ui::fee_label#2` | Normal | Normal | | OK | button |
| `ui::fee_label#3` | Slow | Slow | | OK | button |
| `ui::ask_custom_fee#1` | Send a fee rate in sat/vB — a whole number, at least {n}. | Send fee rate for sat/vB — full number, at least {n}. | `{n}` | OK | |
| `ui::confirm_card#1` | Confirm payment | Confirm di payment | | OK | header |
| `ui::confirm_card#2` | Fee bump | Raise di fee | | OK | header |
| `ui::confirm_card#3` | 🤝 Payjoin will be attempted — it makes this payment harder to trace. | 🤝 We go try payjoin — e dey make dis payment hard to trace. | 🤝 | OK | |
| `ui::confirm_card#4` | Replaces `{id}` | E dey replace `{id}` | `{id}`, `<code>` | OK | |
| `ui::confirm_card#5` | ⏳ Expires in {t} | ⏳ E go expire for {t} | `{t}`, ⏳ | OK | |
| `ui::bump_fee_card#1` | **Raise the fee** / Replacing `{id}` | **Raise di fee** / E dey replace `{id}` | `{id}`, `<b>`, `<code>` | OK | |
| `ui::bump_fee_card#2` | A replacement has to outbid what it replaces, so anything at or above the minimum will relay and anything below it will not. | Replacement must pay pass di one wey e dey replace, so anything from di minimum go waka and anything below am no go waka. | | OK | |
| `ui::bump_fee_card#3` | This chain has no fee estimates above that, so the minimum is the only rate worth offering — or type your own. | Dis chain no get fee estimate wey pass dat one, so na di minimum only make sense — or type your own. | | OK | |
| `ui::quote_expired_card#1` | **Expired** / That card quoted a fee that may now be stale, so it was not signed. Nothing was sent. Start /send again. | **E don expire** / Dat card show fee wey fit don old, so we no sign am. Nothing no comot. Start /send again. | `<b>`, `/send` | OK | |
| `ui::broadcasting#1` | 📡 Signing and broadcasting… | 📡 We dey sign am, we dey send am go network… | 📡 | OK | |
| `ui::broadcast_done#1` | 📡 **Sent** / {a} + {f} fee | 📡 **E don comot** / {a} + {f} fee | `{a}`, `{f}`, `<b>`, 📡 | OK | |
| `ui::broadcast_done#2` | Tracking confirmations. | I dey watch for confirmation. | | OK | |
| `ui::broadcast_done#3` | 🤝 Payjoin ✅ | 🤝 Payjoin ✅ | 🤝, ✅ | OK | |
| `ui::send_usage#1` | **Send bitcoin** | **Send bitcoin** | `<b>` | OK | |
| `ui::send_usage#2` | `/send <address> max` | `/send <address> max` | **`max`**, `<code>` | OK | **`max` is parsed at `send.rs:55` — must not be translated** |
| `ui::send_usage#3` | A BIP21 URI can carry its own amount, and a payjoin endpoint if the receiver offers one. | BIP21 URI fit carry im own amount, and payjoin endpoint if di person wey dey collect get one. | | OK | |
| `ui::send_cancelled#1` | Cancelled. Nothing was sent. | We don cancel am. Nothing no comot. | | OK | |
| `ui::fee_rate_out_of_range#1` | That is not a fee rate I can use. Send a whole number of sat/vB. | Dat one no be fee rate wey I fit use. Send full number for sat/vB. | | OK | |
| `btn::confirm` | ✅ Confirm & sign | ✅ Confirm & sign | ✅, `&` | ?? | button; `&` is fine in a label |
| `btn::cancel` | ✖ Cancel | ✖ Cancel | ✖ | OK | button |
| `btn::custom_fee` | Custom sat/vB | My own sat/vB | | ?? | button |
| `btn::min_rate` | At least {n} sat/vB | At least {n} sat/vB | `{n}` | OK | button |
| `btn::refresh` | 🔄 Refresh | 🔄 Refresh | 🔄 | OK | button, `onchain.rs:109,145` |
| `btn::pj_cancel` | ✖ Cancel {id} | ✖ Cancel {id} | `{id}`, ✖ | OK | button, `payjoin.rs:141` |

## 7. Faucet and mining

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::faucet_usage#1` | `/faucet [sats]` — test coins for this regtest chain. Between {a} and {b} sats, or leave it out for the default. | `/faucet [sats]` — test coin for dis regtest chain. From {a} to {b} sats, or leave am make e use di default. | `{a}`, `{b}`, `<code>` | OK | |
| `ui::faucet_needs_wallet#1` | There is nowhere to put them yet. /create a wallet first, then /faucet. | Place no dey wey e go enter yet. /create wallet first, den /faucet. | `/create`, `/faucet` | OK | |
| `ui::faucet_dry#1` | The faucet is empty: the node's own wallet holds {a}. Its coins come from mining, so `/mine 101` fills it — a coinbase needs 100 blocks before it can be spent, which is where the 101 comes from. | Di faucet don dry: di node own wallet get {a}. Im coin dey come from mining, so `/mine 101` go fill am — coinbase need 100 block before you fit spend am, na where di 101 come from. | `{a}`, `/mine 101`, `<code>` | OK | |
| `ui::faucet_sent#1` | 🚰 Sent {a} to your wallet, and mined a block so it is spendable now. | 🚰 I don send {a} go your wallet, and I mine block so you fit spend am now. | `{a}`, 🚰 | OK | |
| `ui::faucet_sent#2` | **Confirmed {a}** | **Confirmed {a}** | `{a}`, `<b>` | OK | |
| `ui::mining#1` | ⛏ Mining {n} blocks — processing… | ⛏ I dey mine {n} blocks — e dey work… | `{n}`, ⛏ | OK | |
| `ui::mined#1` | ⛏ Mined {n} block(s). | ⛏ I don mine {n} block(s). | `{n}`, ⛏ | OK | |
| `ui::mined#2` | The rewards are yours, but a freshly mined coin needs 100 more blocks before it can be spent — it shows under immature until then. | Di reward na your own, but coin wey you just mine need 100 more block before you fit spend am — e go dey show under immature till den. | `immature` | ?? | "immature" — see Decisions |

## 8. Payjoin

**`ui::payjoin_receipt` is a photo caption. Total Pidgin prose must stay under ~750
characters** — see Budgets at the top.

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `ui::payjoin_receipt#1` | 🤝 **Payjoin request for {a}** | 🤝 **Payjoin request for {a}** | `{a}`, `<b>`, 🤝 | OK | |
| `ui::payjoin_receipt#2` | Pay this with a wallet that supports payjoin and your two wallets build the transaction together — so the usual assumption that every input belongs to the sender stops holding for this payment. | Pay dis one with wallet wey sabi payjoin, and una two wallet go build di transaction together — so di normal assumption say every input na di sender own no go hold for dis payment. | | ?? | longest one; watch the budget |
| `ui::payjoin_receipt#3` | If the sender's wallet doesn't do payjoin, they can still pay it normally. | If di sender wallet no dey do payjoin, dem fit still pay am di normal way. | | OK | |
| `ui::payjoin_receipt#4` | ℹ️ This hides the payment from outside observers, not from the node provider this bot talks to. | ℹ️ Dis one dey hide di payment from people outside, no be from di node provider wey dis bot dey talk to. | ℹ️ | OK | mainnet only |
| `ui::payjoin_receipt#5` | This request expires in an hour. /pj_sessions shows how it's going. | Dis request go expire for one hour. /pj_sessions go show you how e dey go. | `/pj_sessions` | OK | |
| `ui::payjoin_sessions#1` | No payjoin sessions. /pj_receive `<sats>` starts one. | No payjoin session dey. /pj_receive `<sats>` go start one. | `/pj_receive`, `&lt;sats&gt;` | OK | |
| `ui::payjoin_sessions#2` | **Payjoin sessions** | **Payjoin sessions** | `<b>` | OK | |
| `ui::payjoin_sessions#3` | Receiving | Dey collect | | ?? | |
| `ui::payjoin_sessions#4` | Sending | Dey send | | ?? | |
| `ui::payjoin_state_text#1` | waiting for the other side | dey wait for di other side | | OK | |
| `ui::payjoin_state_text#2` | checking their proposal | dey check dia proposal | | OK | |
| `ui::payjoin_state_text#3` | proposal sent, waiting | proposal don go, dey wait | | OK | |
| `ui::payjoin_state_text#4` | done — payjoin | e don finish — payjoin | | OK | |
| `ui::payjoin_state_text#5` | sent as a regular transaction | e comot as normal transaction | | OK | |
| `ui::payjoin_state_text#6` | expired | e don expire | | OK | |
| `ui::payjoin_state_text#7` | cancelled | dem don cancel am | | OK | |
| `ui::payjoin_state_text#8` | didn't complete — funds untouched | e no complete — dem no touch di money | | OK | |
| `ui::payjoin_event#1` | 🤝 Payjoin proposal received — verifying | 🤝 Payjoin proposal don land — I dey check am | 🤝 | OK | |
| `ui::payjoin_event#2` | 🤝 Payjoin ✅ — {id} | 🤝 Payjoin ✅ — {id} | `{id}`, 🤝, ✅ | OK | |
| `ui::payjoin_event#3` | ↩️ Sent as a regular transaction (payjoin didn't complete) — {id} | ↩️ E comot as normal transaction (payjoin no complete) — {id} | `{id}`, ↩️ | OK | |
| `ui::payjoin_event#4` | Payjoin request expired. Nothing was sent. | Payjoin request don expire. Nothing no comot. | | OK | |
| `ui::payjoin_event#5` | The payjoin didn't complete. Your funds are untouched — /pj_sessions has the detail. | Di payjoin no complete. Dem no touch your money — /pj_sessions get di detail. | `/pj_sessions` | OK | |
| `ui::payjoin_usage#1` | `/pj_receive <sats>` — ask to be paid with payjoin. | `/pj_receive <sats>` — ask make dem pay you with payjoin. | `/pj_receive`, `&lt;sats&gt;`, `<code>` | OK | |
| `ui::payjoin_cancelled#1` | Payjoin session cancelled. | Dem don cancel di payjoin session. | | OK | |

## 9. Command descriptions

Shown by `/help`. **The native Telegram menu stays English regardless** — Telegram
requires a two-letter ISO 639-1 code and Nigerian Pidgin is `pcm` (ISO 639-3), so
there is no value that means Pidgin. These are for the `/help` we render ourselves.

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `cmd::_header` | A non-custodial Bitcoin wallet. Commands: | Bitcoin wallet wey na you dey hold di key. Commands: | | ?? | see `ui::welcome#6` |
| `cmd::start` | start here, or return to the main menu | start from here, or come back to di main menu | | OK | |
| `cmd::help` | show this list | show dis list | | OK | |
| `cmd::create` | create a new wallet | make new wallet | | OK | |
| `cmd::restore` | restore a wallet from a seed phrase | bring back wallet with seed phrase | | OK | |
| `cmd::unlock` | unlock for signing | open am make you fit sign | | OK | |
| `cmd::lock` | lock immediately | lock am now now | | OK | |
| `cmd::export` | show your seed phrase (PIN required) | show your seed phrase (PIN dey necessary) | | OK | |
| `cmd::delete` | delete your wallet (PIN required) | delete your wallet (PIN dey necessary) | | OK | |
| `cmd::receive` | show a receiving address | show address wey person go pay you for | | OK | |
| `cmd::addresses` | list your addresses | list your address dem | | OK | |
| `cmd::balance` | show your balance | show your balance | | OK | |
| `cmd::history` | list your transactions | list your transaction dem | | OK | |
| `cmd::tx` | show one transaction | show one transaction | | OK | |
| `cmd::send` | send bitcoin: /send `<address\|bip21>` [amount] | send bitcoin: /send `<address\|bip21>` [amount] | `/send`, the usage | OK | |
| `cmd::bumpfee` | raise the fee on a stuck transaction | raise di fee for transaction wey stuck | | OK | |
| `cmd::pj_receive` | receive via payjoin: /pj_receive `<sats>` | collect with payjoin: /pj_receive `<sats>` | `/pj_receive` | OK | |
| `cmd::pj_sessions` | list your payjoin sessions | list your payjoin session dem | | OK | |
| `cmd::status` | backend and session status | backend and session status | | OK | |
| `cmd::network` | which chain this bot is bound to | which chain dis bot dey for | | OK | |
| `cmd::mine` | regtest only, admins only: /mine `<n>` | regtest only, admin only: /mine `<n>` | `/mine` | OK | |
| `cmd::faucet` | regtest only: fund your wallet, /faucet [sats] | regtest only: put money for your wallet, /faucet [sats] | `/faucet` | OK | |
| `cmd::pidgin` | switch between English and Nigerian Pidgin | change between English and Naija Pidgin | | OK | **new command** |

## 10. Refusals — `auth::Refusal`

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `auth::NotPrivate` | This bot only works in a private chat — a wallet command in a group would put your balance, and possibly your seed phrase, in front of everyone. Message me directly. | Dis bot dey work for private chat only — wallet command inside group go show your balance, and maybe your seed phrase, to everybody. Message me direct. | | OK | "private chat" pinned by a test |
| `auth::NotAllowed` | This bot is private and your account isn't on its list. | Dis bot na private and your account no dey im list. | | OK | |
| `auth::NoSender` | I can't tell who sent that. | I no fit tell who send dat one. | | OK | **always English in practice** — no sender means no preference |
| `auth::TooFast` | That's a lot of commands at once — give me a moment and try again. | Na plenty command at once — give me small time make you try again. | | OK | |

## 11. Inline literals in `handlers/`

| ID | English | Pidgin | Must keep | Conf | Notes / your corrections |
|---|---|---|---|---|---|
| `handlers::admin.not_admin` | That command is for this bot's admins. | Dat command na for dis bot admin dem. | | OK | |
| `handlers::admin.mine_range` | Mine between 1 and 500 blocks. | Mine between 1 and 500 block. | | OK | |
| `handlers::onchain.bad_txid` | That doesn't look like a transaction id. Try `/tx <txid>`. | Dat one no look like transaction id. Try `/tx <txid>`. | `/tx`, `&lt;txid&gt;`, `<code>` | OK | |
| `handlers::onchain.no_tx` | I don't know that transaction. /history lists the ones this wallet has. | I no sabi dat transaction. /history go list di ones wey dis wallet get. | `/history` | OK | |
| `handlers::send.bump_usage` | Which transaction? `/bumpfee <txid>` — /history lists them. | Which transaction? `/bumpfee <txid>` — /history go list dem. | `/bumpfee`, `&lt;txid&gt;`, `<code>` | OK | |
| `handlers::start.not_yet` | That command isn't wired up in this build yet. /create, /restore, /unlock, /lock, /export, /delete, /status and /network work. | Dat command never ready for dis build. /create, /restore, /unlock, /lock, /export, /delete, /status and /network dey work. | all the commands | ?? | this text is already stale in English — worth fixing while translating |
| `handlers::wallet.send_seed` | Send your seed phrase as text. | Send your seed phrase as text. | | OK | |
| `ui::language_switched.pcm` | — | Na Pidgin you dey read now. Tap /pidgin again if you want English. | `/pidgin` | OK | **new** — rendered in the new language |
| `ui::language_switched.en` | You're reading English now. Tap /pidgin again for Nigerian Pidgin. | — | `/pidgin` | OK | **new** |
| `ui::pidgin_advert` | — | You fit read dis for Pidgin — /pidgin | `/pidgin` | OK | **new** — shown in the *English* build so a Pidgin speaker finds it |

---

## Decisions

Rulings on the `??` and `XX` rows. Decide once, apply everywhere.

| # | Question | My recommendation | Your call |
|---|---|---|---|
| 1 | **"non-custodial"** — no settled Pidgin term. | "wey na you dey hold di key" — describes it rather than naming it. Alternatives: keep "non-custodial" as a loanword; or "na you get am, no be us". | |
| 2 | **"seed phrase"** — keep English or "your secret words"? | Keep **seed phrase**. It is the term every wallet uses and the one a user will meet elsewhere; inventing a local term makes the concept harder to carry. | |
| 3 | **"immature"** (coinbase maturity, `ui::mined#2`, `balance` label). | Keep the `<code>` label **Immature** in English (it is an aligned column) and explain it in the prose: "coin wey you just mine need 100 more block". | |
| 4 | **"stale"** (`err::QuoteExpired`, `ui::quote_expired_card`). | "don old" — natural and clear. Alternative: "no be di current one again". | |
| 5 | **"lockout"** (`err::PinLocked`). | "dem go lock you out" as a verb rather than a noun. | |
| 6 | **`status_text` values** — `pending`, `{n} conf`. Translate? | **Translate** `pending` → "e never confirm". It is a value, not an aligned label, and `/history` uses dynamic column widths so nothing breaks. Accepts a visible seam: the English header `Status` above a Pidgin cell. | |
| 7 | **Table cell `yes`** (addresses, Used column). | Keep **English**. It is inside an aligned `comfy-table` and one word either way carries no meaning a Pidgin speaker would miss. | |
| 8 | **`status` card labels** — `Tip`, `Latency`, `Calls`. | Keep **English**. Aligned, and they are jargon in both languages. | |
| 9 | **`Fast` button** → "Sharp"? | "Sharp" is natural and 5 characters. Alternative: keep "Fast". | |
| 10 | **`handlers::start.not_yet`** is already stale in English — it lists commands as if others do not work, but every command is now routed, so it is unreachable. | Fix the English while translating: make it a generic "I no sabi dat command — /help go show you wetin dey". | |

## What is NOT in this file

- Numbers, amounts and dates — formatted by `group`, `sats`, `usd`, `duration`.
- `shorten`, `escape`, `badge`, `direction_mark`, `payjoin_badge` — mechanical.
- Anything in `wallet-core`. It contains no prose by construction, enforced by
  `crates/wallet-core/tests/boundary.rs`.
- `wallet-cli` — the second front end has its own prose and is out of scope.
