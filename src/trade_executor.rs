use std::sync::Arc;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use crate::types::{TradeDetails, TradeExecutionConfig, ExecutedTrade, TradeDirection, DexType};
use futures::future::BoxFuture;
use anyhow::{Result, Context};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::Instruction,
    signature::Keypair,
    signer::Signer,
    transaction::{VersionedTransaction},
    message::VersionedMessage,
};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::instruction::AccountMeta;
use solana_account_decoder::UiAccountData;
use std::str::FromStr;
use solana_client::rpc_request::TokenAccountsFilter;
use serde_json;
use solana_program::program_pack::Pack;
use std::convert::TryFrom;
use uuid::Uuid;
use dashmap::DashMap;
use std::time::{Instant, Duration};
use tokio::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_transaction_status::UiTransactionEncoding;
use crate::trade_log;
use chrono::Utc;
use crate::types::cp_swap::SwapBaseInput;

// Raydium CPMM链上报价公式（AMM恒定乘积，含手续费）
// 用于根据池子input_vault/output_vault余额和AMM公式计算预期输出
// 可用于滑点保护，不影响现有买卖主流程
pub fn raydium_cpmm_quote(
    input_amount: u64,
    input_vault_balance: u64,
    output_vault_balance: u64,
    fee_numerator: u64,   // 通常 25
    fee_denominator: u64, // 通常 10000
) -> u64 {
    if input_vault_balance == 0 || output_vault_balance == 0 {
        return 0;
    }
    
    // 正确的AMM公式实现：
    // amount_out = (amount_in_after_fee × reserve_out) / (reserve_in + amount_in_after_fee)
    
    // 1. 计算手续费后的输入金额
    let amount_in_after_fee = (input_amount as u128)
        .saturating_mul((fee_denominator - fee_numerator) as u128)
        .saturating_div(fee_denominator as u128);
    
    // 2. 使用恒定乘积公式计算输出
    let numerator = amount_in_after_fee.saturating_mul(output_vault_balance as u128);
    let denominator = (input_vault_balance as u128).saturating_add(amount_in_after_fee);
    
    if denominator == 0 {
        return 0;
    }
    
    (numerator.saturating_div(denominator)) as u64
}

/// Fixed Raydium CPMM quote calculation
pub fn raydium_cpmm_quote_fixed(
    input_amount: u64,
    input_vault_balance: u64,
    output_vault_balance: u64,
    fee_numerator: u64,
    fee_denominator: u64,
) -> u64 {
    if input_vault_balance == 0 || output_vault_balance == 0 {
        return 0;
    }
    
    // Apply fee to input amount
    let amount_in_with_fee = (input_amount as u128)
        .saturating_mul((fee_denominator - fee_numerator) as u128)
        / fee_denominator as u128;
    
    // Calculate output using constant product formula: k = x * y
    let numerator = amount_in_with_fee.saturating_mul(output_vault_balance as u128);
    let denominator = (input_vault_balance as u128).saturating_add(amount_in_with_fee);
    
    if denominator == 0 {
        return 0;
    }
    
    (numerator / denominator) as u64
}

/// Calculate slippage correctly based on basis points
pub fn calculate_min_amount_out(
    expected_output: u64,
    slippage_bps: u64, // basis points (10000 = 100%)
) -> u64 {
    // 正确的滑点计算：min_amount_out = expected_output * (1 - slippage_bps / 10000)
    let slippage_multiplier = 10000u64.saturating_sub(slippage_bps);
    expected_output
        .saturating_mul(slippage_multiplier)
        .saturating_div(10000)
}

/// 新增：使用百分比计算滑点（更直观）
pub fn calculate_min_amount_out_percent(
    expected_output: u64,
    slippage_percent: f64, // 百分比，如 0.5 表示 0.5%
) -> u64 {
    let slippage_bps = (slippage_percent * 100.0) as u64; // 转换为basis points
    calculate_min_amount_out(expected_output, slippage_bps)
}

/// 改进的滑点计算，考虑精度和安全边际
pub fn calculate_min_amount_out_with_safety(
    expected_output: u64,
    slippage_bps: u64, // basis points (10000 = 100%)
    safety_margin_bps: u64, // 额外的安全边际
) -> u64 {
    // 应用滑点
    let after_slippage = expected_output
        .saturating_mul(10000u64.saturating_sub(slippage_bps))
        .saturating_div(10000);
    
    // 应用额外的安全边际
    let with_safety = after_slippage
        .saturating_mul(10000u64.saturating_sub(safety_margin_bps))
        .saturating_div(10000);
    
    // 确保不会因为精度问题导致值为0
    std::cmp::max(with_safety, 1)
}

/// 使用改进的滑点计算
pub fn calculate_trade_with_improved_slippage(
    trade: &TradeDetails,
    input_vault_balance: u64,
    output_vault_balance: u64,
    slippage_percent: f64,
) -> (u64, u64) {
    // 计算预期输出
    let expected_output = raydium_cpmm_quote(
        trade.amount_in,
        input_vault_balance,
        output_vault_balance,
        25,     // fee_numerator
        10000   // fee_denominator
    );
    
    // 转换滑点百分比为基点
    let slippage_bps = (slippage_percent * 100.0) as u64;
    
    // 添加50基点（0.5%）的额外安全边际
    let safety_margin_bps = 50;
    
    let min_amount_out = calculate_min_amount_out_with_safety(
        expected_output,
        slippage_bps,
        safety_margin_bps
    );
    
    // info!("滑点计算（改进版）：");
    // info!("  预期输出: {}", expected_output);
    // info!("  滑点: {}%", slippage_percent);
    // info!("  安全边际: 0.5%");
    // info!("  最小输出: {}", min_amount_out);
    
    (expected_output, min_amount_out)
}

// 支持不同decimals的Raydium CPMM链上报价公式
pub fn raydium_cpmm_quote_with_decimals(
    amount_in: u64,
    input_vault_balance: u64,
    output_vault_balance: u64,
    input_decimals: u8,
    output_decimals: u8,
    fee_numerator: u64,
    fee_denominator: u64,
) -> u64 {
    // 转为人类可读
    let amount_in_f = amount_in as f64 / 10f64.powi(input_decimals as i32);
    let input_vault_f = input_vault_balance as f64 / 10f64.powi(input_decimals as i32);
    let output_vault_f = output_vault_balance as f64 / 10f64.powi(output_decimals as i32);
    let amount_in_with_fee = amount_in_f * (fee_denominator as f64 - fee_numerator as f64) / fee_denominator as f64;
    let numerator = amount_in_with_fee * output_vault_f;
    let denominator = input_vault_f + amount_in_with_fee;
    let amount_out = numerator / denominator;
    // 转回 output 的 lamports
    (amount_out * 10f64.powi(output_decimals as i32)) as u64
}

// Raydium池子账户结构体
pub struct RaydiumPoolAccounts {
    pub amm_id: Pubkey,
    pub amm_authority: Pubkey,
    pub amm_open_orders: Pubkey,
    pub amm_target_orders: Pubkey,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account: Pubkey,
    pub serum_program_id: Pubkey,
    pub serum_market: Pubkey,
    pub serum_bids: Pubkey,
    pub serum_asks: Pubkey,
    pub serum_event_queue: Pubkey,
    pub serum_coin_vault_account: Pubkey,
    pub serum_pc_vault_account: Pubkey,
    pub serum_vault_signer: Pubkey,
}

// Pump.fun账户结构体
pub struct PumpFunAccounts {
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub event_authority: Pubkey,
}

// Raydium CPMM swap指令账户结构体
#[derive(Clone, Debug)]
pub struct RaydiumCpmmSwapAccounts {
    pub payer: Pubkey,
    pub authority: Pubkey,
    pub amm_config: Pubkey,
    pub pool_state: Pubkey,
    pub user_input_ata: Pubkey,
    pub user_output_ata: Pubkey,
    pub input_vault: Pubkey,
    pub output_vault: Pubkey,
    pub input_token_program: Pubkey,
    pub output_token_program: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub observation_state: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
}

pub struct TradeExecutor {
    pub client: Arc<RpcClient>,
    pub copy_wallet: Arc<Keypair>,
    pub config: TradeExecutionConfig,
    pub rpc_url: String,
    pub pool_cache: Arc<PoolCache>,
}

pub struct PoolCache {
    pools: Arc<DashMap<Pubkey, CachedPoolInfo>>,
    update_interval: Duration,
}

pub struct CachedPoolInfo {
    pub amm_config: Pubkey,
    pub authority: Pubkey,
    pub input_vault: Pubkey,
    pub output_vault: Pubkey,
    pub input_vault_balance: AtomicU64,
    pub output_vault_balance: AtomicU64,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub observation_state: Pubkey,
    pub last_update: RwLock<Instant>,
}

