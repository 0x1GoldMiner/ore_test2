use std::{collections::HashMap, str::FromStr};
use std::fs::{self, OpenOptions};
use serde::Deserialize;

use meteora_pools_sdk::accounts::Pool;
use meteora_vault_sdk::accounts::Vault;
use ore_api::prelude::*;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    client_error::{reqwest::StatusCode, ClientErrorKind},
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    native_token::lamports_to_sol,
    pubkey,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    slot_hashes::SlotHashes,
    transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use spl_token::{amount_to_ui_amount, ui_amount_to_amount};
use steel::{AccountDeserialize, Clock, Discriminator, Instruction};
use tokio::time::{sleep, Duration};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Deserialize)]
struct CliConfig {
    #[serde(rename = "KEYPAIR")] keypair: Option<String>,
    #[serde(rename = "RPC")] rpc: Option<String>,
    #[serde(rename = "COMMAND")] command: Option<String>,
    #[serde(rename = "AMOUNT")] amount: Option<String>,
    #[serde(rename = "SQUARE")] square: Option<String>,
    #[serde(rename = "AUTHORITY")] authority: Option<String>,
    #[serde(rename = "ID")] id: Option<String>,
    #[serde(rename = "FEE_COLLECTOR")] fee_collector: Option<String>,
    #[serde(rename = "MINT")] mint: Option<String>,
    // 新增：自动挖矿相关（按网页显示单位：SOL 小数）
    #[serde(rename = "THRESHOLD_SOL")] threshold_sol: Option<f64>,
    #[serde(rename = "MIN_SQUARES_REQUIRED")] min_squares_required: Option<usize>,
    #[serde(rename = "START_BEFORE_SECONDS")] start_before_seconds: Option<f64>,
    #[serde(rename = "PICK_SQUARES")] pick_squares: Option<usize>,
    #[serde(rename = "MAX_LOOPS")] max_loops: Option<usize>,
    // 可选：直接使用 SOL 金额（优先级低于 AMOUNT（lamports））
    #[serde(rename = "AMOUNT_SOL")] amount_sol: Option<f64>,
    // 交易费用相关配置
    #[serde(rename = "COMPUTE_UNIT_PRICE")] compute_unit_price: Option<u64>, // microlamports per compute unit
    #[serde(rename = "COMPUTE_UNIT_LIMIT")] compute_unit_limit: Option<u32>, // compute units
}

fn load_and_apply_config_from_file() {
    // 默认在当前工作目录查找 ore.config.json
    let cfg_path = "ore.config.json";
    if let Ok(bytes) = fs::read(cfg_path) {
        if let Ok(cfg) = serde_json::from_slice::<CliConfig>(&bytes) {
            let set_if_missing = |k: &str, v: &Option<String>| {
                if let Some(val) = v {
                    if std::env::var(k).is_err() {
                        std::env::set_var(k, val);
                    }
                }
            };
            set_if_missing("KEYPAIR", &cfg.keypair);
            set_if_missing("RPC", &cfg.rpc);
            set_if_missing("COMMAND", &cfg.command);
            set_if_missing("AMOUNT", &cfg.amount);
            set_if_missing("SQUARE", &cfg.square);
            set_if_missing("AUTHORITY", &cfg.authority);
            set_if_missing("ID", &cfg.id);
            set_if_missing("FEE_COLLECTOR", &cfg.fee_collector);
            set_if_missing("MINT", &cfg.mint);
            // 将 AMOUNT_SOL 转为 lamports 写入 AMOUNT（若 AMOUNT 未设置）
            if std::env::var("AMOUNT").is_err() {
                if let Some(a) = cfg.amount_sol {
                    let lamports = solana_sdk::native_token::sol_to_lamports(a);
                    std::env::set_var("AMOUNT", lamports.to_string());
                }
            }
            // 处理数值类型配置：转换为字符串并设置为环境变量
            if std::env::var("THRESHOLD_SOL").is_err() {
                if let Some(ts) = cfg.threshold_sol {
                    std::env::set_var("THRESHOLD_SOL", ts.to_string());
                }
            }
            if std::env::var("MIN_SQUARES_REQUIRED").is_err() {
                if let Some(msr) = cfg.min_squares_required {
                    std::env::set_var("MIN_SQUARES_REQUIRED", msr.to_string());
                }
            }
            if std::env::var("START_BEFORE_SECONDS").is_err() {
                if let Some(sbs) = cfg.start_before_seconds {
                    std::env::set_var("START_BEFORE_SECONDS", sbs.to_string());
                }
            }
            if std::env::var("PICK_SQUARES").is_err() {
                if let Some(ps) = cfg.pick_squares {
                    std::env::set_var("PICK_SQUARES", ps.to_string());
                }
            }
            if std::env::var("MAX_LOOPS").is_err() {
                if let Some(ml) = cfg.max_loops {
                    std::env::set_var("MAX_LOOPS", ml.to_string());
                }
            }
            if std::env::var("COMPUTE_UNIT_PRICE").is_err() {
                if let Some(cup) = cfg.compute_unit_price {
                    std::env::set_var("COMPUTE_UNIT_PRICE", cup.to_string());
                }
            }
            if std::env::var("COMPUTE_UNIT_LIMIT").is_err() {
                if let Some(cul) = cfg.compute_unit_limit {
                    std::env::set_var("COMPUTE_UNIT_LIMIT", cul.to_string());
                }
            }
            println!("[info] 已加载当前目录的 ore.config.json");
        } else {
            println!("[warn] ore.config.json 解析失败，请检查 JSON 格式是否正确。");
        }
    } else {
        println!(
            "[warn] 未在当前目录检测到 ore.config.json，将仅使用环境变量。如果是首次运行，请在当前目录创建 ore.config.json 后重试。"
        );
    }
}

#[tokio::main]
async fn main() {
    // 优先从 ore.config.json 注入缺失的环境变量
    load_and_apply_config_from_file();
    // 若仍缺少 COMMAND，默认降级为 interactive
    if std::env::var("COMMAND").is_err() {
        println!("[warn] 未设置 COMMAND，默认使用 interactive 模式。");
        std::env::set_var("COMMAND", "interactive");
    }
    // Read keypair from file
    let payer =
        read_keypair_file(&std::env::var("KEYPAIR").expect("Missing KEYPAIR env var")).unwrap();

    // Build transaction
    let rpc_url = std::env::var("RPC").expect("Missing RPC env var");
    // 使用 processed 确认级别以获得最快的数据读取（几乎实时）
    // processed < confirmed < finalized
    // - processed: 最快（~400ms），数据可能被回滚，适合实时监控
    // - confirmed: 中等（~1-2秒），需要 1 个区块确认，适合大多数场景
    // - finalized: 最慢（~30秒），需要 32 个区块确认，数据不可回滚
    // 对于自动挖矿，使用 processed 可以获得最快的响应，减少延迟导致的数据不一致
    let commitment = CommitmentConfig::processed();
    let rpc = RpcClient::new_with_commitment(rpc_url, commitment);
    match std::env::var("COMMAND")
        .expect("Missing COMMAND env var")
        .as_str()
    {
        "automations" => {
            log_automations(&rpc).await.unwrap();
        }
        "clock" => {
            log_clock(&rpc).await.unwrap();
        }
        "claim" => {
            claim(&rpc, &payer).await.unwrap();
        }
        "board" => {
            log_board(&rpc).await.unwrap();
        }
        "config" => {
            log_config(&rpc).await.unwrap();
        }
        "initialize" => {
            initialize(&rpc, &payer).await.unwrap();
        }
        "bury" => {
            bury(&rpc, &payer).await.unwrap();
        }
        "reset" => {
            reset(&rpc, &payer).await.unwrap();
        }
        "treasury" => {
            log_treasury(&rpc).await.unwrap();
        }
        "miner" => {
            log_miner(&rpc, &payer).await.unwrap();
        }
        "pool" => {
            log_meteora_pool(&rpc).await.unwrap();
        }
        "deploy" => {
            deploy(&rpc, &payer).await.unwrap();
        }
        "stake" => {
            log_stake(&rpc, &payer).await.unwrap();
        }
        "deploy_all" => {
            deploy_all(&rpc, &payer).await.unwrap();
        }
        "round" => {
            log_round(&rpc).await.unwrap();
        }
        "seeker" => {
            log_seeker(&rpc).await.unwrap();
        }
        "set_admin" => {
            set_admin(&rpc, &payer).await.unwrap();
        }
        "set_fee_collector" => {
            set_fee_collector(&rpc, &payer).await.unwrap();
        }
        "ata" => {
            ata(&rpc, &payer).await.unwrap();
        }
        "checkpoint" => {
            checkpoint(&rpc, &payer).await.unwrap();
        }
        "checkpoint_all" => {
            checkpoint_all(&rpc, &payer).await.unwrap();
        }
        "close_all" => {
            close_all(&rpc, &payer).await.unwrap();
        }
        "claim_seeker" => {
            claim_seeker(&rpc, &payer).await.unwrap();
        }
        "participating_miners" => {
            participating_miners(&rpc).await.unwrap();
        }
        "keys" => {
            keys().await.unwrap();
        }
        "auto_mine" => {
            // 命令行直接调用时，默认使用阈值算法（原算法）
            auto_mine(&rpc, &payer, SquareSelectionAlgorithm::Threshold).await.unwrap();
        }
        "interactive" => {
            interactive_menu(&rpc, &payer).await.unwrap();
        }
        _ => panic!("Invalid command"),
    };
}

