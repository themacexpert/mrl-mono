use std::{collections::HashMap, future::Future, vec};

use ethers_core::types::{Chain, H160, U256, U64};
use ethers_etherscan::{
    account::{Sort, TokenQueryOption, TxListParams},
    Client,
};
use serde::{Deserialize, Serialize};
use worker::{
    console_error, console_log, event, query, Date, DateInit, Env, Request, Response, Result,
    Router, ScheduleContext, ScheduledEvent,
};

mod twelve_data;
use twelve_data::get_twelve_data;

use crate::twelve_data::TimeSeries;

#[derive(Serialize)]
struct Liquidity {
    token_name: String,
    token_symbol: String,
    contract_address: String,
    tokens: u128,
    usd: f32,
    number_of_transfers: u32,
}

#[derive(Deserialize, Serialize)]
struct Token {
    contract_addr: String,
    token_name: String,
    token_sym: String,
    decimals: u32,
}

impl Default for Token {
    fn default() -> Self {
        Self { contract_addr: Default::default(), token_name: Default::default(), token_sym: Default::default(), decimals: 18 }
    }
}

#[derive(Deserialize, Serialize)]
struct TransferForward {
    tx_hash: String,
    token_addr: String,
    token_count: u128,
    usd: f32,
    block_num: u64,
    timestamp: String,
    to_chain: u32,
}

