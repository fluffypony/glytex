use std::{
    cmp,
    convert::TryInto,
    env::current_dir,
    fs::{self, File},
    io::Write,
    num,
    panic,
    path::{Path, PathBuf},
    process,
    str::FromStr,
    sync::{Arc, OnceLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use clap::Parser;
#[cfg(feature = "nvidia")]
use cust::{
    memory::{AsyncCopyDestination, DeviceCopy},
    prelude::*,
};
use gpu_status_file::GpuStatus;
use http::stats_collector::{self, HashrateSample};
use log::{debug, error, info, warn};
use minotari_app_grpc::{
    conversions::block,
    tari_rpc::{Block, BlockHeader as grpc_header, NewBlockTemplate, TransactionOutput as GrpcTransactionOutput},
};
use multi_engine_wrapper::{EngineType, MultiEngineWrapper};
use num_format::{Locale, ToFormattedString};
use tari_common::configuration::Network;
use tari_common_types::{tari_address::TariAddress, types::FixedHash};
use tari_core::{
    blocks::BlockHeader,
    consensus::ConsensusManager,
    transactions::{
        tari_amount::MicroMinotari,
        transaction_components::{CoinBaseExtra, RangeProofType},
        transaction_key_manager::create_memory_db_key_manager,
    },
};
use tari_shutdown::{Shutdown, ShutdownSignal};
use tari_utilities::epoch_time::EpochTime;
use tokio::{
    runtime::Runtime,
    signal,
    sync::{broadcast::Sender, RwLock},
    time::sleep,
};

use crate::{
    config_file::ConfigFile,
    engine_impl::EngineImpl,
    gpu_engine::GpuEngine,
    gpu_status_file::GpuStatusFile,
    http::{config::Config, server::HttpServer},
    node_client::ClientType,
    tari_coinbase::generate_coinbase,
};

mod config_file;
mod context_impl;
mod multi_engine_wrapper;

#[cfg(feature = "nvidia")]
mod cuda_engine;
mod engine_impl;
mod function_impl;
mod gpu_engine;
mod gpu_status_file;
mod http;
mod node_client;

#[cfg(feature = "opencl")]
mod opencl_engine;
mod p2pool_client;
mod stats_store;
mod tari_coinbase;

#[cfg(feature = "metal")]
mod metal_engine;

const LOG_TARGET: &str = "tari::gpuminer";
static block_template_cache: OnceLock<RwLock<Option<BlockTemplateData>>> = OnceLock::new();

fn get_block_template_cache() -> &'static RwLock<Option<BlockTemplateData>> {
    block_template_cache.get_or_init(|| RwLock::new(None))
}

async fn clear_block_template_cache() {
    let mut cache = get_block_template_cache().write().await;
    *cache = None;
}

async fn replace_block_template_cache(template: BlockTemplateData) {
    let mut cache = get_block_template_cache().write().await;
    *cache = Some(template);
}

#[derive(Debug, Clone)]
struct BlockTemplateData {
    target_difficulty: u64,
    block: minotari_app_grpc::tari_rpc::Block,
    header: BlockHeader,
    mining_hash: FixedHash,
}

#[tokio::main]
async fn main() {
    // Set a custom panic hook
    panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|loc| format!("file: '{}', line: {}", loc.file(), loc.line()))
            .unwrap_or_else(|| "unknown location".to_string());

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic message".to_string()
        };

        error!(target: "tari::p2pool::main", "Panic occurred at {}: {}", location, message);
        eprintln!("Panic at {}: {}", location, message);

        // Optionally, write a custom message directly to the file
        let mut file = File::create("panic.log").unwrap();
        file.write_all(format!("Panic at {}: {}", location, message).as_bytes())
            .unwrap();
        process::exit(500);
    }));

    match main_inner().await {
        Ok(()) => {
            info!(target: LOG_TARGET, "Gpu miner startup process completed successfully");
            std::process::exit(0);
        },
        Err(err) => {
            error!(target: LOG_TARGET, "Gpu miner startup process error: {}", err);
            std::process::exit(1);
        },
    }
}

fn detect_and_return_status_file<T: AsRef<Path>>(
    multi_engine_wrapper: &mut MultiEngineWrapper,
    gpu_status_path: T,
) -> GpuStatusFile {
    let _engines = multi_engine_wrapper.create_status_files_for_each_engine(gpu_status_path.as_ref().parent().unwrap());
    let res = GpuStatusFile::load(gpu_status_path.as_ref()).unwrap_or_else(|err| {
        let default = GpuStatusFile::default();
        default
            .save(gpu_status_path.as_ref())
            .expect("Could not save default gpu status");
        default
    });
    res
}

