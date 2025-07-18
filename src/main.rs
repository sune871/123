// https://solana-rpc.publicnode.com/f884f7c2cfa0e7ecbf30e7da70ec1da91bda3c9d04058269397a5591e7fd013e";
// CuwxHwz42cNivJqWGBk6HcVvfGq47868Mo6zi4u6z9vC

mod parser;
mod types;
mod grpc_monitor;
mod dex;
mod config;
mod trade_executor;
mod trade_recorder;
mod test_runner;
mod mock_monitor;
mod pool_cache;
mod fast_executor;

use anyhow::Result;
use grpc_monitor::GrpcMonitor;
use trade_executor::TradeExecutor;
use trade_recorder::TradeRecorder;
use test_runner::TestRunner;
use mock_monitor::MockMonitor;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tracing::{info, error, warn};
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::signature::Keypair;
use solana_client::rpc_client::RpcClient;
use anyhow::Context;
use solana_sdk::signer::Signer;
use std::process::Command;
use std::sync::Arc;
use crate::pool_cache::PoolCache;
use futures::FutureExt;

/// 运行测试模式
async fn run_test_mode() -> Result<()> {
    let test_runner = TestRunner::new()?;
    test_runner.run_all_tests().await
}

/// 运行性能测试
async fn run_performance_test() -> Result<()> {
    let test_runner = TestRunner::new()?;
    test_runner.run_performance_test()
}

/// 运行模拟监控模式
async fn run_mock_mode() -> Result<()> {
    // 加载配置
    let config = config::Config::load()?;
    info!("配置加载成功");
    
    // 获取目标钱包
    let wallet_address = &config.target_wallets[0];
    let wallet_pubkey = Pubkey::from_str(wallet_address)?;
    
    // 创建模拟监控器
    let mut mock_monitor = MockMonitor::new(wallet_pubkey)?;
    
    // 启动模拟监控
    match mock_monitor.start_monitoring().await {
        Ok(_) => info!("模拟监控正常结束"),
        Err(e) => error!("模拟监控出错: {}", e),
    }
    
    Ok(())
}

/// 正常运行模式
async fn run_normal_mode() -> Result<()> {
    // 加载配置
    let config = config::Config::load()?;
    info!("配置加载成功");
    
    // 创建交易记录器
    let recorder = TradeRecorder::new("trades/trade_records.json");
    recorder.ensure_directory()?;
    info!("交易记录器初始化完成");
    
    // 创建交易执行器
    let executor = TradeExecutor::new(&config.rpc_url, config.get_execution_config())?;
    
    // 显示钱包余额
    match executor.get_wallet_balance() {
        Ok(balance) => {
            info!("跟单钱包余额: {:.6} SOL", balance);
        }
        Err(e) => {
            warn!("无法获取钱包余额: {}", e);
        }
    }
    
    // 配置信息
    let grpc_endpoint = "https://solana-yellowstone-grpc.publicnode.com:443";
    let auth_token = Some("your-auth-token".to_string());
    let wallet_address = &config.target_wallets[0];
    let wallet_pubkey = Pubkey::from_str(wallet_address)?;
    
    // 创建gRPC监控器（传入交易执行器和记录器）
    let monitor = GrpcMonitor::new_with_executor_and_recorder(
        grpc_endpoint.to_string(),
        auth_token,
        wallet_pubkey,
        std::sync::Arc::new(executor),
        recorder,
    );
    
    // 启动监控
    match monitor.start_monitoring().await {
        Ok(_) => info!("监控程序正常结束"),
        Err(e) => error!("监控程序出错: {}", e),
    }
    
    Ok(())
}

/// 打印使用说明
fn print_usage() {
    println!("Solana钱包监控和跟单程序");
    println!();
    println!("使用方法:");
    println!("  cargo run                    # 正常运行模式");
    println!("  cargo run --test             # 运行测试模式");
    println!("  cargo run --performance      # 运行性能测试");
    println!("  cargo run --mock             # 运行模拟监控模式");
    println!("  cargo run --update-pools     # 拉取最新池子参数");
    println!("  cargo run --help             # 显示此帮助信息");
    println!();
    println!("模式说明:");
    println!("  正常运行模式: 连接真实gRPC服务，监控真实交易");
    println!("  测试模式: 验证程序核心功能，无需网络连接");
    println!("  性能测试: 模拟处理1000个交易并测量性能");
    println!("  模拟监控: 生成模拟交易数据，测试交易处理流程");
    println!();
    println!("测试模式将验证:");
    println!("  - 配置加载和验证");
    println!("  - 交易解析功能");
    println!("  - 交易记录功能");
    println!("  - 模拟交易处理");
}

