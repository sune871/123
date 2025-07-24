// https://solana-rpc.publicnode.com/f884f7c2cfa0e7ecbf30e7da70ec1da91bda3c9d04058269397a5591e7fd013e";
// CuwxHwz42cNivJqWGBk6HcVvfGq47868Mo6zi4u6z9vC

mod types;
mod trade_executor;
mod config;
mod grpc_monitor; // 恢复gRPC监控
mod parser; // 添加parser模块

use std::sync::Arc;
use anyhow::Result;
use tracing::{info, error, warn};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use std::str::FromStr;
use crate::trade_executor::TradeExecutor;
use crate::config::Config;
use grpc_monitor::GrpcMonitor; // 恢复gRPC监控
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::message::VersionedMessage;
use std::env;
use clap::Parser;
use crate::types::{TradeDetails, TokenInfo, DexType, TradeDirection};
use crate::trade_executor::{RaydiumCpmmSwapAccounts};
use spl_associated_token_account::get_associated_token_address;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_account_decoder::UiAccountData;
use once_cell::sync::OnceCell;
use crate::trade_executor::PoolCache;
pub static POOL_CACHE: OnceCell<PoolCache> = OnceCell::new();

#[macro_export]
macro_rules! perf_critical {
    ($($arg:tt)*) => {
        #[cfg(feature = "verbose-logging")]
        tracing::trace!($($arg)*);
    };
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        tracing::debug!($($arg)*);
    };
}

#[macro_export]
macro_rules! trade_log {
    ($($arg:tt)*) => {
        tracing::info!($($arg)*);
    };
}

#[derive(Parser)]
#[command(name = "wallet_copier")]
#[command(about = "Solana钱包监控和跟单程序")]
struct Cli {
    /// 买币模式：指定要购买的代币符号
    #[arg(long, value_name = "TOKEN")]
    buy: Option<String>,
    
    /// 卖币模式：指定要卖出的代币符号
    #[arg(long, value_name = "TOKEN")]
    sell: Option<String>,
    
    /// 卖出所有代币
    #[arg(long)]
    sell_all: bool,
    
    /// 查看钱包余额
    #[arg(long)]
    balance: bool,
    
    /// 交易金额（SOL）
    #[arg(long, default_value = "0.01")]
    amount: f64,
    
    /// 滑点百分比
    #[arg(long, default_value = "0.5")]
    slippage: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    
    let cli = Cli::parse();
    
    info!("🚀 启动Solana钱包监控和跟单程序");
    let config = Config::load()?;
    info!("配置加载成功");
    // 初始化全局池子缓存
    POOL_CACHE.set(PoolCache::new()).ok();
    // 2. 创建 TradeExecutor
    let executor = Arc::new(TradeExecutor::new(
        &config.rpc_url,
        config.get_execution_config(),
    )?);
    
    match executor.get_wallet_balance() {
        Ok(balance) => {
            info!("跟单钱包余额: {:.6} SOL", balance);
        }
        Err(e) => {
            warn!("无法获取钱包余额: {}", e);
        }
    }
    
    // 检查命令行参数，决定运行模式
    if cli.balance {
        info!("💰 查看钱包余额");
        return show_wallet_balance(&executor).await;
    }
    
    if cli.sell_all {
        info!("💰 卖出所有代币");
        // 使用配置文件中的滑点设置
        let config_slippage = config.execution_config.slippage_tolerance;
        info!("📊 使用配置文件滑点设置: {}%", config_slippage * 100.0);
        return sell_all_tokens(&executor, config_slippage).await;
    }
    
    if let Some(token_symbol) = cli.buy {
        info!("🛒 进入买币模式，购买代币: {}", token_symbol);
        // 使用配置文件中的滑点设置
        let config_slippage = config.execution_config.slippage_tolerance;
        info!("📊 使用配置文件滑点设置: {}%", config_slippage * 100.0);
        return buy_token(&executor, &token_symbol, cli.amount, config_slippage).await;
    }
    