#[derive(Parser)]
struct Cli {
    /// Config file path
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Do benchmark
    #[arg(short, long)]
    benchmark: bool,

    #[arg(short, long)]
    find_optimal: bool,

    /// (Optional) Tari wallet address to send rewards to
    #[arg(short = 'a', long)]
    tari_address: Option<String>,

    /// (Optional) Tari base node/p2pool node URL
    #[arg(short = 'u', long)]
    tari_node_url: Option<String>,

    /// P2Pool enabled
    #[arg(long)]
    p2pool_enabled: bool,

    /// Enable/disable http server
    ///
    /// It exposes health-check, version and stats endpoints
    #[arg(long)]
    http_server_enabled: Option<bool>,

    /// Port of HTTP server
    #[arg(long)]
    http_server_port: Option<u16>,

    #[arg(long, alias = "block-size")]
    block_size: Option<u32>,

    /// grid_size for the gpu
    #[arg(long, alias = "grid-size")]
    grid_size: Option<String>,
    /// Coinbase extra data
    #[arg(long)]
    coinbase_extra: Option<String>,

    /// (Optional) log config file
    #[arg(long, alias = "log-config-file", value_name = "log-config-file")]
    log_config_file: Option<PathBuf>,

    /// (Optional) log dir
    #[arg(long, alias = "log-dir", value_name = "log-dir")]
    log_dir: Option<PathBuf>,

    /// (Optional) log dir
    #[arg(short = 'd', long, alias = "detect")]
    detect: Option<bool>,

    /// Gpu status file path
    #[arg(short, long, value_name = "gpu-status")]
    gpu_status_file: Option<PathBuf>,

    #[arg(short, long)]
    template_timeout_secs: Option<u64>,

    #[arg(long)]
    max_template_failures: Option<usize>,

    #[arg(long)]
    engine: Option<String>,

    /// The number of iterations to do on the GPU before returning. If this is not set,
    /// it will try to optimize it to the point that it takes 1 second to run.
    #[arg(short, long)]
    iterations_per_cycle: Option<u32>,
}