// 在main.rs末尾添加主动卖出TAKI换WSOL的测试函数
async fn sell_taki_test() -> Result<()> {
    use std::sync::Arc;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_client::rpc_client::RpcClient;
    use crate::trade_executor::{TradeExecutor, RaydiumCpmmSwapAccounts};
    use crate::types::{TradeDetails, TokenInfo, DexType, TradeDirection};
    use spl_associated_token_account::get_associated_token_address;
    use chrono::Utc;
    use std::str::FromStr;
    use tracing::{info, warn, error};

    // === 1. 初始化 ===
    let config = config::Config::load()?;
    let rpc_client = RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );
    let private_key_bytes = bs58::decode(&config.copy_wallet_private_key)
        .into_vec()
        .context("无法解码私钥")?;
    let copy_wallet = Arc::new(Keypair::from_bytes(&private_key_bytes)
        .context("无法从私钥创建钱包")?);
    let user_pubkey = copy_wallet.pubkey();

    // === 2. 定义代币和池子信息 ===
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    let taki_mint = Pubkey::from_str("4AXnbEf3N3iLNChHL2TWHcyMnBKEvJLJ82okFFUFbonk").unwrap();
    let pool_state = Pubkey::from_str("GHq3zKabrM5k8tuEDz92hF5ZYMsszigytY6oUFhMYM2N").unwrap();

    // === 3. 获取TAKI余额 ===
    let taki_ata = get_associated_token_address(&user_pubkey, &taki_mint);
    let taki_balance_response = rpc_client.get_token_account_balance(&taki_ata)?;
    let taki_balance = taki_balance_response.amount.parse::<u64>().unwrap_or(0);
    
    if taki_balance == 0 {
        warn!("TAKI余额为0，无需卖出");
        return Ok(());
    }
    
    info!("检测到TAKI余额: {} ({})", 
        taki_balance, 
        taki_balance as f64 / 1_000_000_000.0
    );

    // === 4. 使用PoolCache动态获取池子参数 ===
    let pool_cache = PoolCache::new(300);
    if let Err(e) = pool_cache.load_from_file() {
        warn!("加载池子文件失败: {}", e);
    }
    
    let cpmm_accounts = match pool_cache.build_swap_accounts(
        &rpc_client,
        &pool_state,
        &user_pubkey,
        &taki_mint,
        &wsol_mint
    ) {
        Ok(accounts) => accounts,
        Err(e) => {
            error!("构建CPMM账户失败: {}", e);
            return Err(e);
        }
    };

    // === 5. 构造卖出交易 ===
    let trade = TradeDetails {
        signature: "manual-sell-all-taki".to_string(),
        wallet: user_pubkey,
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Sell,
        token_in: TokenInfo {
            mint: taki_mint,
            symbol: Some("TAKI".to_string()),
            decimals: 9,
        },
        token_out: TokenInfo {
            mint: wsol_mint,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        amount_in: taki_balance, // 卖出全部余额
        amount_out: 0,
        price: 0.0,
        pool_address: pool_state,
        timestamp: Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap(),
    };

    let extra_accounts: Vec<Pubkey> = vec![];
    let min_amount_out = 1; // 设置最小为1，接受任何价格

    // === 6. 执行卖出交易 ===
    info!("准备卖出所有TAKI代币...");
    info!("卖出数量: {} TAKI", taki_balance as f64 / 1_000_000_000.0);
    
    let result = TradeExecutor::execute_raydium_cpmm_trade_static(
        &rpc_client,
        &copy_wallet,
        &trade,
        &cpmm_accounts,
        &extra_accounts,
        min_amount_out,
        None, None, None, None, None
    ).await?;
    
    println!("卖出结果: {:?}", result);
    
    // === 7. 验证卖出后的余额 ===
    if result.success {
        let new_balance_response = rpc_client.get_token_account_balance(&taki_ata)?;
        let new_balance = new_balance_response.amount.parse::<u64>().unwrap_or(0);
        info!("卖出后TAKI余额: {} ({})", 
            new_balance, 
            new_balance as f64 / 1_000_000_000.0
        );
    }
    
    Ok(())
}

