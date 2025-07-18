use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use solana_sdk::pubkey::Pubkey;
use solana_client::rpc_client::RpcClient;
use anyhow::Result;
use tracing::{info, warn, error};
use crate::trade_executor::RaydiumCpmmSwapAccounts;
use std::str::FromStr;
use std::fs;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer, Deserialize, Deserializer};
use serde::de::{self, Visitor, MapAccess, Error as DeError};
use std::fmt;
use std::collections::BTreeMap;
use std::collections::HashSet;
use crate::rpc_extensions::{RpcClientExt, get_account_with_timeout_async};
use tokio::time::timeout;

fn pubkey_from_str<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Pubkey::from_str(&s).map_err(DeError::custom)
}

fn pubkey_to_str<S>(pk: &Pubkey, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&pk.to_string())
}

#[derive(Clone, Debug, Serialize)]
pub struct CachedPoolParams {
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub pool_state: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub authority: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub amm_config: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub input_vault: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub output_vault: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub input_mint: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub output_mint: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub observation_state: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub input_token_program: Pubkey,
    #[serde(deserialize_with = "pubkey_from_str", serialize_with = "pubkey_to_str")]
    pub output_token_program: Pubkey,
    #[serde(default)]
    pub access_count: u64,
    #[serde(skip)]
    pub last_updated: Instant,
    #[serde(skip)]
    pub last_accessed: Instant,
}

impl<'de> Deserialize<'de> for CachedPoolParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            #[serde(deserialize_with = "pubkey_from_str")]
            pool_state: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            authority: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            amm_config: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            input_vault: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            output_vault: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            input_mint: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            output_mint: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            observation_state: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            input_token_program: Pubkey,
            #[serde(deserialize_with = "pubkey_from_str")]
            output_token_program: Pubkey,
            #[serde(default)]
            access_count: u64,
        }
        let helper = Helper::deserialize(deserializer)?;
        Ok(CachedPoolParams {
            pool_state: helper.pool_state,
            authority: helper.authority,
            amm_config: helper.amm_config,
            input_vault: helper.input_vault,
            output_vault: helper.output_vault,
            input_mint: helper.input_mint,
            output_mint: helper.output_mint,
            observation_state: helper.observation_state,
            input_token_program: helper.input_token_program,
            output_token_program: helper.output_token_program,
            access_count: helper.access_count,
            last_updated: Instant::now(),
            last_accessed: Instant::now(),
        })
    }
}

pub struct PoolCache {
    cache: Arc<RwLock<HashMap<Pubkey, CachedPoolParams>>>,
    ttl: Duration,
    cache_file: String,
    max_cache_size: usize,        // 最大缓存数量
    access_stats: Arc<RwLock<BTreeMap<Pubkey, u64>>>, // 访问统计
}