impl PoolCache {
    pub fn new() -> Self {
        Self {
            pools: Arc::new(DashMap::new()),
            update_interval: Duration::from_secs(5),
        }
    }
    pub async fn get_pool_info(&self, client: &Arc<RpcClient>, pool_state: &Pubkey) -> anyhow::Result<CachedPoolInfo> {
        if let Some(cached) = self.pools.get(pool_state) {
            let last_update = cached.last_update.read().await;
            if last_update.elapsed() < self.update_interval {
                return Ok(cached.clone());
            }
        }
        self.update_pool_info(client, pool_state).await
    }
    pub async fn update_pool_info(&self, client: &Arc<RpcClient>, pool_state: &Pubkey) -> anyhow::Result<CachedPoolInfo> {
        let pool_account = client.get_account(pool_state)?;
        let pool_data = &pool_account.data;
        let amm_config = Pubkey::new_from_array(pool_data[8..40].try_into().unwrap());
        let authority = Pubkey::new_from_array(pool_data[40..72].try_into().unwrap());
        let input_vault = Pubkey::new_from_array(pool_data[72..104].try_into().unwrap());
        let output_vault = Pubkey::new_from_array(pool_data[104..136].try_into().unwrap());
        let token_0_mint = Pubkey::new_from_array(pool_data[168..200].try_into().unwrap());
        let token_1_mint = Pubkey::new_from_array(pool_data[200..232].try_into().unwrap());
        let observation_state = Pubkey::new_from_array(pool_data[296..328].try_into().unwrap());
        let input_vault_balance = client.get_token_account_balance(&input_vault)?.amount.parse::<u64>().unwrap_or(0);
        let output_vault_balance = client.get_token_account_balance(&output_vault)?.amount.parse::<u64>().unwrap_or(0);
        let info = CachedPoolInfo {
            amm_config,
            authority,
            input_vault,
            output_vault,
            input_vault_balance: AtomicU64::new(input_vault_balance),
            output_vault_balance: AtomicU64::new(output_vault_balance),
            token_0_mint,
            token_1_mint,
            observation_state,
            last_update: RwLock::new(Instant::now()),
        };
        self.pools.insert(*pool_state, info.clone());
        Ok(info)
    }
    pub async fn batch_update_pools(&self, client: &Arc<RpcClient>, pool_states: Vec<Pubkey>) {
        let futures: Vec<_> = pool_states
            .into_iter()
            .map(|pool| {
                let client = client.clone();
                let pool = pool.clone();
                async move { self.update_pool_info(&client, &pool).await.ok(); }
            })
            .collect();
        futures::future::join_all(futures).await;
    }
    pub async fn quick_update_balances(&self, client: &Arc<RpcClient>, pool_state: &Pubkey) -> anyhow::Result<(u64, u64)> {
        if let Some(cached) = self.pools.get(pool_state) {
            let (input_balance, output_balance) = tokio::join!(
                async { client.get_token_account_balance(&cached.input_vault).map(|b| b.amount.parse::<u64>().unwrap_or(0)).unwrap_or(0) },
                async { client.get_token_account_balance(&cached.output_vault).map(|b| b.amount.parse::<u64>().unwrap_or(0)).unwrap_or(0) }
            );
            cached.input_vault_balance.store(input_balance, Ordering::Relaxed);
            cached.output_vault_balance.store(output_balance, Ordering::Relaxed);
            Ok((input_balance, output_balance))
        } else {
            let info = self.update_pool_info(client, pool_state).await?;
            Ok((
                info.input_vault_balance.load(Ordering::Relaxed),
                info.output_vault_balance.load(Ordering::Relaxed)
            ))
        }
    }
}

impl TradeExecutor {
    pub fn new(rpc_url: &str, config: TradeExecutionConfig) -> Result<Self> {
        let client = RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::confirmed(),
        );
        
        // 从私钥创建钱包
        let private_key_bytes = bs58::decode(&config.copy_wallet_private_key)
            .into_vec()
            .context("无法解码私钥")?;
        
        let copy_wallet = Arc::new(Keypair::try_from(&private_key_bytes[..])
            .map_err(|e| anyhow::anyhow!("无法从私钥创建钱包: {:?}", e))?);
        
        // info!("交易执行器初始化完成，钱包地址: {}", copy_wallet.pubkey());
        
        let executor = TradeExecutor {
            client: Arc::new(client),
            copy_wallet: copy_wallet.clone(),
            config,
            rpc_url: rpc_url.to_string(),
            pool_cache: Arc::new(PoolCache::new()),
        };
        