// 修改 buy_taki_test 签名
async fn buy_taki_test(pool_cache: Arc<PoolCache>) -> Result<()> {
    use std::sync::Arc;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_client::rpc_client::RpcClient;
    use crate::trade_executor::{TradeExecutor, RaydiumCpmmSwapAccounts};
    use crate::types::{TradeDetails, TokenInfo, DexType, TradeDirection};
    use spl_associated_token_account::get_associated_token_address;
    use chrono::Utc;
    use std::str::FromStr;

    // 初始化
    let config = config::Config::load()?;
    let rpc_client = RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );
    let private_key_bytes = bs58::decode(&config.copy_wallet_private_key)
        .into_vec()
        .context("无法解码私钥")?;
    let copy_wallet = Arc::new(Keypair::from_bytes(&private_key_bytes)
        .context("无法从私钥创建钱包")?);
    let user_pubkey = copy_wallet.pubkey();

    // 池子地址（请根据实际需求修改）
    let pool_state = Pubkey::from_str("GHq3zKabrM5k8tuEDz92hF5ZYMsszigytY6oUFhMYM2N").unwrap();
    
    // 从文件加载现有池子
    if let Err(e) = pool_cache.load_from_file() {
        warn!("加载池子文件失败: {}", e);
    }
    
    // 目标买入币种 TAKI
    let target_buy_mint = Pubkey::from_str("4AXnbEf3N3iLNChHL2TWHcyMnBKEvJLJ82okFFUFbonk")?;
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;

    // 动态获取池子参数
    let pool_param = match pool_cache.get_pool_params(&rpc_client, &pool_state) {
        Ok(params) => params,
        Err(e) => {
            error!("获取池子参数失败: {}", e);
            return Err(e);
        }
    };

    // 构建交易账户
    let cpmm_accounts = match pool_cache.build_swap_accounts(
        &rpc_client,
        &pool_state,
        &user_pubkey,
        &wsol_mint,
        &target_buy_mint
    ) {
        Ok(accounts) => accounts,
        Err(e) => {
            error!("构建CPMM账户失败: {}", e);
            return Err(e);
        }
    };
    
    info!("提取的池子参数：");
    info!("  input_mint: {}", cpmm_accounts.input_mint);
    info!("  output_mint: {}", cpmm_accounts.output_mint);
    info!("  observation_state: {}", cpmm_accounts.observation_state);
    
    // 构造交易详情
    let amount_in = (0.01 * 1_000_000_000.0) as u64;
    let trade = TradeDetails {
        signature: "manual-buy-taki".to_string(),
        wallet: user_pubkey,
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Buy,
        token_in: TokenInfo {
            mint: cpmm_accounts.input_mint,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        token_out: TokenInfo {
            mint: cpmm_accounts.output_mint,
            symbol: Some("TAKI".to_string()),
            decimals: 9,
        },
        amount_in,
        amount_out: 0,
        price: 0.0,
        pool_address: pool_state,
        timestamp: Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap(),
    };
    
    let extra_accounts: Vec<Pubkey> = vec![];
    let min_amount_out = 1;
    
    let result = TradeExecutor::execute_raydium_cpmm_trade_static(
        &rpc_client,
        &copy_wallet,
        &trade,
        &cpmm_accounts,
        &extra_accounts,
        min_amount_out,
        None, None, None, None, None
    ).await?;
    
    println!("主动买入TAKI结果: {:?}", result);
    Ok(())
}