impl PoolCache {
    pub fn new(ttl_seconds: u64) -> Self {
        PoolCache {
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_seconds),
            cache_file: "cpmm_pools.json".to_string(),
            max_cache_size: 100,   // 最多缓存100个池子
            access_stats: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// 从文件加载池子参数（智能加载）
    pub fn load_from_file(&self) -> Result<()> {
        if let Ok(content) = fs::read_to_string(&self.cache_file) {
            let pools: Vec<CachedPoolParams> = serde_json::from_str(&content)?;
            let total = pools.len();
            let mut sorted_pools = pools;
            sorted_pools.sort_by(|a, b| b.access_count.cmp(&a.access_count));
            let pools_to_load = sorted_pools.into_iter().take(self.max_cache_size);
            for mut pool in pools_to_load {
                pool.last_updated = Instant::now();
                pool.last_accessed = Instant::now();
                self.cache.write().unwrap().insert(pool.pool_state, pool);
            }
            info!("智能加载了 {} 个热门池子参数（从文件中的 {} 个池子）", 
                self.cache.read().unwrap().len(), total);
        }
        Ok(())
    }

    /// 保存池子参数到文件（智能保存）
    pub fn save_to_file(&self) -> Result<()> {
        let cache = self.cache.read().unwrap();
        let access_stats = self.access_stats.read().unwrap();
        
        // 合并缓存和访问统计
        let mut pools_to_save: Vec<CachedPoolParams> = cache.values().cloned().collect();
        
        // 按访问次数排序
        pools_to_save.sort_by(|a, b| {
            let a_count = access_stats.get(&a.pool_state).unwrap_or(&0);
            let b_count = access_stats.get(&b.pool_state).unwrap_or(&0);
            b_count.cmp(a_count)
        });
        
        let content = serde_json::to_string_pretty(&pools_to_save)?;
        fs::write(&self.cache_file, content)?;
        info!("智能保存了 {} 个池子参数到文件", pools_to_save.len());
        Ok(())
    }

    /// 预加载常用池子参数
    pub async fn preload_pools(&self, rpc: &RpcClient, pool_addresses: Vec<&str>) -> Result<()> {
        info!("开始预加载 {} 个池子参数...", pool_addresses.len());
        for pool_addr in pool_addresses {
            let pool_pubkey = Pubkey::from_str(pool_addr)?;
            match self.load_pool_params_async(rpc, &pool_pubkey).await {
                Ok(params) => {
                    self.cache.write().unwrap().insert(pool_pubkey, params);
                    info!("成功缓存池子: {}", pool_addr);
                }
                Err(e) => {
                    warn!("加载池子 {} 失败: {}", pool_addr, e);
                }
            }
        }
        info!("池子参数预加载完成");
        Ok(())
    }

    /// 获取缓存的池子参数（如果不存在则从链上加载）
    pub async fn get_pool_params(&self, rpc: &RpcClient, pool_state: &Pubkey) -> Result<CachedPoolParams> {
        // 先读锁查缓存
        {
            let cache = self.cache.read().unwrap();
            if let Some(params) = cache.get(pool_state) {
                if params.last_updated.elapsed() <= self.ttl {
                    info!("[DEBUG] get_pool_params缓存命中: {}", pool_state);
                    return Ok(params.clone());
                } else {
                    warn!("[DEBUG] get_pool_params缓存过期: {}", pool_state);
                }
            }
        }
        // 释放锁后再await慢操作
        info!("[DEBUG] get_pool_params未命中缓存，开始异步加载池子参数: {}", pool_state);
        let load_result = timeout(
            std::time::Duration::from_secs(5),
            get_account_with_timeout_async(rpc, pool_state, std::time::Duration::from_secs(5))
        ).await;
        let account = match load_result {
            Ok(Ok(account)) => account,
            Ok(Err(e)) => {
                warn!("[DEBUG] get_pool_params链上加载失败: {}", e);
                return Err(e);
            }
            Err(_) => {
                warn!("[DEBUG] get_pool_params超时: {}", pool_state);
                return Err(anyhow::anyhow!("池子参数加载超时"));
            }
        };
        let data = account.data;
        let cpmm_program_id = Pubkey::from_str(crate::types::RAYDIUM_CPMM)?;
        if account.owner != cpmm_program_id {
            return Err(anyhow::anyhow!("账户 {} 不是CPMM池子，owner是: {}", pool_state, account.owner));
        }
        if data.len() < 328 {
            return Err(anyhow::anyhow!("池子数据长度不足: {} < 328", data.len()));
        }
        let params = CachedPoolParams {
            pool_state: *pool_state,
            authority: Pubkey::from_str("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL").unwrap(),
            amm_config: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            input_vault: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            output_vault: Pubkey::new_from_array(data[104..136].try_into().unwrap()),
            input_mint: Pubkey::new_from_array(data[168..200].try_into().unwrap()),
            output_mint: Pubkey::new_from_array(data[200..232].try_into().unwrap()),
            input_token_program: Pubkey::new_from_array(data[232..264].try_into().unwrap()),
            output_token_program: Pubkey::new_from_array(data[264..296].try_into().unwrap()),
            observation_state: Pubkey::new_from_array(data[296..328].try_into().unwrap()),
            last_updated: Instant::now(),
            access_count: 1,
            last_accessed: Instant::now(),
        };
        // 写锁插入缓存
        {
            let mut cache = self.cache.write().unwrap();
            if cache.len() >= self.max_cache_size {
                self.evict_least_used(&mut cache);
            }
            cache.insert(*pool_state, params.clone());
        }
        info!("[DEBUG] get_pool_params链上加载并缓存完成: {}", pool_state);
        Ok(params)
    }

    /// 淘汰最不常用的池子
    fn evict_least_used(&self, cache: &mut HashMap<Pubkey, CachedPoolParams>) {
        let stats = self.access_stats.read().unwrap();
        
        // 找到访问次数最少的池子
        let least_used = cache.iter()
            .min_by_key(|(pool_state, _)| stats.get(pool_state).unwrap_or(&0))
            .map(|(pool_state, _)| *pool_state);
        
        if let Some(pool_to_evict) = least_used {
            cache.remove(&pool_to_evict);
            info!("淘汰最不常用的池子: {} (访问次数: {})", 
                pool_to_evict, stats.get(&pool_to_evict).unwrap_or(&0));
        }
    }

    /// 动态添加新池子
    pub async fn add_pool_dynamically(&self, rpc: &RpcClient, pool_state: &Pubkey) -> Result<()> {
        info!("[DEBUG] add_pool_dynamically: {}", pool_state);
        // 先读锁查缓存
        {
            let cache = self.cache.read().unwrap();
            if cache.contains_key(pool_state) {
                info!("池子已存在: {}", pool_state);
                return Ok(());
            }
        }
        // 释放锁后再await慢操作
        let params = self.get_pool_params(rpc, pool_state).await?;
        // 写锁插入缓存（get_pool_params已做，这里可省略）
        // 保存到文件
        self.save_to_file()?;
        info!("成功添加新池子: {}", pool_state);
        Ok(())
    }

    /// 构建交易账户（极速版）
    pub async fn build_swap_accounts(&self, 
        rpc: &RpcClient,
        pool_state: &Pubkey, 
        user_pubkey: &Pubkey,
        _input_mint: &Pubkey, // 忽略外部传入
        _output_mint: &Pubkey // 忽略外部传入
    ) -> Result<RaydiumCpmmSwapAccounts> {
        let params = self.get_pool_params(rpc, pool_state).await?;
        Ok(RaydiumCpmmSwapAccounts {
            payer: *user_pubkey,
            authority: params.authority,
            amm_config: params.amm_config,
            pool_state: params.pool_state,
            user_input_ata: spl_associated_token_account::get_associated_token_address(user_pubkey, &params.input_mint),
            user_output_ata: spl_associated_token_account::get_associated_token_address(user_pubkey, &params.output_mint),
            input_vault: params.input_vault,
            output_vault: params.output_vault,
            input_token_program: params.input_token_program,
            output_token_program: params.output_token_program,
            input_mint: params.input_mint,
            output_mint: params.output_mint,
            observation_state: params.observation_state,
        })
    }

    /// 从链上加载池子参数（异步）
    pub async fn load_pool_params_async(&self, rpc: &RpcClient, pool_state: &Pubkey) -> Result<CachedPoolParams> {
        use std::time::Duration;
        let account = get_account_with_timeout_async(rpc, pool_state, Duration::from_secs(5)).await?;
        let data = account.data;
        let cpmm_program_id = Pubkey::from_str(crate::types::RAYDIUM_CPMM)?;
        if account.owner != cpmm_program_id {
            return Err(anyhow::anyhow!("账户 {} 不是CPMM池子，owner是: {}", pool_state, account.owner));
        }
        if data.len() < 328 {
            return Err(anyhow::anyhow!("池子数据长度不足: {} < 328", data.len()));
        }
        Ok(CachedPoolParams {
            pool_state: *pool_state,
            authority: Pubkey::from_str("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL").unwrap(),
            amm_config: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            input_vault: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            output_vault: Pubkey::new_from_array(data[104..136].try_into().unwrap()),
            input_mint: Pubkey::new_from_array(data[168..200].try_into().unwrap()),
            output_mint: Pubkey::new_from_array(data[200..232].try_into().unwrap()),
            input_token_program: Pubkey::new_from_array(data[232..264].try_into().unwrap()),
            output_token_program: Pubkey::new_from_array(data[264..296].try_into().unwrap()),
            observation_state: Pubkey::new_from_array(data[296..328].try_into().unwrap()),
            last_updated: Instant::now(),
            access_count: 0,
            last_accessed: Instant::now(),
        })
    }

    /// 获取缓存统计信息
    pub fn get_cache_stats(&self) -> (usize, usize, usize) {
        let cache = self.cache.read().unwrap();
        let stats = self.access_stats.read().unwrap();
        let total = cache.len();
        let expired = cache.values()
            .filter(|p| p.last_updated.elapsed() > self.ttl)
            .count();
        let total_accesses = stats.values().sum::<u64>();
        (total, expired, total_accesses as usize)
    }

    /// 清理过期和冷门池子
    pub fn cleanup_cache(&self) -> Result<()> {
        let mut cache = self.cache.write().unwrap();
        let stats = self.access_stats.read().unwrap();
        
        let before_count = cache.len();
        
        // 移除过期的池子
        cache.retain(|_, pool| pool.last_updated.elapsed() <= self.ttl);
        
        // 如果缓存仍然太大，移除最冷门的池子
        while cache.len() > self.max_cache_size / 2 {
            let least_used = cache.iter()
                .min_by_key(|(pool_state, _)| stats.get(pool_state).unwrap_or(&0))
                .map(|(pool_state, _)| *pool_state);
            
            if let Some(pool_to_remove) = least_used {
                cache.remove(&pool_to_remove);
            } else {
                break;
            }
        }
        
        let after_count = cache.len();
        info!("缓存清理完成: {} -> {} 个池子", before_count, after_count);
        
        // 保存清理后的缓存
        self.save_to_file()?;
        
        Ok(())
    }

    /// 获取热门池子列表
    pub fn get_hot_pools(&self, limit: usize) -> Vec<(Pubkey, u64)> {
        let stats = self.access_stats.read().unwrap();
        let mut hot_pools: Vec<_> = stats.iter()
            .map(|(pool, &count)| (*pool, count))
            .collect();
        hot_pools.sort_by(|a, b| b.1.cmp(&a.1));
        hot_pools.into_iter().take(limit).collect()
    }

    /// 获取所有已知池子地址
    pub fn get_all_pool_states(&self) -> HashSet<Pubkey> {
        let cache = self.cache.read().unwrap();
        cache.keys().cloned().collect()
    }
} 