async fn main_inner() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    debug!(target: LOG_TARGET, "Xtrgpuminer init");
    if let Some(ref log_dir) = cli.log_dir {
        tari_common::initialize_logging(
            &log_dir.join("log4rs_config.yml"),
            &log_dir.join("xtrgpuminer"),
            include_str!("../log4rs_sample.yml"),
        )
        .expect("Could not set up logging");
    }

    let benchmark = cli.benchmark;

    let submit = true;

    #[cfg(not(any(feature = "nvidia", feature = "opencl", feature = "metal")))]
    {
        eprintln!("No GPU engine available");
        process::exit(1);
    }

    let selected_cli_engine: EngineType = match cli.engine {
        Some(ref engine) => EngineType::from_string(engine),
        None => {
            eprintln!("No engine specified");
            process::exit(105);
        },
    };

    let mut multi_engine_wrapper = MultiEngineWrapper::new(selected_cli_engine.clone());
    multi_engine_wrapper.init().expect("Could not init engine");

    // http server
    let mut shutdown = Shutdown::new();

    let signal = shutdown.to_signal();
    // just create the context to test if it can run
    if let Some(_detect) = cli.detect {
        let default_path = {
            let path = current_dir().expect("no current directory");
            path
        };

        let mut engines_that_detected_any_device: Vec<EngineType> =
            multi_engine_wrapper.create_status_files_for_each_engine(cli.gpu_status_file.unwrap_or(default_path));

        if engines_that_detected_any_device.is_empty() {
            eprintln!("No GPU devices detected");
            process::exit(1);
        }

        return Ok(());
    }

    let mut config = match ConfigFile::load(&cli.config.as_ref().cloned().unwrap_or_else(|| {
        let mut path = current_dir().expect("no current directory");
        path.push("config.json");
        path
    })) {
        Ok(config) => {
            info!(target: LOG_TARGET, "Config file loaded successfully");
            config
        },
        Err(err) => {
            eprintln!("Error loading config file: {}. Creating new one", err);
            let default = ConfigFile::default();
            let path = cli.config.unwrap_or_else(|| {
                let mut path = current_dir().expect("no current directory");
                path.push("config.json");
                path
            });
            dbg!(&path);
            fs::create_dir_all(path.parent().expect("no parent"))?;
            default.save(&path).expect("Could not save default config");
            default
        },
    };

    let default_gpu_status_path = {
        let path = current_dir().expect("no current directory");
        path
    };
    let mut gpu_status_path = cli.gpu_status_file.clone().unwrap_or(default_gpu_status_path);
    let gpu_status_file_name = format!("{}_gpu_status.json", selected_cli_engine.to_string());
    gpu_status_path.push(gpu_status_file_name);

    let gpu_status_file = match GpuStatusFile::load(&gpu_status_path) {
        Ok(f) => {
            if f.gpu_devices.is_empty() {
                println!("No GPU devices detected in status file, detecting...");
                info!(target: LOG_TARGET, "No GPU devices detected in status file, detecting...");
                detect_and_return_status_file(&mut multi_engine_wrapper, &gpu_status_path)
            } else {
                f
            }
        },
        Err(err) => {
            eprintln!("Error loading gpu status file: {}. Detecting...", err);
            // Run detect.
            detect_and_return_status_file(&mut multi_engine_wrapper, &gpu_status_path)
        },
    };

    if let Some(ref addr) = cli.tari_address {
        config.tari_address = addr.clone();
    }
    if let Some(ref url) = cli.tari_node_url {
        config.tari_node_url = url.clone();
    }
    if cli.p2pool_enabled {
        config.p2pool_enabled = true;
    }
    if let Some(enabled) = cli.http_server_enabled {
        config.http_server_enabled = enabled;
    }
    if let Some(port) = cli.http_server_port {
        config.http_server_port = port;
    }
    if let Some(block_size) = cli.block_size {
        config.block_size = block_size;
    }
    if let Some(grid_size) = cli.grid_size {
        let sizes: Vec<u32> = grid_size.split(',').map(|s| s.parse::<u32>().unwrap()).collect();
        if sizes.len() == 1 {
            config.single_grid_size = sizes[0];
        } else {
            config.per_device_grid_sizes = sizes;
        }
    }
    if let Some(coinbase_extra) = cli.coinbase_extra {
        config.coinbase_extra = coinbase_extra;
    }

    if let Some(template_timeout) = cli.template_timeout_secs {
        config.template_timeout_secs = template_timeout;
    }

    if let Some(max_template_failures) = cli.max_template_failures {
        config.max_template_failures = max_template_failures as u64;
    }

    if let Some(iterations_per_cycle) = cli.iterations_per_cycle {
        config.iterations_per_cycle = Some(iterations_per_cycle);
    }

    let gpu_devices = gpu_status_file.gpu_devices.clone();

    if gpu_devices.is_empty() {
        eprintln!("No GPU devices detected");
        process::exit(1);
    }

    gpu_devices.iter().for_each(|gpu_device| {
        println!(
            "Device: {} is available: {} is excluded {}",
            gpu_device.device_name, gpu_device.settings.is_available, gpu_device.settings.is_excluded
        );
    });

    let num_devices = multi_engine_wrapper.num_devices()?;
    let devices_to_use: Vec<u32> = gpu_devices
        .iter()
        .filter(|d| d.settings.is_available && !d.settings.is_excluded)
        .map(|d| d.device_index)
        .collect();

    println!(
        "Device indexes to use: {:?} from the total number of devices: {:?}",
        devices_to_use.len(),
        num_devices
    );

    if cli.find_optimal {
        let mut best_hashrate = 0;
        let mut best_grid_size = 1;
        let mut current_grid_size = 32;
        let mut is_doubling_stage = true;
        let mut last_grid_size_increase = 0;
        let mut prev_hashrate = 0;

        while true {
            let mut config = config.clone();
            config.single_grid_size = current_grid_size;
            // config.block_size = ;
            let mut threads = vec![];
            let (tx, rx) = tokio::sync::broadcast::channel(100);
            for i in 0..num_devices {
                if !devices_to_use.contains(&i) {
                    continue;
                }
                let c = config.clone();
                let gpu = multi_engine_wrapper.clone();
                let x = tx.clone();

                let signal2 = signal.clone();
                threads.push(thread::spawn(move || {
                    run_thread(gpu, num_devices as u64, i as u32, c, true, x, signal2)
                }));
            }
            let thread_len = threads.len();
            let mut thread_hashrate = Vec::with_capacity(thread_len);
            for t in threads {
                match t.join() {
                    Ok(res) => match res {
                        Ok(hashrate) => {
                            info!(target: LOG_TARGET, "Thread join succeeded: {}", hashrate.to_formatted_string(&Locale::en));
                            thread_hashrate.push(hashrate);
                        },
                        Err(err) => {
                            eprintln!("Thread join succeeded but result failed: {:?}", err);
                            error!(target: LOG_TARGET, "Thread join succeeded but result failed: {:?}", err);
                        },
                    },
                    Err(err) => {
                        eprintln!("Thread join failed: {:?}", err);
                        error!(target: LOG_TARGET, "Thread join failed: {:?}", err);
                    },
                }
            }
            let total_hashrate: u64 = thread_hashrate.iter().sum();
            if total_hashrate > best_hashrate {
                best_hashrate = total_hashrate;
                best_grid_size = current_grid_size;
                // best_grid_size = config.single_grid_size;
                // best_block_size = config.block_size;
                println!(
                    "Best hashrate: {} grid_size: {}, current_grid: {} block_size: {} Prev Hash {}",
                    best_hashrate, best_grid_size, current_grid_size, config.block_size, prev_hashrate
                );
            }
            // if total_hashrate < prev_hashrate {
            //     println!("total decreased, breaking");
            //     break;
            // }
            if is_doubling_stage {
                if total_hashrate > prev_hashrate {
                    last_grid_size_increase = current_grid_size;
                    current_grid_size = current_grid_size * 2;
                } else {
                    is_doubling_stage = false;
                    last_grid_size_increase = last_grid_size_increase / 2;
                    current_grid_size = current_grid_size.saturating_sub(last_grid_size_increase);
                }
            } else {
                // Bisecting stage
                if last_grid_size_increase < 2 {
                    break;
                }
                if total_hashrate > prev_hashrate {
                    last_grid_size_increase = last_grid_size_increase / 2;
                    current_grid_size += last_grid_size_increase;
                } else {
                    last_grid_size_increase = last_grid_size_increase / 2;
                    current_grid_size = current_grid_size.saturating_sub(last_grid_size_increase);
                }
            }
            prev_hashrate = total_hashrate;
        }
        return Ok(());
    }

    let (stats_tx, stats_rx) = tokio::sync::broadcast::channel(100);
    if config.http_server_enabled {
        let mut stats_collector = stats_collector::StatsCollector::new(shutdown.to_signal(), stats_rx);
        let stats_client = stats_collector.create_client();
        info!(target: LOG_TARGET, "Stats collector started");
        tokio::spawn(async move {
            stats_collector.run().await;
            info!(target: LOG_TARGET, "Stats collector shutdown");
        });
        let http_server_config = Config::new(config.http_server_port);
        info!(target: LOG_TARGET, "HTTP server runs on port: {}", &http_server_config.port);
        let http_server = HttpServer::new(shutdown.to_signal(), http_server_config, stats_client);
        info!(target: LOG_TARGET, "HTTP server enabled");
        tokio::spawn(async move {
            if let Err(error) = http_server.start().await {
                println!("Failed to start HTTP server: {error:?}");
                error!(target: LOG_TARGET, "Failed to start HTTP server: {:?}", error);
            } else {
                info!(target: LOG_TARGET, "Success to start HTTP server");
            }
        });
    }

    let mut threads = vec![];

    info!(target: LOG_TARGET, "Starting template height watcher");

    if num_devices > 0 && !benchmark {
        let c = config.clone();
        let s = signal.clone();
        threads.push(thread::spawn(move || {
            let runtime = Runtime::new().unwrap();
            runtime.block_on(async { run_template_height_watcher(c, s).await })
        }));
    }

    info!(target: LOG_TARGET, "Starting mining threads: {}", devices_to_use.len());

    for i in 0..num_devices {
        println!("Device index: {}", i);
        if devices_to_use.contains(&i) {
            println!("Starting thread for device index: {}", i);
            let c = config.clone();
            let gpu = multi_engine_wrapper.clone();
            let curr_stats_tx = stats_tx.clone();
            let s = signal.clone();
            threads.push(thread::spawn(move || {
                run_thread(gpu, num_devices as u64, i as u32, c, benchmark, curr_stats_tx, s)
            }));
        }
    }

    let thread_len = threads.len();
    let mut thread_hashrate = Vec::with_capacity(thread_len);
    if !benchmark {
        'outer: loop {
            for t in &threads {
                // if any thread is finished, stop all of them
                if t.is_finished() {
                    println!("One job has finished, stopping others");
                    shutdown.trigger();
                    break 'outer;
                }
            }
            sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    println!("Stopping all jobs");
    for t in threads {
        match t.join() {
            Ok(res) => match res {
                Ok(hashrate) => {
                    info!(target: LOG_TARGET, "Thread join succeeded: {}", hashrate.to_formatted_string(&Locale::en));
                    thread_hashrate.push(hashrate);
                },
                Err(err) => {
                    error!(target: LOG_TARGET, "Thread join succeeded but result failed: {:?}", err);
                },
            },
            Err(err) => {
                error!(target: LOG_TARGET, "Thread join failed: {:?}", err);
            },
        }
    }
    println!("Threads finished successfully");

    // kill other threads
    // shutdown.trigger();
    if thread_hashrate.len() == thread_len {
        let total_hashrate: u64 = thread_hashrate.iter().sum();
        warn!(target: LOG_TARGET, "Total hashrate: {}", total_hashrate.to_formatted_string(&Locale::en));
    } else {
        error!(target: LOG_TARGET, "Not all threads finished successfully");
    }

    Ok(())
}