// 自动全部卖出TAKI换WSOL（自动查链参数+余额）
async fn sell_taki_all_test() -> Result<()> {
    use std::sync::Arc;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_client::rpc_client::RpcClient;
    use crate::trade_executor::{TradeExecutor};
    use crate::types::{TradeDetails, TokenInfo, DexType, TradeDirection};
    use spl_associated_token_account::get_associated_token_address;
    use chrono::Utc;
    use std::str::FromStr;
    use tracing::info;

    // === 1. 初始化 ===
    let config = config::Config::load()?;
    let rpc_client = RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );
    let private_key_bytes = bs58::decode(&config.copy_wallet_private_key)
        .into_vec()
        .expect("无法解码私钥");
    let copy_wallet = Arc::new(Keypair::from_bytes(&private_key_bytes)
        .expect("无法从私钥创建钱包"));
    let user_pubkey = copy_wallet.pubkey();

    // === 2. 池子地址（和买入一致） ===
    let pool_state = Pubkey::from_str("GHq3zKabrM5k8tuEDz92hF5ZYMsszigytY6oUFhMYM2N").unwrap();

    // === 3. 使用PoolCache动态获取池子参数 ===
    let pool_cache = PoolCache::new(300);
    if let Err(e) = pool_cache.load_from_file() {
        warn!("加载池子文件失败: {}", e);
    }
    
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    let taki_mint = Pubkey::from_str("4AXnbEf3N3iLNChHL2TWHcyMnBKEvJLJ82okFFUFbonk").unwrap();
    
    let cpmm_accounts = match pool_cache.build_swap_accounts(
        &rpc_client,
        &pool_state,
        &user_pubkey,
        &taki_mint,
        &wsol_mint
    ) {
        Ok(accounts) => accounts,
        Err(e) => {
            error!("构建CPMM账户失败: {}", e);
            return Err(e);
        }
    };

    info!("修正后池子参数：");
    info!("  input_mint(卖出): {}", cpmm_accounts.input_mint);
    info!("  output_mint(换回): {}", cpmm_accounts.output_mint);
    info!("  observation_state: {}", cpmm_accounts.observation_state);

    // === 4. 查询用户TAKI余额 ===
    let user_taki_ata = cpmm_accounts.user_input_ata;
    let taki_balance = rpc_client.get_token_account_balance(&user_taki_ata)
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    if taki_balance == 0 {
        println!("当前TAKI余额为0，无需卖出。");
        return Ok(());
    }
    info!("用户TAKI余额: {}（准备全部卖出）", taki_balance);

    // === 5. 构造TradeDetails（全部卖出TAKI换WSOL） ===
    let trade = TradeDetails {
        signature: "manual-sell-taki-all".to_string(),
        wallet: user_pubkey,
        dex_type: DexType::RaydiumCPMM,
        trade_direction: TradeDirection::Sell,
        token_in: TokenInfo {
            mint: taki_mint,
            symbol: Some("TAKI".to_string()),
            decimals: 9,
        },
        token_out: TokenInfo {
            mint: wsol_mint,
            symbol: Some("WSOL".to_string()),
            decimals: 9,
        },
        amount_in: taki_balance,
        amount_out: 0,
        price: 0.0,
        pool_address: pool_state,
        timestamp: Utc::now().timestamp(),
        gas_fee: 0,
        program_id: Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap(),
    };

    let extra_accounts: Vec<Pubkey> = vec![];
    let min_amount_out = 1;

    // === 6. 执行主动全部卖出 ===
    let result = TradeExecutor::execute_raydium_cpmm_trade_static(
        &rpc_client,
        &copy_wallet,
        &trade,
        &cpmm_accounts,
        &extra_accounts,
        min_amount_out,
        None, None, None, None, None
    ).await?;
    println!("主动全部卖出TAKI结果: {:?}", result);
    Ok(())
}