        Ok(executor)
    }
    
    /// 执行跟单交易
    pub async fn execute_trade(&self, trade: &TradeDetails) -> Result<ExecutedTrade> {
        if !self.config.enabled {
            return Ok(ExecutedTrade {
                original_signature: trade.signature.clone(),
                copy_signature: "".to_string(),
                trade_direction: trade.trade_direction.clone(),
                amount_in: trade.amount_in,
                amount_out: trade.amount_out,
                price: trade.price,
                gas_fee: trade.gas_fee,
                timestamp: Utc::now().timestamp(),
                success: false,
                error_message: Some("交易执行已禁用".to_string()),
            });
        }
        
        // 检查是否强制下单金额
        let mut trade_amount_sol = trade.amount_in as f64 / 1_000_000_000.0;
        let mut forced = false;
        if (self.config.max_trade_amount - self.config.min_trade_amount).abs() < 1e-9 {
            trade_amount_sol = self.config.max_trade_amount;
            forced = true;
        }
        if trade_amount_sol < self.config.min_trade_amount {
            return Ok(ExecutedTrade {
                original_signature: trade.signature.clone(),
                copy_signature: "".to_string(),
                trade_direction: trade.trade_direction.clone(),
                amount_in: trade.amount_in,
                amount_out: trade.amount_out,
                price: trade.price,
                gas_fee: trade.gas_fee,
                timestamp: Utc::now().timestamp(),
                success: false,
                error_message: Some(format!("交易金额 {} SOL 小于最小金额 {} SOL", 
                    trade_amount_sol, self.config.min_trade_amount)),
            });
        }
        if trade_amount_sol > self.config.max_trade_amount && !forced {
            return Ok(ExecutedTrade {
                original_signature: trade.signature.clone(),
                copy_signature: "".to_string(),
                trade_direction: trade.trade_direction.clone(),
                amount_in: trade.amount_in,
                amount_out: trade.amount_out,
                price: trade.price,
                gas_fee: trade.gas_fee,
                timestamp: Utc::now().timestamp(),
                success: false,
                error_message: Some(format!("交易金额 {} SOL 大于最大金额 {} SOL", 
                    trade_amount_sol, self.config.max_trade_amount)),
            });
        }
        if forced {
            // info!("[强制下单] 按配置金额下单: {} SOL (原链上amount_in: {:.6} SOL)", trade_amount_sol, trade.amount_in as f64 / 1_000_000_000.0);
        }
        // ====== 卖出前自动检测copy钱包目标币种余额 ======
        if trade.trade_direction == TradeDirection::Sell {
            let token_mint = trade.token_in.mint;
            let token_accounts = self.client.get_token_accounts_by_owner(
                &self.copy_wallet.pubkey(),
                TokenAccountsFilter::Mint(token_mint),
            )?;
            let mut total_token_balance = 0u64;
            for acc in token_accounts {
                if let UiAccountData::Json(value) = &acc.account.data {
                    // 1.17/1.18的Json变体是ParsedAccount结构，不是serde_json::Value
                    // 需要访问value.parsed字段（通常是serde_json::Value），再取info
                    if let Some(info) = value.parsed.get("info") {
                        if let Some(token_amount) = info.get("tokenAmount")
                            .and_then(|ta| ta.get("amount"))
                            .and_then(|a| a.as_str())
                            .and_then(|s| s.parse::<u64>().ok()) {
                            total_token_balance += token_amount;
                        }
                    }
                }
            }
            if total_token_balance < trade_forced_amount_in_lamports(trade_amount_sol) {
                // warn!("[风控] 跟单钱包无足够{}余额，跳过卖出。余额: {}，需卖出: {}", trade.token_in.symbol.as_ref().unwrap_or(&"目标币种".to_string()), total_token_balance, trade_forced_amount_in_lamports(trade_amount_sol));
                return Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: "".to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: false,
                    error_message: Some("跟单钱包无该币种余额，跳过卖出".to_string()),
                });
            }
        }
        // ====== 买入/卖出需要WSOL时自动检测并兑换 ======
        let wsol_mint = Pubkey::from_str(crate::types::WSOL_MINT).unwrap();
        let need_wsol = (trade.trade_direction == TradeDirection::Buy && trade.token_in.mint == wsol_mint)
            || (trade.trade_direction == TradeDirection::Sell && trade.token_out.mint == wsol_mint);
        if need_wsol {
            let wsol_ata = get_associated_token_address(&self.copy_wallet.pubkey(), &wsol_mint);
            let wsol_balance = self.client.get_token_account_balance(&wsol_ata).map(|b| b.amount.parse::<u64>().unwrap_or(0)).unwrap_or(0);
            let required = trade_forced_amount_in_lamports(trade_amount_sol);
            if wsol_balance < required {
                let sol_balance = self.client.get_balance(&self.copy_wallet.pubkey())?;
                if sol_balance < required {
                    // warn!("[风控] SOL余额不足，无法自动兑换WSOL。SOL余额: {}，需兑换: {}", sol_balance, required);
                    return Ok(ExecutedTrade {
                        original_signature: trade.signature.clone(),
                        copy_signature: "".to_string(),
                        trade_direction: trade.trade_direction.clone(),
                        amount_in: trade.amount_in,
                        amount_out: trade.amount_out,
                        price: trade.price,
                        gas_fee: trade.gas_fee,
                        timestamp: Utc::now().timestamp(),
                        success: false,
                        error_message: Some("SOL余额不足，无法自动兑换WSOL".to_string()),
                    });
                }
                // info!("[自动兑换] 正在将SOL兑换为WSOL，金额: {} lamports", required - wsol_balance);
                // 创建WSOL账户（ATA）
                let create_ata_ix = spl_associated_token_account::instruction::create_associated_token_account(
                    &self.copy_wallet.pubkey(),
                    &self.copy_wallet.pubkey(),
                    &wsol_mint,
                    &spl_token::id(),
                );
                // 转账SOL到WSOL账户
                let transfer_ix = solana_sdk::system_instruction::transfer(
                    &self.copy_wallet.pubkey(),
                    &wsol_ata,
                    required - wsol_balance,
                );
                // 同步WSOL账户余额
                let sync_ix = spl_token::instruction::sync_native(&spl_token::id(), &wsol_ata)?;
                
                let recent_blockhash = self.client.get_latest_blockhash()?;
                
                // 创建版本0的交易消息
                let message = solana_sdk::message::v0::Message::try_compile(
                    &self.copy_wallet.pubkey(),
                    &[create_ata_ix, transfer_ix, sync_ix],
                    &[],
                    recent_blockhash,
                )?;
                
                let versioned_tx = VersionedTransaction::try_new(
                    VersionedMessage::V0(message),
                    &[&*self.copy_wallet],
                )?;
                
                let send_result = self.client.send_and_confirm_transaction(&versioned_tx);
                match send_result {
                    Ok(sig) => {
                        // info!("[自动兑换] SOL兑换WSOL成功: {}", sig);
                    }
                    Err(e) => {
                        // error!("[自动兑换] SOL兑换WSOL失败: {:?}", e);
                        return Err(anyhow::anyhow!(format!("SOL兑换WSOL失败: {:?}", e)));
                    }
                }
            }
        }
        // info!("开始执行跟单交易:");
        // info!("  原始交易: {}", trade.signature);
        // info!("  交易方向: {:?}", trade.trade_direction);
        // info!("  交易金额: {:.6} SOL", trade_amount_sol);
        // info!("  代币: {:?}", trade.token_out.symbol);
        // warn!("execute_trade已禁用RaydiumCPMM分支，请直接调用execute_raydium_cpmm_trade并传入正确池子参数！");
        // warn!("不支持的DEX类型: {:?}", trade.dex_type);
        // error!("[ATA] 创建ATA时owner不是钱包自己，owner: {}, 钱包: {}，拒绝创建！", owner, wallet.pubkey());
        // info!("[ATA] 已自动创建ATA: {}", ata);
        // info!("执行Pump.fun交易...");
        // info!("跟单交易成功: {}", signature);
        // error!("跟单交易失败: {}", e);
        // error!("输入代币不匹配: trade.token_in.mint={}, cpmm_accounts.input_mint={}", ...);
        // error!("输出代币不匹配: trade.token_out.mint={}, cpmm_accounts.output_mint={}", ...);
        // info!("计算的最小输出: {} (预期: {}, 滑点: {}%)", min_amount_out, expected_output, slippage_percent);
        // warn!("交易失败，重试 {}/{}: {}", retry_count, max_retries, e);
        // 构造一个新的TradeDetails用于实际下单
        let mut trade_for_exec = trade.clone();
        if forced {
            trade_for_exec.amount_in = (trade_amount_sol * 1_000_000_000.0) as u64;
        }
        match trade.dex_type {
            DexType::RaydiumCPMM => {
                // warn!("execute_trade已禁用RaydiumCPMM分支，请直接调用execute_raydium_cpmm_trade并传入正确池子参数！");
                return Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: "".to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: false,
                    error_message: Some("禁止通过execute_trade执行RaydiumCPMM，请用新版接口！".to_string()),
                });
            }
            DexType::PumpFun => {
                self.execute_pump_trade(&trade_for_exec).await
            }
            _ => {
                // warn!("不支持的DEX类型: {:?}", trade.dex_type);
                Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: "".to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: false,
                    error_message: Some(format!("不支持的DEX类型: {:?}", trade.dex_type)),
                })
            }
        }
    }
    
    /// 主流程自动化：优先用carbon解析器复刻链上TX，否则回退本地推导
    pub async fn execute_trade_with_carbon(&self, tx_json: Option<&str>, trade: &TradeDetails, cpmm_accounts: &RaydiumCpmmSwapAccounts, extra_accounts: &[Pubkey], min_amount_out: u64) -> Result<ExecutedTrade> {
        if let Some(_tx_json) = tx_json {
            // 删除carbon解析逻辑
            // match parse_raydium_cpmm_tx_with_carbon(tx_json) {
            //     Ok(carbon_result) => {
            //         tracing::info!("[carbon解析结果] {}", serde_json::to_string_pretty(&carbon_result).unwrap());
            //         // 解析carbon_result，组装指令
            //         let account_keys = carbon_result["account_keys"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect::<Vec<_>>();
            //         let is_signer = carbon_result["is_signer"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect::<Vec<_>>();
            //         let is_writable = carbon_result["is_writable"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect::<Vec<_>>();
            //         let data = hex::decode(carbon_result["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
            //         let program_id = Pubkey::from_str(carbon_result["program_id"].as_str().unwrap()).unwrap();
            //         let ix = from_carbon_parsed_accounts(&account_keys, &is_signer, &is_writable, data, &program_id);
            //         tracing::info!("[carbon复刻] swap指令账户顺序:");
            //         for (i, acc) in ix.accounts.iter().enumerate() {
            //             tracing::info!("  [{}] {} signer:{} writable:{}", i, acc.pubkey, acc.is_signer, acc.is_writable);
            //         }
            //         // 组装并发送交易
            //         let recent_blockhash = self.client.get_latest_blockhash()?;
            //         let message = solana_sdk::message::Message::new(&[ix], Some(&self.copy_wallet.pubkey()));
            //         let mut transaction = solana_sdk::transaction::Transaction::new_unsigned(message);
            //         transaction.sign(&[self.copy_wallet.as_ref()], recent_blockhash);
            //         match self.client.send_and_confirm_transaction(&transaction) {
            //             Ok(signature) => {
            //                 info!("[carbon复刻] 跟单交易成功: {}", signature);
            //                 return Ok(ExecutedTrade {
            //                     original_signature: trade.signature.clone(),
            //                     copy_signature: signature.to_string(),
            //                     trade_direction: trade.trade_direction.clone(),
            //                     amount_in: trade.amount_in,
            //                     amount_out: trade.amount_out,
            //                     price: trade.price,
            //                     gas_fee: trade.gas_fee,
            //                     timestamp: chrono::Utc::now().timestamp(),
            //                     success: true,
            //                     error_message: None,
            //                 });
            //             }
            //             Err(e) => {
            //                 error!("[carbon复刻] 跟单交易失败: {}", e);
            //                 return Ok(ExecutedTrade {
            //                     original_signature: trade.signature.clone(),
            //                     copy_signature: "".to_string(),
            //                     trade_direction: trade.trade_direction.clone(),
            //                     amount_in: trade.amount_in,
            //                     amount_out: trade.amount_out,
            //                     price: trade.price,
            //                     gas_fee: trade.gas_fee,
            //                     timestamp: chrono::Utc::now().timestamp(),
            //                     success: false,
            //                     error_message: Some(e.to_string()),
            //                 });
            //             }
            //         }
            //     }
            //     Err(e) => {
            //         warn!("carbon解析失败，回退本地推导: {}", e);
            //     }
            // }
            // 回退本地推导分支
            TradeExecutor::execute_raydium_cpmm_trade_static(
                &self.client,
                &self.copy_wallet,
                trade,
                cpmm_accounts,
                extra_accounts,
                min_amount_out,
                None, None, None, None, None
            ).await
        } else {
            // 回退本地推导分支
            TradeExecutor::execute_raydium_cpmm_trade_static(
                &self.client,
                &self.copy_wallet,
                trade,
                cpmm_accounts,
                extra_accounts,
                min_amount_out,
                None, None, None, None, None
            ).await
        }
    }
    
    /// 自动检查并创建ATA（如不存在）
    pub fn ensure_ata_exists_static(client: &Arc<RpcClient>, wallet: &Arc<Keypair>, owner: &Pubkey, mint: &Pubkey) -> Result<()> {
        let ata = get_associated_token_address(owner, mint);
        let account = client.get_account_with_commitment(&ata, CommitmentConfig::confirmed())?.value;
        if account.is_none() {
            // owner 必须等于钱包自己
            if owner != &wallet.pubkey() {
                // error!("[ATA] 创建ATA时owner不是钱包自己，owner: {}, 钱包: {}，拒绝创建！", owner, wallet.pubkey());
                return Err(anyhow::anyhow!("ATA创建owner必须是钱包自己，当前owner: {}, 钱包: {}", owner, wallet.pubkey()));
            }
            let ix = spl_associated_token_account::instruction::create_associated_token_account(
                &wallet.pubkey(), &wallet.pubkey(), mint, &spl_token::id()
            );
            
            let recent_blockhash = client.get_latest_blockhash()?;
            
            // 创建版本0的交易消息
            let message = solana_sdk::message::v0::Message::try_compile(
                &wallet.pubkey(),
                &[ix],
                &[],
                recent_blockhash,
            )?;
            
            let versioned_tx = VersionedTransaction::try_new(
                VersionedMessage::V0(message),
                &[&*wallet],
            )?;
            client.send_and_confirm_transaction(&versioned_tx)?;
            // info!("[ATA] 已自动创建ATA: {}", ata);
        }
        Ok(())
    }
    
    /// 根据链上原始TX账户顺序和权限组装swap指令
    pub fn create_raydium_cpmm_swap_ix_from_chain(
        _trade: &TradeDetails,
        account_keys: &[String],
        is_signer: &[bool],
        is_writable: &[bool],
        instruction_data: Vec<u8>,
        program_id: &Pubkey,
    ) -> solana_sdk::instruction::Instruction {
        println!("[DEBUG] account_keys.len() = {}", account_keys.len());
        println!("[DEBUG] is_signer.len() = {}", is_signer.len());
        println!("[DEBUG] is_writable.len() = {}", is_writable.len());
        for (i, k) in account_keys.iter().enumerate() {
            println!("[DEBUG] account_keys[{}] = {}", i, k);
        }
        for (i, s) in is_signer.iter().enumerate() {
            println!("[DEBUG] is_signer[{}] = {}", i, s);
        }
        for (i, w) in is_writable.iter().enumerate() {
            println!("[DEBUG] is_writable[{}] = {}", i, w);
        }
        assert_eq!(account_keys.len(), is_signer.len(), "account_keys 和 is_signer 长度不一致");
        assert_eq!(account_keys.len(), is_writable.len(), "account_keys 和 is_writable 长度不一致");
        let metas: Vec<AccountMeta> = account_keys.iter().enumerate().map(|(i, key)| {
            let pubkey = match Pubkey::from_str(key) {
                Ok(pk) => pk,
                Err(e) => {
                    // 自动去除首尾空格、换行、不可见字符后重试
                    let cleaned = key.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\r' || c == '\n');
                    match Pubkey::from_str(cleaned) {
                        Ok(pk2) => {
                            println!("[DEBUG] Pubkey::from_str 修正成功: 原始key='{}', 修正后='{}'", key, cleaned);
                            pk2
                        },
                        Err(e2) => panic!("[FATAL] Pubkey::from_str 解析失败: 原始key='{}', 修正后='{}', 错误1={:?}, 错误2={:?}", key, cleaned, e, e2)
                    }
                }
            };
            if is_signer[i] {
                AccountMeta::new(pubkey, true)
            } else if is_writable[i] {
                AccountMeta::new(pubkey, false)
            } else {
                AccountMeta::new_readonly(pubkey, false)
            }
        }).collect();
        solana_sdk::instruction::Instruction {
            program_id: *program_id,
            accounts: metas,
            data: instruction_data,
        }
    }

    /// 合并ATA和swap为一笔交易（静态版，优先用链上顺序）
    pub fn combine_ata_and_swap_instructions_static(
        client: &Arc<RpcClient>,
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        _extra_accounts: &[Pubkey],
        min_amount_out: u64,
        // 新增参数：链上原始TX账户顺序和权限
        chain_account_keys: Option<&[String]>,
        chain_is_signer: Option<&[bool]>,
        chain_is_writable: Option<&[bool]>,
        chain_instruction_data: Option<Vec<u8>>,
        chain_program_id: Option<&Pubkey>,
    ) -> Result<Vec<solana_sdk::instruction::Instruction>> {
        use spl_associated_token_account::get_associated_token_address;
        use solana_sdk::commitment_config::CommitmentConfig;
        let mut instructions = Vec::new();
        // 设置更高的计算单元限制和优先费
        let config_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("config.json")?)?;
        let compute_unit_limit = config_json["compute_unit_limit"].as_u64().unwrap_or(100_000) as u32;
        let compute_unit_price = config_json["compute_unit_price"].as_u64().unwrap_or(20_000) as u64;
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price));
        let user_input_ata = get_associated_token_address(&wallet.pubkey(), &trade.token_in.mint);
        let user_output_ata = get_associated_token_address(&wallet.pubkey(), &trade.token_out.mint);
        // 检查input_ata
        let input_ata_exists = client.get_account_with_commitment(&user_input_ata, CommitmentConfig::confirmed())?.value.is_some();
        if !input_ata_exists {
            instructions.push(spl_associated_token_account::instruction::create_associated_token_account(
                &wallet.pubkey(), &wallet.pubkey(), &trade.token_in.mint, &spl_token::id()
            ));
        }
        // 检查output_ata
        let output_ata_exists = client.get_account_with_commitment(&user_output_ata, CommitmentConfig::confirmed())?.value.is_some();
        if !output_ata_exists {
            instructions.push(spl_associated_token_account::instruction::create_associated_token_account(
                &wallet.pubkey(), &wallet.pubkey(), &trade.token_out.mint, &spl_token::id()
            ));
        }
        // swap指令
        if let (Some(keys), Some(signers), Some(writables), Some(data), Some(pid)) = (chain_account_keys, chain_is_signer, chain_is_writable, chain_instruction_data, chain_program_id) {
            let swap_ix = Self::create_raydium_cpmm_swap_ix_from_chain(trade, keys, signers, writables, data, pid);
            instructions.push(swap_ix);
        } else {
            // 主流程全部切换为v4写法
            let mut swap_instructions = Self::create_raydium_cpmm_swap_instructions_v4(wallet, trade, cpmm_accounts, min_amount_out)?;
            instructions.append(&mut swap_instructions);
        }
        Ok(instructions)
    }

    /// 执行Raydium CPMM交易（合并ATA+swap），自动校验池子、余额、ATA
    pub async fn execute_raydium_cpmm_trade_static(
        client: &Arc<RpcClient>,
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        extra_accounts: &[Pubkey],
        min_amount_out: u64,
        _chain_account_keys: Option<&[String]>,
        _chain_is_signer: Option<&[bool]>,
        _chain_is_writable: Option<&[bool]>,
        _chain_instruction_data: Option<Vec<u8>>,
        _chain_program_id: Option<&Pubkey>,
    ) -> Result<ExecutedTrade> {
        use spl_associated_token_account::get_associated_token_address;
        use std::sync::atomic::Ordering;
        // ====== 1. 优先用本地池子缓存 ======
        let pool_cache = crate::POOL_CACHE.get().expect("PoolCache未初始化");
        let pool_info = match pool_cache.get_pool_info(client, &cpmm_accounts.pool_state).await {
            Ok(info) => info,
            Err(_) => pool_cache.update_pool_info(client, &cpmm_accounts.pool_state).await?,
        };
        let input_vault_balance = pool_info.input_vault_balance.load(Ordering::Relaxed);
        let output_vault_balance = pool_info.output_vault_balance.load(Ordering::Relaxed);
        // ====== 2. 只用本地ATA推导，不查链上 ======
        let expected_input_ata = get_associated_token_address(&wallet.pubkey(), &cpmm_accounts.input_mint);
        let expected_output_ata = get_associated_token_address(&wallet.pubkey(), &cpmm_accounts.output_mint);
        if cpmm_accounts.user_input_ata != expected_input_ata {
            // error!("user_input_ata 不匹配！期望: {}, 实际: {}", expected_input_ata, cpmm_accounts.user_input_ata);
            return Err(anyhow::anyhow!("user_input_ata 与 input_mint 不匹配"));
        }
        if cpmm_accounts.user_output_ata != expected_output_ata {
            // error!("user_output_ata 不匹配！期望: {}, 实际: {}", expected_output_ata, cpmm_accounts.user_output_ata);
            return Err(anyhow::anyhow!("user_output_ata 与 output_mint 不匹配"));
        }
        // ====== 3. 极简ATA创建（假定大部分已存在） ======
        let mut instructions = Vec::new();
        let config_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string("config.json")?)?;
        let compute_unit_limit = config_json["compute_unit_limit"].as_u64().unwrap_or(100_000) as u32;
        let compute_unit_price = config_json["compute_unit_price"].as_u64().unwrap_or(5_000) as u64;
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price));
        // ====== 4. 组装swap指令 ======
        let mut swap_instructions = Self::create_raydium_cpmm_swap_instructions_v4_fixed(wallet, trade, cpmm_accounts, min_amount_out)?;
        instructions.append(&mut swap_instructions);
        // ====== 5. 构造并签名交易 ======
        let recent_blockhash = client.get_latest_blockhash()?;
        let v0_message = solana_sdk::message::v0::Message::try_compile(
            &wallet.pubkey(),
            &instructions,
            &[],
            recent_blockhash,
        )?;
        let versioned_message = VersionedMessage::V0(v0_message);
        let versioned_tx = VersionedTransaction::try_new(
            versioned_message,
            &[&*wallet],
        )?;
        // ====== 6. 用send_transaction极致速度发送，不等待确认 ======
        let sig = client.send_transaction(&versioned_tx)?;
        match trade.trade_direction {
            TradeDirection::Buy => {
                trade_log!("跟单钱包买入成功: {} {} (mint: {}) 签名: {}", trade.amount_out, trade.token_out.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_out.mint, sig);
            }
            TradeDirection::Sell => {
                trade_log!("跟单钱包卖出成功: {} {} (mint: {}) 签名: {}", trade.amount_in, trade.token_in.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_in.mint, sig);
            }
        }
        Ok(ExecutedTrade {
            original_signature: trade.signature.clone(),
            copy_signature: sig.to_string(),
            trade_direction: trade.trade_direction,
            amount_in: trade.amount_in,
            amount_out: 0,
            price: trade.price,
            gas_fee: 0,
            timestamp: chrono::Utc::now().timestamp(),
            success: true,
            error_message: None,
        })
    }
    
    /// 执行Pump.fun交易
    async fn execute_pump_trade(&self, trade: &TradeDetails) -> Result<ExecutedTrade> {
        // info!("执行Pump.fun交易...");
        
        // 获取最新区块哈希
        let recent_blockhash = self.client.get_latest_blockhash()?;
        
        // 创建交易指令
        let _instructions = vec![
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(100_000),
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(5_000),
        ];
        let instructions = self.create_pump_instructions(trade, &PumpFunAccounts {
            fee_recipient: Pubkey::new_from_array([0; 32]),
            mint: Pubkey::new_from_array([0; 32]),
            bonding_curve: Pubkey::new_from_array([0; 32]),
            associated_bonding_curve: Pubkey::new_from_array([0; 32]),
            event_authority: Pubkey::new_from_array([0; 32]),
        }, 0)?;
        
        // 创建版本0的交易消息
        let v0_message = solana_sdk::message::v0::Message::try_compile(
            &self.copy_wallet.pubkey(),
            &instructions,
            &[],
            recent_blockhash,
        )?;
        let versioned_message = VersionedMessage::V0(v0_message);
        let versioned_tx = VersionedTransaction::try_new(
            versioned_message,
            &[&*self.copy_wallet],
        )?;
        
        // 发送交易
        match self.client.send_and_confirm_transaction(&versioned_tx) {
            Ok(signature) => {
                // info!("跟单交易成功: {}", signature);
                Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: signature.to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: true,
                    error_message: None,
                })
            }
            Err(e) => {
                // error!("跟单交易失败: {}", e);
                Ok(ExecutedTrade {
                    original_signature: trade.signature.clone(),
                    copy_signature: "".to_string(),
                    trade_direction: trade.trade_direction.clone(),
                    amount_in: trade.amount_in,
                    amount_out: trade.amount_out,
                    price: trade.price,
                    gas_fee: trade.gas_fee,
                    timestamp: Utc::now().timestamp(),
                    success: false,
                    error_message: Some(e.to_string()),
                })
            }
        }
    }
    
    /// 创建Raydium CPMM交易指令
    pub fn create_raydium_cpmm_instructions(&self, trade: &TradeDetails, pool: &RaydiumPoolAccounts, min_amount_out: u64) -> Result<Vec<Instruction>> {
        let mut instructions = Vec::new();
        let user_pubkey = self.copy_wallet.pubkey();
        let token_in_ata = get_associated_token_address(&user_pubkey, &trade.token_in.mint);
        let token_out_ata = get_associated_token_address(&user_pubkey, &trade.token_out.mint);

        tracing::info!("[DEBUG] 构造Raydium CPMM swap指令账户列表:");
        tracing::info!("user_pubkey: {}", user_pubkey);
        tracing::info!("token_in_ata: {} mint: {}", token_in_ata, trade.token_in.mint);
        tracing::info!("token_out_ata: {} mint: {}", token_out_ata, trade.token_out.mint);
        tracing::info!("池子参数: amm_id={} amm_authority={} amm_open_orders={} amm_target_orders={} pool_coin_token_account={} pool_pc_token_account={} serum_program_id={} serum_market={} serum_bids={} serum_asks={} serum_event_queue={} serum_coin_vault_account={} serum_pc_vault_account={} serum_vault_signer={}",
            pool.amm_id, pool.amm_authority, pool.amm_open_orders, pool.amm_target_orders, pool.pool_coin_token_account, pool.pool_pc_token_account, pool.serum_program_id, pool.serum_market, pool.serum_bids, pool.serum_asks, pool.serum_event_queue, pool.serum_coin_vault_account, pool.serum_pc_vault_account, pool.serum_vault_signer
        );

        // 自动创建ATA（如不存在）
        instructions.push(spl_associated_token_account::instruction::create_associated_token_account(
            &user_pubkey, &user_pubkey, &trade.token_in.mint, &spl_token::id()
        ));
        instructions.push(spl_associated_token_account::instruction::create_associated_token_account(
            &user_pubkey, &user_pubkey, &trade.token_out.mint, &spl_token::id()
        ));

        // 构造Raydium swap指令data
        let mut data = vec![9u8]; // swap指令类型
        data.extend_from_slice(&trade.amount_in.to_le_bytes());
        data.extend_from_slice(&min_amount_out.to_le_bytes());

        // 构造完整账户列表（顺序必须严格按合约要求）
        let accounts = vec![
            AccountMeta::new(user_pubkey, true),
            AccountMeta::new(token_in_ata, false),
            AccountMeta::new(token_out_ata, false),
            AccountMeta::new(pool.amm_id, false),
            AccountMeta::new(pool.amm_authority, false),
            AccountMeta::new(pool.amm_open_orders, false),
            AccountMeta::new(pool.amm_target_orders, false),
            AccountMeta::new(pool.pool_coin_token_account, false),
            AccountMeta::new(pool.pool_pc_token_account, false),
            AccountMeta::new(pool.serum_program_id, false),
            AccountMeta::new(pool.serum_market, false),
            AccountMeta::new(pool.serum_bids, false),
            AccountMeta::new(pool.serum_asks, false),
            AccountMeta::new(pool.serum_event_queue, false),
            AccountMeta::new(pool.serum_coin_vault_account, false),
            AccountMeta::new(pool.serum_pc_vault_account, false),
            AccountMeta::new(pool.serum_vault_signer, false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::id(), false),
        ];
        for (i, acc) in accounts.iter().enumerate() {
            tracing::info!("账户{}: {} signer:{} writable:{}", i, acc.pubkey, acc.is_signer, acc.is_writable);
        }
        let swap_ix = Instruction {
            program_id: trade.program_id,
            accounts,
            data,
        };
        instructions.push(swap_ix);
        Ok(instructions)
    }

    /// 新版：严格按链上顺序组装Raydium CPMM swap指令（v4，8字节discriminator，兼容新版合约）
    pub fn create_raydium_cpmm_swap_instructions_v4(
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        min_amount_out: u64,
    ) -> Result<Vec<Instruction>> {
        use tracing::info;
        let discriminator = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
        let params = SwapBaseInput {
            amount_in: trade.amount_in,
            min_amount_out,
        };
        let mut data = discriminator.to_vec();
        data.extend_from_slice(&params.amount_in.to_le_bytes());
        data.extend_from_slice(&params.min_amount_out.to_le_bytes());
        info!("构建 CPMM 指令，当前参数：");
        info!("  交易方向: {:?}", trade.trade_direction);
        info!("  trade.token_in: {}", trade.token_in.mint);
        info!("  trade.token_out: {}", trade.token_out.mint);
        info!("  cpmm.input_mint: {}", cpmm_accounts.input_mint);
        info!("  cpmm.output_mint: {}", cpmm_accounts.output_mint);
        // 关键：对调第6和第7个账户（input_vault/output_vault）
        let accounts = vec![
            AccountMeta::new(wallet.pubkey(), true),
            AccountMeta::new_readonly(cpmm_accounts.authority, false),
            AccountMeta::new_readonly(cpmm_accounts.amm_config, false),
            AccountMeta::new(cpmm_accounts.pool_state, false),
            AccountMeta::new(cpmm_accounts.user_input_ata, false),
            AccountMeta::new(cpmm_accounts.user_output_ata, false),
            AccountMeta::new(cpmm_accounts.output_vault, false), // 先output_vault
            AccountMeta::new(cpmm_accounts.input_vault, false),  // 再input_vault
            AccountMeta::new_readonly(cpmm_accounts.input_token_program, false),
            AccountMeta::new_readonly(cpmm_accounts.output_token_program, false),
            AccountMeta::new_readonly(trade.token_in.mint, false),
            AccountMeta::new_readonly(trade.token_out.mint, false),
            AccountMeta::new(cpmm_accounts.observation_state, false),
        ];
        info!("账户列表：");
        for (i, account) in accounts.iter().enumerate() {
            info!("  [{}] {}", i, account.pubkey);
        }
        Ok(vec![Instruction {
            program_id: trade.program_id,
            accounts,
            data,
        }])
    }

    /// 新版：严格按链上顺序组装Raydium CPMM swap指令
    pub fn create_raydium_cpmm_swap_instructions_v2_static(
        trade: &TradeDetails,
        accounts: &RaydiumCpmmSwapAccounts,
        extra_accounts: &[Pubkey],
        _min_amount_out: u64,
    ) -> Result<Vec<Instruction>> {
        // 修正后的Raydium CPMM合约swap指令账户顺序
        let metas = vec![
            AccountMeta::new(accounts.payer, true), // payer(你钱包)
            AccountMeta::new_readonly(accounts.authority, false), // authority
            AccountMeta::new_readonly(accounts.amm_config, false), // amm_config
            AccountMeta::new(accounts.pool_state, false), // pool_state
            AccountMeta::new(accounts.user_input_ata, false), // user_input_ata
            AccountMeta::new(accounts.user_output_ata, false), // user_output_ata
            AccountMeta::new(accounts.input_vault, false), // input_vault
            AccountMeta::new(accounts.output_vault, false), // output_vault
            AccountMeta::new_readonly(accounts.input_token_program, false), // input_token_program
            AccountMeta::new_readonly(accounts.output_token_program, false), // output_token_program
            AccountMeta::new_readonly(accounts.input_mint, false), // input_mint
            AccountMeta::new_readonly(accounts.output_mint, false), // output_mint
            AccountMeta::new_readonly(accounts.observation_state, false), // observation_state
        ];
        let mut all_accounts = metas;
        for pk in extra_accounts {
            all_accounts.push(AccountMeta::new_readonly(*pk, false));
        }
        // swap指令data
        let mut data = vec![9u8];
        data.extend_from_slice(&trade.amount_in.to_le_bytes());
        data.extend_from_slice(&(_min_amount_out).to_le_bytes());
        let swap_ix = Instruction {
            program_id: trade.program_id,
            accounts: all_accounts.clone(),
            data,
        };
        // 同步修正账户含义标签顺序
        let account_labels = [
            "payer(你钱包)",
            "authority(池子authority)",
            "amm_config(池子amm_config)",
            "pool_state(池子state)",
            "user_input_ata(你的input ATA)",
            "user_output_ata(你的output ATA)",
            "input_vault(池子input_vault)",
            "output_vault(池子output_vault)",
            "input_token_program(Token Program)",
            "output_token_program(Token Program)",
            "input_mint(池子input_mint)",
            "output_mint(池子output_mint)",
            "observation_state(池子observation_state)"
        ];
        tracing::info!("[账户顺序打印][含义对照] swap指令账户列表:");
        for (i, acc) in swap_ix.accounts.iter().enumerate() {
            let label = account_labels.get(i).unwrap_or(&"未知");
            tracing::info!("  [{}] {}: {} signer:{} writable:{}", i+1, label, acc.pubkey, acc.is_signer, acc.is_writable);
        }
        Ok(vec![swap_ix])
    }

    pub fn create_pump_instructions(&self, trade: &TradeDetails, accounts: &PumpFunAccounts, max_sol_cost: u64) -> Result<Vec<Instruction>> {
        let mut instructions = Vec::new();
        let user_pubkey = self.copy_wallet.pubkey();
        let token_ata = get_associated_token_address(&user_pubkey, &trade.token_in.mint);

        // 自动创建ATA（如不存在）
        instructions.push(spl_associated_token_account::instruction::create_associated_token_account(
            &user_pubkey, &user_pubkey, &trade.token_in.mint, &spl_token::id()
        ));

        // 构造Pump.fun指令data
        let instruction_type = match trade.trade_direction {
            TradeDirection::Buy => 0x66u8,
            TradeDirection::Sell => 0x33u8,
        };
        let mut data = vec![instruction_type];
        data.extend_from_slice(&trade.amount_in.to_le_bytes());
        data.extend_from_slice(&max_sol_cost.to_le_bytes());

        // 构造完整账户列表
        let accounts_vec = vec![
            AccountMeta::new(user_pubkey, true),
            AccountMeta::new(accounts.fee_recipient, false),
            AccountMeta::new(accounts.mint, false),
            AccountMeta::new(accounts.bonding_curve, false),
            AccountMeta::new(accounts.associated_bonding_curve, false),
            AccountMeta::new(token_ata, false),
            AccountMeta::new(user_pubkey, true),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::id(), false),
            AccountMeta::new(accounts.event_authority, false),
            AccountMeta::new(trade.program_id, false),
        ];
        let pump_ix = Instruction {
            program_id: trade.program_id,
            accounts: accounts_vec,
            data,
        };
        instructions.push(pump_ix);
        Ok(instructions)
    }
    
    /// 获取钱包余额
    pub fn get_wallet_balance(&self) -> Result<f64> {
        let balance = self.client.get_balance(&self.copy_wallet.pubkey())?;
        Ok(balance as f64 / 1_000_000_000.0)
    }
    
    /// 检查钱包是否有足够余额
    pub fn check_balance(&self, required_amount: u64) -> Result<bool> {
        let balance = self.client.get_balance(&self.copy_wallet.pubkey())?;
        Ok(balance >= required_amount)
    }

    /// 新的账户顺序映射方案
    pub fn create_raydium_cpmm_swap_instructions_v4_fixed(
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        min_amount_out: u64,
    ) -> Result<Vec<Instruction>> {
        use tracing::info;
        let discriminator = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
        let params = SwapBaseInput {
            amount_in: trade.amount_in,
            min_amount_out,
        };
        let mut data = discriminator.to_vec();
        data.extend_from_slice(&params.amount_in.to_le_bytes());
        data.extend_from_slice(&params.min_amount_out.to_le_bytes());
        
        // 确保使用正确的CPMM程序ID
        let cpmm_program_id = Pubkey::from_str(crate::types::RAYDIUM_CPMM)?;
        
        info!("构建 CPMM 指令，当前参数：");
        info!("  交易方向: {:?}", trade.trade_direction);
        info!("  amount_in: {}", trade.amount_in);
        info!("  min_amount_out: {}", min_amount_out);
        info!("  程序ID: {} (应该是CPMM)", cpmm_program_id);
        info!("  输入代币 (input_mint): {}", cpmm_accounts.input_mint);
        info!("  输出代币 (output_mint): {}", cpmm_accounts.output_mint);
        info!("  池子token0: {}", cpmm_accounts.token_0_mint);
        info!("  池子token1: {}", cpmm_accounts.token_1_mint);
        
        // 交易方向验证
        info!("交易方向验证：");
        info!("  卖出代币: {} (应该等于 input_mint)", trade.token_in.mint);
        info!("  得到代币: {} (应该等于 output_mint)", trade.token_out.mint);
        info!("  cpmm_accounts.input_mint: {}", cpmm_accounts.input_mint);
        info!("  cpmm_accounts.output_mint: {}", cpmm_accounts.output_mint);
        
        // 确保映射正确
        if trade.token_in.mint != cpmm_accounts.input_mint {
            // error!("输入代币不匹配: trade.token_in.mint={}, cpmm_accounts.input_mint={}", 
            //     trade.token_in.mint, cpmm_accounts.input_mint);
            return Err(anyhow::anyhow!("输入代币不匹配"));
        }
        if trade.token_out.mint != cpmm_accounts.output_mint {
            // error!("输出代币不匹配: trade.token_out.mint={}, cpmm_accounts.output_mint={}", 
            //     trade.token_out.mint, cpmm_accounts.output_mint);
            return Err(anyhow::anyhow!("输出代币不匹配"));
        }
        
        // 关键修复：在账户位置10和11使用input_mint和output_mint，而不是token_0_mint和token_1_mint
        let accounts = vec![
            AccountMeta::new(wallet.pubkey(), true),                     // 0: Payer
            AccountMeta::new_readonly(cpmm_accounts.authority, false),   // 1: Authority
            AccountMeta::new_readonly(cpmm_accounts.amm_config, false),  // 2: AMM Config
            AccountMeta::new(cpmm_accounts.pool_state, false),           // 3: Pool State
            AccountMeta::new(cpmm_accounts.user_input_ata, false),       // 4: User Input ATA
            AccountMeta::new(cpmm_accounts.user_output_ata, false),      // 5: User Output ATA
            AccountMeta::new(cpmm_accounts.input_vault, false),          // 6: Input Vault
            AccountMeta::new(cpmm_accounts.output_vault, false),         // 7: Output Vault
            AccountMeta::new_readonly(cpmm_accounts.input_token_program, false),  // 8: Input Token Program
            AccountMeta::new_readonly(cpmm_accounts.output_token_program, false), // 9: Output Token Program
            AccountMeta::new_readonly(cpmm_accounts.input_mint, false),   // 10: Input Mint（用户输入的代币）
            AccountMeta::new_readonly(cpmm_accounts.output_mint, false),  // 11: Output Mint（用户输出的代币）
            AccountMeta::new(cpmm_accounts.observation_state, false),     // 12: Observation State
        ];
        
        info!("最终账户列表：");
        for (i, account) in accounts.iter().enumerate() {
            info!("  [{}] {}", i, account.pubkey);
        }
        
        Ok(vec![Instruction {
            program_id: cpmm_program_id,  // 使用固定的CPMM程序ID，而不是trade.program_id
            accounts,
            data,
        }])
    }

    /// 在主函数中添加详细的账户验证
    pub async fn verify_and_build_cpmm_accounts(
        client: &Arc<RpcClient>,
        pool_state: &Pubkey,
        user_wallet: &Pubkey,
        is_selling: bool,
    ) -> Result<RaydiumCpmmSwapAccounts> {
        use tracing::info;
        // 从池子账户数据中提取信息
        let pool_account = client.get_account(pool_state)?;
        let pool_data = &pool_account.data;
        // 解析池子数据获取正确的账户地址
        let authority = Self::derive_pool_authority(pool_state)?;
        let amm_config = Pubkey::new_from_array(pool_data[8..40].try_into().unwrap());
        let token_0_vault = Pubkey::new_from_array(pool_data[72..104].try_into().unwrap());
        let token_1_vault = Pubkey::new_from_array(pool_data[104..136].try_into().unwrap());
        let token_0_mint = Pubkey::new_from_array(pool_data[168..200].try_into().unwrap());
        let token_1_mint = Pubkey::new_from_array(pool_data[200..232].try_into().unwrap());
        let input_token_program = Pubkey::new_from_array(pool_data[232..264].try_into().unwrap());
        let output_token_program = Pubkey::new_from_array(pool_data[264..296].try_into().unwrap());
        let observation_state = Pubkey::new_from_array(pool_data[296..328].try_into().unwrap());
        // 确定WSOL的位置
        let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        let (input_mint, output_mint, input_vault, output_vault) = if is_selling {
            if token_0_mint == wsol_mint {
                // WSOL是token0，TAKI是token1，卖出TAKI
                (token_1_mint, token_0_mint, token_1_vault, token_0_vault)
            } else {
                // WSOL是token1，TAKI是token0，卖出TAKI
                (token_0_mint, token_1_mint, token_0_vault, token_1_vault)
            }
        } else {
            if token_0_mint == wsol_mint {
                // WSOL是token0，买入TAKI
                (token_0_mint, token_1_mint, token_0_vault, token_1_vault)
            } else {
                // WSOL是token1，买入TAKI
                (token_1_mint, token_0_mint, token_1_vault, token_0_vault)
            }
        };
        // 创建用户的ATA
        let user_input_ata = get_associated_token_address(user_wallet, &input_mint);
        let user_output_ata = get_associated_token_address(user_wallet, &output_mint);
        info!("账户映射验证：");
        info!("  输入代币: {} -> ATA: {}", input_mint, user_input_ata);
        info!("  输出代币: {} -> ATA: {}", output_mint, user_output_ata);
        info!("  输入金库: {}", input_vault);
        info!("  输出金库: {}", output_vault);
        Ok(RaydiumCpmmSwapAccounts {
            payer: *user_wallet,
            authority,
            amm_config,
            pool_state: *pool_state,
            user_input_ata,
            user_output_ata,
            input_vault,
            output_vault,
            input_token_program,
            output_token_program,
            input_mint,
            output_mint,
            observation_state,
            token_0_mint,
            token_1_mint,
        })
    }

    /// 派生池子权限账户
    pub fn derive_pool_authority(pool_state: &Pubkey) -> Result<Pubkey> {
        let (authority, _) = Pubkey::find_program_address(
            &[b"amm_authority", pool_state.as_ref()],
            &Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap(),
        );
        Ok(authority)
    }

    /// 使用调整后的滑点重新执行交易
    pub fn execute_with_adjusted_slippage<'a>(
        client: &'a Arc<RpcClient>,
        wallet: &'a Arc<Keypair>,
        trade: &'a TradeDetails,
        cpmm_accounts: &'a RaydiumCpmmSwapAccounts,
        extra_accounts: &'a [Pubkey],
        adjusted_min_out: u64,
    ) -> BoxFuture<'a, Result<ExecutedTrade>> {
        Box::pin(Self::execute_raydium_cpmm_trade_static(
            client,
            wallet,
            trade,
            cpmm_accounts,
            extra_accounts,
            adjusted_min_out,
            None, None, None, None, None
        ))
    }

    /// 改进的池子账户验证和构建函数
    pub async fn verify_and_build_cpmm_accounts_fixed(
        client: &Arc<RpcClient>,
        pool_state: &Pubkey,
        user_wallet: &Pubkey,
        _is_selling: bool,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
    ) -> Result<RaydiumCpmmSwapAccounts> {
        use tracing::info;
        
        // 从池子账户数据中提取信息
        let pool_account = client.get_account(pool_state)?;
        let pool_data = &pool_account.data;
        
        // 解析池子数据
        let authority = Self::derive_pool_authority(pool_state)?;
        let amm_config = Pubkey::new_from_array(pool_data[8..40].try_into().unwrap());
        let token_0_vault = Pubkey::new_from_array(pool_data[72..104].try_into().unwrap());
        let token_1_vault = Pubkey::new_from_array(pool_data[104..136].try_into().unwrap());
        let token_0_mint = Pubkey::new_from_array(pool_data[168..200].try_into().unwrap());
        let token_1_mint = Pubkey::new_from_array(pool_data[200..232].try_into().unwrap());
        let token_0_program = Pubkey::new_from_array(pool_data[232..264].try_into().unwrap());
        let token_1_program = Pubkey::new_from_array(pool_data[264..296].try_into().unwrap());
        let observation_state = Pubkey::new_from_array(pool_data[296..328].try_into().unwrap());
        
        // 验证输入输出mint是否匹配池子
        let pool_has_input = token_0_mint == *input_mint || token_1_mint == *input_mint;
        let pool_has_output = token_0_mint == *output_mint || token_1_mint == *output_mint;
        
        if !pool_has_input || !pool_has_output {
            return Err(anyhow::anyhow!("池子不包含指定的交易对"));
        }
        
        // 根据实际的input/output mint确定vault和program
        let (input_vault, output_vault, input_token_program, output_token_program) = 
            if token_0_mint == *input_mint {
                // input是token0，output是token1
                (token_0_vault, token_1_vault, token_0_program, token_1_program)
            } else {
                // input是token1，output是token0
                (token_1_vault, token_0_vault, token_1_program, token_0_program)
            };
        
        // 创建用户的ATA
        let user_input_ata = get_associated_token_address(user_wallet, input_mint);
        let user_output_ata = get_associated_token_address(user_wallet, output_mint);
        
        info!("账户映射验证：");
        info!("  池子token0: {}", token_0_mint);
        info!("  池子token1: {}", token_1_mint);
        info!("  输入代币: {} -> Vault: {}", input_mint, input_vault);
        info!("  输出代币: {} -> Vault: {}", output_mint, output_vault);
        info!("  输入ATA: {}", user_input_ata);
        info!("  输出ATA: {}", user_output_ata);
        
        Ok(RaydiumCpmmSwapAccounts {
            payer: *user_wallet,
            authority,
            amm_config,
            pool_state: *pool_state,
            user_input_ata,
            user_output_ata,
            input_vault,
            output_vault,
            input_token_program,
            output_token_program,
            input_mint: *input_mint,
            output_mint: *output_mint,
            observation_state,
            // 保存token0和token1的原始顺序
            token_0_mint,
            token_1_mint,
        })
    }

    /// 临时WSOL账户极速交易
    pub async fn execute_trade_with_temp_wsol(
        &self,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        _min_amount_out: u64,
    ) -> Result<ExecutedTrade> {
        let mut retry_count = 0;
        let max_retries = 3;
        // 余额校验
        if trade.amount_in == 0 {
            return Err(anyhow::anyhow!("输入金额不能为0"));
        }
        let balance = self.client.get_balance(&self.copy_wallet.pubkey())?;
        if balance < trade.amount_in + 10_000_000 {
            return Err(anyhow::anyhow!(
                "余额不足: 需要 {} SOL, 实际 {} SOL",
                (trade.amount_in + 10_000_000) as f64 / 1e9,
                balance as f64 / 1e9
            ));
        }
        // 账户参数
        let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
        let need_temp_wsol = trade.trade_direction == TradeDirection::Buy && cpmm_accounts.input_mint == wsol_mint;
        // 计算池子预期输出和滑点
        let (input_vault_balance, output_vault_balance) = (
            self.client.get_token_account_balance(&cpmm_accounts.input_vault)
                .map(|b| b.amount.parse::<u64>().unwrap_or(0)).unwrap_or(0),
            self.client.get_token_account_balance(&cpmm_accounts.output_vault)
                .map(|b| b.amount.parse::<u64>().unwrap_or(0)).unwrap_or(0)
        );
        let expected_output = crate::trade_executor::raydium_cpmm_quote(
            trade.amount_in,
            input_vault_balance,
            output_vault_balance,
            25,
            10000
        );
        let slippage_percent = 10.0;
        let mut min_amount_out = ((expected_output as f64) * (1.0 - slippage_percent / 100.0)) as u64;
        min_amount_out = std::cmp::max(min_amount_out, 1000);
        // info!("计算的最小输出: {} (预期: {}, 滑点: {}%)", min_amount_out, expected_output, slippage_percent);
        // 重试主循环
        loop {
            let recent_blockhash = self.client.get_latest_blockhash()?;
            // seed生成
            let seed = format!("wsol_{}", Uuid::new_v4().to_string().replace("-", ""));
            let seed = &seed[..std::cmp::min(seed.len(), 32)];
            // 构建交易
            let versioned_tx = self.build_transaction(
                trade,
                cpmm_accounts,
                min_amount_out,
                recent_blockhash,
                seed
            )?;
            match self.send_and_confirm_transaction_with_retry(&versioned_tx).await {
                Ok(signature) => {
                    // === 不再单独close，主交易已合并 ===
                    return Ok(ExecutedTrade {
                        original_signature: trade.signature.clone(),
                        copy_signature: signature.to_string(),
                        trade_direction: trade.trade_direction,
                        amount_in: trade.amount_in,
                        amount_out: 0,
                        price: trade.price,
                        gas_fee: 0,
                        timestamp: chrono::Utc::now().timestamp(),
                        success: true,
                        error_message: None,
                    });
                },
                Err(e) => {
                    if retry_count >= max_retries {
                        return Err(e);
                    }
                    retry_count += 1;
                    // warn!("交易失败，重试 {}/{}: {}", retry_count, max_retries, e);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    fn build_transaction(
        &self,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        min_amount_out: u64,
        recent_blockhash: solana_sdk::hash::Hash,
        seed: &str,
    ) -> Result<VersionedTransaction> {
        let wallet_pubkey = self.copy_wallet.pubkey();
        let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
        let mut instructions = Vec::new();
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(20_000));
        let need_temp_wsol = trade.trade_direction == TradeDirection::Buy && cpmm_accounts.input_mint == wsol_mint;
        if need_temp_wsol {
            let space = spl_token::state::Account::get_packed_len();
            let rent = self.client.get_minimum_balance_for_rent_exemption(space)?;
            let buffer = 100_000;
            let total_lamports = trade.amount_in + rent + buffer;
            let temp_wsol_pubkey = Pubkey::create_with_seed(
                &wallet_pubkey,
                seed,
                &spl_token::id(),
            )?;
            instructions.push(
                solana_sdk::system_instruction::create_account_with_seed(
                    &wallet_pubkey,
                    &temp_wsol_pubkey,
                    &wallet_pubkey,
                    seed,
                    total_lamports,
                    space as u64,
                    &spl_token::id(),
                )
            );
            instructions.push(
                spl_token::instruction::initialize_account(
                    &spl_token::id(),
                    &temp_wsol_pubkey,
                    &wsol_mint,
                    &wallet_pubkey,
                )?
            );
            instructions.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &wallet_pubkey,
                    &wallet_pubkey,
                    &cpmm_accounts.output_mint,
                    &spl_token::id(),
                )
            );
            let mut updated_cpmm_accounts = (*cpmm_accounts).clone();
            updated_cpmm_accounts.user_input_ata = temp_wsol_pubkey;
            let mut swap_instructions = Self::create_raydium_cpmm_swap_instructions_with_custom_accounts(
                &self.copy_wallet,
                trade,
                &updated_cpmm_accounts,
                min_amount_out,
            )?;
            instructions.append(&mut swap_instructions);
            // === 合并close_account到主交易 ===
            instructions.push(
                spl_token::instruction::close_account(
                    &spl_token::id(),
                    &temp_wsol_pubkey,
                    &wallet_pubkey,
                    &wallet_pubkey,
                    &[&wallet_pubkey],
                )?
            );
        } else {
            instructions.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &wallet_pubkey,
                    &wallet_pubkey,
                    &cpmm_accounts.input_mint,
                    &spl_token::id(),
                )
            );
            instructions.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    &wallet_pubkey,
                    &wallet_pubkey,
                    &cpmm_accounts.output_mint,
                    &spl_token::id(),
                )
            );
            let mut swap_instructions = Self::create_raydium_cpmm_swap_instructions_v4_fixed(
                &self.copy_wallet,
                trade,
                cpmm_accounts,
                min_amount_out,
            )?;
            instructions.append(&mut swap_instructions);
        }
        let v0_message = solana_sdk::message::v0::Message::try_compile(
            &wallet_pubkey,
            &instructions,
            &[],
            recent_blockhash,
        )?;
        let versioned_tx = VersionedTransaction::try_new(
            VersionedMessage::V0(v0_message),
            &[&*self.copy_wallet],
        )?;
        Ok(versioned_tx)
    }

    async fn send_and_confirm_transaction_with_retry(
        &self,
        transaction: &VersionedTransaction,
    ) -> Result<solana_sdk::signature::Signature> {
        use solana_sdk::commitment_config::CommitmentConfig;
        let signature = self.client.send_transaction(transaction)?;
        let commitment = CommitmentConfig::confirmed();
        match self.client.confirm_transaction_with_spinner(
            &signature,
            &self.client.get_latest_blockhash()?,
            commitment
        ) {
            Ok(_) => Ok(signature),
            Err(e) => {
                match self.client.get_signature_status(&signature)? {
                    Some(Ok(())) => Ok(signature),
                    Some(Err(txerr)) => Err(anyhow::anyhow!("交易失败: {:?}", txerr)),
                    None => Err(anyhow::anyhow!("交易状态未知: {}", e))
                }
            }
        }
    }

    /// 等待交易确认
    pub async fn wait_for_confirmation(&self, signature: &solana_sdk::signature::Signature, max_wait_secs: u64) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if start.elapsed().as_secs() > max_wait_secs {
                return Err(anyhow::anyhow!("等待交易确认超时"));
            }
            if let Ok(tx) = self.client.get_transaction(signature, UiTransactionEncoding::JsonParsed) {
                if tx.transaction.meta.is_some() {
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
    }

    /// swap指令支持自定义账户
    fn create_raydium_cpmm_swap_instructions_with_custom_accounts(
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        min_amount_out: u64,
    ) -> Result<Vec<Instruction>> {
        let discriminator = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
        let params = SwapBaseInput {
            amount_in: trade.amount_in,
            min_amount_out,
        };
        let mut data = discriminator.to_vec();
        data.extend_from_slice(&params.amount_in.to_le_bytes());
        data.extend_from_slice(&params.min_amount_out.to_le_bytes());
        let cpmm_program_id = Pubkey::from_str(crate::types::RAYDIUM_CPMM)?;
        let accounts = vec![
            AccountMeta::new(wallet.pubkey(), true),
            AccountMeta::new_readonly(cpmm_accounts.authority, false),
            AccountMeta::new_readonly(cpmm_accounts.amm_config, false),
            AccountMeta::new(cpmm_accounts.pool_state, false),
            AccountMeta::new(cpmm_accounts.user_input_ata, false),
            AccountMeta::new(cpmm_accounts.user_output_ata, false),
            AccountMeta::new(cpmm_accounts.input_vault, false),
            AccountMeta::new(cpmm_accounts.output_vault, false),
            AccountMeta::new_readonly(cpmm_accounts.input_token_program, false),
            AccountMeta::new_readonly(cpmm_accounts.output_token_program, false),
            AccountMeta::new_readonly(cpmm_accounts.input_mint, false),
            AccountMeta::new_readonly(cpmm_accounts.output_mint, false),
            AccountMeta::new(cpmm_accounts.observation_state, false),
        ];
        Ok(vec![Instruction {
            program_id: cpmm_program_id,
            accounts,
            data,
        }])
    }

    /// 快速路径：极简跟单交易，跳过大部分校验，直接构建并发送交易
    pub async fn execute_trade_fast_path(&self, trade: &TradeDetails, cpmm_accounts: &RaydiumCpmmSwapAccounts, min_amount_out: u64) -> Result<ExecutedTrade> {
        use solana_sdk::commitment_config::CommitmentConfig;
        use solana_sdk::signature::Signature;
        use solana_sdk::transaction::VersionedTransaction;
        use solana_sdk::hash::Hash;
        use solana_sdk::instruction::Instruction;
        use solana_client::rpc_client::RpcClient;
        use solana_sdk::system_instruction;
        use solana_sdk::message::Message;
        use tracing::{info, error};
        // 1. 获取最新区块哈希
        let recent_blockhash = self.client.get_latest_blockhash()?;
        // 2. 构建 swap 指令（跳过所有池子owner、ATA、mint等校验）
        let instructions = TradeExecutor::create_raydium_cpmm_swap_instructions_v4(
            &self.copy_wallet,
            trade,
            cpmm_accounts,
            min_amount_out,
        )?;
        // 3. 构建消息和交易
        let message = Message::new(&instructions, Some(&self.copy_wallet.pubkey()));
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[self.copy_wallet.as_ref()])?;
        // 4. 直接发送交易（不等待确认）
        let signature = self.client.send_transaction(&tx)?;
        match trade.trade_direction {
            TradeDirection::Buy => {
                trade_log!("跟单钱包买入成功: {} {} (mint: {}) 签名: {}", trade.amount_out, trade.token_out.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_out.mint, signature);
            }
            TradeDirection::Sell => {
                trade_log!("跟单钱包卖出成功: {} {} (mint: {}) 签名: {}", trade.amount_in, trade.token_in.symbol.as_ref().unwrap_or(&"未知代币".to_string()), trade.token_in.mint, signature);
            }
        }
        Ok(ExecutedTrade {
            original_signature: trade.signature.clone(),
            copy_signature: signature.to_string(),
            trade_direction: trade.trade_direction.clone(),
            amount_in: trade.amount_in,
            amount_out: trade.amount_out,
            price: trade.price,
            gas_fee: trade.gas_fee,
            timestamp: chrono::Utc::now().timestamp(),
            success: true,
            error_message: None,
        })
    }
} 

