use std::sync::Arc;
use std::time::Duration;

use axum::{routing::get, Router};
use log::{error, info, warn};
use tari_shutdown::ShutdownSignal;
use thiserror::Error;
use tokio::io;
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer};
use tower_http::timeout::TimeoutLayer;

use super::stats_collector::StatsClient;
use crate::{
    http::{
        config,
        handlers::{health, stats, version},
    },
    stats_store::StatsStore,
};

const LOG_TARGET: &str = "tari::gpuminer::server";

/// An HTTP server that provides stats and other useful information.
pub struct HttpServer {
    shutdown_signal: ShutdownSignal,
    config: config::Config,
    stats_client: StatsClient,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    IO(#[from] io::Error),
}

#[derive(Clone)]
pub struct AppState {
    pub stats_client: StatsClient,
}

impl HttpServer {
    pub fn new(shutdown_signal: ShutdownSignal, config: config::Config, stats_client: StatsClient) -> Self {
        Self {
            shutdown_signal,
            config,
            stats_client,
        }
    }

    pub fn routes(&self) -> Router {
        let router = Router::new()
            .route("/health", get(health::handle_health))
            .route("/health/detailed", get(health::handle_health_detailed))
            .route("/version", get(version::handle_version))
            .route("/stats", get(stats::handle_get_stats))
            .with_state(AppState {
                stats_client: self.stats_client.clone(),
            });

        // Add timeout and connection limit middleware to prevent hanging connections
        router.layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(self.config.request_timeout))
                .layer(ConcurrencyLimitLayer::new(self.config.max_connections))
                .into_inner(),
        )
    }

    /// Starts the http server on the port passed in ['HttpServer::new']
    pub async fn start(&self) -> Result<(), Error> {
        info!(target: LOG_TARGET, "HTTP server starting on port {} with timeouts enabled", self.config.port);
        info!(target: LOG_TARGET, "Request timeout: {:?}, Connection timeout: {:?}, Max connections: {}", 
              self.config.request_timeout, self.config.connection_timeout, self.config.max_connections);
        
        let router = self.routes();
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", self.config.port))
            .await
            .map_err(Error::IO)?;
            
        println!("🌐 HTTP server starting at http://127.0.0.1:{}", self.config.port);
        println!("   Request timeout: {:?}", self.config.request_timeout);
        println!("   Max connections: {}", self.config.max_connections);
        
        info!(target: LOG_TARGET, "HTTP listener bound to {:?}", listener.local_addr());
        
        // Configure axum server with graceful shutdown
        axum::serve(listener, router)
            .with_graceful_shutdown(self.shutdown_signal.clone())
            .await
            .map_err(Error::IO)?;
            
        println!("🛑 HTTP server stopped gracefully");
        info!(target: LOG_TARGET, "HTTP server stopped gracefully");
        Ok(())
    }
}