#[event(fetch)]
pub async fn fetch(req: Request, _env: Env, _ctx: worker::Context) -> Result<Response> {
    let router = Router::new();

    router
        .get_async("/totalLiquidityForward", |_req: Request, _ctx| async move {
            let res: Liquidity = Liquidity {
                token_name: "Bitcoin".to_string(),
                token_symbol: "WBTC".to_string(),
                contract_address: "0x000".to_string(),
                tokens: 100,
                usd: 1000000.25,
                number_of_transfers: 30,
            };

            Response::from_json(&res)
        })
        .get_async("/testaroonie", |_req, _ctx| async move {
            let timestamp = "2023-06-28 18:00:00";
            let date = Date::from(DateInit::String(timestamp.to_owned()));

            Response::from_json(&(date.as_millis(), date.to_string()))
        })
        .get_async("/getTokens", |_req, ctx| async move {
            let d1 = ctx.env.d1("DB")?;
            let statement = worker::query!(&d1, "SELECT * FROM Token");
            let result = statement.all().await?;

            if !result.success() {
                return Response::error(
                    result.error().unwrap_or("No error given".to_string()),
                    500,
                );
            }

            let x = result.results::<Token>()?;
            Response::from_json(&x)
        })
        .post_async("/reset", |_req, ctx| async move {
            let d1 = ctx.env.d1("DB")?;
            let statements = vec![
                query!(&d1, "DROP TABLE IF EXISTS TransfersForward"),
                query!(&d1, "DROP TABLE IF EXISTS Token"),
            ];
            let result = d1.batch(statements).await?;

            let mut message: String = "".to_owned();
            for r in result {
                if r.success() {
                    message += "Success; ";
                } else {
                    message += &(r.error().unwrap_or("No error given".to_string()) + "; ");
                }
            }

            let statements = vec![
                d1.prepare(
                    "
                    CREATE TABLE IF NOT EXISTS Token (
                        contract_addr TEXT NOT NULL PRIMARY KEY,
                        token_name TEXT NOT NULL,
                        token_sym TEXT NOT NULL,
                        decimals UNSIGNED INT NOT NULL
                    );
                ",
                ),
                d1.prepare(
                    "
                    CREATE TABLE IF NOT EXISTS TransfersForward (
                        tx_hash TEXT PRIMARY KEY,
                        token_addr TEXT NOT NULL REFERENCES Token(contract_addr),
                        token_count UNSIGNED INT NOT NULL,
                        usd REAL NOT NULL,
                        block_num UNSIGNED INT NOT NULL,
                        timestamp TEXT,
                        to_chain UNSIGNED INT NOT NULL
                    );
                ",
                ),
            ];

            let result = d1.batch(statements).await?;
            for r in result {
                if r.success() {
                    message += "Success; ";
                } else {
                    message += &(r.error().unwrap_or("No error given".to_string()) + "; ");
                }
            }

            Response::ok(message)
        })
        .run(req, _env)
        .await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, _env: Env, _ctx: ScheduleContext) {
    console_log!("Beginning CRON scheduler event.");
    let Ok(db) = _env.d1("DB") else {
        println!("Error occurred with getting the DB during a scheduled event!");
        return
    };

    // 1. Get the last entry so that we know when to query from.
    let statement = db.prepare("SELECT MAX(block_num) AS most_recent_block FROM TransfersForward");
    let result = statement.first::<u64>(Some("most_recent_block")).await;
    let block: u64 = match result {
        Err(e) => {
            console_error!("Error with most_recent_block: {:?}", e);
            4164120
        }
        Ok(Some(r)) => r,
        Ok(None) => 4164120,
    };

    // 2. Query etherscan
    let Ok(moonscan_key) = _env.var("MOONSCAN_KEY") else {
        console_error!("Error discovering MoonScan API key!");
        return
    };
    let Ok(client) = Client::new(Chain::Moonbeam, moonscan_key.to_string()) else {
        console_error!("Error occurred with creating an etherscan client!");
        return
    };
    let Ok(gmp_precompile) = "0x0000000000000000000000000000000000000816".parse() else {
        console_error!("Error occurred when parsing GMP precompile address!");
        return
    };
    let etherscan_result = client
        .get_erc20_token_transfer_events(
            TokenQueryOption::ByAddress(gmp_precompile),
            // None
            Some(TxListParams::new(block + 1, 999999999, 0, 0, Sort::Asc)),
        )
        .await;
    let Ok(etherscan_result) = etherscan_result else {
        console_error!("Error occurred when querying etherscan!");
        return
    };
    console_log!("No transactions discovered after block {}.", block);
    if etherscan_result.len() == 0 {
        return;
    }

    // 3. Sort & format data (lowest timestamp are first)
    let mut filtered_etherscan_data: Vec<TransferForward> = etherscan_result
        .iter()
        .filter_map(|e| {
            if e.from == H160::default() {
                Some(TransferForward {
                    tx_hash: format!("{:?}", e.hash),
                    token_addr: format!("{:?}", e.contract_address),
                    token_count: e.value.as_u128(), // Possibility of panicking if MRL allows for custom tokens with super high values
                    usd: 0.,                        // TODO: query for USD value at the timestamp
                    block_num: e.block_number.as_number().unwrap_or(U64::from(0)).as_u64(),
                    timestamp: e.time_stamp.to_owned(),
                    to_chain: 1000, // TODO: parse the transaction data
                })
            } else {
                None
            }
        })
        .collect();
    console_log!(
        "First etherscan data timestamp: {}",
        filtered_etherscan_data.first().unwrap().timestamp
    );
    console_log!(
        "Last etherscan data timestamp: {}",
        filtered_etherscan_data.last().unwrap().timestamp
    );

    // 4. Ensure all of the tokens are already known
    let token_hash: HashMap<String, Token> = etherscan_result
        .iter()
        .filter_map(|e: &ethers_etherscan::account::ERC20TokenTransferEvent| {
            if e.from == H160::default() {
                let addr = format!("{:?}", e.contract_address);
                Some((
                    addr.clone(),
                    Token {
                        contract_addr: addr,
                        token_name: e.token_name.clone(),
                        token_sym: e.token_symbol.clone(),
                        decimals: e.token_decimal.parse::<u32>().unwrap_or(18),
                    },
                ))
            } else {
                None
            }
        })
        .collect::<HashMap<String, Token>>();
    let token_statement: String = token_hash
        .iter()
        .map(|e| {
            let token = e.1;
            format!(
                "('{}', '{}', '{}', {})",
                token.contract_addr, token.token_name, token.token_sym, token.decimals
            )
        })
        .collect::<Vec<String>>()
        .join(", ");
    let token_statement: String =
        "INSERT OR IGNORE INTO Token (contract_addr, token_name, token_sym, decimals) VALUES "
            .to_string()
            + &token_statement;
    let token_res = db.prepare(token_statement).run().await;
    match token_res {
        Ok(r) => {
            if !r.success() {
                console_error!(
                    "Internal error when inserting Tokens into DB: {:?}",
                    r.error()
                );
            }
        }
        Err(e) => {
            console_error!("Error when inserting tokens: {}", e);
            return;
        }
    };

    // 4. Query for historical prices
    let Ok(twelve_key) = _env.var("TWELVE_DATA_KEY") else {
        console_error!("Error discovering Twelve Data API key!");
        return
    };
    // TODO: query for multiple data points, not just ETH :)
    let Ok(twelve_data) = get_twelve_data(twelve_key.to_string(), "ETH".to_string()).await else {
        console_error!("Error fetching Twelve Data!");
        return
    };
    console_log!(
        "First time series data timestamp: {}",
        twelve_data
            .first()
            .unwrap_or(&TimeSeries::default())
            .timestamp
    );
    console_log!(
        "Last time series data timestamp: {}",
        twelve_data
            .last()
            .unwrap_or(&TimeSeries::default())
            .timestamp
    );
    let mut twelve_index = 0;
    for tx in &mut filtered_etherscan_data {
        let token_decimals = token_hash.get(&tx.token_addr).unwrap_or(&Token::default()).decimals;

        // Skips if it's a USD stablecoin
        if is_usd_stablecoin(&token_hash, &tx.token_addr) {
            tx.usd = calculate_usd(1., tx.token_count, token_decimals);
            continue;
        }

        // TODO: implement BTC also
        if token_hash.get(&tx.token_addr).unwrap_or(&Token::default()).token_sym.contains("BTC") {
            continue;
        }
        
        // Otherwise, we get the TimeSeries that is closest to the TransferForward index
        // TODO: differentiate between ETH and BTC
        let ts: &TimeSeries = loop {
            // Ensures that we are always returning at least the most recent value
            if twelve_index >= twelve_data.len() {
                break twelve_data.last().unwrap();
            }

            // Current timeseries
            let cur = &twelve_data[twelve_index];
            let nxt = twelve_data.get(twelve_index + 1).unwrap_or(&cur);
            let tx_timestamp = tx.timestamp.parse().unwrap_or(0);

            // If the current is closer to the tx timestamp, use current.
            if cur.timestamp.abs_diff(tx_timestamp) <= nxt.timestamp.abs_diff(tx_timestamp) {
                break cur;
            }

            // Otherwise, we move on
            twelve_index += 1;
        };

        tx.usd = calculate_usd(ts.estimate(), tx.token_count, token_decimals);
    }

    // Prepare statement(s) to insert data
    let base_statement = "INSERT INTO TransfersForward (tx_hash, token_addr, token_count, usd, block_num, timestamp, to_chain) VALUES ".to_string();
    let statements: Vec<worker::D1PreparedStatement> = filtered_etherscan_data
        .chunks(250)
        .map(|chunk| {
            let values: Vec<String> = chunk
                .iter()
                .map(|transfer| {
                    format!(
                        "('{}', '{}', {}, {}, {}, '{}', {})",
                        transfer.tx_hash,
                        transfer.token_addr,
                        transfer.token_count,
                        transfer.usd,
                        transfer.block_num,
                        transfer.timestamp,
                        transfer.to_chain
                    )
                })
                .collect::<Vec<String>>();
            let statement = format!("{}{}", base_statement, values.join(", "));
            db.prepare(statement)
        })
        .collect();

    // Insert into database
    let db_res = db.batch(statements).await;
    match db_res {
        Ok(res) => {
            for r in res {
                if !r.success() {
                    console_error!(
                        "Internal error when inserting new TransferForward txs into DB: {:?}",
                        r.error()
                    );
                }
            }
        }
        Err(e) => {
            console_error!("Error when batching statements: {}", e);
        }
    }
    console_log!(
        "Successfully inserted {} transactions into the TransferForward table.",
        filtered_etherscan_data.len()
    );
}

fn is_usd_stablecoin(token_hash: &HashMap<String, Token>, token_addr: &String) -> bool {
    let sym = token_hash.get(token_addr).unwrap_or(&Token::default()).token_sym.clone();
    sym.contains("USDT") || sym.contains("USDC") || sym.contains("DAI")
}

fn calculate_usd(exchange_rate: f32, token_count: u128, token_decimals: u32) -> f32 {
    if token_decimals >= 6 {
        let six_sig_figs = (token_count / 10_u128.pow(token_decimals - 6)) as f32;
        exchange_rate * six_sig_figs / 10_f32.powf(6.)
    }
    else {
        exchange_rate * (token_count / 10_u128.pow(token_decimals)) as f32
    }
}