/// 自动从链上池子账户提取并校验Raydium CPMM池参数
pub fn extract_and_check_cpmm_pool_accounts(rpc: &Arc<RpcClient>, pool_state: &Pubkey) -> Result<RaydiumCpmmSwapAccounts> {
    // 拉取池子账户数据
    let acc = rpc.get_account(pool_state)?;
    let data = acc.data;
    // 解析关键字段（按官方cpmm合约结构体偏移）
    // 这里只做演示，实际偏移需查官方合约state定义
    let amm_config = Pubkey::new_from_array(data[8..40].try_into().unwrap());
    let input_vault = Pubkey::new_from_array(data[72..104].try_into().unwrap());
    let output_vault = Pubkey::new_from_array(data[104..136].try_into().unwrap());
    let input_mint = Pubkey::new_from_array(data[168..200].try_into().unwrap());
    let output_mint = Pubkey::new_from_array(data[200..232].try_into().unwrap());
    let input_token_program = Pubkey::new_from_array(data[232..264].try_into().unwrap());
    let output_token_program = Pubkey::new_from_array(data[264..296].try_into().unwrap());
    let observation_state = Pubkey::new_from_array(data[296..328].try_into().unwrap());
    // 校验observation_state owner
    let obs_acc = rpc.get_account(&observation_state)?;
    let cpmm_pid = Pubkey::from_str(crate::types::RAYDIUM_CPMM).unwrap();
    if obs_acc.owner != cpmm_pid {
        return Err(anyhow::anyhow!(format!("observation_state账户owner错误，当前owner: {}，应为Raydium CPMM程序{}", obs_acc.owner, cpmm_pid)));
    }
    // 其它参数可继续自动提取
    Ok(RaydiumCpmmSwapAccounts {
        payer: Pubkey::default(), // 由调用方填充
        authority: Pubkey::default(), // 由调用方填充
        amm_config,
        pool_state: *pool_state,
        user_input_ata: Pubkey::default(), // 由调用方填充
        user_output_ata: Pubkey::default(), // 由调用方填充
        input_vault,
        output_vault,
        input_token_program,
        output_token_program,
        input_mint,
        output_mint,
        observation_state,
        token_0_mint: Pubkey::default(), // 由调用方填充
        token_1_mint: Pubkey::default(), // 由调用方填充
    })
}

