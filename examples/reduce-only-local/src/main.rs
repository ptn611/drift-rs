use std::{borrow::Cow, str::FromStr};

use drift_rs::{
    dlob::builder::DLOBBuilder,
    math::constants::BASE_PRECISION_I64,
    types::{
        accounts::{User, UserStats},
        solana_sdk::{
            instruction::{AccountMeta, Instruction},
            message::Message,
            pubkey::Pubkey,
            transaction::VersionedMessage,
        },
        MarketId, MarketType, OrderParams, OrderStatus, OrderType, PositionDirection,
    },
    Context, DriftClient, RpcClient, TransactionBuilder, Wallet,
};
use solana_transaction_status::TransactionConfirmationStatus;

const RPC: &str = "http://127.0.0.1:8899";
const WALLETS_FILE: &str = "/tmp/opencode/harness_wallets.json";
const PYTH_PROGRAM_ID: &str = "AeMXb5H8Cfv2cdDzPCSmDX9ydchB4N559B19ahgyx1fW";

const SOL_MARKET_INDEX: u16 = 0;
const USDC_SPOT_MARKET_INDEX: u16 = 0;
/// contract price precision (1e10): oracle 100.00 == 100 * PRICE_PRECISION
const PRICE_PRECISION: u64 = 10_000_000_000;
const SOL: u64 = BASE_PRECISION_I64 as u64; // 1e9 base units
const DEPOSIT_USDC: u64 = 10_000 * 1_000_000; // 10k USDC, 6dp raw
const P99: u64 = 99 * PRICE_PRECISION;
const P101: u64 = 101 * PRICE_PRECISION;

async fn new_client(name: &str, key: &str) -> DriftClient {
    let wallet = Wallet::try_from_str(key).unwrap_or_else(|e| panic!("{name} wallet: {e:?}"));
    DriftClient::new(Context::MainNet, RpcClient::new(RPC.to_string()), wallet)
        .await
        .unwrap_or_else(|e| panic!("{name} client: {e:?}"))
}