async fn participating_miners(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let round_id = std::env::var("ID").expect("Missing ID env var");
    let round_id = u64::from_str(&round_id).expect("Invalid ID");
    let miners = get_miners_participating(rpc, round_id).await?;
    for (i, (_address, miner)) in miners.iter().enumerate() {
        println!("{}: {}", i, miner.authority);
    }
    Ok(())
}

async fn log_stake(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let authority = std::env::var("AUTHORITY").unwrap_or(payer.pubkey().to_string());
    let authority = Pubkey::from_str(&authority).expect("Invalid AUTHORITY");
    let staker_address = ore_api::state::stake_pda(authority).0;
    let stake = get_stake(rpc, authority).await?;
    println!("Stake");
    println!("  address: {}", staker_address);
    println!("  authority: {}", authority);
    println!(
        "  balance: {} ORE",
        amount_to_ui_amount(stake.balance, TOKEN_DECIMALS)
    );
    println!("  last_claim_at: {}", stake.last_claim_at);
    println!("  last_deposit_at: {}", stake.last_deposit_at);
    println!("  last_withdraw_at: {}", stake.last_withdraw_at);
    println!(
        "  rewards_factor: {}",
        stake.rewards_factor.to_i80f48().to_string()
    );
    println!(
        "  rewards: {} ORE",
        amount_to_ui_amount(stake.rewards, TOKEN_DECIMALS)
    );
    println!(
        "  lifetime_rewards: {} ORE",
        amount_to_ui_amount(stake.lifetime_rewards, TOKEN_DECIMALS)
    );

    Ok(())
}

async fn ata(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let user = pubkey!("FgZFnb3bi7QexKCdXWPwWy91eocUD7JCFySHb83vLoPD");
    let token = pubkey!("8H8rPiWW4iTFCfEkSnf7jpqeNpFfvdH9gLouAL3Fe2Zx");
    let ata = get_associated_token_address(&user, &token);
    let ix = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        &user,
        &token,
        &spl_token::ID,
    );
    submit_transaction(rpc, payer, &[ix]).await?;
    let account = rpc.get_account(&ata).await?;
    println!("ATA: {}", ata);
    println!("Account: {:?}", account);
    Ok(())
}

async fn keys() -> Result<(), anyhow::Error> {
    let treasury_address = ore_api::state::treasury_pda().0;
    let config_address = ore_api::state::config_pda().0;
    let board_address = ore_api::state::board_pda().0;
    let address = pubkey!("pqspJ298ryBjazPAr95J9sULCVpZe3HbZTWkbC1zrkS");
    let miner_address = ore_api::state::miner_pda(address).0;
    println!("Treasury: {}", treasury_address);
    println!("Config: {}", config_address);
    println!("Board: {}", board_address);
    println!("Miner: {}", miner_address);
    Ok(())
}

