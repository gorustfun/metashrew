use actix_cors::Cors;
use actix_web::error;
use actix_web::http::{header::ContentType, StatusCode};
use actix_web::{post, web, App, HttpResponse, HttpServer, Responder, Result};
use metashrew_rockshrew_runtime::{query_height, set_label, RocksDBRuntimeAdapter};
use metashrew_runtime::MetashrewRuntime;
use std::fmt;
use anyhow;
use clap::{Parser, CommandFactory};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use serde_json;
use std::env;
use std::fs::File;
use std::io::{prelude::*, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use substring::Substring;
use tiny_keccak::{Hasher, Sha3};
use rocksdb::Options;

/// RocksDB-backed view server for metashrew
#[derive(Parser, Debug, Default)]
#[command(author, version, about, long_about = None)]
struct RockshrewViewArgs {
    /// Path to the indexer WASM program
    #[arg(long, env = "PROGRAM_PATH", default_value = "/mnt/volume/indexer.wasm")]
    program_path: PathBuf,
    
    /// Optional RocksDB label for the database
    #[arg(long, env = "ROCKS_LABEL")]
    rocks_label: Option<String>,
    
    /// Path to the primary RocksDB database directory
    #[arg(long, env = "ROCKS_DB_PATH", default_value = "rocksdb_data")]
    db_path: String,
    
    /// Path for secondary instance files (required for secondary mode)
    #[arg(long, env = "SECONDARY_PATH", default_value = "rocksdb_secondary")]
    secondary_path: String,
    
    /// Host address to bind the server to
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,
    
    /// Port number to listen on
    #[arg(long, env = "PORT", default_value_t = 8080)]
    port: u16,
}

fn from_anyhow(err: anyhow::Error) -> actix_web::Error {
    error::InternalError::new(
        err.to_string(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
    .into()
}

#[derive(Deserialize, Serialize)]
struct JsonRpcRequest {
    id: u32,
    method: String,
    params: Vec<String>,
    #[allow(dead_code)]
    jsonrpc: String,
}

#[derive(Serialize)]
struct JsonRpcError {
    id: u32,
    error: String,
    jsonrpc: String,
}

#[derive(Serialize)]
struct JsonRpcResult {
    id: u32,
    result: String,
    jsonrpc: String,
    error: String,
}

struct Context {
    #[allow(dead_code)]
    hash: [u8; 32],
    #[allow(dead_code)]
    program: Vec<u8>,
    runtime: MetashrewRuntime<RocksDBRuntimeAdapter>,
}

static mut _HEIGHT: u32 = 0;

pub fn height() -> u32 {
    unsafe { _HEIGHT }
}

pub fn set_height(h: u32) -> u32 {
    unsafe {
        _HEIGHT = h;
        _HEIGHT
    }
}

pub async fn fetch_and_set_height(internal_db: &RocksDBRuntimeAdapter) -> Result<u32> {
    let height = query_height(internal_db.db.clone(), 0)
        .await
        .map_err(|e| from_anyhow(e))?;
    Ok(set_height(height))
}

#[post("/")]
async fn jsonrpc_call(
    body: web::Json<JsonRpcRequest>,
    context: web::Data<Context>,
) -> Result<impl Responder> {
    {
        debug!("{}", serde_json::to_string(&body).unwrap());
    }
    if body.method == "metashrew_view" {
        let height: u32 = if body.params[2] == "latest" {
            fetch_and_set_height(&context.runtime.context.lock().unwrap().db).await?
        } else {
            let h = body.params[2].parse::<u32>().unwrap();
            if h > height() {
                fetch_and_set_height(&context.runtime.context.lock().unwrap().db).await?;
            }
            h
        };
        let (res_string, err) = match context.runtime.view(
            body.params[0].clone(),
            &hex::decode(
                body.params[1]
                    .to_string()
                    .substring(2, body.params[1].len()),
            )
            .unwrap(),
            height,
        ) {
            Ok(str) => (str, "".to_string()),
            Err(err) => {
                println!("{:#?}", err);
                (vec![], err.to_string())
            }
        };
        let result = JsonRpcResult {
            id: body.id,
            result: String::from("0x") + hex::encode(res_string).as_str(),
            error: err,
            jsonrpc: "2.0".to_string(),
        };
        return Ok(HttpResponse::Ok().json(result));
    } else if body.method == "metashrew_height" {
        let height = fetch_and_set_height(&context.runtime.context.lock().unwrap().db).await?;
        let result = JsonRpcResult {
            id: body.id,
            result: height.to_string(),
            error: "".to_string(),
            jsonrpc: "2.0".to_string(),
        };
        return Ok(HttpResponse::Ok().json(result));
    } else if body.method == "metashrew_preview" {
        let height: u32 = if body.params[3] == "latest" {
            fetch_and_set_height(&context.runtime.context.lock().unwrap().db).await?
        } else {
            let h = body.params[3].parse::<u32>().unwrap();
            if h > height() {
                fetch_and_set_height(&context.runtime.context.lock().unwrap().db).await?;
            }
            h
        };

        let block_hex = body.params[0].clone();
        // Remove 0x prefix if present and decode hex
        let block_data =
            hex::decode(block_hex.trim_start_matches("0x")).map_err(|e| from_anyhow(e.into()))?;

        // Use preview to execute block and view
        let (res_string, err) = match context.runtime.preview(
            &block_data,
            body.params[1].clone(),
            &hex::decode(
                body.params[2]
                    .to_string()
                    .substring(2, body.params[2].len()),
            )
            .unwrap(),
            height,
        ) {
            Ok(str) => (str, "".to_string()),
            Err(err) => {
                println!("{:#?}", err);
                (vec![], err.to_string())
            }
        };

        let result = JsonRpcResult {
            id: body.id,
            result: String::from("0x") + hex::encode(res_string).as_str(),
            error: err,
            jsonrpc: "2.0".to_string(),
        };

        return Ok(HttpResponse::Ok().json(result));
    } else {
        let result = JsonRpcResult {
            id: body.id,
            result: "".to_owned(),
            error: format!("RPC method {} not found", body.method),
            jsonrpc: "2.0".to_string(),
        };
        return Ok(HttpResponse::Ok().json(result));
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    
    // Parse command line arguments (falls back to env vars via #[arg(env)])
    let args = RockshrewViewArgs::parse();
    
    if let Some(label) = args.rocks_label {
        set_label(label);
    }
    
    let program = File::open(&args.program_path).expect("Failed to open program file");
    let mut buf = BufReader::new(program);
    let mut bytes: Vec<u8> = vec![];
    let _ = buf.read_to_end(&mut bytes);
    let mut hasher = Sha3::v256();
    let mut output = [0; 32];
    hasher.update(bytes.as_slice());
    hasher.finalize(&mut output);
    info!("program hash: 0x{}", hex::encode(output));
    
    // Configure RocksDB options for optimal performance
    let mut opts = Options::default();
    opts.create_if_missing(false);
    opts.set_max_open_files(10000);
    
    // Read-mostly optimizations
    opts.optimize_for_point_lookup(32 * 1024 * 1024);
    opts.increase_parallelism(4);
    opts.set_max_background_jobs(4);
    
    // Cache settings for reads
    opts.set_table_cache_num_shard_bits(6);
    opts.set_max_file_opening_threads(16);
    
    // Secondary instance specific settings
    opts.set_max_background_compactions(0); // No compactions needed for secondary
    opts.set_disable_auto_compactions(true);
    
    // Create secondary path if it doesn't exist
    std::fs::create_dir_all(&args.secondary_path)?;
    
    // Setup periodic catch-up with primary
    let secondary_path = args.secondary_path.clone();
    let db_path = args.db_path.clone();
    
    let opts_clone = opts.clone();
    let catch_up_interval = std::time::Duration::from_secs(1);
    actix_web::rt::spawn(async move {
        let mut interval = actix_web::rt::time::interval(catch_up_interval);
        loop {
            interval.tick().await;
            if let Ok(db) = rocksdb::DB::open_as_secondary(&opts_clone, &db_path, &secondary_path) {
                let _ = db.try_catch_up_with_primary(); // Ignore temporary errors
            }
        }
    });

    HttpServer::new(move || {
        App::new()
            .wrap(Cors::default().allowed_origin_fn(|origin, _| {
                if let Ok(origin_str) = origin.to_str() {
                    origin_str.starts_with("http://localhost:")
                } else {
                    false
                }
            }))
            .app_data(web::Data::new(Context {
                hash: output,
                program: bytes.clone(),
                runtime: MetashrewRuntime::load(
                    args.program_path.clone(),
                    RocksDBRuntimeAdapter::open_secondary(
                        args.db_path.clone(),
                        args.secondary_path.clone(),
                        opts.clone()
                    ).unwrap(),
                )
                .unwrap(),
            }))
            .service(jsonrpc_call)
    })
    .bind((args.host.as_str(), args.port))?
    .run()
    .await
}
