use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use crate::types::{TradeDetails, TradeDirection, TokenInfo, DexType};
use crate::types::{RAYDIUM_CPMM_SWAP_BASE_INPUT, RAYDIUM_CPMM_SWAP_BASE_OUTPUT};
use chrono::Utc;
use crate::types::WSOL_MINT;
use solana_client::rpc_client::RpcClient;

pub async fn parse_raydium_cpmm_swap(
    signature: &str,
    account_keys: &[String],
    instruction_data: &[u8],
    pre_balances: &[u64],
    post_balances: &[u64],
    pre_token_balances: &[serde_json::Value],
    post_token_balances: &[serde_json::Value],
    _logs: &[String],
) -> Result<Option<TradeDetails>> {
    // 1. 校验指令类型
    if instruction_data.len() < 8 {
        return Ok(None);
    }
    let discriminator = &instruction_data[0..8];
    if discriminator != RAYDIUM_CPMM_SWAP_BASE_INPUT && discriminator != RAYDIUM_CPMM_SWAP_BASE_OUTPUT {
        return Ok(None);
    }

    // 2. 提取池子参数
    let pool_account_index = 3; // CPMM swap指令池子地址索引
    if account_keys.len() <= pool_account_index {
        return Ok(None);
    }
    let pool_pubkey = Pubkey::from_str(&account_keys[pool_account_index])?;
    let rpc = RpcClient::new("https://solana-rpc.publicnode.com/f884f7c2cfa0e7ecbf30e7da70ec1da91bda3c9d04058269397a5591e7fd013e".to_string());
    let pool_param = rpc.get_account(&pool_pubkey)?.data;
    // CPMM池子input_mint/output_mint偏移
    let input_mint = Pubkey::new_from_array(pool_param[168..200].try_into().unwrap());
    let output_mint = Pubkey::new_from_array(pool_param[200..232].try_into().unwrap());
    // 3. 分析余额变化，推断方向、币对、金额
    let (trade_direction, token_in, token_out, amount_in, amount_out) =
        analyze_token_changes_cpmm(
            pre_token_balances,
            post_token_balances,
            account_keys,
            &input_mint,
            &output_mint,
        )?;

    // 4. 组装 TradeDetails
    let trade_details = TradeDetails {
        signature: signature.to_string(),
        wallet: Pubkey::from_str(&account_keys[0])?,
        dex_type: DexType::RaydiumCPMM,
        trade_direction,
        token_in,
        token_out,
        amount_in,
        amount_out,
        price: if amount_out > 0 { amount_in as f64 / amount_out as f64 } else { 0.0 },
        pool_address: pool_pubkey,
        timestamp: Utc::now().timestamp(),
        gas_fee: calculate_gas_fee(pre_balances, post_balances)?,
        program_id: Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C")?,
    };
    Ok(Some(trade_details))
}

fn analyze_token_changes_cpmm(
    pre_token_balances: &[serde_json::Value],
    post_token_balances: &[serde_json::Value],
    account_keys: &[String],
    _input_mint: &Pubkey,
    _output_mint: &Pubkey,
) -> Result<(TradeDirection, TokenInfo, TokenInfo, u64, u64)> {
    let user_wallet = &account_keys[0];
    let mut max_in_token = None;
    let mut max_out_token = None;
    let mut max_in_amount = 0u64;
    let mut max_out_amount = 0u64;
    let mut max_in_decimals = 6u8;
    let mut max_out_decimals = 6u8;
    for (pre, post) in pre_token_balances.iter().zip(post_token_balances.iter()) {
        let mint = pre.get("mint").and_then(|m| m.as_str()).unwrap_or("");
        let owner = pre.get("owner").and_then(|o| o.as_str()).unwrap_or("");
        let decimals = pre.get("uiTokenAmount").and_then(|ui| ui.get("decimals")).and_then(|d| d.as_u64()).unwrap_or(6) as u8;
        if owner == user_wallet {
            let pre_amt = pre.get("uiTokenAmount").and_then(|ui| ui.get("amount")).and_then(|a| a.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let post_amt = post.get("uiTokenAmount").and_then(|ui| ui.get("amount")).and_then(|a| a.as_str()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            if pre_amt > post_amt {
                let diff = pre_amt - post_amt;
                if diff > max_in_amount {
                    max_in_amount = diff;
                    max_in_token = Some(mint.to_string());
                    max_in_decimals = decimals;
                }
            } else if post_amt > pre_amt {
                let diff = post_amt - pre_amt;
                if diff > max_out_amount {
                    max_out_amount = diff;
                    max_out_token = Some(mint.to_string());
                    max_out_decimals = decimals;
                }
            }
        }
    }
    let trade_direction = if max_in_token.as_ref() == Some(&WSOL_MINT.to_string()) {
        TradeDirection::Buy
    } else {
        TradeDirection::Sell
    };
    let token_in_str = max_in_token.as_ref().map(|s| s.as_str()).unwrap_or(WSOL_MINT);
    let token_out_str = max_out_token.as_ref().map(|s| s.as_str()).unwrap_or(WSOL_MINT);
    let token_in = TokenInfo {
        mint: Pubkey::from_str(token_in_str)?,
        symbol: get_token_symbol(token_in_str),
        decimals: max_in_decimals,
    };
    let token_out = TokenInfo {
        mint: Pubkey::from_str(token_out_str)?,
        symbol: get_token_symbol(token_out_str),
        decimals: max_out_decimals,
    };
    Ok((trade_direction, token_in, token_out, max_in_amount, max_out_amount))
}

fn calculate_gas_fee(pre_balances: &[u64], post_balances: &[u64]) -> Result<u64> {
    if pre_balances.is_empty() || post_balances.is_empty() {
        return Ok(0);
    }
    let pre_sol = pre_balances[0];
    let post_sol = post_balances[0];
    Ok(pre_sol.saturating_sub(post_sol))
}

fn get_token_symbol(mint: &str) -> Option<String> {
    match mint {
        "So11111111111111111111111111111111111111112" => Some("SOL".to_string()),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => Some("USDC".to_string()),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => Some("USDT".to_string()),
        _ => None,
    }
}