async fn run_template_height_watcher(config: ConfigFile, shutdown: ShutdownSignal) -> Result<u64, anyhow::Error> {
    let client_type = if config.p2pool_enabled {
        ClientType::P2Pool(TariAddress::from_str(config.tari_address.as_str()).unwrap())
    } else {
        ClientType::BaseNode
    };

    let mut node_client =
        node_client::create_client(client_type, &config.tari_node_url, config.coinbase_extra.clone()).await?;

    let mut curr_node_height = 0;
    let mut curr_p2pool_height = 0;
    let mut curr_hash = vec![];
    let mut curr_p2pool_hash = vec![];
    let mut last_template_time = Instant::now();
    let mut num_failures = 0;
    // let mut curr_block_template = None;

    let timeout_dur = std::time::Duration::from_secs(config.template_timeout_secs);
    loop {
        // Sleep first otherwise we call continue a lot
        sleep(Duration::from_secs(config.height_check_secs)).await;
        if num_failures > config.max_template_failures as u64 {
            error!(target: LOG_TARGET, "Max template failures reached. Exiting.");
            // This is a temporary hack to stop mining
            return Err(anyhow!("Max template failures reached"));
        }
        let mut must_refresh = false;
        if shutdown.is_triggered() {
            break;
        }

        // let current_data = {
        //     let d = get_block_template_cache().read().await;
        //     d.map(|d| (d.header.height, d.header.prev_hash, d.p2pool_height, d.p2pool_prev_hash))
        // };

        let height_data = match tokio::time::timeout(timeout_dur, node_client.get_height()).await {
            Ok(Ok(height_data)) => height_data,
            Ok(Err(e)) => {
                error!(target: LOG_TARGET, "Error getting height: {:?}", e);
                num_failures += 1;
                continue;
            },
            Err(e) => {
                error!(target: LOG_TARGET, "Timeout getting height: {:?}", e);
                num_failures += 1;
                continue;
            },
        };
        num_failures = 0;
        if height_data.height > curr_node_height || height_data.tip_hash != curr_hash {
            info!(target: LOG_TARGET, "Tari chain changed. Dumping block template. New height:{}, old height:{}.", height_data.height, curr_node_height);
            must_refresh = true;
            // clear_block_template_cache().await;
        }
        if height_data.p2pool_height > curr_p2pool_height || curr_p2pool_hash != height_data.p2pool_tip_hash {
            info!(target: LOG_TARGET, "P2Pool chain changed. Dumping block template. New height:{}, old height:{}.", height_data.p2pool_height, curr_p2pool_height);
            must_refresh = true;
            // clear_block_template_cache().await;
        }

        if last_template_time.elapsed() > Duration::from_secs(config.template_refresh_secs) {
            info!(target: LOG_TARGET, "Template refresh time elapsed. Dumping block template.");
            must_refresh = true;
        }

        curr_node_height = height_data.height;
        curr_hash = height_data.tip_hash;
        curr_p2pool_height = height_data.p2pool_height;
        curr_p2pool_hash = height_data.p2pool_tip_hash;

        println!(
            "Current height: {}, P2Pool height: {} time: {}s",
            curr_node_height,
            curr_p2pool_height,
            last_template_time.elapsed().as_secs()
        );

        if must_refresh {
            println!("Refreshing block template");
            let template = match tokio::time::timeout(
                std::time::Duration::from_secs(config.template_timeout_secs),
                get_template_from_client(&mut node_client, config.clone()),
            )
            .await
            {
                Ok(Ok(template)) => template,
                Ok(Err(e)) => {
                    error!(target: LOG_TARGET, "Error getting block template: {}", e);
                    num_failures += 1;
                    continue;
                },
                Err(e) => {
                    error!(target: LOG_TARGET, "Timeout getting block template: {}", e);
                    num_failures += 1;
                    continue;
                },
            };
            last_template_time = Instant::now();
            // clear_block_template_cache().await;
            replace_block_template_cache(template).await;
            println!("Block template refreshed");
        }

        // let height = template
        //     .new_block_template
        //     .as_ref()
        //     .and_then(|b| b.header.as_ref())
        //     .map(|h| h.height)
        //     .unwrap_or(0);
        // if height > curr_height.load(Ordering::SeqCst) {
        //     curr_height.store(height, Ordering::SeqCst);
        // }
    }
    Ok(0)
}