fn check_wsol_balance_or_exit(rpc: &RpcClient, wallet: &Keypair, min_required: u64) {
    let wsol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    let wsol_ata = get_associated_token_address(&wallet.pubkey(), &wsol_mint);
    let wsol_balance = rpc.get_token_account_balance(&wsol_ata)
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    if wsol_balance < min_required {
        tracing::error!("[启动检查] 跟单钱包WSOL余额不足，当前余额: {}，请手动补充WSOL后再启动！", wsol_balance);
        std::process::exit(1);
    } else {
        tracing::info!("[启动检查] 跟单钱包WSOL余额充足: {}", wsol_balance);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志系统
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    
    info!("🚀 启动Solana钱包监控和跟单程序");
    
    // 读取配置，初始化钱包和RPC
    let config = config::Config::load()?;
    let rpc_client = RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );
    let private_key_bytes = bs58::decode(&config.copy_wallet_private_key)
        .into_vec()
        .context("无法解码私钥")?;
    let copy_wallet = Keypair::from_bytes(&private_key_bytes)
        .context("无法从私钥创建钱包")?;
    let min_required = 10_000_000; // 0.01 SOL，或自定义
    check_wsol_balance_or_exit(&rpc_client, &copy_wallet, min_required);

    // ====== 创建池子缓存并预加载 ======
    let pool_cache_arc = Arc::new(PoolCache::new(300)); // 300秒缓存
    
    // 从文件加载现有池子
    if let Err(e) = pool_cache_arc.load_from_file() {
        warn!("加载池子文件失败: {}", e);
    }
    
    let common_pools = vec![
        "GHq3zKabrM5k8tuEDz92hF5ZYMsszigytY6oUFhMYM2N", // WSOL-TAKI
    ];
    pool_cache_arc.preload_pools(&rpc_client, common_pools)?;
    info!("池子参数预加载完成");

    // ====== 启动定期缓存清理任务 ======
    let pool_cache_for_cleanup = Arc::clone(&pool_cache_arc);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // 每小时清理一次
        loop {
            interval.tick().await;
            
            // 获取缓存统计
            let (total, expired, total_accesses) = pool_cache_for_cleanup.get_cache_stats();
            info!("缓存统计: 总数={}, 过期={}, 总访问={}", total, expired, total_accesses);
            
            // 如果缓存过大，进行清理
            if total > 50 {
                if let Err(e) = pool_cache_for_cleanup.cleanup_cache() {
                    error!("缓存清理失败: {}", e);
                }
            }
            
            // 显示热门池子
            let hot_pools = pool_cache_for_cleanup.get_hot_pools(5);
            if !hot_pools.is_empty() {
                info!("热门池子 TOP 5:");
                for (i, (pool, count)) in hot_pools.iter().enumerate() {
                    info!("  {}. {} (访问次数: {})", i + 1, pool, count);
                }
            }
        }
    });

    // ====== 处理命令行参数 ======
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "--test" | "-t" => {
                info!("🧪 运行测试模式...");
                return run_test_mode().await;
            }
            "--performance" | "-p" => {
                info!("⚡ 运行性能测试...");
                return run_performance_test().await;
            }
            "--mock" | "-m" => {
                info!("🎭 运行模拟监控模式...");
                return run_mock_mode().await;
            }
            "--buy-taki" => {
                info!("🪙 主动买入TAKI测试...");
                return buy_taki_test(Arc::clone(&pool_cache_arc)).await;
            }
            "--sell-taki" => {
                info!("💱 主动卖出TAKI换WSOL测试...");
                return sell_taki_test().await;
            }
            "--sell-taki-all" => {
                info!("💱 主动全部卖出TAKI（自动查链参数+余额）...");
                return sell_taki_all_test().await;
            }
            "--update-pools" => {
                info!("⏬ 正在拉取最新池子参数...");
                let status = Command::new("cargo")
                    .args(&["run", "--bin", "fetch_pools"])
                    .status()
                    .expect("failed to update pools");
                if status.success() {
                    println!("池子参数已成功更新！");
                } else {
                    eprintln!("池子参数更新失败，请检查fetch_pools脚本和网络连接。");
                }
                return Ok(());
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            _ => {
                error!("未知参数: {}", args[1]);
                print_usage();
                return Ok(());
            }
        }
    }
    
    // 创建交易记录器
    let recorder = TradeRecorder::new("trades/trade_records.json");
    recorder.ensure_directory()?;
    info!("交易记录器初始化完成");
    
    // 创建交易执行器
    let executor = TradeExecutor::new(&config.rpc_url, config.get_execution_config())?;
    
    // 显示钱包余额
    match executor.get_wallet_balance() {
        Ok(balance) => {
            info!("跟单钱包余额: {:.6} SOL", balance);
        }
        Err(e) => {
            warn!("无法获取钱包余额: {}", e);
        }
    }
    
    // 配置信息
    let grpc_endpoint = "https://solana-yellowstone-grpc.publicnode.com:443";
    let auth_token = Some("your-auth-token".to_string());
    let wallet_address = &config.target_wallets[0];
    let wallet_pubkey = Pubkey::from_str(wallet_address)?;
    
    // 创建gRPC监控器（传入交易执行器和记录器）
    let monitor = GrpcMonitor::new_with_executor_and_recorder(
        grpc_endpoint.to_string(),
        auth_token,
        wallet_pubkey,
        std::sync::Arc::new(executor),
        recorder,
    );
    
    // 启动监控
    match monitor.start_monitoring().await {
        Ok(_) => info!("监控程序正常结束"),
        Err(e) => error!("监控程序出错: {}", e),
    }
    
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .then(|_| async {
            info!("收到终止信号，正在关闭...");
        })
        .await;
}