async fn get_user(client: &DriftClient) -> User {
    let sub = client.wallet().default_sub_account();
    for _ in 0..100 {
        match client.get_user_account(&sub).await {
            Ok(u) => return u,
            Err(e) => eprintln!("  [get_user] {sub}: {e:?}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("user account {sub} never appeared")
}

async fn sign_and_send(client: &DriftClient, tx: VersionedMessage, label: &str) {
    match client.sign_and_send(tx).await {
        Ok(sig) => {
            println!("  [{label}] sig={sig}");
            // localnet: wait for finality before reading state (finalized lags after --reset)
            let rpc = client.rpc();
            loop {
                match rpc.get_signature_statuses(&[sig]).await {
                    Ok(statuses) => {
                        if let Some(Some(s)) = statuses.value.first() {
                            match &s.status {
                                Ok(()) => {
                                    if s.confirmation_status
                                        == Some(TransactionConfirmationStatus::Finalized)
                                    {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    println!("  [{label}] tx FAILED: {e:?}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("  [{label}] status check error: {e:?}");
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        Err(e) => println!("  [{label}] ERROR: {e:?}"),
    }
}

async fn ensure_user_and_deposit(client: &DriftClient) {
    let sub = client.wallet().default_sub_account();
    let owner = client.wallet().authority();
    let existing = client.get_user_account(&sub).await;
    let user = match existing {
        Ok(u) => {
            println!("  {sub}: user exists");
            u
        }
        Err(_) => {
            println!("  {sub}: initializing user");
            let mut u = User::default();
            u.authority = *owner;
            let tx = TransactionBuilder::new(client.program_data(), sub, Cow::Owned(u), false)
                .initialize_user_account(0, None, None)
                .build();
            sign_and_send(client, tx, "initialize_user_account").await;
            get_user(client).await
        }
    };
    if user.total_deposits == 0 {
        println!("  {sub}: depositing {DEPOSIT_USDC} USDC");
        let tx = TransactionBuilder::new(client.program_data(), sub, Cow::Borrowed(&user), false)
            .deposit(DEPOSIT_USDC, USDC_SPOT_MARKET_INDEX, None, None)
            .build();
        println!("    deposit accounts:");
        match &tx {
            VersionedMessage::Legacy(m) => {
                for (i, k) in m.account_keys.iter().enumerate() {
                    println!("      [{i}] {k}");
                }
            }
            VersionedMessage::V0(m) => {
                let mut keys: Vec<Pubkey> = m.account_keys.iter().copied().collect();
                for lookup in m.address_table_lookups.iter() {
                    keys.extend(
                        lookup
                            .writable_indexes
                            .iter()
                            .map(|idx| lookup.account_key),
                    );
                    keys.extend(
                        lookup
                            .readonly_indexes
                            .iter()
                            .map(|idx| lookup.account_key),
                    );
                }
                for (i, k) in keys.iter().enumerate() {
                    println!("      [{i}] {k}");
                }
            }
        }
        sign_and_send(client, tx, "deposit").await;
    } else {
        println!("  {sub}: already has {total} deposits", total = user.total_deposits);
    }
}

/// mock pyth: set_price(price) is permissionless; mock layout mirrors pyth-client 0.2.2
/// (expo @ 0x14, agg.price @ 0xd0). Keeps the feed "alive" before each scenario.
async fn refresh_oracle(client: &DriftClient) {
    let market = client
        .get_perp_market_account_and_slot(SOL_MARKET_INDEX)
        .await
        .expect("perp market")
        .data;
    let feed = market.amm.oracle;
    let account = client
        .rpc()
        .get_account(&feed)
        .await
        .expect("feed account");
    let expo = i32::from_le_bytes(account.data[0x14..0x18].try_into().unwrap());
    let raw_price = (100.0 * 10f64.powi(-expo)) as i64;
    let mut data = vec![16u8, 19, 182, 8, 149, 83, 72, 181];
    data.extend_from_slice(&raw_price.to_le_bytes());
    let ix = Instruction {
        program_id: Pubkey::from_str(PYTH_PROGRAM_ID).unwrap(),
        accounts: vec![AccountMeta::new(feed, false)],
        data,
    };
    let payer = client.wallet().authority();
    let tx = VersionedMessage::Legacy(Message::new(&[ix], Some(&payer)));
    println!(
        "  refresh oracle feed={feed} expo={expo} raw={raw_price} (normalised ~100.00)"
    );
    sign_and_send(client, tx, "pyth.set_price").await;
}

async fn place_order(client: &DriftClient, params: OrderParams, label: &str) {
    let sub = client.wallet().default_sub_account();
    let tx = client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_orders(vec![params])
        .build();
    sign_and_send(client, tx, label).await;
}

async fn place_and_take(client: &DriftClient, params: OrderParams, label: &str) {
    let sub = client.wallet().default_sub_account();
    let tx = client
        .init_tx(&sub, false)
        .await
        .unwrap()
        .place_and_take(params, &[], None, None, None)
        .build();
    sign_and_send(client, tx, label).await;
}

fn print_user(label: &str, user: &User) {
    println!("  [{label}] authority={}", user.authority);
    if let Some(pos) = user.perp_positions.iter().find(|p| p.market_index == SOL_MARKET_INDEX) {
        println!(
            "    perp pos: base={} quote={} open_bids={} open_asks={}",
            pos.base_asset_amount, pos.quote_asset_amount, pos.open_bids, pos.open_asks
        );
    } else {
        println!("    perp pos: none");
    }
    for o in user.orders.iter() {
        if o.market_index == SOL_MARKET_INDEX && o.status != OrderStatus::Init {
            println!(
                "    order id={} {:?} {:?} status={:?} reduce_only={} amount={} filled={} price={}",
                o.order_id,
                o.order_type,
                o.direction,
                o.status,
                o.reduce_only,
                o.base_asset_amount,
                o.base_asset_amount_filled,
                o.price
            );
        }
    }
}

/// keeper-side DLOB snapshot + cross scan (no tx sent)
async fn keeper_scan(keeper: &DriftClient, users: &[User], label: &str) {
    let slot = keeper.rpc().get_slot().await.unwrap();
    let market = keeper
        .get_perp_market_account_and_slot(SOL_MARKET_INDEX)
        .await
        .unwrap()
        .data;
    let oracle = market.amm.historical_oracle_data.last_oracle_price as u64;

    let dlob_builder = DLOBBuilder::new_with_users(users.iter(), slot);
    dlob_builder.update_slot_and_oracle(
        MarketId::new(SOL_MARKET_INDEX, MarketType::Perp),
        slot,
        oracle,
    );
    let dlob = dlob_builder.dlob();

    println!(
        "  [{label}] slot={slot} oracle={oracle} (={} raw) max_fill_frac={} max_slippage={}",
        oracle as f64 / PRICE_PRECISION as f64,
        market.amm.max_fill_reserve_fraction,
        market.amm.max_slippage_ratio
    );
    let book = {
        let mut book = None;
        for _ in 0..100 {
            if let Some(b) = dlob.get_l3_snapshot_safe(SOL_MARKET_INDEX, MarketType::Perp) {
                if b.bids(Some(oracle), Some(&market), None).next().is_some()
                    || b.asks(Some(oracle), Some(&market), None).next().is_some()
                {
                    book = Some(b);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        match book {
            Some(book) => book,
            None => {
                println!("    (no orderbook entries for perp market {SOL_MARKET_INDEX})");
                return;
            }
        }
    };
    println!("    asks:");
    for o in book.asks(Some(oracle), Some(&market), None) {
        println!(
            "      user={} price={} size={} reduce_only={} post_only={} is_taker={}",
            o.user,
            o.price,
            o.size,
            o.is_reduce_only(),
            o.is_post_only(),
            o.is_taker()
        );
    }
    println!("    bids:");
    for o in book.bids(Some(oracle), Some(&market), None) {
        println!(
            "      user={} price={} size={} reduce_only={} post_only={} is_taker={}",
            o.user,
            o.price,
            o.size,
            o.is_reduce_only(),
            o.is_post_only(),
            o.is_taker()
        );
    }

    let crosses = dlob.find_crosses_for_auctions(
        SOL_MARKET_INDEX,
        MarketType::Perp,
        slot,
        oracle,
        Some(&market),
        oracle,
        None,
    );
    println!(
        "    crosses: {} taker crossings; top_maker_asks={:?} top_maker_bids={:?}",
        crosses.crosses.len(), crosses.top_maker_asks, crosses.top_maker_bids
    );
    for (taker, makers) in crosses.crosses.iter() {
        println!(
            "      TAKER user={} size={} price={} dir={:?} vamm_cross={} partial={}",
            taker.user,
            taker.size,
            taker.price,
            makers.taker_direction,
            makers.has_vamm_cross,
            makers.is_partial
        );
        for (maker, fill) in makers.orders.iter() {
            println!(
                "        MAKER user={} price={} size={} reduce_only={} fill={}",
                maker.user,
                maker.price,
                maker.size,
                maker.is_reduce_only(),
                fill
            );
        }
    }
}

async fn keeper_fill(
    keeper: &DriftClient,
    taker_sub: &Pubkey,
    taker: &User,
    taker_stats: &UserStats,
    taker_order_id: Option<u32>,
    makers: &[User],
    label: &str,
) {
    let filler_sub = keeper.wallet().default_sub_account();
    let tx = keeper
        .init_tx(&filler_sub, false)
        .await
        .unwrap()
        .fill_perp_order(
            SOL_MARKET_INDEX,
            *taker_sub,
            taker,
            taker_stats,
            taker_order_id,
            makers,
            None,
        )
        .build();
    match keeper.simulate_tx(tx.clone()).await {
        Ok(res) => {
            println!("  [{label}] simulate err={:?}", res.err);
            if let Some(logs) = &res.logs {
                let mut shown = 0;
                for l in logs.iter() {
                    if l.contains("reduce")
                        || l.contains("Order")
                        || l.contains("Keeper")
                        || l.contains("Fill")
                        || l.contains("Error")
                        || l.contains("panicked")
                    {
                        println!("      {l}");
                        shown += 1;
                        if shown > 20 {
                            break;
                        }
                    }
                }
            }
            sign_and_send(keeper, tx, label).await;
        }
        Err(e) => println!("  [{label}] simulate error: {e:?}"),
    }
}

fn limit_order(direction: PositionDirection, amount: u64, price: u64, reduce_only: bool) -> OrderParams {
    OrderParams {
        order_type: OrderType::Limit,
        market_type: MarketType::Perp,
        direction,
        base_asset_amount: amount,
        price,
        market_index: SOL_MARKET_INDEX,
        reduce_only,
        ..Default::default()
    }
}

fn market_order(direction: PositionDirection, amount: u64) -> OrderParams {
    OrderParams {
        order_type: OrderType::Market,
        market_type: MarketType::Perp,
        direction,
        base_asset_amount: amount,
        market_index: SOL_MARKET_INDEX,
        ..Default::default()
    }
}

#[tokio::main]
async fn main() {
    let _ = env_logger::try_init();

    let wallets: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(WALLETS_FILE).unwrap()).unwrap();
    let key = |name: &str| wallets[name].as_str().unwrap().to_string();

    let trader_a = new_client("traderA", &key("traderA")).await;
    let trader_b = new_client("traderB", &key("traderB")).await;
    let filler = new_client("filler", &key("filler")).await;

    let _sub_a = trader_a.wallet().default_sub_account();
    let sub_b = trader_b.wallet().default_sub_account();
    let _sub_f = filler.wallet().default_sub_account();

    println!("=== setup: users + deposits ===");
    ensure_user_and_deposit(&trader_a).await;
    ensure_user_and_deposit(&trader_b).await;
    ensure_user_and_deposit(&filler).await;
    refresh_oracle(&trader_a).await;

    // ---- Scenario A: maker reduce-only with NO position - rests on book (SDK allows it) ----
    println!("\n=== Scenario A: traderA (no position) places REDUCE_ONLY limit SELL 5 @ 99 ===");
    place_order(&trader_a, limit_order(PositionDirection::Short, 5 * SOL, P99, true), "A: place RO SELL 5 @99").await;
    let user_a = get_user(&trader_a).await;
    print_user("traderA after A", &user_a);

    // ---- Scenario B: open a position via AMM ----
    println!("\n=== Scenario B: traderB opens long +5 (market BUY vs AMM) ===");
    refresh_oracle(&trader_b).await;
    place_and_take(&trader_b, market_order(PositionDirection::Long, 5 * SOL), "B: market BUY 5").await;
    let user_b = get_user(&trader_b).await;
    print_user("traderB after B", &user_b);

    // ---- Scenario C: maker reduce-only WITH position - rests on book ----
    println!("\n=== Scenario C: traderB (long +5) places REDUCE_ONLY limit SELL 3 @ 101 ===");
    place_order(&trader_b, limit_order(PositionDirection::Short, 3 * SOL, P101, true), "C: place RO SELL 3 @101").await;
    print_user("traderB after C", &get_user(&trader_b).await);

    // ---- Scenario D: taker reduce-only with NO position - cancelled at fill (no position flip) ----
    println!("\n=== Scenario D: traderA (no position) REDUCE_ONLY market SELL 3 via place_and_take ===");
    refresh_oracle(&trader_a).await;
    let mut ro_sell = market_order(PositionDirection::Short, 3 * SOL);
    ro_sell.reduce_only = true;
    place_and_take(&trader_a, ro_sell, "D: RO market SELL 3").await;
    print_user("traderA after D", &get_user(&trader_a).await);

    // ---- Scenario E: pending taker + keeper fill against the ghost reduce-only maker ----
    println!("\n=== Scenario E: traderB pending market BUY 3; keeper scans + fills (maker = traderA RO SELL) ===");
    refresh_oracle(&trader_b).await;
    place_order(&trader_b, market_order(PositionDirection::Long, 3 * SOL), "E: place pending market BUY 3").await;
    let user_b = get_user(&trader_b).await;
    print_user("traderB before keeper fill", &user_b);

    let user_a = get_user(&trader_a).await;
    let user_f = get_user(&filler).await;
    keeper_scan(&filler, &[user_a, user_b.clone(), user_f], "E: DLOB scan").await;

    let taker_order_id = user_b
        .orders
        .iter()
        .find(|o| {
            o.market_index == SOL_MARKET_INDEX
                && o.order_type == OrderType::Market
                && o.direction == PositionDirection::Long
                && o.status == OrderStatus::Open
        })
        .map(|o| o.order_id);
    println!("  taker order id = {taker_order_id:?}");

    let stats_b = filler
        .get_user_stats(&trader_b.wallet().authority())
        .await
        .unwrap();
    let user_a = get_user(&trader_a).await;
    keeper_fill(&filler, &sub_b, &user_b, &stats_b, taker_order_id, &[user_a], "E: keeper fill").await;

    println!("\n=== final state ===");
    print_user("traderA", &get_user(&trader_a).await);
    print_user("traderB", &get_user(&trader_b).await);
    print_user("filler", &get_user(&filler).await);
    println!("\nDONE");
}