fn run_thread(
    gpu_engine: MultiEngineWrapper,
    num_threads: u64,
    thread_index: u32,
    config: ConfigFile,
    benchmark: bool,
    stats_tx: Sender<HashrateSample>,
    shutdown: ShutdownSignal,
) -> Result<u64, anyhow::Error> {
    let fixed_num_iterations = config.iterations_per_cycle;
    let tari_node_url = config.tari_node_url.clone();
    let runtime = Runtime::new()?;
    let client_type = if benchmark {
        ClientType::Benchmark
    } else if config.p2pool_enabled {
        ClientType::P2Pool(TariAddress::from_str(config.tari_address.as_str())?)
    } else {
        ClientType::BaseNode
    };
    let mut template_fetch_failures = 0;
    let coinbase_extra = config.coinbase_extra.clone();
    let node_client = Arc::new(RwLock::new(runtime.block_on(async move {
        node_client::create_client(client_type, &tari_node_url, coinbase_extra).await
    })?));
    let mut rounds = 0;
    let running_time = Instant::now();

    let context = gpu_engine.create_context(thread_index)?;

    let gpu_function = gpu_engine.create_main_function(&context)?;

    // let (mut grid_size, block_size) = gpu_function
    //    .suggested_launch_configuration()
    //    .context("get suggest config")?;
    // let (grid_size, block_size) = (23, 50);
    let block_size = config.block_size;
    let grid_size = if config.per_device_grid_sizes.is_empty() {
        config.single_grid_size
    } else {
        config.per_device_grid_sizes[thread_index as usize]
    };
    // grid_size =
    //    (grid_size as f64 / 1000f64 * cmp::max(cmp::min(100, config.gpu_percentage as usize), 1) as f64).round() as
    // u32; let (mut grid_size, block_size) = gpu_function
    //     .suggested_launch_configuration()
    //     .context("get suggest config")?;
    // let (grid_size, block_size) = (23, 50);

    let output = vec![0u64; 5];
    // let mut output_buf = output.as_slice().as_dbuf()?;

    let mut data = vec![0u64; 6];
    // let mut data_buf = data.as_slice().as_dbuf()?;

    let mut num_iterations = 1;
    if let Some(fixed_num_iterations) = fixed_num_iterations {
        info!(target: LOG_TARGET, "Using fixed num iterations: {}", fixed_num_iterations);
        num_iterations = fixed_num_iterations;
    }
    loop {
        if shutdown.is_triggered() {
            return Ok(0);
        }
        rounds += 1;

        if rounds > 101 {
            rounds = 0;
        }
        let clone_node_client = node_client.clone();
        let clone_config = config.clone();
        let target_difficulty: u64;
        let block: Block;
        let mut header: BlockHeader;
        let mining_hash: FixedHash;
        match runtime.block_on(async move { get_template(benchmark).await }) {
            Some(res) => {
                // template_fetch_failures = 0;
                // info!(target: LOG_TARGET, "Getting next block...");
                // println!("Getting next block...{}", res_header.height);
                target_difficulty = res.target_difficulty;
                block = res.block;
                header = res.header;
                mining_hash = res.mining_hash;
                // previous_template = Some((target_difficulty, block.clone(), header.clone(), mining_hash.clone()));
            },
            None => {
                info!(target: LOG_TARGET, "Waiting for template to be populated");
                thread::sleep(std::time::Duration::from_secs(1));
                continue;
            },
        }

        let hash64 = copy_u8_to_u64(mining_hash.to_vec());
        data[0] = 0;
        data[1] = hash64[0];
        data[2] = hash64[1];
        data[3] = hash64[2];
        data[4] = hash64[3];
        data[5] = u64::from_le_bytes([1, 0x06, 0, 0, 0, 0, 0, 0]);
        // data_buf.copy_from(&data).expect("Could not copy data to buffer");
        // output_buf.copy_from(&output).expect("Could not copy output to buffer");

        let mut nonce_start = (u64::MAX / num_threads) * thread_index as u64;
        let first_nonce = nonce_start;
        let elapsed = Instant::now();
        let mut max_diff = 0;
        let mut last_printed = Instant::now();
        let mut last_reported_stats = Instant::now();
        let kernel = gpu_engine.create_kernel(&gpu_function)?;
        loop {
            if running_time.elapsed() > Duration::from_secs(10) && benchmark {
                let hash_rate = (nonce_start - first_nonce) / elapsed.elapsed().as_secs();
                return Ok(hash_rate);
            }
            debug!(target: LOG_TARGET, "Inside loop");
            if elapsed.elapsed().as_secs() > config.template_refresh_secs {
                debug!(target: LOG_TARGET, "Elapsed {:?} > {:?}", elapsed.elapsed().as_secs(), config.template_refresh_secs );
                break;
            }
            let mining_time = Instant::now();
            let result = gpu_engine.mine(
                &kernel,
                &gpu_function,
                &context,
                &data,
                (u64::MAX / (target_difficulty)).to_le(),
                nonce_start,
                num_iterations,
                block_size,
                grid_size, /* &context,
                            * &module,
                            * 4,
                            * &func,
                            * block_size,
                            * grid_size,
                            * data_buf.as_device_ptr(),
                            * &output_buf, */
            );
            // if fixed_num_iterations.is_none() {
            //     if mining_time.elapsed().as_secs() > 1500 {
            //         num_iterations = cmp::max(1, num_iterations - 1);
            //     } else if mining_time.elapsed().as_millis() < 1000 {
            //         num_iterations = num_iterations + 1;
            //     }
            // }
            let (nonce, hashes, diff) = match result {
                Ok(values) => {
                    debug!(target: LOG_TARGET,
                        "Mining successful: nonce={:?}, hashes={}, difficulty={}",
                        values.0, values.1, values.2
                    );
                    (values.0, values.1, values.2)
                },
                Err(e) => {
                    error!(target: LOG_TARGET, "Mining failed: {}", e);
                    eprintln!("Mining failed: {}", e);
                    return Err(e.into());
                },
            };
            if let Some(ref n) = nonce {
                header.nonce = *n;
            }
            if diff > max_diff {
                max_diff = diff;
            }
            nonce_start = nonce_start + hashes as u64;
            debug!(target: LOG_TARGET, "Nonce start {:?}", nonce_start.to_formatted_string(&Locale::en));
            if elapsed.elapsed().as_secs() > 1 {
                let hash_rate = (nonce_start - first_nonce) / elapsed.elapsed().as_secs();
                if Instant::now() - last_reported_stats > std::time::Duration::from_millis(500) {
                    last_reported_stats = Instant::now();
                    stats_tx.send(HashrateSample {
                        device_id: thread_index,
                        hashrate: hash_rate,
                        timestamp: EpochTime::now(),
                    })?;
                }
                if Instant::now() - last_printed > std::time::Duration::from_secs(2) {
                    last_printed = Instant::now();
                    println!(
                        "[Thread:{}] total {:} grid: {} max_diff: {}, target: {} hashes/sec: {}",
                        thread_index,
                        nonce_start.to_formatted_string(&Locale::en),
                        grid_size,
                        max_diff.to_formatted_string(&Locale::en),
                        target_difficulty.to_formatted_string(&Locale::en),
                        hash_rate.to_formatted_string(&Locale::en)
                    );
                    info!(target: LOG_TARGET, "[THREAD:{}] total {:} grid: {} max_diff: {}, target: {} hashes/sec: {}",
                    thread_index,
                    nonce_start.to_formatted_string(&Locale::en),
                    grid_size,
                    max_diff.to_formatted_string(&Locale::en),
                    target_difficulty.to_formatted_string(&Locale::en),
                    hash_rate.to_formatted_string(&Locale::en));
                }
            }
            debug!(target: LOG_TARGET, "Inside loop nonce {:?}", nonce.clone().is_some());
            if nonce.is_some() {
                debug!(target: LOG_TARGET, "Inside loop nonce is some {:?}", nonce.clone().is_some());
                header.nonce = nonce.unwrap();

                let mut mined_block = block.clone();
                mined_block.header = Some(grpc_header::from(header));
                let clone_client = node_client.clone();
                match runtime.block_on(async {
                    let mut client = clone_client.write().await;
                    tokio::time::timeout(
                        std::time::Duration::from_secs(config.template_timeout_secs),
                        client.submit_block(mined_block),
                    )
                    .await?
                }) {
                    Ok(_) => {
                        // stats_store.inc_accepted_blocks();
                        println!("Block submitted");
                    },
                    Err(e) => {
                        // stats_store.inc_rejected_blocks();
                        println!("Error submitting block: {:?}", e);
                    },
                }
                break;
            }
            debug!(target: LOG_TARGET, "Inside thread loop break {:?}", num_threads);
            // break;
        }
    }
}