    if let Some(token_symbol) = cli.sell {
        info!("💰 进入卖币模式，卖出代币: {}", token_symbol);
        // 使用配置文件中的滑点设置
        let config_slippage = config.execution_config.slippage_tolerance;
        info!("📊 使用配置文件滑点设置: {}%", config_slippage * 100.0);
        return sell_token(&executor, &token_symbol, cli.amount, config_slippage).await;
    }
    
    // 默认进入跟单模式
    info!("📡 进入跟单模式");
    info!("开始解析目标钱包地址...");
    let target_wallets: Vec<Pubkey> = config.target_wallets.iter()
        .enumerate()
        .map(|(index, wallet_str)| {
            info!("解析目标钱包 [{}]: {}", index, wallet_str);
            match Pubkey::from_str(wallet_str) {
                Ok(pubkey) => {
                    info!("成功解析钱包地址 [{}]: {}", index, pubkey);
                    Ok(pubkey)
                }
        Err(e) => {
                    error!("解析钱包地址失败 [{}]: {} - 错误: {:?}", index, wallet_str, e);
                    Err(anyhow::anyhow!("Invalid wallet address at index {}: {}", index, wallet_str))
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if target_wallets.is_empty() {
        return Err(anyhow::anyhow!("No valid target wallets found in configuration"));
    }
    let target_wallet = target_wallets[0];
    info!("使用目标钱包: {}", target_wallet);
    
    // 在进入跟单模式前：
    
    // 测试多通道竞速广播功能 - 已注释，不需要测试
    /*
    info!("测试多通道竞速广播功能...");
    let test_tx = VersionedTransaction::try_new(
        VersionedMessage::V0(solana_sdk::message::v0::Message::try_compile(
            &executor.copy_wallet.pubkey(),
            &[],
            &[],
            temp_client.get_latest_blockhash()?,
        )?),
        &[&*executor.copy_wallet],
    )?;
    
    match executor.broadcast_transaction_race(&test_tx).await {
        Ok(sig) => info!("多通道广播测试成功: {}", sig),
        Err(e) => warn!("多通道广播测试失败: {}", e),
    }
    */
    
    info!("检查GRPC认证配置...");
    let auth_token = match env::var("GRPC_AUTH_TOKEN") {
        Ok(token) => {
            info!("找到GRPC_AUTH_TOKEN环境变量，长度: {}", token.len());
            if !token.is_empty() {
                match bs58::decode(&token).into_vec() {
                    Ok(_) => {
                        info!("GRPC_AUTH_TOKEN是有效的Base58字符串");
                        Some(token)
                    }
                    Err(e) => {
                        warn!("GRPC_AUTH_TOKEN不是Base58格式: {} - 将作为普通字符串使用", e);
                        Some(token)
                    }
                }
                } else {
                info!("GRPC_AUTH_TOKEN为空");
                None
            }
        }
        Err(_) => {
            info!("未设置GRPC_AUTH_TOKEN环境变量");
            None
        }
    };
    info!("创建GrpcMonitor实例...");
    let grpc_endpoint = config.grpc_endpoint.clone();
    let rpc_url = config.rpc_url.clone();
    let monitor = GrpcMonitor::new_with_executor(
        grpc_endpoint,
        auth_token,
        target_wallet,
        executor,
        rpc_url,
    );
    info!("GrpcMonitor创建成功");
    // 启动监控
    match Arc::new(monitor).start_monitoring().await {
        Ok(_) => info!("监控程序正常结束"),
        Err(e) => error!("监控程序出错: {}", e),
    }
    
    Ok(())
}

// 买币功能
async fn buy_token(executor: &Arc<TradeExecutor>, token_symbol: &str, amount_sol: f64, slippage: f64) -> Result<()> {
    info!("🛒 开始购买代币: {}", token_symbol);
    info!("💰 购买金额: {} SOL", amount_sol);
    info!("📊 滑点设置: {}%", slippage);
    
    // 代币符号到mint地址的映射
    let token_mints = get_token_mints();
    let token_mint = match token_mints.get(token_symbol.to_lowercase().as_str()) {
        Some(mint) => {
            info!("✅ 找到代币 {} 的mint地址: {}", token_symbol, mint);
            Pubkey::from_str(mint)?
        }
        None => {
            error!("❌ 未找到代币 {} 的mint地址", token_symbol);
            return Err(anyhow::anyhow!("未找到代币 {} 的mint地址", token_symbol));
        }
    };
    
    // 查找对应的交易池
    let pool_info = find_pool_for_token(&token_mint)?;
    info!("🏊 找到交易池: {}", pool_info.pool_state);
    
    // 查询池子当前状态
    let pool_state_pubkey = Pubkey::from_str(&pool_info.pool_state)?;
    let pool_account = executor.client.get_account(&pool_state_pubkey)?;
    
    // 解析池子数据获取vault地址
    let pool_data = pool_account.data.as_slice();
    if pool_data.len() < 328 {
        return Err(anyhow::anyhow!("池子数据长度不足，实际长度: {}", pool_data.len()));
    }
    
    // 读取池子中的vault地址
    let input_vault = Pubkey::new_from_array(pool_data[72..104].try_into()?);
    let output_vault = Pubkey::new_from_array(pool_data[104..136].try_into()?);
    
    // 查询vault账户的余额
    let input_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.input_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    let output_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.output_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    
    info!("📊 池子状态:");
    info!("   输入vault: {}", input_vault);
    info!("   输出vault: {}", output_vault);
    info!("   输入代币余额: {} lamports", input_vault_balance);
    info!("   输出代币余额: {} tokens", output_vault_balance);
    
    // 计算预期输出（使用Raydium CPMM公式）
    let amount_in_lamports = (amount_sol * 1_000_000_000.0) as u64;
    let expected_output = crate::trade_executor::raydium_cpmm_quote(
        amount_in_lamports,
        input_vault_balance,
        output_vault_balance,
        25,  // fee_numerator
        10000, // fee_denominator
    );
    
    info!("📈 预期输出: {} tokens", expected_output);
    
    // 计算最小输出金额（滑点保护）
    let slippage_bps = (slippage * 10000.0) as u64; // 转换为basis points (0.5 -> 5000 bps)
    let min_amount_out = crate::trade_executor::calculate_min_amount_out_with_safety(
        expected_output,
        slippage_bps,
        50, // 额外50基点安全边际
    );
    
    info!("🛡️ 滑点保护:");
    info!("   预期输出: {} tokens", expected_output);
    info!("   最小输出: {} tokens", min_amount_out);
    info!("   滑点保护: {} tokens", expected_output - min_amount_out);
    info!("   滑点百分比: {}%", slippage * 100.0);
    
    // 创建TradeDetails
    let trade = TradeDetails {
        signature: format!("manual_buy_{}", chrono::Utc::now().timestamp()),
        wallet: executor.copy_wallet.pubkey(),
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Buy,
        token_in: TokenInfo {
            mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        token_out: TokenInfo {
            mint: token_mint,
            symbol: Some(token_symbol.to_string()),
            decimals: 6, // 大部分代币是6位小数
        },
        amount_in: amount_in_lamports,
        amount_out: expected_output, // 使用计算出的预期输出
        price: 0.0, // 将由池子计算
        pool_address: Pubkey::from_str(&pool_info.pool_state)?,
        timestamp: chrono::Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str(crate::types::RAYDIUM_CPMM)?,
    };
    
    // 构建Raydium CPMM账户
    let cpmm_accounts = RaydiumCpmmSwapAccounts {
        payer: executor.copy_wallet.pubkey(),
        authority: Pubkey::from_str(&pool_info.authority)?,
        amm_config: Pubkey::from_str(&pool_info.amm_config)?,
        pool_state: Pubkey::from_str(&pool_info.pool_state)?,
        user_input_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &Pubkey::from_str(crate::types::WSOL_MINT)?),
        user_output_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &token_mint),
        input_vault: Pubkey::from_str(&pool_info.input_vault)?,
        output_vault: Pubkey::from_str(&pool_info.output_vault)?,
        input_token_program: Pubkey::from_str(&pool_info.input_token_program)?,
        output_token_program: Pubkey::from_str(&pool_info.output_token_program)?,
        input_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        output_mint: token_mint,
        observation_state: Pubkey::from_str(&pool_info.observation_state)?,
        token_0_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        token_1_mint: token_mint,
    };
    
    info!("📈 执行买币交易...");
    info!("   输入: {} SOL", amount_sol);
    info!("   预期输出: {} {}", token_symbol, expected_output);
    info!("   最小输出: {} {}", token_symbol, min_amount_out);
    info!("   池子: {}", pool_info.pool_state);
    
    // 执行交易
    match TradeExecutor::execute_raydium_cpmm_trade_static(
        &executor.client,
        &executor.copy_wallet,
        &trade,
        &cpmm_accounts,
        &[],
        min_amount_out,
        None,
        None,
        None,
        None,
        None,
    ).await {
        Ok(executed_trade) => {
            if executed_trade.success {
                info!("✅ 买币成功!");
                info!("   交易签名: {}", executed_trade.copy_signature);
                info!("   输入金额: {} SOL", executed_trade.amount_in as f64 / 1_000_000_000.0);
                info!("   实际输出: {} {}", token_symbol, executed_trade.amount_out);
                info!("   预期输出: {} {}", token_symbol, expected_output);
                info!("   滑点损失: {} {}", token_symbol, expected_output - executed_trade.amount_out);
            } else {
                error!("❌ 买币失败: {}", executed_trade.error_message.unwrap_or_default());
            }
        }
        Err(e) => {
            error!("❌ 买币执行失败: {}", e);
            return Err(e);
        }
    }
    
    Ok(())
}

// 卖币功能
async fn sell_token(executor: &Arc<TradeExecutor>, token_symbol: &str, amount_sol: f64, slippage: f64) -> Result<()> {
    info!("💰 开始卖出代币: {}", token_symbol);
    info!("💰 卖出金额: {} SOL", amount_sol);
    info!("📊 滑点设置: {}%", slippage);
    
    // 代币符号到mint地址的映射
    let token_mints = get_token_mints();
    let token_mint = match token_mints.get(token_symbol.to_lowercase().as_str()) {
        Some(mint) => {
            info!("✅ 找到代币 {} 的mint地址: {}", token_symbol, mint);
            Pubkey::from_str(mint)?
        }
        None => {
            error!("❌ 未找到代币 {} 的mint地址", token_symbol);
            return Err(anyhow::anyhow!("未找到代币 {} 的mint地址", token_symbol));
        }
    };
    
    // 查找对应的交易池
    let pool_info = find_pool_for_token(&token_mint)?;
    info!("🏊 找到交易池: {}", pool_info.pool_state);
    
    // 查询池子当前状态
    let pool_state_pubkey = Pubkey::from_str(&pool_info.pool_state)?;
    let pool_account = executor.client.get_account(&pool_state_pubkey)?;
    
    // 解析池子数据获取vault地址
    let pool_data = pool_account.data.as_slice();
    if pool_data.len() < 328 {
        return Err(anyhow::anyhow!("池子数据长度不足，实际长度: {}", pool_data.len()));
    }
    
    // 读取池子中的vault地址
    let input_vault = Pubkey::new_from_array(pool_data[72..104].try_into()?);
    let output_vault = Pubkey::new_from_array(pool_data[104..136].try_into()?);
    
    // 查询vault账户的余额
    let input_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.input_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    let output_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.output_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    
    info!("📊 池子状态:");
    info!("   输入vault: {}", input_vault);
    info!("   输出vault: {}", output_vault);
    info!("   输入代币余额: {} tokens", input_vault_balance);
    info!("   输出代币余额: {} lamports", output_vault_balance);
    
    // 计算预期输出（使用Raydium CPMM公式）
    let amount_in_tokens = (amount_sol * 1_000_000_000.0) as u64; // 转换为最小单位
    let expected_output = crate::trade_executor::raydium_cpmm_quote(
        amount_in_tokens,
        input_vault_balance,
        output_vault_balance,
        25,  // fee_numerator
        10000, // fee_denominator
    );
    
    info!("📈 预期输出: {} lamports ({} SOL)", expected_output, expected_output as f64 / 1_000_000_000.0);
    
    // 计算最小输出金额（滑点保护）
    let slippage_bps = (slippage * 10000.0) as u64; // 转换为basis points (0.5 -> 5000 bps)
    let min_amount_out = crate::trade_executor::calculate_min_amount_out_with_safety(
        expected_output,
        slippage_bps,
        50, // 额外50基点安全边际
    );
    
    info!("🛡️ 滑点保护:");
    info!("   预期输出: {} SOL", expected_output as f64 / 1_000_000_000.0);
    info!("   最小输出: {} SOL", min_amount_out as f64 / 1_000_000_000.0);
    info!("   滑点保护: {} SOL", (expected_output - min_amount_out) as f64 / 1_000_000_000.0);
    info!("   滑点百分比: {}%", slippage * 100.0);
    
    // 创建TradeDetails
    let trade = TradeDetails {
        signature: format!("manual_sell_{}", chrono::Utc::now().timestamp()),
        wallet: executor.copy_wallet.pubkey(),
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Sell,
        token_in: TokenInfo {
            mint: token_mint,
            symbol: Some(token_symbol.to_string()),
            decimals: 6,
        },
        token_out: TokenInfo {
            mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        amount_in: amount_in_tokens,
        amount_out: expected_output, // 使用计算出的预期输出
        price: 0.0, // 将由池子计算
        pool_address: Pubkey::from_str(&pool_info.pool_state)?,
        timestamp: chrono::Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str(crate::types::RAYDIUM_CPMM)?,
    };
    
    // 构建Raydium CPMM账户（修正：用pool_cache自动映射，不手动对调vault/token_program）
    let cpmm_accounts = RaydiumCpmmSwapAccounts {
        payer: executor.copy_wallet.pubkey(),
        authority: Pubkey::from_str(&pool_info.authority)?,
        amm_config: Pubkey::from_str(&pool_info.amm_config)?,
        pool_state: Pubkey::from_str(&pool_info.pool_state)?,
        user_input_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &token_mint),
        user_output_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &Pubkey::from_str(crate::types::WSOL_MINT)?),
        input_vault: Pubkey::from_str(&pool_info.output_vault)?,
        output_vault: Pubkey::from_str(&pool_info.input_vault)?,
        input_token_program: Pubkey::from_str(&pool_info.output_token_program)?,
        output_token_program: Pubkey::from_str(&pool_info.input_token_program)?,
        input_mint: token_mint,
        output_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        observation_state: Pubkey::from_str(&pool_info.observation_state)?,
        token_0_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        token_1_mint: token_mint,
    };
    
    info!("📈 执行卖币交易...");
    info!("   输入: {} {}", token_symbol, amount_sol);
    info!("   预期输出: {} SOL", expected_output as f64 / 1_000_000_000.0);
    info!("   最小输出: {} SOL", min_amount_out as f64 / 1_000_000_000.0);
    info!("   池子: {}", pool_info.pool_state);
    
    // 执行交易
    match TradeExecutor::execute_raydium_cpmm_trade_static(
        &executor.client,
        &executor.copy_wallet,
        &trade,
        &cpmm_accounts,
        &[],
        min_amount_out,
        None,
        None,
        None,
        None,
        None,
    ).await {
        Ok(executed_trade) => {
            if executed_trade.success {
                info!("✅ 卖币成功!");
                info!("   交易签名: {}", executed_trade.copy_signature);
                info!("   输入金额: {} {}", token_symbol, executed_trade.amount_in);
                info!("   实际输出: {} SOL", executed_trade.amount_out as f64 / 1_000_000_000.0);
                info!("   预期输出: {} SOL", expected_output as f64 / 1_000_000_000.0);
                info!("   滑点损失: {} SOL", (expected_output - executed_trade.amount_out) as f64 / 1_000_000_000.0);
            } else {
                error!("❌ 卖币失败: {}", executed_trade.error_message.unwrap_or_default());
            }
        }
        Err(e) => {
            error!("❌ 卖币执行失败: {}", e);
            return Err(e);
        }
    }
    
    Ok(())
}

// 代币符号到mint地址的映射
fn get_token_mints() -> std::collections::HashMap<String, String> {
    let mut mints = std::collections::HashMap::new();
    mints.insert("taki".to_string(), "95Wqh64NDAwpRGcRmwxvQsjZALfNGwbig4ioTbJdWMYW".to_string());
    mints.insert("bonk".to_string(), "CNNQZyEWfz9mDBRCiRNwjaUvMaLnaRWem8HeJYh7bonk".to_string());
    mints.insert("dogwifhat".to_string(), "FWj3yi1x2ZHaZZsB1mteximPVZJ3LtGjNzasknTpbonk".to_string());
    mints.insert("book".to_string(), "5Kkdo1gypUjhzpNGv4qUcwukosi3t4ucAzgMokaLbonk".to_string());
    mints.insert("cat".to_string(), "4YWy8JNjB4CLjG71hxGwzFXWd4DtpdvsAY2GqQ1Fbonk".to_string());
    mints.insert("popcat".to_string(), "CN8V1z4TNsQ3FfkcyQe4UcJUG1YZUCGsPDrCheF3bonk".to_string());
    mints.insert("myro".to_string(), "4AXnbEf3N3iLNChHL2TWHcyMnBKEvJLJ82okFFUFbonk".to_string());
    mints
}

// 池子信息结构
#[derive(Debug)]
struct PoolInfo {
    pool_state: String,
    authority: String,
    amm_config: String,
    input_vault: String,
    output_vault: String,
    input_mint: String,
    output_mint: String,
    observation_state: String,
    input_token_program: String,
    output_token_program: String,
}

// 根据代币mint查找对应的池子
fn find_pool_for_token(token_mint: &Pubkey) -> Result<PoolInfo> {
    let pool_data = include_str!("../cpmm_pools.json");
    let pools: Vec<serde_json::Value> = serde_json::from_str(pool_data)?;
    
    for pool in pools {
        let output_mint = pool["output_mint"].as_str().unwrap_or("");
        let input_mint = pool["input_mint"].as_str().unwrap_or("");
        
        if output_mint == token_mint.to_string() || input_mint == token_mint.to_string() {
            return Ok(PoolInfo {
                pool_state: pool["pool_state"].as_str().unwrap_or("").to_string(),
                authority: pool["authority"].as_str().unwrap_or("").to_string(),
                amm_config: pool["amm_config"].as_str().unwrap_or("").to_string(),
                input_vault: pool["input_vault"].as_str().unwrap_or("").to_string(),
                output_vault: pool["output_vault"].as_str().unwrap_or("").to_string(),
                input_mint: pool["input_mint"].as_str().unwrap_or("").to_string(),
                output_mint: pool["output_mint"].as_str().unwrap_or("").to_string(),
                observation_state: pool["observation_state"].as_str().unwrap_or("").to_string(),
                input_token_program: pool["input_token_program"].as_str().unwrap_or("").to_string(),
                output_token_program: pool["output_token_program"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    
    Err(anyhow::anyhow!("未找到代币 {} 对应的交易池", token_mint))
}

// 查看钱包余额
async fn show_wallet_balance(executor: &Arc<TradeExecutor>) -> Result<()> {
    info!("💰 查看钱包余额");
    
    // 获取SOL余额
    let sol_balance = executor.client.get_balance(&executor.copy_wallet.pubkey())?;
    info!("SOL余额: {:.6} SOL", sol_balance as f64 / 1_000_000_000.0);
    
    // 获取所有代币账户
    let token_accounts = executor.client.get_token_accounts_by_owner(
        &executor.copy_wallet.pubkey(),
        TokenAccountsFilter::ProgramId(spl_token::id()),
    )?;
    
    if token_accounts.is_empty() {
        info!("没有代币余额");
        return Ok(());
    }
    
    info!("代币余额:");
    for (i, account) in token_accounts.iter().enumerate() {
        if let UiAccountData::Json(value) = &account.account.data {
            if let Some(info) = value.parsed.get("info") {
                if let Some(token_amount) = info.get("tokenAmount") {
                    let amount = token_amount.get("amount").and_then(|a| a.as_str()).unwrap_or("0");
                    let decimals = token_amount.get("decimals").and_then(|d| d.as_u64()).unwrap_or(0);
                    let mint = info.get("mint").and_then(|m| m.as_str()).unwrap_or("unknown");
                    
                    let amount_u64 = amount.parse::<u64>().unwrap_or(0);
                    if amount_u64 > 0 {
                        let display_amount = amount_u64 as f64 / (10_u64.pow(decimals as u32) as f64);
                        info!("  [{}] {}: {} (mint: {})", i + 1, mint, display_amount, mint);
                    }
                }
            }
        }
    }
    
    Ok(())
}

// 卖出所有代币
async fn sell_all_tokens(executor: &Arc<TradeExecutor>, slippage: f64) -> Result<()> {
    info!("💰 开始卖出所有代币");
    info!("📊 滑点设置: {}%", slippage);
    
    // 获取所有代币账户
    let token_accounts = executor.client.get_token_accounts_by_owner(
        &executor.copy_wallet.pubkey(),
        TokenAccountsFilter::ProgramId(spl_token::id()),
    )?;
    
    if token_accounts.is_empty() {
        info!("没有代币需要卖出");
        return Ok(());
    }
    
    let mut sold_count = 0;
    let mut failed_count = 0;
    
    for account in token_accounts {
        if let UiAccountData::Json(value) = &account.account.data {
            if let Some(info) = value.parsed.get("info") {
                if let Some(token_amount) = info.get("tokenAmount") {
                    let amount = token_amount.get("amount").and_then(|a| a.as_str()).unwrap_or("0");
                    let decimals = token_amount.get("decimals").and_then(|d| d.as_u64()).unwrap_or(0);
                    let mint = info.get("mint").and_then(|m| m.as_str()).unwrap_or("unknown");
                    
                    let amount_u64 = amount.parse::<u64>().unwrap_or(0);
                    if amount_u64 > 0 {
                        let display_amount = amount_u64 as f64 / (10_u64.pow(decimals as u32) as f64);
                        
                        // 跳过WSOL和USDC
                        if mint == crate::types::WSOL_MINT || mint == crate::types::USDC_MINT {
                            info!("跳过稳定币: {} (余额: {})", mint, display_amount);
                            continue;
                        }
                        
                        info!("尝试卖出: {} (余额: {})", mint, display_amount);
                        
                        // 尝试通过代币符号卖出
                        if let Some(token_symbol) = get_token_symbol_by_mint(mint) {
                            match sell_token_by_mint(executor, mint, amount_u64, slippage).await {
                                Ok(_) => {
                                    info!("✅ 成功卖出: {} {}", display_amount, token_symbol);
                                    sold_count += 1;
                                }
                                Err(e) => {
                                    error!("❌ 卖出失败: {} - {}", token_symbol, e);
                                    failed_count += 1;
                                }
                            }
                        } else {
                            // 尝试通过mint地址卖出
                            match sell_token_by_mint(executor, mint, amount_u64, slippage).await {
                                Ok(_) => {
                                    info!("✅ 成功卖出: {} (mint: {})", display_amount, mint);
                                    sold_count += 1;
                                }
                                Err(e) => {
                                    error!("❌ 卖出失败: {} - {}", mint, e);
                                    failed_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    info!("🎯 卖出完成:");
    info!("   成功卖出: {} 个代币", sold_count);
    info!("   失败: {} 个代币", failed_count);
    
    Ok(())
}

// 通过mint地址卖出代币
async fn sell_token_by_mint(executor: &Arc<TradeExecutor>, mint: &str, amount: u64, slippage: f64) -> Result<()> {
    let token_mint = Pubkey::from_str(mint)?;
    
    // 查找对应的交易池
    let pool_info = find_pool_for_token(&token_mint)?;
    
    // 查询池子当前状态
    let pool_state_pubkey = Pubkey::from_str(&pool_info.pool_state)?;
    let pool_account = executor.client.get_account(&pool_state_pubkey)?;
    
    // 解析池子数据获取vault地址
    let pool_data = pool_account.data.as_slice();
    if pool_data.len() < 328 {
        return Err(anyhow::anyhow!("池子数据长度不足，实际长度: {}", pool_data.len()));
    }
    
    // 读取池子中的vault地址
    let input_vault = Pubkey::new_from_array(pool_data[72..104].try_into()?);
    let output_vault = Pubkey::new_from_array(pool_data[104..136].try_into()?);
    
    // 查询vault账户的余额
    let input_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.input_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    let output_vault_balance = executor.pool_cache.get_pool_info(&executor.client, &pool_state_pubkey).await?.output_vault_balance.load(std::sync::atomic::Ordering::Relaxed);
    
    // 计算预期输出
    let expected_output = crate::trade_executor::raydium_cpmm_quote(
        amount,
        input_vault_balance,
        output_vault_balance,
        25,
        10000,
    );
    
    // 计算最小输出金额（滑点保护）
    let slippage_bps = (slippage * 10000.0) as u64; // 转换为basis points (0.5 -> 5000 bps)
    let min_amount_out = crate::trade_executor::calculate_min_amount_out_with_safety(
        expected_output,
        slippage_bps,
        50, // 额外50基点安全边际
    );
    
    // 创建TradeDetails
    let trade = TradeDetails {
        signature: format!("sell_all_{}", chrono::Utc::now().timestamp()),
        wallet: executor.copy_wallet.pubkey(),
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Sell,
        token_in: TokenInfo {
            mint: token_mint,
            symbol: None,
            decimals: 6,
        },
        token_out: TokenInfo {
            mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        amount_in: amount,
        amount_out: expected_output,
        price: 0.0,
        pool_address: Pubkey::from_str(&pool_info.pool_state)?,
        timestamp: chrono::Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str(crate::types::RAYDIUM_CPMM)?,
    };
    
    // 构建Raydium CPMM账户
    let cpmm_accounts = RaydiumCpmmSwapAccounts {
        payer: executor.copy_wallet.pubkey(),
        authority: Pubkey::from_str(&pool_info.authority)?,
        amm_config: Pubkey::from_str(&pool_info.amm_config)?,
        pool_state: Pubkey::from_str(&pool_info.pool_state)?,
        user_input_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &token_mint),
        user_output_ata: get_associated_token_address(&executor.copy_wallet.pubkey(), &Pubkey::from_str(crate::types::WSOL_MINT)?),
        input_vault: Pubkey::from_str(&pool_info.output_vault)?,
        output_vault: Pubkey::from_str(&pool_info.input_vault)?,
        input_token_program: Pubkey::from_str(&pool_info.output_token_program)?,
        output_token_program: Pubkey::from_str(&pool_info.input_token_program)?,
        input_mint: token_mint,
        output_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        observation_state: Pubkey::from_str(&pool_info.observation_state)?,
        token_0_mint: Pubkey::from_str(crate::types::WSOL_MINT)?,
        token_1_mint: token_mint,
    };
    
    // 执行交易
    match TradeExecutor::execute_raydium_cpmm_trade_static(
        &executor.client,
        &executor.copy_wallet,
        &trade,
        &cpmm_accounts,
        &[],
        min_amount_out,
        None,
        None,
        None,
        None,
        None,
    ).await {
        Ok(executed_trade) => {
            if executed_trade.success {
                info!("✅ 卖出成功: {}", executed_trade.copy_signature);
            } else {
                return Err(anyhow::anyhow!("卖出失败: {}", executed_trade.error_message.unwrap_or_default()));
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!("卖出执行失败: {}", e));
        }
    }
    
    Ok(())
}

// 通过mint地址获取代币符号
fn get_token_symbol_by_mint(mint: &str) -> Option<String> {
    let token_mints = get_token_mints();
    for (symbol, mint_addr) in token_mints {
        if mint_addr == mint {
            return Some(symbol);
        }
    }
    None
}