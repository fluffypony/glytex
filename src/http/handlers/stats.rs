use std::{collections::HashMap, time::Duration};

use axum::{extract::State, http::StatusCode, Json};
use log::{error, warn};
use serde::{Deserialize, Serialize};

use crate::http::{
    server::AppState,
    stats_collector::{AverageHashrate, GetHashrateResponse},
};

#[derive(Serialize)]
pub struct Stats {
    hashrate_per_device: HashMap<u32, AverageHashrate>,
    total_hashrate: AverageHashrate,
}

pub async fn handle_get_stats(State(state): State<AppState>) -> Result<Json<Stats>, StatusCode> {
    // Use shorter timeout for HTTP requests to prevent hanging
    let timeout = Duration::from_secs(3);
    
    let hashrate = state.stats_client.get_hashrate_with_timeout(timeout).await.map_err(|e| {
        if e.to_string().contains("timed out") {
            warn!("Stats request timed out after {:?} - possible collector hang", timeout);
            StatusCode::REQUEST_TIMEOUT
        } else {
            error!("Failed to get hashrate: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;
    
    let stats = Stats {
        hashrate_per_device: hashrate.devices,
        total_hashrate: hashrate.total,
    };
    Ok(Json(stats))
}