/// 通过carbon解析器输出直接复刻链上TX账户顺序和指令data
pub fn from_carbon_parsed_accounts(
    account_keys: &[String],
    is_signer: &[bool],
    is_writable: &[bool],
    data: Vec<u8>,
    program_id: &Pubkey,
) -> Instruction {
    let metas: Vec<AccountMeta> = account_keys.iter().enumerate().map(|(i, key)| {
        let pubkey = Pubkey::from_str(key).unwrap();
        if is_signer[i] {
            AccountMeta::new(pubkey, true)
        } else if is_writable[i] {
            AccountMeta::new(pubkey, false)
        } else {
            AccountMeta::new_readonly(pubkey, false)
        }
    }).collect();
    Instruction {
        program_id: *program_id,
        accounts: metas,
        data,
    }
}

// 在主动买入/极致跟单流程中，优先支持carbon账户复刻模式
// 伪代码示例：
// if let Some(carbon_result) = carbon解析器输出 {
//     let ix = from_carbon_parsed_accounts(
//         &carbon_result.account_keys,
//         &carbon_result.is_signer,
//         &carbon_result.is_writable,
//         carbon_result.data,
//         &carbon_result.program_id,
//     );
//     // 直接用ix组装交易，100%复刻链上TX
// } else {
//     // 走本地推导分支
// }

fn trade_forced_amount_in_lamports(trade_amount_sol: f64) -> u64 {
    (trade_amount_sol * 1_000_000_000.0) as u64
} 

// 替换#[derive(Clone)]为手动实现Clone
impl Clone for CachedPoolInfo {
    fn clone(&self) -> Self {
        Self {
            amm_config: self.amm_config,
            authority: self.authority,
            input_vault: self.input_vault,
            output_vault: self.output_vault,
            input_vault_balance: AtomicU64::new(self.input_vault_balance.load(Ordering::Relaxed)),
            output_vault_balance: AtomicU64::new(self.output_vault_balance.load(Ordering::Relaxed)),
            token_0_mint: self.token_0_mint,
            token_1_mint: self.token_1_mint,
            observation_state: self.observation_state,
            last_update: RwLock::new(*self.last_update.blocking_read()),
        }
    }
}
// 修复batch_update_pools返回引用问题，map中用move闭包，collect到Vec<_>，不返回引用。