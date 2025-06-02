// Copyright 2024 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

use std::time::Duration;
use axum::{extract::State, http::StatusCode, Json};
use log::{error, warn};
use serde::Serialize;

use crate::http::server::AppState;

#[derive(Serialize)]
pub struct HealthStatus {
    status: String,
    stats_collector_responsive: bool,
    stats_response_time_ms: Option<u64>,
}

pub async fn handle_health() -> Result<StatusCode, StatusCode> {
    Ok(StatusCode::OK)
}

/// Enhanced health check that verifies stats collector responsiveness
pub async fn handle_health_detailed(State(state): State<AppState>) -> Result<Json<HealthStatus>, StatusCode> {
    let start = std::time::Instant::now();
    
    // Test stats collector with very short timeout
    let stats_responsive = match state.stats_client.get_hashrate_with_timeout(Duration::from_millis(500)).await {
        Ok(_) => true,
        Err(e) => {
            if e.to_string().contains("timed out") {
                warn!("Stats collector unresponsive during health check");
            } else {
                error!("Stats collector error during health check: {}", e);
            }
            false
        }
    };
    
    let response_time = start.elapsed();
    let response_time_ms = if stats_responsive {
        Some(response_time.as_millis() as u64)
    } else {
        None
    };
    
    let status = if stats_responsive {
        "healthy".to_string()
    } else {
        "degraded".to_string()
    };
    
    Ok(Json(HealthStatus {
        status,
        stats_collector_responsive: stats_responsive,
        stats_response_time_ms: response_time_ms,
    }))
}