async fn initialize(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let ix = ore_api::sdk::initialize(payer.pubkey());
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

async fn claim(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let ix_sol = ore_api::sdk::claim_sol(payer.pubkey());
    let ix_ore = ore_api::sdk::claim_ore(payer.pubkey());
    submit_transaction(rpc, payer, &[ix_sol, ix_ore]).await?;
    Ok(())
}

async fn bury(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let amount_str = std::env::var("AMOUNT").expect("Missing AMOUNT env var");
    let amount_f64 = f64::from_str(&amount_str).expect("Invalid AMOUNT");
    let amount_u64 = ui_amount_to_amount(amount_f64, TOKEN_DECIMALS);
    let wrap_ix = ore_api::sdk::wrap(payer.pubkey());
    let bury_ix = ore_api::sdk::bury(payer.pubkey(), amount_u64);
    simulate_transaction(rpc, payer, &[wrap_ix, bury_ix]).await;
    Ok(())
}

async fn reset(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let board = get_board(rpc).await?;
    let config = get_config(rpc).await?;
    let slot_hashes = get_slot_hashes(rpc).await?;
    if let Some(slot_hash) = slot_hashes.get(&board.end_slot) {
        let id = get_winning_square(&slot_hash.to_bytes());
        println!("Winning square: {}", id);
    };
    let reset_ix = ore_api::sdk::reset(
        payer.pubkey(),
        config.fee_collector,
        board.round_id,
        Pubkey::default(),
    );
    submit_transaction(rpc, payer, &[reset_ix]).await?;
    Ok(())
}

async fn deploy(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let amount = std::env::var("AMOUNT").expect("Missing AMOUNT env var");
    let amount = u64::from_str(&amount).expect("Invalid AMOUNT");
    let square_id = std::env::var("SQUARE").expect("Missing SQUARE env var");
    let square_id = u64::from_str(&square_id).expect("Invalid SQUARE");
    let board = get_board(rpc).await?;
    let mut squares = [false; 25];
    squares[square_id as usize] = true;
    let ix = ore_api::sdk::deploy(
        payer.pubkey(),
        payer.pubkey(),
        amount,
        board.round_id,
        squares,
    );
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

async fn deploy_all(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let amount = std::env::var("AMOUNT").expect("Missing AMOUNT env var");
    let amount = u64::from_str(&amount).expect("Invalid AMOUNT");
    let board = get_board(rpc).await?;
    let squares = [true; 25];
    let ix = ore_api::sdk::deploy(
        payer.pubkey(),
        payer.pubkey(),
        amount,
        board.round_id,
        squares,
    );
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

// ============ 新增：自动挖矿 ============

fn read_auto_params_from_env() -> (u64, f64, usize, usize, usize) {
    // 下注金额（lamports），优先 AMOUNT，否则 0
    let amount_lamports: u64 = std::env::var("AMOUNT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // 阈值（SOL）
    let threshold_sol: f64 = std::env::var("THRESHOLD_SOL")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            // 从 ore.config.json 中（已在 load 中设置 env）
            None
        })
        .unwrap_or(0.01);

    // 最少满足条件的格子数量
    let min_squares_required: usize = std::env::var("MIN_SQUARES_REQUIRED")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(12);

    // 选择的格子数量
    let pick_squares: usize = std::env::var("PICK_SQUARES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5);

    // 最大循环次数
    let max_loops: usize = std::env::var("MAX_LOOPS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);

    (amount_lamports, threshold_sol, min_squares_required, pick_squares, max_loops)
}

// 算法类型枚举
enum SquareSelectionAlgorithm {
    Threshold,  // 阈值算法（原算法）
    Optimized,  // 最优化算法（新算法）
}

const REWARD_LOG_FILE: &str = "reward.log";

fn append_reward_log(message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(REWARD_LOG_FILE)
    {
        let _ = writeln!(file, "[{}] {}", timestamp, message);
    }
}

async fn auto_mine(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    algorithm: SquareSelectionAlgorithm,
) -> Result<(), anyhow::Error> {
    let (amount_lamports, threshold_sol, min_squares_required, pick_squares, max_loops) =
        read_auto_params_from_env();
    if amount_lamports == 0 {
        println!("[auto] AMOUNT/AMOUNT_SOL 未设置或为 0，退出。");
        return Ok(());
    }

    let mut processed_round: Option<u64> = None;
    // 保存本轮部署信息：round_id -> (格子数量, 花费 SOL)
    let mut round_deployment_info: Option<(u64, usize, u64)> = None;
    let mut loops_done: usize = 0;
    let mut total_spent: u128 = 0;

    // 持久化记录已部署轮次，避免重复部署
    const LAST_DEPLOYED_ROUND_FILE: &str = "ore.last_deployed_round";
    let read_last_deployed_round = || -> Option<u64> {
        fs::read_to_string(LAST_DEPLOYED_ROUND_FILE)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let write_last_deployed_round = |round_id: u64| {
        let _ = fs::write(LAST_DEPLOYED_ROUND_FILE, round_id.to_string());
    };
    let clear_last_deployed_round = || {
        let _ = std::fs::remove_file(LAST_DEPLOYED_ROUND_FILE);
    };

    loop {
        if loops_done >= max_loops { break; }

        // 使用重试机制处理 RPC 错误，避免因网络问题导致程序崩溃
        let board = match get_board(rpc).await {
            Ok(b) => b,
            Err(e) => {
                println!("[auto] ⚠️  读取 Board 失败: {:?}，等待 2 秒后重试...", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let clock = match get_clock(rpc).await {
            Ok(c) => c,
            Err(e) => {
                println!("[auto] ⚠️  读取 Clock 失败: {:?}，等待 2 秒后重试...", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let current_slot = clock.slot;

        // 数据一致性验证：确保 Board 和 Clock 数据是有效的
        if board.end_slot <= board.start_slot {
            println!("[auto] ⚠️  警告：Board 数据异常 (start_slot={} >= end_slot={})，等待 2 秒后重试...",
                board.start_slot, board.end_slot);
            sleep(Duration::from_secs(2)).await;
            continue;
        }

        // 使用项目原始代码中的简单计算方法（与 print_board 保持一致）
        let slot_diff = if board.end_slot > current_slot {
            board.end_slot.saturating_sub(current_slot)
        } else {
            0
        };
        let secs_left = (slot_diff as f64) * 0.4;

        // 输出状态
        println!(
            "[auto] round={} 剩余 {} slots ({:.2}s)，等待触发阈值（< START_BEFORE_SECONDS）",
            board.round_id, slot_diff, secs_left
        );

        let start_before_seconds: f64 = std::env::var("START_BEFORE_SECONDS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(40.0);

        if secs_left <= start_before_seconds {
            // 读取持久化记录，避免同一轮次重复部署（即使进程重启）
            let persisted_last = read_last_deployed_round();
            if processed_round == Some(board.round_id) || persisted_last == Some(board.round_id) {
                // 已成功部署过该回合，等待下一回合，跳过所有读取和判定
                if let Some((round_id, square_count, cost_lamports)) = round_deployment_info {
                    if round_id == board.round_id {
                        println!("[auto] 本轮 (round={}) 已部署完成：{} 个格子，花费 {:.6} SOL，等待下一轮...", 
                            board.round_id, square_count, lamports_to_sol(cost_lamports));
                    } else {
                        println!("[auto] 本轮 (round={}) 已部署完成，等待下一轮...", board.round_id);
                    }
                } else {
                    println!("[auto] 本轮 (round={}) 已部署完成，等待下一轮...", board.round_id);
                }
            } else {
                // 未成功部署，继续读取棋盘格并判定
                // 获取当前回合部署分布（使用重试机制）
                let round = match get_round(rpc, board.round_id).await {
                    Ok(r) => {
                        // 立即验证 round_id 一致性，避免使用过时的 Round 数据
                        if r.id != board.round_id {
                            println!("[auto] ⚠️  Round ID 不一致 (board.round_id={}, round.id={})，可能是新回合刚启动，等待 1 秒后重试...", board.round_id, r.id);
                            sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                        r
                    }
                    Err(e) => {
                        println!("[auto] ⚠️  读取 Round {} 失败: {:?}，等待 1 秒后重试...", board.round_id, e);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                
                // 输出调试信息：显示当前 slot 和数据获取时间
                println!("[auto] 数据获取时间: slot={}, 当前回合: {}", current_slot, board.round_id);
                
                let all_squares: Vec<(usize, f64)> = round
                    .deployed
                    .iter()
                    .enumerate()
                    .map(|(i, &lamports)| (i, lamports_to_sol(lamports)))
                    .collect();
                
                // 输出所有 25 个格子的部署情况
                println!("[auto] 当前回合所有格子的部署情况:");
                for (square_idx, sol_amt) in &all_squares {
                    print!("  #{}: {:.6} SOL  ", square_idx, sol_amt);
                    if (square_idx + 1) % 5 == 0 {
                        println!(); // 每 5 个换行，形成 5x5 网格显示
                    }
                }
                if all_squares.len() % 5 != 0 {
                    println!(); // 如果最后一行不满 5 个，也要换行
                }
                
                // 根据算法类型选择格子
                let picked = match algorithm {
                    SquareSelectionAlgorithm::Threshold => {
                        // 原算法：阈值算法
                        let mut candidates: Vec<(usize, f64)> = all_squares
                            .iter()
                            .cloned()
                            .filter(|(_, v_sol)| *v_sol < threshold_sol)
                            .collect();
                        println!(
                            "[auto] [阈值算法] 低于阈值({:.4} SOL)的格子数量: {}",
                            threshold_sol,
                            candidates.len()
                        );
                        if candidates.len() >= min_squares_required {
                            // 从小到大排序
                            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                            let picked = candidates
                                .into_iter()
                                .take(pick_squares)
                                .map(|(idx, _)| idx)
                                .collect::<Vec<_>>();
                            if picked.is_empty() {
                                println!("[auto] 未选中任何格子，跳过。");
                                None
                            } else {
                                Some(picked)
                            }
                        } else {
                            println!("[auto] 符合阈值的格子不足 {} 个，跳过本次。", min_squares_required);
                            None
                        }
                    }
                    SquareSelectionAlgorithm::Optimized => {
                        // 新算法：最优化算法
                        // 1. 统计所有25个格子的部署总和
                        let total_deployed: u64 = round.deployed.iter().sum();
                        let total_deployed_sol = lamports_to_sol(total_deployed);

                        // 2. 计算阈值：(0.036 * 部署总数) - 0.005
                        // 修复：确保运算优先级正确
                        let threshold = (total_deployed_sol * 0.036) - 0.005;

                        println!(
                            "[auto] [最优化算法] 所有格子部署总和: {:.6} SOL, 阈值: {:.6} SOL (0.036 * 总和 - 0.005)",
                            total_deployed_sol, threshold
                        );

                        // 3. 选择所有部署数量 < (0.036 * 总和 - 0.005) 的格子
                        let mut candidates: Vec<(usize, f64)> = all_squares
                            .iter()
                            .cloned()
                            .filter(|(_, v_sol)| *v_sol < threshold)
                            .collect();

                        println!(
                            "[auto] [最优化算法] 符合条件的格子数量: {}",
                            candidates.len()
                        );

                        // 检查是否符合最低下限要求
                        if candidates.len() >= min_squares_required {
                            // 从小到大排序
                            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                            // 受 PICK_SQUARES 限制
                            let picked = candidates
                                .into_iter()
                                .take(pick_squares)
                                .map(|(idx, _)| idx)
                                .collect::<Vec<_>>();
                            if picked.is_empty() {
                                println!("[auto] 未选中任何格子，跳过。");
                                None
                            } else {
                                Some(picked)
                            }
                        } else {
                            println!("[auto] [最优化算法] 符合条件的格子不足 {} 个，跳过本次。", min_squares_required);
                            None
                        }
                    }
                };

                if let Some(picked) = picked {
                        println!("[auto] 选中格子: {:?}", picked);
                        
                        // 部署前检查是否需要 checkpoint
                        // 重要：只有在满足以下条件时才执行 checkpoint：
                        // 1. miner 所在的 round_id < 当前 board 的 round_id
                        // 2. miner 尚未 checkpoint 到该 round
                        // 3. 当前轮次还有充足时间部署
                        let mut did_checkpoint = false;
                        match get_miner(rpc, payer.pubkey()).await {
                            Ok(miner) => {
                                let miner_before = miner;
                                // 修复：更严格的 checkpoint 条件检查
                                // 只有当 miner 完全处于旧轮次时才需要 checkpoint
                                if miner.round_id < board.round_id && miner.checkpoint_id < miner.round_id {
                                    println!("[auto] 检测到需要 checkpoint：miner.round_id={}, checkpoint_id={}, 当前 round_id={}",
                                        miner.round_id, miner.checkpoint_id, board.round_id);
                                    println!("[auto] 正在执行 checkpoint...");
                                    let checkpoint_ix = ore_api::sdk::checkpoint(
                                        payer.pubkey(),
                                        payer.pubkey(),
                                        miner.round_id,
                                    );
                                    match submit_transaction(rpc, payer, &[checkpoint_ix]).await {
                                        Ok(sig) => {
                                            println!("[auto] ✅ Checkpoint 成功！交易签名: {}", sig);
                                            if let Ok(miner_after) = get_miner(rpc, payer.pubkey()).await {
                                                let delta_rewards_sol = miner_after
                                                    .rewards_sol
                                                    .saturating_sub(miner_before.rewards_sol);
                                                let delta_rewards_ore = miner_after
                                                    .rewards_ore
                                                    .saturating_sub(miner_before.rewards_ore);
                                                let delta_refined_ore = miner_after
                                                    .refined_ore
                                                    .saturating_sub(miner_before.refined_ore);
                                                append_reward_log(&format!(
                                                    "round={} event=checkpoint delta_sol={:.6} delta_rewards_ore={} delta_refined_ore={} tx={}",
                                                    miner_before.round_id,
                                                    lamports_to_sol(delta_rewards_sol),
                                                    amount_to_ui_amount(
                                                        delta_rewards_ore,
                                                        TOKEN_DECIMALS
                                                    ),
                                                    amount_to_ui_amount(
                                                        delta_refined_ore,
                                                        TOKEN_DECIMALS
                                                    ),
                                                    sig
                                                ));
                                            }
                                            did_checkpoint = true;
                                        }
                                        Err(e) => {
                                            // Checkpoint 可能失败（例如 round 还未结束或已过期），尝试继续部署
                                            // 如果部署时仍然失败，会在部署阶段报错
                                            println!("[auto] ⚠️  Checkpoint 失败（可能 round 还未结束或已过期）: {:?}", e);
                                            println!("[auto] 尝试继续部署...");
                                        }
                                    }
                                } else if miner.round_id == board.round_id && miner.checkpoint_id < miner.round_id {
                                    // 同一轮但未 checkpoint，这种情况不需要 checkpoint，可以直接部署
                                    println!("[auto] Miner 已在当前轮次，无需 checkpoint，直接部署");
                                }
                            }
                            Err(e) => {
                                println!("[auto] 警告：无法读取 Miner 账户: {:?}，继续尝试部署", e);
                            }
                        }
                        // 如果刚刚执行了 checkpoint，则跳过本次部署，进入下一循环刷新最新的 board/round 状态
                        if did_checkpoint {
                            println!("[auto] 已完成 checkpoint，本次不部署，等待状态刷新...");
                            continue;
                        }
                        
                        // 部署前再次验证 Board/Round 一致性，并尽量使用最新快照，降低竞态
                        let latest_board = match get_board(rpc).await {
                            Ok(b) => b,
                            Err(e) => {
                                println!("[auto] 警告：读取 Board 失败: {:?}，跳过本次部署", e);
                                continue;
                            }
                        };

                        // 验证Round ID是否变化（说明轮次已经结束或转移）
                        if latest_board.round_id != board.round_id {
                            println!("[auto] ⚠️  轮次已变化！检测到新轮次 {} -> {}，跳过本次部署，等待下一轮", board.round_id, latest_board.round_id);
                            // 重置为新轮次，让主循环检测到变化
                            processed_round = None;
                            round_deployment_info = None;
                            clear_last_deployed_round();
                            continue;
                        }

                        let latest_round = match get_round(rpc, latest_board.round_id).await {
                            Ok(r) => r,
                            Err(e) => {
                                println!("[auto] 警告：Round 账户 {} 无法读取: {:?}，跳过本次部署", latest_board.round_id, e);
                                continue;
                            }
                        };
                        if latest_round.id != latest_board.round_id {
                            println!("[auto] 警告：Board/Round ID不一致 (board.round_id={}, round.id={})，可能正在轮次切换，跳过本次部署", latest_board.round_id, latest_round.id);
                            continue;
                        }

                        let current_slot_for_check = match get_clock(rpc).await {
                            Ok(c) => c.slot,
                            Err(e) => {
                                println!("[auto] 警告：读取 Clock 失败（检查回合结束）: {:?}，跳过本次部署", e);
                                continue;
                            }
                        };

                        // 检查轮次是否即将结束
                        let slots_remaining = if latest_board.end_slot > current_slot_for_check {
                            latest_board.end_slot - current_slot_for_check
                        } else {
                            0
                        };

                        // 定义两个阈值：
                        // - danger_zone_slots (约6秒): 在这个时间内，只进行单次快速提交，不重试
                        // - buffer_slots (约2秒): 这个时间内不再尝试提交
                        let danger_zone_slots = 15u64;  // ~6秒 (15 * 0.4秒)
                        let buffer_slots = 5u64;        // ~2秒 (5 * 0.4秒)

                        if slots_remaining <= buffer_slots {
                            println!("[auto] ⚠️  轮次即将结束：剩余 {} slots (~{:.1}s，< {:.1}s 缓冲)，跳过本次部署以避免交易过期",
                                slots_remaining, slots_remaining as f64 * 0.4, buffer_slots as f64 * 0.4);
                            continue;
                        }

                        if latest_board.end_slot <= current_slot_for_check {
                            println!("[auto] ⚠️  当前回合已结束，跳过本次部署");
                            continue;
                        }

                        // 判断是否处于危险区间（轮次剩余时间很短）
                        let is_danger_zone = slots_remaining <= danger_zone_slots;
                        if is_danger_zone {
                            println!("[auto] ⚠️  进入危险区间：轮次剩余 {:.1}s (~{} slots)，将进行单次快速提交（不重试）",
                                slots_remaining as f64 * 0.4, slots_remaining);
                        }
                        
                        let mut squares = [false; 25];
                        for &i in &picked {
                            if i < 25 {
                                squares[i] = true;
                            }
                        }

                        // 部署前记录关键信息
                        println!("[auto] 准备部署到轮次 {}，剩余时间约 {:.2}s，格子: {:?}",
                            latest_board.round_id,
                            (latest_board.end_slot as f64 - current_slot_for_check as f64) * 0.4,
                            picked);

                        let ix = ore_api::sdk::deploy(
                            payer.pubkey(),
                            payer.pubkey(),
                            amount_lamports,
                            latest_board.round_id,
                            squares,
                        );

                        // 改进错误处理：不 panic，记录错误并继续
                        let this_round_cost = (amount_lamports as u128) * (picked.len() as u128);
                        let this_round_cost_u64 =
                            this_round_cost.min(u64::MAX as u128) as u64;

                        // 根据轮次剩余时间选择提交策略
                        // 危险区间（剩余时间少于6秒）：单次快速提交，不重试
                        // 安全区间：有重试的提交
                        let submit_result = if is_danger_zone {
                            println!("[auto] 💨 危险区间：采用快速单次提交！");
                            submit_transaction_danger_zone_no_retry(rpc, payer, &[ix]).await
                        } else {
                            submit_transaction(rpc, payer, &[ix]).await
                        };

                        match submit_result {
                            Ok(sig) => {
                                println!("[auto] ✅ 部署成功！交易签名: {}", sig);
                                println!("[auto] 本次部署花费: {:.6} SOL ({} 个格子 × {:.6} SOL/格子)",
                                    lamports_to_sol(this_round_cost_u64),
                                    picked.len(),
                                    lamports_to_sol(amount_lamports));
                                total_spent += this_round_cost;
                                // 只有成功部署后，才标记为已处理，后续等待下一轮
                                processed_round = Some(latest_board.round_id);
                                // 保存本轮部署信息，用于后续循环显示
                                round_deployment_info =
                                    Some((latest_board.round_id, picked.len(), this_round_cost_u64));

                                let algo_label = match algorithm {
                                    SquareSelectionAlgorithm::Threshold => "threshold",
                                    SquareSelectionAlgorithm::Optimized => "optimized",
                                };
                                append_reward_log(&format!(
                                    "round={} event=deploy algorithm={} squares={} cost_sol={:.6} cost_lamports={} tx={}",
                                    latest_board.round_id,
                                    algo_label,
                                    picked.len(),
                                    lamports_to_sol(this_round_cost_u64),
                                    this_round_cost_u64,
                                    sig
                                ));

                                // 写入持久化记录（避免同轮次重复部署）
                                write_last_deployed_round(latest_board.round_id);

                                // 输出收益信息
                                if let Ok(miner) = get_miner(rpc, payer.pubkey()).await {
                                    println!(
                                        "[auto] 累计花费 {:.6} SOL，当前可领 ORE: {} ORE，SOL: {:.6}",
                                        lamports_to_sol(total_spent as u64),
                                        amount_to_ui_amount(miner.rewards_ore + miner.refined_ore, TOKEN_DECIMALS),
                                        lamports_to_sol(miner.rewards_sol),
                                    );
                                }
                                println!("[auto] 本轮已部署完成，等待下一轮...");
                            }
                            Err(e) => {
                                println!("[auto] ⚠️  部署失败: {:?}", e);
                                println!("[auto] 可能原因：Round 账户数据无效、账户未初始化、或网络问题。将重试。");
                                // 不设置 processed_round，下次循环继续尝试
                                // 重要：使用 latest_board.round_id 而非 board.round_id，确保轮次一致
                            }
                        }
                } else {
                    // 未选中任何格子，继续尝试
                    // 注意：不设置 processed_round，下次循环继续尝试读取和判定
                }
            }
        }

        sleep(Duration::from_millis(500)).await;

        // 重新获取最新的 board 和 clock，检查是否进入新轮次（使用重试机制）
        let new_board = match get_board(rpc).await {
            Ok(b) => b,
            Err(e) => {
                println!("[auto] ⚠️  读取 Board 失败（检查新轮次）: {:?}，等待 2 秒后重试...", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let new_clock = match get_clock(rpc).await {
            Ok(c) => c,
            Err(e) => {
                println!("[auto] ⚠️  读取 Clock 失败（检查新轮次）: {:?}，等待 2 秒后重试...", e);
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        // 检查轮次是否变化
        if new_board.round_id != board.round_id {
            // 轮次已经变化，这是正常的轮次切换
            println!("[auto] ✅ 检测到新轮次：{} -> {}", board.round_id, new_board.round_id);
            loops_done += 1;
            processed_round = None;
            round_deployment_info = None; // 清除上一轮的部署信息
            // 清除持久化记录，允许新轮次重新部署
            clear_last_deployed_round();
        } else if new_clock.slot >= board.end_slot {
            // slot 已经超过或等于 end_slot，但 round_id 还没变化
            // 这可能表示：
            // 1. 轮次正在重置过程中
            // 2. Board 账户还未更新
            // 3. 出现了网络延迟
            // 最安全的做法是再等一会，然后重新检查
            println!("[auto] ⚠️  当前 slot {} >= end_slot {}，轮次可能正在切换，等待状态更新...", new_clock.slot, board.end_slot);
            // 如果 processed_round 已设置，则等待下一个轮次；否则继续尝试
            if processed_round.is_some() {
                // 已经部署过，等待轮次变化
                println!("[auto] 已在本轮部署，等待新轮次到来...");
                sleep(Duration::from_secs(3)).await;
            }
        }
    }

    println!(
        "[auto] 结束。总花费约 {:.6} SOL",
        lamports_to_sol(total_spent as u64)
    );
    Ok(())
}

// ============ 新增：交互式菜单 ============

async fn interactive_menu(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    // 显示当前奖励
    let miner = get_miner(rpc, payer.pubkey()).await.ok();
    if let Some(m) = &miner {
        println!(
            "当前可领：SOL {:.6}，ORE {}",
            lamports_to_sol(m.rewards_sol),
            amount_to_ui_amount(m.rewards_ore + m.refined_ore, TOKEN_DECIMALS)
        );
    }
    println!("请选择：");
    println!("1) 按预设自动挖矿（阈值算法）");
    println!("2) 按预设自动挖矿（最优化算法）");
    println!("3) claim 所有 SOL");
    println!("4) claim 所有 ORE");
    println!("5) 查询账户状态（余额/是否为矿工/可领取）");
    print!("输入选项序号并回车: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    let choice = line.trim();

    match choice {
        "1" => {
            auto_mine(rpc, payer, SquareSelectionAlgorithm::Threshold).await?;
        }
        "2" => {
            auto_mine(rpc, payer, SquareSelectionAlgorithm::Optimized).await?;
        }
        "3" => {
            if let Some(m) = &miner {
                let sol_amt = lamports_to_sol(m.rewards_sol);
                if sol_amt <= 0.0 {
                    println!("当前可领 SOL 为 0，已取消。");
                    return Ok(());
                }
                println!("当前可领 SOL {:.6}。输入 y 确认领取，其他任意键取消：", sol_amt);
                let mut c = String::new();
                let _ = io::stdin().read_line(&mut c);
                if c.trim().to_lowercase() != "y" { println!("已取消。"); return Ok(()); }
            }
            let ix_sol = ore_api::sdk::claim_sol(payer.pubkey());
            submit_transaction(rpc, payer, &[ix_sol]).await?;
        }
        "4" => {
            if let Some(m) = &miner {
                let ore_amount = amount_to_ui_amount(m.rewards_ore + m.refined_ore, TOKEN_DECIMALS);
                if ore_amount <= 0.0 {
                    println!("当前可领 ORE 为 0，已取消。");
                    return Ok(());
                }
                println!("当前可领 ORE {}。输入 y 确认领取，其他任意键取消：", ore_amount);
                let mut c = String::new();
                let _ = io::stdin().read_line(&mut c);
                if c.trim().to_lowercase() != "y" { println!("已取消。"); return Ok(()); }
            }
            let ix_ore = ore_api::sdk::claim_ore(payer.pubkey());
            submit_transaction(rpc, payer, &[ix_ore]).await?;
        }
        "5" => {
            query_account_status(rpc, payer).await?;
        }
        _ => println!("已取消。"),
    }

    Ok(())
}

async fn query_account_status(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    println!("[status] 开始查询账户状态...");
    let address = payer.pubkey();
    // 基本网络连通与钱包 SOL 余额
    match rpc.get_balance(&address).await {
        Ok(lamports) => {
            println!("钱包地址: {}", address);
            println!("钱包余额: {:.6} SOL", lamports_to_sol(lamports));
        }
        Err(e) => {
            println!("[error] 无法读取钱包余额: {}", e);
            println!("可能原因：RPC 不可用/网络不匹配。");
            return Ok(());
        }
    }

    // 读取 ORE 配置与当前回合，验证网络是否存在程序状态
    match get_board(rpc).await {
        Ok(board) => {
            println!("当前回合: {}，距结束约 {:.2}s", board.round_id, (board.end_slot as f64) * 0.4);
        }
        Err(_) => {
            println!("[warn] 读取 ORE Board 失败，可能连接了错误网络（例如 devnet）。");
        }
    }

    // Miner 账户与可领取
    match get_miner(rpc, address).await {
        Ok(miner) => {
            let claimable_ore = amount_to_ui_amount(miner.rewards_ore + miner.refined_ore, TOKEN_DECIMALS);
            let claimable_sol = lamports_to_sol(miner.rewards_sol);
            println!("矿工账户: 存在");
            println!("可领取 ORE: {}", claimable_ore);
            println!("可领取 SOL: {:.6}", claimable_sol);
            println!("当前回合ID: {}，checkpoint到: {}", miner.round_id, miner.checkpoint_id);
            if claimable_ore == 0.0 && claimable_sol == 0.0 {
                println!("提示：当前无可领取奖励。如刚部署，请在回合结束后执行 checkpoint 再领取。");
            }
        }
        Err(_) => {
            println!("矿工账户: 不存在 (未注册/未初始化)。你需要先成功部署一次来创建 Miner 账户。");
        }
    }

    Ok(())
}

async fn claim_seeker(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let seeker_mint = pubkey!("5mXbkqKz883aufhAsx3p5Z1NcvD2ppZbdTTznM6oUKLj");
    let ix = ore_api::sdk::claim_seeker(payer.pubkey(), seeker_mint);
    simulate_transaction(rpc, payer, &[ix]).await;
    Ok(())
}

async fn set_admin(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let ix = ore_api::sdk::set_admin(payer.pubkey(), payer.pubkey());
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

async fn set_fee_collector(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let fee_collector = std::env::var("FEE_COLLECTOR").expect("Missing FEE_COLLECTOR env var");
    let fee_collector = Pubkey::from_str(&fee_collector).expect("Invalid FEE_COLLECTOR");
    let ix = ore_api::sdk::set_fee_collector(payer.pubkey(), fee_collector);
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

async fn checkpoint(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let authority = std::env::var("AUTHORITY").unwrap_or(payer.pubkey().to_string());
    let authority = Pubkey::from_str(&authority).expect("Invalid AUTHORITY");
    let miner = get_miner(rpc, authority).await?;
    let ix = ore_api::sdk::checkpoint(payer.pubkey(), authority, miner.round_id);
    submit_transaction(rpc, payer, &[ix]).await?;
    Ok(())
}

async fn checkpoint_all(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let clock = get_clock(rpc).await?;
    let miners = get_miners(rpc).await?;
    let mut expiry_slots = HashMap::new();
    let mut ixs = vec![];
    for (i, (_address, miner)) in miners.iter().enumerate() {
        if miner.checkpoint_id < miner.round_id {
            // Log the expiry slot for the round.
            if !expiry_slots.contains_key(&miner.round_id) {
                if let Ok(round) = get_round(rpc, miner.round_id).await {
                    expiry_slots.insert(miner.round_id, round.expires_at);
                }
            }

            // Get the expiry slot for the round.
            let Some(expires_at) = expiry_slots.get(&miner.round_id) else {
                continue;
            };

            // If we are in fee collection period, checkpoint the miner.
            if clock.slot >= expires_at - TWELVE_HOURS_SLOTS {
                println!(
                    "[{}/{}] Checkpoint miner: {} ({} s)",
                    i + 1,
                    miners.len(),
                    miner.authority,
                    (expires_at - clock.slot) as f64 * 0.4
                );
                ixs.push(ore_api::sdk::checkpoint(
                    payer.pubkey(),
                    miner.authority,
                    miner.round_id,
                ));
            }
        }
    }

    // Batch and submit the instructions.
    while !ixs.is_empty() {
        let batch = ixs
            .drain(..std::cmp::min(10, ixs.len()))
            .collect::<Vec<Instruction>>();
        submit_transaction(rpc, payer, &batch).await?;
    }

    Ok(())
}

async fn close_all(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let rounds = get_rounds(rpc).await?;
    let mut ixs = vec![];
    let clock = get_clock(rpc).await?;
    for (_i, (_address, round)) in rounds.iter().enumerate() {
        if clock.slot >= round.expires_at {
            ixs.push(ore_api::sdk::close(
                payer.pubkey(),
                round.id,
                round.rent_payer,
            ));
        }
    }

    // Batch and submit the instructions.
    while !ixs.is_empty() {
        let batch = ixs
            .drain(..std::cmp::min(12, ixs.len()))
            .collect::<Vec<Instruction>>();
        submit_transaction(rpc, payer, &batch).await?;
    }

    Ok(())
}

async fn log_meteora_pool(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let address = pubkey!("GgaDTFbqdgjoZz3FP7zrtofGwnRS4E6MCzmmD5Ni1Mxj");
    let pool = get_meteora_pool(rpc, address).await?;
    let vault_a = get_meteora_vault(rpc, pool.a_vault).await?;
    let vault_b = get_meteora_vault(rpc, pool.b_vault).await?;

    println!("Pool");
    println!("  address: {}", address);
    println!("  lp_mint: {}", pool.lp_mint);
    println!("  token_a_mint: {}", pool.token_a_mint);
    println!("  token_b_mint: {}", pool.token_b_mint);
    println!("  a_vault: {}", pool.a_vault);
    println!("  b_vault: {}", pool.b_vault);
    println!("  a_token_vault: {}", vault_a.token_vault);
    println!("  b_token_vault: {}", vault_b.token_vault);
    println!("  a_vault_lp_mint: {}", vault_a.lp_mint);
    println!("  b_vault_lp_mint: {}", vault_b.lp_mint);
    println!("  a_vault_lp: {}", pool.a_vault_lp);
    println!("  b_vault_lp: {}", pool.b_vault_lp);
    println!("  protocol_token_fee: {}", pool.protocol_token_b_fee);

    // pool: *pool.key,
    // user_source_token: *user_source_token.key,
    // user_destination_token: *user_destination_token.key,
    // a_vault: *a_vault.key,
    // b_vault: *b_vault.key,
    // a_token_vault: *a_token_vault.key,
    // b_token_vault: *b_token_vault.key,
    // a_vault_lp_mint: *a_vault_lp_mint.key,
    // b_vault_lp_mint: *b_vault_lp_mint.key,
    // a_vault_lp: *a_vault_lp.key,
    // b_vault_lp: *b_vault_lp.key,
    // protocol_token_fee: *protocol_token_fee.key,
    // user: *user.key,
    // vault_program: *vault_program.key,
    // token_program: *token_program.key,

    Ok(())
}

async fn log_automations(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let automations = get_automations(rpc).await?;
    for (i, (address, automation)) in automations.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, automations.len(), address);
        println!("  authority: {}", automation.authority);
        println!("  balance: {}", automation.balance);
        println!("  executor: {}", automation.executor);
        println!("  fee: {}", automation.fee);
        println!("  mask: {}", automation.mask);
        println!("  strategy: {}", automation.strategy);
        println!();
    }
    Ok(())
}

async fn log_treasury(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let treasury_address = ore_api::state::treasury_pda().0;
    let treasury = get_treasury(rpc).await?;
    println!("Treasury");
    println!("  address: {}", treasury_address);
    println!("  balance: {} SOL", lamports_to_sol(treasury.balance));
    println!(
        "  motherlode: {} ORE",
        amount_to_ui_amount(treasury.motherlode, TOKEN_DECIMALS)
    );
    println!(
        "  miner_rewards_factor: {}",
        treasury.miner_rewards_factor.to_i80f48().to_string()
    );
    println!(
        "  stake_rewards_factor: {}",
        treasury.stake_rewards_factor.to_i80f48().to_string()
    );
    println!(
        "  total_staked: {} ORE",
        amount_to_ui_amount(treasury.total_staked, TOKEN_DECIMALS)
    );
    println!(
        "  total_unclaimed: {} ORE",
        amount_to_ui_amount(treasury.total_unclaimed, TOKEN_DECIMALS)
    );
    println!(
        "  total_refined: {} ORE",
        amount_to_ui_amount(treasury.total_refined, TOKEN_DECIMALS)
    );
    Ok(())
}

async fn log_round(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let id = std::env::var("ID").expect("Missing ID env var");
    let id = u64::from_str(&id).expect("Invalid ID");
    let round_address = round_pda(id).0;
    let round = get_round(rpc, id).await?;
    let rng = round.rng();
    println!("Round");
    println!("  Address: {}", round_address);
    println!("  Count: {:?}", round.count);
    println!("  Deployed: {:?}", round.deployed);
    println!("  Expires at: {}", round.expires_at);
    println!("  Id: {:?}", round.id);
    println!("  Motherlode: {}", round.motherlode);
    println!("  Rent payer: {}", round.rent_payer);
    println!("  Slot hash: {:?}", round.slot_hash);
    println!("  Top miner: {:?}", round.top_miner);
    println!("  Top miner reward: {}", round.top_miner_reward);
    println!("  Total deployed: {}", round.total_deployed);
    println!("  Total vaulted: {}", round.total_vaulted);
    println!("  Total winnings: {}", round.total_winnings);
    if let Some(rng) = rng {
        println!("  Winning square: {}", round.winning_square(rng));
    }
    // if round.slot_hash != [0; 32] {
    //     println!("  Winning square: {}", get_winning_square(&round.slot_hash));
    // }
    Ok(())
}

async fn log_miner(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
) -> Result<(), anyhow::Error> {
    let authority = std::env::var("AUTHORITY").unwrap_or(payer.pubkey().to_string());
    let authority = Pubkey::from_str(&authority).expect("Invalid AUTHORITY");
    let miner_address = ore_api::state::miner_pda(authority).0;
    let miner = get_miner(&rpc, authority).await?;
    println!("Miner");
    println!("  address: {}", miner_address);
    println!("  authority: {}", authority);
    println!("  deployed: {:?}", miner.deployed);
    println!("  cumulative: {:?}", miner.cumulative);
    println!("  rewards_sol: {} SOL", lamports_to_sol(miner.rewards_sol));
    println!(
        "  rewards_ore: {} ORE",
        amount_to_ui_amount(miner.rewards_ore, TOKEN_DECIMALS)
    );
    println!(
        "  refined_ore: {} ORE",
        amount_to_ui_amount(miner.refined_ore, TOKEN_DECIMALS)
    );
    println!("  round_id: {}", miner.round_id);
    println!("  checkpoint_id: {}", miner.checkpoint_id);
    println!(
        "  lifetime_rewards_sol: {} SOL",
        lamports_to_sol(miner.lifetime_rewards_sol)
    );
    println!(
        "  lifetime_rewards_ore: {} ORE",
        amount_to_ui_amount(miner.lifetime_rewards_ore, TOKEN_DECIMALS)
    );
    Ok(())
}

async fn log_seeker(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let mint = std::env::var("MINT").unwrap();
    let mint = Pubkey::from_str(&mint).expect("Invalid MINT");
    let seeker = get_seeker(&rpc, mint).await?;
    let seeker_address = ore_api::state::seeker_pda(mint).0;
    println!("Seeker");
    println!("  address: {}", seeker_address);
    println!("  mint: {}", seeker.mint);
    Ok(())
}

async fn log_clock(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let clock = get_clock(&rpc).await?;
    println!("Clock");
    println!("  slot: {}", clock.slot);
    println!("  epoch_start_timestamp: {}", clock.epoch_start_timestamp);
    println!("  epoch: {}", clock.epoch);
    println!("  leader_schedule_epoch: {}", clock.leader_schedule_epoch);
    println!("  unix_timestamp: {}", clock.unix_timestamp);
    Ok(())
}

async fn log_config(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let config = get_config(&rpc).await?;
    println!("Config");
    println!("  admin: {}", config.admin);
    println!("  bury_authority: {}", config.bury_authority);
    println!("  fee_collector: {}", config.fee_collector);
    println!("  last_boost: {}", config.last_boost);
    println!(
        "  is_seeker_activation_enabled: {}",
        config.is_seeker_activation_enabled
    );

    Ok(())
}

async fn log_board(rpc: &RpcClient) -> Result<(), anyhow::Error> {
    let board = get_board(&rpc).await?;
    let clock = get_clock(&rpc).await?;
    print_board(board, &clock);
    Ok(())
}

fn print_board(board: Board, clock: &Clock) {
    let current_slot = clock.slot;
    println!("Board");
    println!("  Id: {:?}", board.round_id);
    println!("  Start slot: {}", board.start_slot);
    println!("  End slot: {}", board.end_slot);
    // 使用理论值计算（在 log_board 中我们已经获取了 clock，这里简单显示）
    let secs_left = if board.end_slot > current_slot {
        (board.end_slot.saturating_sub(current_slot) as f64) * 0.4
    } else {
        0.0
    };
    println!("  Time remaining: {:.2} sec", secs_left);
}

async fn get_automations(rpc: &RpcClient) -> Result<Vec<(Pubkey, Automation)>, anyhow::Error> {
    const REGOLITH_EXECUTOR: Pubkey = pubkey!("HNWhK5f8RMWBqcA7mXJPaxdTPGrha3rrqUrri7HSKb3T");
    let filter = RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
        56,
        &REGOLITH_EXECUTOR.to_bytes(),
    ));
    let automations = get_program_accounts::<Automation>(rpc, ore_api::ID, vec![filter]).await?;
    Ok(automations)
}

async fn get_meteora_pool(rpc: &RpcClient, address: Pubkey) -> Result<Pool, anyhow::Error> {
    let data = rpc.get_account_data(&address).await?;
    let pool = Pool::from_bytes(&data)?;
    Ok(pool)
}

async fn get_meteora_vault(rpc: &RpcClient, address: Pubkey) -> Result<Vault, anyhow::Error> {
    let data = rpc.get_account_data(&address).await?;
    let vault = Vault::from_bytes(&data)?;
    Ok(vault)
}

async fn get_board(rpc: &RpcClient) -> Result<Board, anyhow::Error> {
    let board_pda = ore_api::state::board_pda();
    // 使用 processed 确认级别以获得最快响应
    let account = rpc.get_account_with_commitment(&board_pda.0, CommitmentConfig::processed()).await?;
    let account = account.value.ok_or_else(|| anyhow::anyhow!("Board account not found"))?;
    let board = Board::try_from_bytes(&account.data)?;
    Ok(*board)
}

async fn get_slot_hashes(rpc: &RpcClient) -> Result<SlotHashes, anyhow::Error> {
    let data = rpc
        .get_account_data(&solana_sdk::sysvar::slot_hashes::ID)
        .await?;
    let slot_hashes = bincode::deserialize::<SlotHashes>(&data)?;
    Ok(slot_hashes)
}

async fn get_round(rpc: &RpcClient, id: u64) -> Result<Round, anyhow::Error> {
    let round_pda = ore_api::state::round_pda(id);
    // 使用 processed 确认级别以获得最快响应
    let account = rpc.get_account_with_commitment(&round_pda.0, CommitmentConfig::processed()).await?;
    let account = account.value.ok_or_else(|| anyhow::anyhow!("Round account not found"))?;
    let round = Round::try_from_bytes(&account.data)?;
    Ok(*round)
}

async fn get_treasury(rpc: &RpcClient) -> Result<Treasury, anyhow::Error> {
    let treasury_pda = ore_api::state::treasury_pda();
    let account = rpc.get_account(&treasury_pda.0).await?;
    let treasury = Treasury::try_from_bytes(&account.data)?;
    Ok(*treasury)
}

async fn get_config(rpc: &RpcClient) -> Result<Config, anyhow::Error> {
    let config_pda = ore_api::state::config_pda();
    let account = rpc.get_account(&config_pda.0).await?;
    let config = Config::try_from_bytes(&account.data)?;
    Ok(*config)
}

async fn get_miner(rpc: &RpcClient, authority: Pubkey) -> Result<Miner, anyhow::Error> {
    let miner_pda = ore_api::state::miner_pda(authority);
    let account = rpc.get_account(&miner_pda.0).await?;
    let miner = Miner::try_from_bytes(&account.data)?;
    Ok(*miner)
}

async fn get_clock(rpc: &RpcClient) -> Result<Clock, anyhow::Error> {
    // Clock sysvar 使用 processed 确认级别以获得最快响应
    let account = rpc.get_account_with_commitment(&solana_sdk::sysvar::clock::ID, CommitmentConfig::processed()).await?;
    let data = account.value.ok_or_else(|| anyhow::anyhow!("Clock account not found"))?.data;
    let clock = bincode::deserialize::<Clock>(&data)?;
    Ok(clock)
}

async fn get_seeker(rpc: &RpcClient, mint: Pubkey) -> Result<Seeker, anyhow::Error> {
    let seeker_pda = ore_api::state::seeker_pda(mint);
    let account = rpc.get_account(&seeker_pda.0).await?;
    let seeker = Seeker::try_from_bytes(&account.data)?;
    Ok(*seeker)
}

async fn get_stake(rpc: &RpcClient, authority: Pubkey) -> Result<Stake, anyhow::Error> {
    let stake_pda = ore_api::state::stake_pda(authority);
    let account = rpc.get_account(&stake_pda.0).await?;
    let stake = Stake::try_from_bytes(&account.data)?;
    Ok(*stake)
}

async fn get_rounds(rpc: &RpcClient) -> Result<Vec<(Pubkey, Round)>, anyhow::Error> {
    let rounds = get_program_accounts::<Round>(rpc, ore_api::ID, vec![]).await?;
    Ok(rounds)
}

#[allow(dead_code)]
async fn get_miners(rpc: &RpcClient) -> Result<Vec<(Pubkey, Miner)>, anyhow::Error> {
    let miners = get_program_accounts::<Miner>(rpc, ore_api::ID, vec![]).await?;
    Ok(miners)
}

async fn get_miners_participating(
    rpc: &RpcClient,
    round_id: u64,
) -> Result<Vec<(Pubkey, Miner)>, anyhow::Error> {
    let filter = RpcFilterType::Memcmp(Memcmp::new_base58_encoded(512, &round_id.to_le_bytes()));
    let miners = get_program_accounts::<Miner>(rpc, ore_api::ID, vec![filter]).await?;
    Ok(miners)
}

fn get_winning_square(slot_hash: &[u8]) -> u64 {
    // Use slot hash to generate a random u64
    let r1 = u64::from_le_bytes(slot_hash[0..8].try_into().unwrap());
    let r2 = u64::from_le_bytes(slot_hash[8..16].try_into().unwrap());
    let r3 = u64::from_le_bytes(slot_hash[16..24].try_into().unwrap());
    let r4 = u64::from_le_bytes(slot_hash[24..32].try_into().unwrap());
    let r = r1 ^ r2 ^ r3 ^ r4;

    // Returns a value in the range [0, 24] inclusive
    r % 25
}

#[allow(dead_code)]
async fn simulate_transaction(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    instructions: &[solana_sdk::instruction::Instruction],
) {
    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let x = rpc
        .simulate_transaction(&Transaction::new_signed_with_payer(
            instructions,
            Some(&payer.pubkey()),
            &[payer],
            blockhash,
        ))
        .await;
    println!("Simulation result: {:?}", x);
}

async fn submit_transaction(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    instructions: &[solana_sdk::instruction::Instruction],
) -> Result<solana_sdk::signature::Signature, anyhow::Error> {
    // 从环境变量读取费用配置，默认使用更合理的值
    // compute_unit_price: 默认 1,000 microlamports (低优先级，适合大多数情况)
    // 如果网络拥堵导致交易失败，可以提高到 5,000-10,000
    // compute_unit_limit: 默认 1,400,000 CU (保持原有限制)
    let compute_unit_price: u64 = std::env::var("COMPUTE_UNIT_PRICE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000); // 从 10,000 进一步降低到 1,000 (再降低 10 倍)

    let compute_unit_limit: u32 = std::env::var("COMPUTE_UNIT_LIMIT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1_400_000);

    // 计算预估费用（用于日志输出）
    // Solana 费用公式：费用(lamports) = (compute_unit_price * compute_units_used) / 1,000,000,000
    // 其中 compute_unit_price 单位是 microlamports per CU
    // 1 microlamport = 0.000000000001 SOL
    // 假设使用 200,000 CU（典型部署交易的实际使用量）
    let typical_cu_usage = 200_000u64;
    // 费用 = (price * cu) / 1e9，然后转换为 SOL (1 SOL = 1e9 lamports)
    let typical_fee_sol = (compute_unit_price as f64 * typical_cu_usage as f64) / 1_000_000_000_000.0;
    let max_fee_sol = (compute_unit_limit as f64) * (compute_unit_price as f64) / 1_000_000_000_000.0;
    println!("[fee] Compute Unit Price: {} microlamports/CU, Limit: {} CU",
        compute_unit_price, compute_unit_limit);
    println!("[fee] 预估费用: {:.6} SOL (典型使用 {} CU), 最大费用: {:.6} SOL",
        typical_fee_sol, typical_cu_usage, max_fee_sol);

    // 添加重试机制：指数退避算法，最多重试4次
    let max_retries = 4;
    let mut retry_count = 0;

    loop {
        let blockhash = match rpc.get_latest_blockhash().await {
            Ok(bh) => bh,
            Err(_e) => {
                if retry_count < max_retries {
                    retry_count += 1;
                    let wait_secs = 2u64.pow(retry_count as u32 - 1);
                    println!("[retry] 获取 blockhash 失败 (第 {} 次), 等待 {} 秒后重试...", retry_count, wait_secs);
                    sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                } else {
                    return Err(anyhow::anyhow!("获取 blockhash 失败，已重试 {} 次", max_retries));
                }
            }
        };

        let mut all_instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
            ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
        ];
        all_instructions.extend_from_slice(instructions);
        let transaction = Transaction::new_signed_with_payer(
            &all_instructions,
            Some(&payer.pubkey()),
            &[payer],
            blockhash,
        );

        match rpc.send_and_confirm_transaction(&transaction).await {
            Ok(signature) => {
                println!("[✓] 交易成功提交: {:?}", signature);
                return Ok(signature);
            }
            Err(e) => {
                let err_str = e.to_string().to_lowercase();
                // 判断是否为可重试的错误
                let is_retryable = err_str.contains("blockhash not found")
                    || err_str.contains("timeout")
                    || err_str.contains("invalid nonce")
                    || err_str.contains("connection")
                    || matches!(e.kind, solana_client::client_error::ClientErrorKind::Io(_));

                if is_retryable && retry_count < max_retries {
                    retry_count += 1;
                    let wait_secs = 2u64.pow(retry_count as u32 - 1);
                    println!("[retry] 交易提交失败 (第 {} 次): {:?}", retry_count, e);
                    println!("[retry] 这是可恢复错误，等待 {} 秒后重试...", wait_secs);
                    sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                } else {
                    println!("[✗] 交易提交失败（不可重试或已达最大重试次数）: {:?}", e);
                    return Err(e.into());
                }
            }
        }
    }
}

// 危险区间快速单次提交：不重试，直接返回结果
// 用于轮次即将结束时的最后冲刺
async fn submit_transaction_danger_zone_no_retry(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    instructions: &[solana_sdk::instruction::Instruction],
) -> Result<solana_sdk::signature::Signature, anyhow::Error> {
    // 获取 blockhash，这一步不重试，直接失败
    let blockhash = rpc.get_latest_blockhash().await?;

    let compute_unit_price: u64 = std::env::var("COMPUTE_UNIT_PRICE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000);

    let compute_unit_limit: u32 = std::env::var("COMPUTE_UNIT_LIMIT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1_400_000);

    let mut all_instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
    ];
    all_instructions.extend_from_slice(instructions);
    let transaction = Transaction::new_signed_with_payer(
        &all_instructions,
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );

    // 单次发送，不重试
    match rpc.send_and_confirm_transaction(&transaction).await {
        Ok(signature) => {
            println!("[✓✓✓] 危险区间提交成功！交易签名: {:?}", signature);
            Ok(signature)
        }
        Err(e) => {
            println!("[✗✗✗] 危险区间提交失败（不重试）: {:?}", e);
            Err(e.into())
        }
    }
}

async fn submit_transaction_no_confirm(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    instructions: &[solana_sdk::instruction::Instruction],
) -> Result<solana_sdk::signature::Signature, anyhow::Error> {
    let blockhash = rpc.get_latest_blockhash().await?;

    // 使用与 submit_transaction 相同的费用配置
    let compute_unit_price: u64 = std::env::var("COMPUTE_UNIT_PRICE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000); // 默认 1,000 microlamports

    let compute_unit_limit: u32 = std::env::var("COMPUTE_UNIT_LIMIT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1_400_000);

    let mut all_instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit),
        ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
    ];
    all_instructions.extend_from_slice(instructions);
    let transaction = Transaction::new_signed_with_payer(
        &all_instructions,
        Some(&payer.pubkey()),
        &[payer],
        blockhash,
    );

    match rpc.send_transaction(&transaction).await {
        Ok(signature) => {
            println!("Transaction submitted: {:?}", signature);
            Ok(signature)
        }
        Err(e) => {
            println!("Error submitting transaction: {:?}", e);
            Err(e.into())
        }
    }
}

pub async fn get_program_accounts<T>(
    client: &RpcClient,
    program_id: Pubkey,
    filters: Vec<RpcFilterType>,
) -> Result<Vec<(Pubkey, T)>, anyhow::Error>
where
    T: AccountDeserialize + Discriminator + Clone,
{
    let mut all_filters = vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
        0,
        &T::discriminator().to_le_bytes(),
    ))];
    all_filters.extend(filters);
    let result = client
        .get_program_accounts_with_config(
            &program_id,
            RpcProgramAccountsConfig {
                filters: Some(all_filters),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;

    match result {
        Ok(accounts) => {
            let accounts = accounts
                .into_iter()
                .filter_map(|(pubkey, account)| {
                    if let Ok(account) = T::try_from_bytes(&account.data) {
                        Some((pubkey, account.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            Ok(accounts)
        }
        Err(err) => match err.kind {
            ClientErrorKind::Reqwest(err) => {
                if let Some(status_code) = err.status() {
                    if status_code == StatusCode::GONE {
                        panic!(
                                "\n{} Your RPC provider does not support the getProgramAccounts endpoint, needed to execute this command. Please use a different RPC provider.\n",
                                "ERROR"
                            );
                    }
                }
                return Err(anyhow::anyhow!("Failed to get program accounts: {}", err));
            }
            _ => return Err(anyhow::anyhow!("Failed to get program accounts: {}", err)),
        },
    }
}