async fn get_template(benchmark: bool) -> Option<BlockTemplateData> {
    if benchmark {
        return Some(BlockTemplateData {
            target_difficulty: u64::MAX,
            block: minotari_app_grpc::tari_rpc::Block::default(),
            header: BlockHeader::new(0),
            mining_hash: FixedHash::default(),
        });
    }
    get_block_template_cache().read().await.clone()
}

async fn get_template_from_client(
    node_client: &mut node_client::Client,
    config: ConfigFile,
) -> Result<BlockTemplateData, anyhow::Error> {
    let key_manager = create_memory_db_key_manager()?;
    let consensus_manager = ConsensusManager::builder(Network::NextNet)
        .build()
        .expect("Could not build consensus manager");

    // let mut lock = node_client.write().await;
    let address = TariAddress::from_str(config.tari_address.as_str())?;
    // p2pool enabled
    if config.p2pool_enabled {
        debug!(target: LOG_TARGET, "p2pool enabled");
        let block_result = tokio::time::timeout(
            std::time::Duration::from_secs(config.template_timeout_secs),
            node_client.get_new_block(NewBlockTemplate::default()),
        )
        .await??;
        // dbg!(&block_result);
        let block = block_result.result.block.unwrap();
        let mut header: BlockHeader = block
            .clone()
            .header
            .unwrap()
            .try_into()
            .map_err(|s: String| anyhow!(s))?;
        header.timestamp = EpochTime::now();
        let mining_hash = header.mining_hash().clone();
        info!(target: LOG_TARGET,
            "block result target difficulty: {}, block timestamp: {}, mining_hash: {}",
            block_result.target_difficulty.to_string(),
            block.clone().header.unwrap().timestamp.to_string(),
            header.mining_hash().clone().to_string()
        );
        println!(
            "New template, difficulty: {}, minerdata {}",
            block_result.target_difficulty,
            block_result
                .result
                .miner_data
                .as_ref()
                .map(|m| m.target_difficulty)
                .unwrap_or_default()
        );
        // return Ok(());
        return Ok(BlockTemplateData {
            target_difficulty: block_result
                .result
                .miner_data
                .as_ref()
                .map(|m| m.target_difficulty)
                .unwrap_or(block_result.target_difficulty),
            block,
            header,
            mining_hash,
        });
    }

    println!("Getting block template");
    let template = tokio::time::timeout(
        std::time::Duration::from_secs(config.template_timeout_secs),
        node_client.get_block_template(),
    )
    .await??;
    let mut block_template = template.new_block_template.clone().unwrap();
    let height = block_template.header.as_ref().unwrap().height;
    let miner_data = template.miner_data.unwrap();
    let fee = MicroMinotari::from(miner_data.total_fees);
    let reward = MicroMinotari::from(miner_data.reward);
    let (coinbase_output, coinbase_kernel) = generate_coinbase(
        fee,
        reward,
        height,
        // config.coinbase_extra.as_bytes(),
        &CoinBaseExtra::try_from(config.coinbase_extra.as_bytes().to_vec())?,
        &key_manager,
        &address,
        true,
        consensus_manager.consensus_constants(height),
        RangeProofType::RevealedValue,
    )
    .await?;
    debug!(target: LOG_TARGET, "Getting block template difficulty {:?}", miner_data.target_difficulty.clone());
    let body = block_template.body.as_mut().expect("no block body");
    let grpc_output = GrpcTransactionOutput::try_from(coinbase_output.clone()).map_err(|s| anyhow!(s))?;
    body.outputs.push(grpc_output);
    body.kernels.push(coinbase_kernel.into());
    let target_difficulty = miner_data.target_difficulty;
    let block_result = node_client.get_new_block(block_template).await?.result;
    let block = block_result.block.unwrap();
    let mut header: BlockHeader = block
        .clone()
        .header
        .unwrap()
        .try_into()
        .map_err(|s: String| anyhow!(s))?;
    // header.timestamp = EpochTime::now();

    let mining_hash = header.mining_hash().clone();
    Ok(BlockTemplateData {
        target_difficulty,
        block,
        header,
        mining_hash,
    })
}
fn copy_u8_to_u64(input: Vec<u8>) -> Vec<u64> {
    let mut output: Vec<u64> = Vec::with_capacity(input.len() / 8);

    for chunk in input.chunks_exact(8) {
        let value = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        output.push(value);
    }

    let remaining_bytes = input.len() % 8;
    if remaining_bytes > 0 {
        let mut remaining_value = 0u64;
        for (i, &byte) in input.iter().rev().take(remaining_bytes).enumerate() {
            remaining_value |= (byte as u64) << (8 * i);
        }
        output.push(remaining_value);
    }

    output
}

fn copy_u64_to_u8(input: Vec<u64>) -> Vec<u8> {
    let mut output: Vec<u8> = Vec::with_capacity(input.len() * 8);

    for value in input {
        output.extend_from_slice(&value.to_le_bytes());
    }

    output
}
