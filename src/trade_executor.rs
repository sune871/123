use anyhow::{Result, Context};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use tracing::{info, warn, error};
use crate::types::{TradeDetails, TradeDirection, TradeExecutionConfig, ExecutedTrade, DexType};
use chrono::Utc;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::instruction::AccountMeta;
use solana_account_decoder::UiAccountData;
use std::str::FromStr;
use solana_client::rpc_request::TokenAccountsFilter;
// 不再引入solana_account_decoder，直接用solana_client::rpc_response::UiAccountData
use std::sync::Arc;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use crate::types::cp_swap::SwapBaseInput;
use anchor_lang::AnchorSerialize;

// Raydium池子账户结构体
#[derive(Clone)]
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
#[derive(Clone)]
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
}

pub struct TradeExecutor {
    pub client: RpcClient,
    pub copy_wallet: Arc<Keypair>,
    pub config: TradeExecutionConfig,
    pub rpc_url: String, // 新增
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
        
        let copy_wallet = Arc::new(Keypair::from_bytes(&private_key_bytes)
            .context("无法从私钥创建钱包")?);
        
        info!("交易执行器初始化完成，钱包地址: {}", copy_wallet.pubkey());
        
        Ok(TradeExecutor {
            client,
            copy_wallet,
            config,
            rpc_url: rpc_url.to_string(), // 新增
        })
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
            info!("[强制下单] 按配置金额下单: {} SOL (原链上amount_in: {:.6} SOL)", trade_amount_sol, trade.amount_in as f64 / 1_000_000_000.0);
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
                warn!("[风控] 跟单钱包无足够{}余额，跳过卖出。余额: {}，需卖出: {}", trade.token_in.symbol.as_ref().unwrap_or(&"目标币种".to_string()), total_token_balance, trade_forced_amount_in_lamports(trade_amount_sol));
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
                    warn!("[风控] SOL余额不足，无法自动兑换WSOL。SOL余额: {}，需兑换: {}", sol_balance, required);
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
                info!("[自动兑换] 正在将SOL兑换为WSOL，金额: {} lamports", required - wsol_balance);
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
                let message = Message::new(&[create_ata_ix, transfer_ix, sync_ix], Some(&self.copy_wallet.pubkey()));
                let recent_blockhash = self.client.get_latest_blockhash()?;
                let mut tx = Transaction::new_unsigned(message);
                let wallet = self.copy_wallet.clone();
                tx.sign(&[wallet.as_ref()], recent_blockhash);
                self.client.send_and_confirm_transaction(&tx)?;
                info!("[自动兑换] SOL兑换WSOL成功");
            }
        }
        info!("开始执行跟单交易:");
        info!("  原始交易: {}", trade.signature);
        info!("  交易方向: {:?}", trade.trade_direction);
        info!("  交易金额: {:.6} SOL", trade_amount_sol);
        info!("  代币: {:?}", trade.token_out.symbol);
        // 构造一个新的TradeDetails用于实际下单
        let mut trade_for_exec = trade.clone();
        if forced {
            trade_for_exec.amount_in = (trade_amount_sol * 1_000_000_000.0) as u64;
        }
        match trade.dex_type {
            DexType::RaydiumCPMM => {
                warn!("execute_trade已禁用RaydiumCPMM分支，请直接调用execute_raydium_cpmm_trade并传入正确池子参数！");
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
                warn!("不支持的DEX类型: {:?}", trade.dex_type);
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
        if let Some(tx_json) = tx_json {
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
    pub fn ensure_ata_exists_static(client: &RpcClient, wallet: &Arc<Keypair>, owner: &Pubkey, mint: &Pubkey) -> Result<()> {
        let ata = get_associated_token_address(owner, mint);
        let account = client.get_account_with_commitment(&ata, CommitmentConfig::confirmed())?.value;
        if account.is_none() {
            // owner 必须等于钱包自己
            if owner != &wallet.pubkey() {
                error!("[ATA] 创建ATA时owner不是钱包自己，owner: {}, 钱包: {}，拒绝创建！", owner, wallet.pubkey());
                return Err(anyhow::anyhow!("ATA创建owner必须是钱包自己，当前owner: {}, 钱包: {}", owner, wallet.pubkey()));
            }
            let ix = spl_associated_token_account::instruction::create_associated_token_account(
                &wallet.pubkey(), &wallet.pubkey(), mint, &spl_token::id()
            );
            let message = Message::new(&[ix], Some(&wallet.pubkey()));
            let recent_blockhash = client.get_latest_blockhash()?;
            let mut tx = Transaction::new_unsigned(message);
            tx.sign(&[wallet.as_ref()], recent_blockhash);
            client.send_and_confirm_transaction(&tx)?;
            info!("[ATA] 已自动创建ATA: {}", ata);
        }
        Ok(())
    }
    
    /// 根据链上原始TX账户顺序和权限组装swap指令
    pub fn create_raydium_cpmm_swap_ix_from_chain(
        trade: &TradeDetails,
        account_keys: &[String],
        is_signer: &[bool],
        is_writable: &[bool],
        instruction_data: Vec<u8>,
        program_id: &Pubkey,
    ) -> solana_sdk::instruction::Instruction {
        use solana_sdk::pubkey::Pubkey;
        use solana_sdk::instruction::AccountMeta;
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
        solana_sdk::instruction::Instruction {
            program_id: *program_id,
            accounts: metas,
            data: instruction_data,
        }
    }

    /// 合并ATA和swap为一笔交易（静态版，优先用链上顺序）
    pub fn combine_ata_and_swap_instructions_static(
        client: &RpcClient,
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        extra_accounts: &[Pubkey],
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
        let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(300_000);
        let compute_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(10_000);
        instructions.push(compute_budget_ix);
        instructions.push(compute_fee_ix);
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

    /// 执行Raydium CPMM交易（合并ATA+swap）
    pub async fn execute_raydium_cpmm_trade_static(
        client: &RpcClient,
        wallet: &Arc<Keypair>,
        trade: &TradeDetails,
        cpmm_accounts: &RaydiumCpmmSwapAccounts,
        extra_accounts: &[Pubkey],
        min_amount_out: u64,
        chain_account_keys: Option<&[String]>,
        chain_is_signer: Option<&[bool]>,
        chain_is_writable: Option<&[bool]>,
        chain_instruction_data: Option<Vec<u8>>,
        chain_program_id: Option<&Pubkey>,
    ) -> Result<ExecutedTrade> {
        info!("执行Raydium CPMM交易(合并ATA+swap)...");
        let recent_blockhash = client.get_latest_blockhash()?;
        let instructions = Self::combine_ata_and_swap_instructions_static(
            client, wallet, trade, cpmm_accounts, extra_accounts, min_amount_out,
            chain_account_keys, chain_is_signer, chain_is_writable, chain_instruction_data, chain_program_id
        )?;
        let message = Message::new(&instructions, Some(&wallet.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[wallet.as_ref()], recent_blockhash);
        match client.send_and_confirm_transaction(&transaction) {
            Ok(signature) => {
                info!("跟单交易成功: {}", signature);
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
                error!("跟单交易失败: {}", e);
                // 兼容所有solana-client版本，直接打印data的Debug信息
                if let solana_client::client_error::ClientErrorKind::RpcError(
                    solana_client::rpc_request::RpcError::RpcResponseError { data, .. }
                ) = &e.kind {
                    error!("[模拟日志] data: {:?}", data);
                }
                // 打印input/output ATA余额、mint、owner
                let input_ata = cpmm_accounts.user_input_ata;
                let output_ata = cpmm_accounts.user_output_ata;
                let input_ata_acc = client.get_account(&input_ata);
                let output_ata_acc = client.get_account(&output_ata);
                error!("[调试] input_ata: {} acc: {:?}", input_ata, input_ata_acc);
                error!("[调试] output_ata: {} acc: {:?}", output_ata, output_ata_acc);
                error!("[调试] input_mint: {} output_mint: {}", cpmm_accounts.input_mint, cpmm_accounts.output_mint);
                error!("[调试] payer: {} authority: {} pool_state: {} observation_state: {}", cpmm_accounts.payer, cpmm_accounts.authority, cpmm_accounts.pool_state, cpmm_accounts.observation_state);
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
    
    /// 执行Pump.fun交易
    async fn execute_pump_trade(&self, trade: &TradeDetails) -> Result<ExecutedTrade> {
        info!("执行Pump.fun交易...");
        
        // 获取最新区块哈希
        let recent_blockhash = self.client.get_latest_blockhash()?;
        
        // 创建交易指令
        let instructions = self.create_pump_instructions(trade, &PumpFunAccounts {
            fee_recipient: Pubkey::new_from_array([0; 32]),
            mint: Pubkey::new_from_array([0; 32]),
            bonding_curve: Pubkey::new_from_array([0; 32]),
            associated_bonding_curve: Pubkey::new_from_array([0; 32]),
            event_authority: Pubkey::new_from_array([0; 32]),
        }, 0)?;
        
        // 创建交易
        let message = Message::new(&instructions, Some(&self.copy_wallet.pubkey()));
        let mut transaction = Transaction::new_unsigned(message);
        
        // 签名交易
        let wallet = self.copy_wallet.clone();
        transaction.sign(&[wallet.as_ref()], recent_blockhash);
        
        // 发送交易
        match self.client.send_and_confirm_transaction(&transaction) {
            Ok(signature) => {
                info!("跟单交易成功: {}", signature);
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
                error!("跟单交易失败: {}", e);
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
        // 用官方结构体和Anchor序列化生成data
        let discriminator = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
        let params = SwapBaseInput {
            amount_in: trade.amount_in,
            min_amount_out,
        };
        let mut data = discriminator.to_vec();
        data.extend_from_slice(&params.try_to_vec()?);
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
} 

/// 自动从链上池子账户提取并校验Raydium CPMM池参数
pub fn extract_and_check_cpmm_pool_accounts(rpc: &RpcClient, pool_state: &Pubkey) -> Result<RaydiumCpmmSwapAccounts> {
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